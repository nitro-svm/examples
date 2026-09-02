//! Scaling a venue's inventory: the SPL token accounts a venue quotes against, rewritten to hold
//! a multiple of what they held, as an account override the direct-fill probe prices against.

use anyhow::{Result, bail, ensure};
use simulator_api::{AccountData, BinaryEncoding, EncodedBinary};
use solana_account::Account;

/// SPL token program, and its 2022 successor. A venue's vault is owned by one of them; anything
/// else reaching [`scale`] is a mis-specified vault.
pub(crate) use backtest_example::utils::parse::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};

/// The base token-account layout, which a 2022 account with extensions extends rather than
/// rearranges — so every offset below holds for both, and a shorter buffer is not a token account.
const TOKEN_ACCOUNT_LEN: usize = 165;
/// `amount`, a u64.
const AMOUNT: std::ops::Range<usize> = 64..72;
/// The `is_native` COption tag. Non-zero means the account is wrapped SOL, whose lamports the
/// runtime ties to its amount.
const IS_NATIVE_TAG: std::ops::Range<usize> = 109..113;
/// The rent-exempt reserve `is_native` carries when set: the lamports that are NOT the balance.
const NATIVE_RESERVE: std::ops::Range<usize> = 113..121;

/// The balance a token account holds, or `None` when the buffer is too short to be one.
pub(crate) fn amount(vault: &Account) -> Option<u64> {
    vault
        .data
        .get(AMOUNT)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
}

#[derive(Debug, Clone)]
pub(crate) struct ScaledVault {
    pub(crate) before: u64,
    pub(crate) after: u64,
    /// Wrapped SOL, whose lamports had to move with the amount.
    pub(crate) native: bool,
    /// Lamports the override carries, which for a native vault is not the account's own.
    pub(crate) lamports: u64,
    pub(crate) account: AccountData,
}

