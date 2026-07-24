use anyhow::Result;
use serde::{Deserialize, Serialize};
use solana_address::Address;
use solana_account_decoder::parse_token::UiTokenAmount;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status::InnerInstructions;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionTokenBalanceSerde {
    pub account_index: u8,
    pub mint: String,
    pub ui_token_amount: UiTokenAmount,
    pub owner: String,
    pub program_id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BalanceDiffs {
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub pre_token_balances: Option<Vec<TransactionTokenBalanceSerde>>,
    pub post_token_balances: Option<Vec<TransactionTokenBalanceSerde>>,
}

impl BalanceDiffs {
    pub fn token_balances_or_empty(
        &self,
    ) -> (
        &[TransactionTokenBalanceSerde],
        &[TransactionTokenBalanceSerde],
    ) {
        let empty: &[TransactionTokenBalanceSerde] = &[];
        (
            self.pre_token_balances.as_deref().unwrap_or(empty),
            self.post_token_balances.as_deref().unwrap_or(empty),
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TxWithMeta {
    pub transaction: VersionedTransaction,
    pub error: Option<TransactionError>,
    pub balance_diffs: Option<BalanceDiffs>,
    pub logs: Option<Vec<String>>,
    pub inner_instructions: Option<Vec<InnerInstructions>>,
}

// ── venue identifiers ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
pub enum TitanVenueDiscriminant {
    ZeroFi = 13,
    HumidiFi = 28,
    GoonFi = 35,
    BisonFi = 55,
    GoonFiV2 = 57,
    Tessera = 23,
}

impl TitanVenueDiscriminant {
    pub fn from_u8(disc: u8) -> Result<Self> {
        match disc {
            13 => Ok(Self::ZeroFi),
            23 => Ok(Self::Tessera),
            28 => Ok(Self::HumidiFi),
            35 => Ok(Self::GoonFi),
            55 => Ok(Self::BisonFi),
            57 => Ok(Self::GoonFiV2),
            other => anyhow::bail!("unknown venue discriminant: {other}"),
        }
    }

    /// Short lowercase venue name, used for default output filenames.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ZeroFi => "zerofi",
            Self::HumidiFi => "humidifi",
            Self::GoonFi => "goonfi",
            Self::BisonFi => "bisonfi",
            Self::GoonFiV2 => "goonfiv2",
            Self::Tessera => "tessera",
        }
    }

    pub fn get_program_id(self) -> Address {
        let program_id = match self {
            Self::ZeroFi => "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY",
            Self::HumidiFi => "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp",
            Self::GoonFi => "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j",
            Self::BisonFi => "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi",
            Self::GoonFiV2 => "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE",
            Self::Tessera => "TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH",
        };

        Address::from_str_const(program_id)
    }
}
