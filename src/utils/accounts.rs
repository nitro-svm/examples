use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use simulator_api::{AccountData, AccountModifications, BinaryEncoding, EncodedBinary};
use simulator_client::BacktestSession;
use solana_address::Address;
use solana_pubkey::Pubkey;

use super::parse::{
    TITAN_PROGRAM, WSOL_MINT, derive_ata, get_account_info, get_mint_token_program,
};

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

/// A real token account's exact on-chain bytes/lamports/owner, so overrides can preserve
/// Token-2022 extensions (e.g. `TransferHookAccount`) a synthetic [`make_token_account`]
/// can't replicate.
pub async fn fetch_real_token_account(ata: &Pubkey) -> Result<Option<(Vec<u8>, u64, Address)>> {
    let Some((data, lamports, owner)) = get_account_info(&ata.to_string()).await? else {
        return Ok(None);
    };
    Ok(Some((
        data,
        lamports,
        owner.parse().context("parse owner")?,
    )))
}

/// Build an account override for a known-real token account, patching only the amount
/// field (offset 64) and leaving every other byte (state, extensions) untouched.
pub fn patch_real_token_account(real: &(Vec<u8>, u64, Address), amount: u64) -> AccountData {
    let (data, lamports, owner) = real;
    let mut data = data.clone();
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    AccountData {
        space: data.len() as u64,
        data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
        executable: false,
        lamports: *lamports,
        owner: *owner,
    }
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

async fn set_native_balance(session: &BacktestSession, owner: &Pubkey, amount: u64) -> Result<u64> {
    let addr: Address = owner.to_string().parse()?;
    let original = session
        .rpc()
        .get_account(&addr)
        .await
        .ok()
        .map(|a| a.lamports)
        .unwrap_or(0);

    let mods = AccountModifications(BTreeMap::from([(addr, make_native_account(amount))]));
    modify_with_retry(session, &mods, "modify_accounts (native)").await?;
    Ok(original)
}

pub async fn set_ata_balance(
    session: &BacktestSession,
    owner: &Pubkey,
    mint: &str,
    amount: u64,
) -> Result<u64> {
    let ata = derive_ata(owner, mint).context("derive_ata failed")?;
    let owner_addr: Address = owner.to_string().parse()?;
    let original = session
        .rpc()
        .get_account(&ata)
        .await
        .ok()
        .filter(|a| a.data.len() >= 72)
        .map(|a| u64::from_le_bytes(a.data[64..72].try_into().unwrap()))
        .unwrap_or(0);

    let account_data = if amount == 0 {
        // amount=0 means the account shouldn't exist.
        // zero it out so there's not a stale empty token account.
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

    let mods = AccountModifications(BTreeMap::from([(
        ata.to_string().parse::<Address>()?,
        account_data,
    )]));
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
