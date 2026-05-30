use std::collections::BTreeMap;

use anyhow::{Context, Result};
use simulator_api::{AccountData, AccountModifications, BinaryEncoding, EncodedBinary};
use simulator_client::BacktestSession;
use solana_address::Address;
use solana_pubkey::Pubkey;

use super::parse::{TOKEN_PROGRAM, WSOL_MINT, derive_ata};

pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const ATA_RENT_EXEMPT: u64 = 2_039_280;
const FEE_BUFFER: u64 = 1_000_000;

pub fn make_token_account(owner: &Address, mint: &str, amount: u64) -> Result<AccountData> {
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
        owner: TOKEN_PROGRAM.parse()?,
        space: 165,
    })
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

    session
        .modify_accounts(&AccountModifications(BTreeMap::from([(
            addr,
            AccountData {
                data: EncodedBinary::from_bytes(&[], BinaryEncoding::Base64),
                executable: false,
                lamports: amount
                    .saturating_add(ATA_RENT_EXEMPT)
                    .saturating_add(FEE_BUFFER),
                owner: SYSTEM_PROGRAM.parse()?,
                space: 0,
            },
        )])))
        .await
        .context("modify_accounts (native) failed")?;
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
        make_token_account(&owner_addr, mint, amount)?
    };

    session
        .modify_accounts(&AccountModifications(BTreeMap::from([(
            ata.to_string().parse::<Address>()?,
            account_data,
        )])))
        .await
        .context("modify_accounts (ata) failed")?;

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
