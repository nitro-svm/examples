use serde::{Deserialize, Serialize};
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
    ) -> (&[TransactionTokenBalanceSerde], &[TransactionTokenBalanceSerde]) {
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