/// Rewrite `vault`'s amount to `multiple` times what it holds.
///
/// A native vault's lamports are raised by the same delta: the token program treats a wrapped-SOL
/// account's lamports above its rent-exempt reserve as the balance, so scaling `amount` alone
/// leaves the two disagreeing and every probe against it reverts.
pub(crate) fn scale(vault: &Account, multiple: f64) -> Result<ScaledVault> {
    ensure!(
        multiple.is_finite() && multiple > 0.0,
        "a capital multiple must be finite and positive, got {multiple}"
    );
    let owner = vault.owner.to_string();
    ensure!(
        owner == TOKEN_PROGRAM || owner == TOKEN_2022_PROGRAM,
        "vault is owned by {owner}, which is not a token program — it is not a token account, so \
         scaling its bytes as one would corrupt it"
    );
    ensure!(
        vault.data.len() >= TOKEN_ACCOUNT_LEN,
        "a token account is at least {TOKEN_ACCOUNT_LEN} bytes, got {}",
        vault.data.len()
    );

    let before = u64::from_le_bytes(vault.data[AMOUNT].try_into()?);
    let scaled = (before as f64) * multiple;
    if scaled >= u64::MAX as f64 {
        bail!(
            "scaling {before} by {multiple}x saturates u64, so this arm would hold less than it \
             claims; drop the multiple or pick a vault with room"
        );
    }
    let after = scaled.round() as u64;

    let native = u32::from_le_bytes(vault.data[IS_NATIVE_TAG].try_into()?) != 0;
    // The reserve is read rather than recomputed: a recomputed rent-exemption would silently move
    // the balance.
    let lamports = match native {
        true => u64::from_le_bytes(vault.data[NATIVE_RESERVE].try_into()?).saturating_add(after),
        false => vault.lamports,
    };

    let data = vault
        .data
        .iter()
        .copied()
        .enumerate()
        .map(|(i, byte)| match AMOUNT.contains(&i) {
            true => after.to_le_bytes()[i - AMOUNT.start],
            false => byte,
        })
        .collect::<Vec<_>>();

    Ok(ScaledVault {
        before,
        after,
        native,
        lamports,
        account: AccountData {
            space: data.len() as u64,
            data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
            executable: vault.executable,
            lamports,
            owner: vault.owner.to_string().parse()?,
        },
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use solana_account::Account;

    use super::*;

    fn vault(amount: u64, native: Option<u64>, owner: &str) -> Account {
        let mut data = vec![0u8; TOKEN_ACCOUNT_LEN];
        data[AMOUNT].copy_from_slice(&amount.to_le_bytes());
        if let Some(reserve) = native {
            data[IS_NATIVE_TAG].copy_from_slice(&1u32.to_le_bytes());
            data[NATIVE_RESERVE].copy_from_slice(&reserve.to_le_bytes());
        }
        Account {
            lamports: native.map_or(2_039_280, |reserve| reserve + amount),
            data,
            owner: owner.parse().expect("a token program address"),
            executable: false,
            rent_epoch: 0,
        }
    }

    #[rstest]
    #[case::identity(1.0, 1_000)]
    #[case::doubled(2.0, 2_000)]
    #[case::cut_to_a_tenth(0.1, 100)]
    #[case::scaled_up(25.0, 25_000)]
    fn a_multiple_scales_the_amount(#[case] multiple: f64, #[case] expected: u64) {
        let scaled = scale(&vault(1_000, None, TOKEN_PROGRAM), multiple).expect("scales");
        assert_eq!(scaled.after, expected);
        assert_eq!(scaled.before, 1_000);
        assert_eq!(
            u64::from_le_bytes(
                scaled.account.data.decode().expect("decodes")[AMOUNT]
                    .try_into()
                    .expect("eight bytes")
            ),
            expected
        );
    }

    #[test]
    fn a_native_vault_moves_its_lamports_with_its_amount() {
        let scaled = scale(&vault(1_000, Some(2_039_280), TOKEN_PROGRAM), 5.0).expect("scales");
        assert!(scaled.native);
        assert_eq!(scaled.after, 5_000);
        assert_eq!(scaled.lamports, 2_039_280 + 5_000);
    }

    #[test]
    fn a_non_native_vault_keeps_its_lamports() {
        let scaled = scale(&vault(1_000, None, TOKEN_PROGRAM), 5.0).expect("scales");
        assert!(!scaled.native);
        assert_eq!(scaled.lamports, 2_039_280);
    }

    #[test]
    fn saturating_a_multiple_is_an_error_rather_than_a_silent_clamp() {
        let error = scale(&vault(u64::MAX / 2, None, TOKEN_PROGRAM), 64.0)
            .expect_err("saturation must not be clamped");
        assert!(error.to_string().contains("saturates u64"), "{error}");
    }

    #[test]
    fn a_short_buffer_is_rejected_rather_than_indexed() {
        let mut short = vault(1_000, None, TOKEN_PROGRAM);
        short.data.truncate(64);
        let error = scale(&short, 2.0).expect_err("a short buffer must not be indexed");
        assert!(error.to_string().contains("at least 165 bytes"), "{error}");
    }

    #[test]
    fn an_account_owned_by_something_other_than_a_token_program_is_rejected() {
        let error = scale(&vault(1_000, None, "11111111111111111111111111111111"), 2.0)
            .expect_err("a non-token account must not be patched as one");
        assert!(error.to_string().contains("not a token program"), "{error}");
    }

    #[rstest]
    #[case::zero(0.0)]
    #[case::negative(-1.0)]
    #[case::not_a_number(f64::NAN)]
    #[case::infinite(f64::INFINITY)]
    fn a_multiple_that_is_not_a_positive_number_is_rejected(#[case] multiple: f64) {
        assert!(scale(&vault(1_000, None, TOKEN_PROGRAM), multiple).is_err());
    }

    #[test]
    fn scaling_leaves_every_byte_outside_the_amount_untouched() {
        let original = vault(1_000, Some(2_039_280), TOKEN_2022_PROGRAM);
        let scaled = scale(&original, 3.0).expect("scales");
        let after = scaled.account.data.decode().expect("decodes");
        let differs = (0..TOKEN_ACCOUNT_LEN)
            .filter(|i| after[*i] != original.data[*i])
            .collect::<Vec<_>>();
        assert!(
            differs.iter().all(|i| AMOUNT.contains(i)),
            "bytes outside the amount changed: {differs:?}"
        );
    }
}
