use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use simulator_api::{AccountData, AccountModifications, BinaryEncoding, EncodedBinary};
use simulator_client::BacktestSession;
use solana_account::Account;
use solana_address::Address;
use solana_pubkey::Pubkey;

use super::chain::{get_account_info, get_mint_token_program};
use super::parse::{TITAN_PROGRAM, WSOL_MINT, derive_ata};

pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const ATA_RENT_EXEMPT: u64 = 2_039_280;
const FEE_BUFFER: u64 = 1_000_000;

/// Anchor discriminator of Titan's `TokenLedger` account (sha256("account:TokenLedger")[..8]).
const TOKEN_LEDGER_DISCRIMINATOR: [u8; 8] = [156, 247, 9, 188, 54, 108, 85, 77];
/// Rent-exempt minimum for the 48-byte ledger account: (48 + 128) * 6960.
const LEDGER_RENT_EXEMPT: u64 = 1_224_960;

/// Build a Titan `TokenLedger` account that snapshots `tracked` at `amount`.
/// Layout: 8-byte discriminator + 32-byte tracked token account + 8-byte u64 amount.
/// A `swap_route_v3` given this account computes `input = balance(tracked) - amount`,
/// so pre-seeding `amount = 0` (with `tracked` starting empty) makes the next swap
/// consume exactly what the previous swap deposited — no runtime amount needed.
pub fn make_token_ledger_account(tracked: &Address, amount: u64) -> AccountData {
    let mut data = [0u8; 48];
    data[0..8].copy_from_slice(&TOKEN_LEDGER_DISCRIMINATOR);
    data[8..40].copy_from_slice(tracked.as_ref());
    data[40..48].copy_from_slice(&amount.to_le_bytes());
    AccountData {
        data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
        executable: false,
        lamports: LEDGER_RENT_EXEMPT,
        owner: TITAN_PROGRAM.parse().expect("Should parse titan program"),
        space: 48,
    }
}

/// The staging simulator intermittently returns an empty HTTP body (EOF while
/// decoding), which fails an otherwise-valid `modify_accounts`. Retry with backoff
/// so a single blip doesn't drop a measurement; only give up if it keeps failing.
async fn modify_with_retry(
    session: &BacktestSession,
    mods: &AccountModifications,
    label: &'static str,
) -> Result<()> {
    const ATTEMPTS: u32 = 5;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=ATTEMPTS {
        match session.modify_accounts(mods).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                let e = anyhow::Error::new(e);
                eprintln!("  [retry] {label} attempt {attempt}/{ATTEMPTS}: {e}");
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(300 * attempt as u64)).await;
            }
        }
    }
    Err(last.unwrap()).with_context(|| format!("{label} failed after {ATTEMPTS} attempts"))
}

