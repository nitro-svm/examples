//! Build a direct-fill market spec for an Orca Whirlpool from the pool account alone.
//!
//! Everything the venue's account run needs is either fixed, derivable from the pool, or read out
//! of it: the vaults and mints are fields, and the tick arrays and oracle are PDAs. Harvesting a
//! run out of a landed Titan transaction works, but pins whichever tick arrays that route happened
//! to use — a one-directional window that reverts every swap going the other way. Deriving instead
//! gives a window centred on the price being replayed, which serves both directions.

use anyhow::{Context, Result, bail};
use simulator_api::{
    DirectFillAccount, DirectFillMarket, DirectFillParams, MintPair, SwapAggregator,
};
use solana_address::Address;

use super::parse::{TOKEN_PROGRAM, derive_ata_with_program};
use solana_pubkey::Pubkey;

/// Orca's Whirlpool program.
pub const WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
/// Titan's routing authority, whose ATAs the route moves funds through.
pub const TITAN_ATLAS: &str = "D5YqVMoSxnqeZAKAUUE1Dm3bmjtdxQ5DCF356ozqN9cM";
const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// Ticks a single `TickArray` spans, per unit of the pool's spacing.
const TICKS_PER_ARRAY: i32 = 88;

/// Byte offsets into a `Whirlpool` account. Cross-checked against a live pool: `tick_current_index`
/// agrees with `sqrt_price` (price = (sqrt_price / 2^64)^2 = 1.0001^tick).
mod offset {
    pub const TICK_SPACING: usize = 41;
    pub const TICK_CURRENT_INDEX: usize = 81;
    pub const TOKEN_MINT_A: usize = 101;
    pub const TOKEN_VAULT_A: usize = 133;
    pub const TOKEN_MINT_B: usize = 181;
    pub const TOKEN_VAULT_B: usize = 213;
}

/// The fields of a `Whirlpool` this needs.
#[derive(Debug, Clone)]
pub struct Whirlpool {
    pub address: Pubkey,
    pub tick_spacing: u16,
    pub tick_current_index: i32,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
}

impl Whirlpool {
    pub fn decode(address: &Pubkey, owner: &Pubkey, data: &[u8]) -> Result<Self> {
        if owner.to_string() != WHIRLPOOL_PROGRAM {
            bail!("{address} is not owned by the Whirlpool program");
        }
        let key = |at: usize| -> Result<Pubkey> {
            let bytes: [u8; 32] = data
                .get(at..at + 32)
                .context("whirlpool account is short")?
                .try_into()?;
            Ok(Pubkey::from(bytes))
        };
        let u16_at = |at: usize| -> Result<u16> {
            Ok(u16::from_le_bytes(
                data.get(at..at + 2).context("short")?.try_into()?,
            ))
        };
        Ok(Self {
            address: *address,
            tick_spacing: u16_at(offset::TICK_SPACING)?,
            tick_current_index: i32::from_le_bytes(
                data.get(offset::TICK_CURRENT_INDEX..offset::TICK_CURRENT_INDEX + 4)
                    .context("short")?
                    .try_into()?,
            ),
            token_mint_a: key(offset::TOKEN_MINT_A)?,
            token_vault_a: key(offset::TOKEN_VAULT_A)?,
            token_mint_b: key(offset::TOKEN_MINT_B)?,
            token_vault_b: key(offset::TOKEN_VAULT_B)?,
        })
    }

    /// Ticks one array spans for this pool.
    pub const fn ticks_per_array(&self) -> i32 {
        TICKS_PER_ARRAY * self.tick_spacing as i32
    }

    /// The start index of the array holding `tick`, flooring toward negative infinity so it stays
    /// correct below zero — where SOL/USDC lives.
    pub fn array_start(&self, tick: i32) -> i32 {
        let per = self.ticks_per_array();
        (tick as f64 / per as f64).floor() as i32 * per
    }

    /// The `TickArray` PDA for a start index.
    fn tick_array(&self, start: i32) -> Result<Pubkey> {
        let program: Pubkey = WHIRLPOOL_PROGRAM.parse()?;
        Ok(Pubkey::find_program_address(
            &[
                b"tick_array",
                self.address.as_ref(),
                start.to_string().as_bytes(),
            ],
            &program,
        )
        .0)
    }

    /// The pool's oracle PDA. Often uninitialised; the route passes it through regardless.
    pub fn oracle(&self) -> Result<Pubkey> {
        let program: Pubkey = WHIRLPOOL_PROGRAM.parse()?;
        Ok(Pubkey::find_program_address(&[b"oracle", self.address.as_ref()], &program).0)
    }

    /// Three arrays centred on `tick`: one below, the one holding it, one above. A route harvested
    /// from a landed transaction carries a one-sided window instead, which fills in that direction
    /// and reverts in the other.
    pub fn tick_array_window(&self, tick: i32) -> Result<[Pubkey; 3]> {
        let per = self.ticks_per_array();
        let start = self.array_start(tick);
        Ok([
            self.tick_array(start - per)?,
            self.tick_array(start)?,
            self.tick_array(start + per)?,
        ])
    }
}

