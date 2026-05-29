use std::collections::BTreeMap;

use anyhow::{Context, Result};
use simulator_api::{AccountData, AccountModifications, BinaryEncoding, EncodedBinary};
use simulator_client::BacktestSession;
use solana_address::Address;
use solana_pubkey::Pubkey;

use super::parse::{TOKEN_PROGRAM, WSOL_MINT, derive_ata};

pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

pub fn make_token_account(owner: &Address, mint: &str, amount: u64) -> Result<AccountData> {
    const RENT_EXEMPT: u64 = 2_039_280;
    let mut data = [0u8; 165];
    data[0..32].copy_from_slice(mint.parse::<Pubkey>()?.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    Ok(AccountData {
        data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
        executable: false,
        lamports: RENT_EXEMPT,
        owner: TOKEN_PROGRAM.parse()?,
        space: 165,
    })
}

async fn set_native_balance(session: &BacktestSession, owner: &Pubkey, amount: u64) -> Result<u64> {
    let addr: Address = owner.to_string().parse()?;
    let original = session.rpc().get_account(&addr).await.ok().map(|a| a.lamports).unwrap_or(0);
    const ATA_RENT: u64 = 2_039_280;
    const FEE: u64 = 1_000_000;
    session
        .modify_accounts(&AccountModifications(BTreeMap::from([(
            addr,
            AccountData {
                data: EncodedBinary::from_bytes(&[], BinaryEncoding::Base64),
                executable: false,
                lamports: amount.saturating_add(ATA_RENT).saturating_add(FEE),
                owner: SYSTEM_PROGRAM.parse()?,
                space: 0,
            },
        )])))
        .await
        .context("modify_accounts (native) failed")?;
    Ok(original)
}

async fn set_ata_balance(
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
    session
        .modify_accounts(&AccountModifications(BTreeMap::from([(
            ata.to_string().parse::<Address>()?,
            make_token_account(&owner_addr, mint, amount)?,
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
) -> Result<u64> {
    if mint == WSOL_MINT {
        set_native_balance(session, owner, amount).await
    } else {
        set_ata_balance(session, owner, mint, amount).await
    }
}