/// `token_program` must be the mint's real owning program (legacy Token or Token-2022) —
/// overriding a Token-2022 account with a legacy-Token owner breaks any CPI into it.
pub fn make_token_account(
    owner: &Address,
    mint: &str,
    amount: u64,
    token_program: &str,
) -> Result<AccountData> {
    let mut data = [0u8; 165];
    data[0..32].copy_from_slice(mint.parse::<Pubkey>()?.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // state = Initialized
    let mut lamports = ATA_RENT_EXEMPT;

    if mint == WSOL_MINT {
        // is_native = Some(RENT_EXEMPT): bytes [109..113] = option tag, [113..121] = value.
        // Without this, Token::SyncNative rejects the account as non-native.
        data[109..113].copy_from_slice(&1u32.to_le_bytes());
        data[113..121].copy_from_slice(&ATA_RENT_EXEMPT.to_le_bytes());
        lamports += amount;
    }

    Ok(AccountData {
        data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
        executable: false,
        lamports,
        owner: token_program.parse().context("parse token program")?,
        space: 165,
    })
}

/// Build an account override for a token 2022 account, patching only the amount
/// field (offset 64) and leaving every other byte (state, extensions) untouched.
pub fn patch_real_token22_account(account: &Account, amount: u64) -> Result<AccountData> {
    let mut data = account.data.clone();
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    Ok(AccountData {
        space: data.len() as u64,
        data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
        executable: account.executable,
        lamports: account.lamports,
        owner: account.owner.to_string().parse().context("parse owner")?,
    })
}

/// Lamports a [`make_native_account`] seeds for `amount`: the requested amount
/// plus the rent-exempt minimum and a fee buffer. Reading a native account's
/// balance back as a swap output must subtract *this*, not just `amount`, or the
/// padding leaks into the reading as a constant offset.
pub fn native_seed_lamports(amount: u64) -> u64 {
    amount
        .saturating_add(ATA_RENT_EXEMPT)
        .saturating_add(FEE_BUFFER)
}

pub fn make_native_account(amount: u64) -> AccountData {
    AccountData {
        data: EncodedBinary::from_bytes(&[], BinaryEncoding::Base64),
        executable: false,
        lamports: native_seed_lamports(amount),
        owner: SYSTEM_PROGRAM.parse().expect("Should parse system program"),
        space: 0,
    }
}

/// Address + account data for a native SOL override at `amount`.
pub fn native_override(owner: &Pubkey, amount: u64) -> Result<(Address, AccountData)> {
    let addr: Address = owner.to_string().parse().context("parse owner")?;
    Ok((addr, make_native_account(amount)))
}

/// Address + account data for `owner`'s ATA of `mint` at `amount`. `amount = 0`
/// zeroes the account out (it "shouldn't exist") rather than leaving a stale
/// empty token account.
pub async fn token_override(
    owner: &Pubkey,
    mint: &str,
    amount: u64,
) -> Result<(Address, AccountData)> {
    let ata = derive_ata(owner, mint).context("derive_ata failed")?;
    let owner_addr: Address = owner.to_string().parse()?;

    let account_data = if amount == 0 {
        AccountData {
            data: EncodedBinary::from_bytes(&[], BinaryEncoding::Base64),
            executable: false,
            lamports: 0,
            owner: SYSTEM_PROGRAM.parse()?,
            space: 0,
        }
    } else {
        let token_program = get_mint_token_program(mint).await?;
        make_token_account(&owner_addr, mint, amount, &token_program)?
    };

    Ok((ata.to_string().parse::<Address>()?, account_data))
}

/// Address + account data for `owner`'s ATA of `mint`, initialized but with a
/// zero balance — for reading back a swap's output without erasing an ATA that
/// already exists on-chain (unlike [`token_override`]'s `amount = 0`, which
/// wipes it to "doesn't exist" and breaks any tx that doesn't recreate it).
pub async fn empty_token_override(owner: &Pubkey, mint: &str) -> Result<(Address, AccountData)> {
    let ata = derive_ata(owner, mint).context("derive_ata failed")?;
    let owner_addr: Address = owner.to_string().parse()?;
    let token_program = get_mint_token_program(mint).await?;
    Ok((
        ata.to_string().parse::<Address>()?,
        make_token_account(&owner_addr, mint, 0, &token_program)?,
    ))
}

/// Address + account data funding `owner` with `amount` of `mint`: native SOL
/// when `mint` is WSOL and `set_native` (e.g. a wrap-and-swap transaction that
/// funds its own wSOL ATA), otherwise a token account override.
pub async fn account_override(
    owner: &Pubkey,
    mint: &str,
    amount: u64,
    set_native: bool,
) -> Result<(Address, AccountData)> {
    if mint == WSOL_MINT && set_native {
        native_override(owner, amount)
    } else {
        token_override(owner, mint, amount).await
    }
}

async fn set_native_balance(session: &BacktestSession, owner: &Pubkey, amount: u64) -> Result<u64> {
    let (addr, account_data) = native_override(owner, amount)?;
    let original = session
        .rpc()
        .get_account(&addr)
        .await
        .ok()
        .map(|a| a.lamports)
        .unwrap_or(0);

    let mods = AccountModifications(BTreeMap::from([(addr, account_data)]));
    modify_with_retry(session, &mods, "modify_accounts (native)").await?;
    Ok(original)
}

pub async fn set_ata_balance(
    session: &BacktestSession,
    owner: &Pubkey,
    mint: &str,
    amount: u64,
) -> Result<u64> {
    let (ata, account_data) = token_override(owner, mint, amount).await?;
    let original = session
        .rpc()
        .get_account(&ata)
        .await
        .ok()
        .filter(|a| a.data.len() >= 72)
        .map(|a| u64::from_le_bytes(a.data[64..72].try_into().unwrap()))
        .unwrap_or(0);

    let mods = AccountModifications(BTreeMap::from([(ata, account_data)]));
    modify_with_retry(session, &mods, "modify_accounts (ata)").await?;

    Ok(original)
}

pub async fn set_account_balance(
    session: &BacktestSession,
    owner: &Pubkey,
    mint: &str,
    amount: u64,
    set_native: bool,
) -> Result<u64> {
    if mint == WSOL_MINT && set_native {
        set_native_balance(session, owner, amount).await
    } else {
        set_ata_balance(session, owner, mint, amount).await
    }
}