/// The venue's account run, in the order Titan forwards it to the Whirlpool CPI, and the mint
/// ordering the direction byte indexes into.
///
/// `mints` is `[token_mint_b, token_mint_a]` — the REVERSE of the pool's own ordering. The router
/// encodes direction as the input's index into this array and Whirlpools reads that as `a_to_b`,
/// so listing the pool's own order inverts every probe. That failure is silent: the CPI succeeds,
/// the transaction succeeds, and the hop moves nothing, so every probe records a fill of zero.
pub fn direct_fill_params(
    pool: &Whirlpool,
    tick: i32,
    slippage_bps: u16,
) -> Result<DirectFillParams> {
    let arrays = pool.tick_array_window(tick)?;
    let atlas: Pubkey = TITAN_ATLAS.parse()?;
    let ata = |mint: &Pubkey| {
        derive_ata_with_program(&atlas, &mint.to_string(), TOKEN_PROGRAM)
            .context("deriving the route's token account")
    };
    let account = |key: Pubkey, writable: bool| DirectFillAccount {
        address: Address::from(key.to_bytes()),
        writable,
    };
    let named = |key: &str, writable: bool| -> Result<DirectFillAccount> {
        Ok(account(key.parse()?, writable))
    };
    Ok(DirectFillParams {
        aggregator: SwapAggregator::Titan,
        venue: "Whirlpools".to_string(),
        pair: MintPair::new(
            Address::from(pool.token_mint_a.to_bytes()),
            Address::from(pool.token_mint_b.to_bytes()),
        ),
        slippage_bps,
        market: DirectFillMarket {
            mints: [
                Address::from(pool.token_mint_b.to_bytes()),
                Address::from(pool.token_mint_a.to_bytes()),
            ],
            accounts: vec![
                named(TOKEN_PROGRAM, false)?,
                named(TOKEN_PROGRAM, false)?,
                named(MEMO_PROGRAM, false)?,
                named(TITAN_ATLAS, true)?,
                account(pool.address, true),
                account(pool.token_mint_a, false),
                account(pool.token_mint_b, false),
                account(ata(&pool.token_mint_a)?, true),
                account(pool.token_vault_a, true),
                account(ata(&pool.token_mint_b)?, true),
                account(pool.token_vault_b, true),
                account(arrays[0], true),
                account(arrays[1], true),
                account(arrays[2], true),
                account(pool.oracle()?, true),
                named(WHIRLPOOL_PROGRAM, false)?,
            ],
        },
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The SOL/USDC splash pool, whose arrays a landed route named, so the derivation can be
    /// checked against addresses that are known-good rather than against itself.
    fn splash_pool() -> Whirlpool {
        Whirlpool {
            address: "BSddxwYW73as8852ZTHRH13pbZEmZ96NBjayc5mSVtkZ"
                .parse()
                .unwrap(),
            tick_spacing: 32896,
            tick_current_index: -22905,
            token_mint_a: "So11111111111111111111111111111111111111112"
                .parse()
                .unwrap(),
            token_vault_a: "F4CAXDT5v7F7XpqPqKqPbBpj2uuncGHXsXg9xD3CCejA"
                .parse()
                .unwrap(),
            token_mint_b: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                .parse()
                .unwrap(),
            token_vault_b: "BHqCWgG33QpPb17ryW5Hbfc4oP18UFcZ1GQdB5aAqu48"
                .parse()
                .unwrap(),
        }
    }

    /// Offsets the landed route named, and the two the earlier upward-window spec did.
    #[rstest]
    #[case::two_below(-2, "3jocSr5NKtdPeQBMxXSg5BYCqW8nLimZ6ih66yd2WV6D")]
    #[case::one_below(-1, "2cwp4s6EgmfLXFA2siHh2o38RLmbCzLNKtK54YQs5Sde")]
    #[case::holds_current_tick(0, "HguvUisdT1aHjFoJQYAuqtD3d93uL1QvvMAajDFK3dZi")]
    #[case::one_above(1, "93u8nsxaTeMfzCN36q6HeyaubUKFMrmL2wPKeYvUHZQU")]
    #[case::two_above(2, "6AtrQyT7VAdX3KE7fxPxx9GGCNvQEjQBHcqfmHCLREBC")]
    fn tick_arrays_match_a_landed_route(#[case] offset: i32, #[case] expected: &str) {
        let pool = splash_pool();
        let at = pool.array_start(pool.tick_current_index) + offset * pool.ticks_per_array();
        assert_eq!(pool.tick_array(at).unwrap().to_string(), expected);
    }

    #[test]
    fn the_oracle_matches_a_landed_route() {
        assert_eq!(
            splash_pool().oracle().unwrap().to_string(),
            "A4GJjZTtFc2TnwpAuEYDqcmnUSqruEkrEexocQ1b8hVp"
        );
    }

    /// The route's own token accounts are the atlas authority's ATAs, identical for every taker,
    /// which is why the run can be tabulated at all.
    #[test]
    fn the_run_names_the_routers_own_token_accounts() {
        let params = direct_fill_params(&splash_pool(), -22_905, 50).unwrap();
        let at = |i: usize| params.market.accounts[i].address.to_string();
        assert_eq!(at(7), "EW9diL91VgHY5i9qYScz53W3PihQPSnPoMtAKCo1Bs7J");
        assert_eq!(at(9), "3nnVbsCfN1mwUk2XSLCnjzY3bDDTdzpnKjvRmd8nESS2");
        assert_eq!(at(15), WHIRLPOOL_PROGRAM, "the CPI target ends the run");
    }

    /// Reversed relative to the pool: the direction byte is read as `a_to_b`.
    #[test]
    fn mints_are_reversed_relative_to_the_pool() {
        let pool = splash_pool();
        let params = direct_fill_params(&pool, pool.tick_current_index, 50).unwrap();
        assert_eq!(
            params.market.mints[0].to_string(),
            pool.token_mint_b.to_string()
        );
        assert_eq!(
            params.market.mints[1].to_string(),
            pool.token_mint_a.to_string()
        );
    }

    /// The window floors below zero, where SOL/USDC lives, and follows the pool's spacing.
    #[test]
    fn the_window_floors_toward_negative_infinity() {
        let pool = Whirlpool {
            tick_spacing: 4,
            ..splash_pool()
        };
        assert_eq!(pool.ticks_per_array(), 352);
        assert_eq!(pool.array_start(-25_709), -26_048);
    }
}
