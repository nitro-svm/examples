//! Scaling a venue's *curve*: the price/size ladder a prop AMM quotes from, which lives in the
//! venue's own state account rather than in its vaults.

use anyhow::{Context, Result, bail, ensure};
use simulator_api::{AccountData, BinaryEncoding, EncodedBinary};
use solana_account::Account;

use crate::plan::StateLayout;

const WHOLE_UNIT_BPS: f64 = 10_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tier {
    pub(crate) price: u128,
    pub(crate) size: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ladder {
    pub(crate) tiers: Vec<Tier>,
}

impl Ladder {
    /// The deepest tier's size, which bounds the largest quotable trade.
    pub(crate) fn top_size(&self) -> u128 {
        self.tiers.iter().map(|tier| tier.size).max().unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScaledState {
    pub(crate) before: Vec<Ladder>,
    pub(crate) after: Vec<Ladder>,
    pub(crate) account: AccountData,
}

/// Read every ladder the layout names. The format is recovered by diffing rather than documented,
/// so every offset lives in the plan file — a venue that upgrades its program can move it — and a
/// layout that disagrees with the account is refused rather than read past.
pub(crate) fn read_ladders(state: &Account, layout: &StateLayout) -> Result<Vec<Ladder>> {
    ensure!(
        state.data.len() == layout.len,
        "the venue's state account is {} bytes, but the plan describes a {}-byte layout — the \
         plan does not describe this account",
        state.data.len(),
        layout.len
    );
    if let Some(expected) = &layout.discriminator {
        let found = state
            .data
            .get(..expected.len())
            .context("the state account is shorter than its discriminator")?;
        ensure!(
            found == expected.as_bytes(),
            "the state account opens with {:?}, not the plan's {expected:?} — either the plan \
             names the wrong account or the venue's format changed",
            String::from_utf8_lossy(found)
        );
    }

    layout
        .ladders
        .iter()
        .enumerate()
        .map(|(side, ladder)| {
            let count = read_u128(&state.data, ladder.count, ladder.width)
                .with_context(|| format!("reading ladder {side}'s tier count"))?;
            let count = usize::try_from(count)
                .ok()
                .filter(|count| (1..=layout.max_tiers).contains(count))
                .with_context(|| {
                    format!(
                        "ladder {side} declares {count} tiers, outside the plan's 1..={} — the \
                         offset is probably not a tier count",
                        layout.max_tiers
                    )
                })?;
            let tiers = (0..count)
                .map(|i| {
                    let at = ladder.entries + ladder.stride * i;
                    Ok(Tier {
                        price: read_u128(&state.data, at, ladder.width)?,
                        size: read_u128(&state.data, at + ladder.width, ladder.width)?,
                    })
                })
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("reading ladder {side}'s tiers"))?;
            ensure!(
                tiers.iter().all(|tier| tier.price > 0 && tier.size > 0),
                "ladder {side} has a tier priced or sized at zero within its declared count, so \
                 the count and the entries disagree"
            );
            Ok(Ladder { tiers })
        })
        .collect()
}

/// Rewrite the state account: every tier's size scaled by `depth`, every tier's price moved toward
/// the other side by `tighten_bps`, and the vault balance mirrors scaled by `capital` so they stay
/// equal to the vaults.
///
/// `capital` decides whether a fill can be paid, `depth` whether it is offered at all, and
/// `tighten_bps` what it is offered at.
pub(crate) fn scale(
    state: &Account,
    layout: &StateLayout,
    vault_amounts: &[u64],
    capital: f64,
    depth: f64,
    tighten_bps: f64,
) -> Result<ScaledState> {
    ensure!(
        capital.is_finite() && capital > 0.0,
        "a capital multiple must be finite and positive, got {capital}"
    );
    ensure!(
        depth.is_finite() && depth > 0.0,
        "a depth multiple must be finite and positive, got {depth}"
    );
    ensure!(
        tighten_bps.is_finite() && (0.0..WHOLE_UNIT_BPS).contains(&tighten_bps),
        "a spread adjustment must be finite and within a whole unit, got {tighten_bps} bps"
    );
    ensure!(
        layout.balance_mirrors.is_empty() || layout.balance_mirrors.len() == vault_amounts.len(),
        "the plan names {} balance mirrors for {} vaults; each mirror belongs to one vault, so \
         name one per vault or none at all",
        layout.balance_mirrors.len(),
        vault_amounts.len()
    );
    // Without the refusal, `tighten` returns the ladders untouched and the arm reports a spread
    // change that never happened.
    ensure!(
        tighten_bps == 0.0 || layout.ladders.len() == 2,
        "a spread adjustment moves one side of the book toward the other, so it needs exactly two \
         ladders; this plan names {}. Scale depth instead, or describe both sides",
        layout.ladders.len()
    );

    let before = read_ladders(state, layout)?;
    let mut data = state.data.clone();

    for (mirror, amount) in layout.balance_mirrors.iter().zip(vault_amounts) {
        let found = read_u128(&data, *mirror, U64)? as u64;
        ensure!(
            found == *amount,
            "the mirror at byte {mirror} holds {found}, but its vault holds {amount}; the plan's \
             mirror offsets do not match this account"
        );
        write_u128(
            &mut data,
            *mirror,
            U64,
            scaled(u128::from(*amount), capital)?,
        )?;
    }

    let after = tighten(before.clone(), tighten_bps);
    let after = after
        .into_iter()
        .map(|ladder| {
            Ok(Ladder {
                tiers: ladder
                    .tiers
                    .into_iter()
                    .map(|tier| {
                        Ok(Tier {
                            price: tier.price,
                            size: scaled(tier.size, depth)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    for (ladder, scaled) in layout.ladders.iter().zip(&after) {
        for (i, tier) in scaled.tiers.iter().enumerate() {
            let at = ladder.entries + ladder.stride * i;
            write_u128(&mut data, at, ladder.width, tier.price)?;
            write_u128(&mut data, at + ladder.width, ladder.width, tier.size)?;
        }
    }

    Ok(ScaledState {
        before,
        after,
        account: AccountData {
            space: data.len() as u64,
            data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
            executable: state.executable,
            lamports: state.lamports,
            owner: state.owner.to_string().parse()?,
        },
    })
}

/// Move every tier's price toward the opposing side's price for the same tier, by `bps` of its own
/// price, never past the midpoint.
///
/// Direction-agnostic: which ladder is the bid and which the ask is the venue's business, so each
/// side is pulled toward the other and the spread narrows whichever way round they are.
fn tighten(mut ladders: Vec<Ladder>, bps: f64) -> Vec<Ladder> {
    // `scale` refuses a non-zero adjustment on anything but two ladders, so this arm is only
    // reached at 0 bps, where the loop below is the identity.
    let [a, b] = &mut ladders[..] else {
        return ladders;
    };
    let tiers = a.tiers.len().min(b.tiers.len());
    for i in 0..tiers {
        let (low, high) = match a.tiers[i].price <= b.tiers[i].price {
            true => (a.tiers[i].price, b.tiers[i].price),
            false => (b.tiers[i].price, a.tiers[i].price),
        };
        let mid = low + (high - low) / 2;
        let step = |price: u128| (price as f64 * bps / WHOLE_UNIT_BPS) as u128;
        let (low, high) = (
            low.saturating_add(step(low)).min(mid),
            high.saturating_sub(step(high)).max(mid),
        );
        match a.tiers[i].price <= b.tiers[i].price {
            true => (a.tiers[i].price, b.tiers[i].price) = (low, high),
            false => (a.tiers[i].price, b.tiers[i].price) = (high, low),
        }
    }
    ladders
}

const U64: usize = 8;

/// Saturation is an error rather than a clamp: an arm holding less than it claims would flatten the
/// top of the curve for arithmetic reasons.
fn scaled(value: u128, multiple: f64) -> Result<u128> {
    // Past 2^53 a round trip through f64 no longer returns what it was given, so the control arm
    // shortcuts rather than scaling by 1.0.
    if multiple == 1.0 {
        return Ok(value);
    }
    let scaled = value as f64 * multiple;
    if scaled >= u128::MAX as f64 {
        bail!(
            "scaling {value} by {multiple}x saturates, so this arm would hold less than it claims"
        );
    }
    Ok(scaled.round() as u128)
}

fn read_u128(data: &[u8], at: usize, width: usize) -> Result<u128> {
    let mut buf = [0u8; 16];
    let bytes = data
        .get(at..at + width)
        .with_context(|| format!("byte {at}..{} is past the account's end", at + width))?;
    buf[..width].copy_from_slice(bytes);
    Ok(u128::from_le_bytes(buf))
}

fn write_u128(data: &mut [u8], at: usize, width: usize, value: u128) -> Result<()> {
    let bytes = value.to_le_bytes();
    ensure!(
        bytes[width..].iter().all(|byte| *byte == 0),
        "{value} does not fit in the {width} bytes at {at}"
    );
    data.get_mut(at..at + width)
        .with_context(|| format!("byte {at}..{} is past the account's end", at + width))?
        .copy_from_slice(&bytes[..width]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::plan::LadderLayout;

    /// Tempest's own shape: two 1056-byte ladder regions, then the two balance mirrors.
    fn layout() -> StateLayout {
        StateLayout {
            account: "FQmFVQ7i8GCLqwE7EriUA4WQCvyCKxAeY4XUr55ZD6D7"
                .parse()
                .expect("an address"),
            discriminator: Some("tempest1".to_string()),
            len: 2385,
            max_tiers: 32,
            balance_mirrors: vec![2321, 2329],
            ladders: vec![
                LadderLayout {
                    count: 209,
                    entries: 225,
                    stride: 32,
                    width: 16,
                },
                LadderLayout {
                    count: 1265,
                    entries: 1281,
                    stride: 32,
                    width: 16,
                },
            ],
        }
    }

    /// The real sizes, and prices in the real relation: side A falls with size, side B rises.
    const SIZES: [u128; 4] = [149_167_091, 745_835_456, 2_983_341_826, 14_916_709_131];
    const PRICES_A: [u128; 4] = [75_288_388, 75_184_178, 75_106_247, 75_079_140];
    const PRICES_B: [u128; 4] = [75_304_200, 75_397_040, 75_486_944, 75_510_210];

    fn state(vault_amounts: [u64; 2]) -> Account {
        let layout = layout();
        let mut data = vec![0u8; layout.len];
        data[..8].copy_from_slice(b"tempest1");
        for (side, prices) in [PRICES_A, PRICES_B].into_iter().enumerate() {
            let ladder = &layout.ladders[side];
            write_u128(&mut data, ladder.count, ladder.width, 4).expect("in bounds");
            for (i, (price, size)) in prices.into_iter().zip(SIZES).enumerate() {
                let at = ladder.entries + ladder.stride * i;
                write_u128(&mut data, at, ladder.width, price).expect("in bounds");
                write_u128(&mut data, at + ladder.width, ladder.width, size).expect("in bounds");
            }
        }
        for (mirror, amount) in layout.balance_mirrors.iter().zip(vault_amounts) {
            write_u128(&mut data, *mirror, U64, u128::from(amount)).expect("in bounds");
        }
        Account {
            lamports: 17_490_480,
            data,
            owner: "2G71FYychcDbuNvMmVcFzU8J9N3giFvgp3a5PS2LmgZm"
                .parse()
                .expect("an address"),
            executable: false,
            rent_epoch: 0,
        }
    }

    const VAULTS: [u64; 2] = [6_022_573_969, 1_785_599_501];

    fn scale_at(capital: f64, depth: f64, tighten_bps: f64) -> ScaledState {
        scale(
            &state(VAULTS),
            &layout(),
            &VAULTS,
            capital,
            depth,
            tighten_bps,
        )
        .expect("scales")
    }

    #[test]
    fn the_ladder_is_read_back_as_the_account_declares_it() {
        let ladders = read_ladders(&state(VAULTS), &layout()).expect("reads");
        assert_eq!(ladders.len(), 2);
        assert_eq!(ladders[0].tiers.len(), 4);
        assert_eq!(ladders[0].tiers[0].price, PRICES_A[0]);
        assert_eq!(ladders[0].top_size(), SIZES[3]);
        assert_eq!(ladders[1].tiers[3].price, PRICES_B[3]);
    }

    #[rstest]
    #[case::identity(1.0)]
    #[case::ten(10.0)]
    #[case::hundred(100.0)]
    fn depth_scales_every_tier_size_and_leaves_every_price_alone(#[case] depth: f64) {
        let scaled = scale_at(1.0, depth, 0.0);
        for (before, after) in scaled.before.iter().zip(&scaled.after) {
            for (before, after) in before.tiers.iter().zip(&after.tiers) {
                assert_eq!(after.size, (before.size as f64 * depth).round() as u128);
                assert_eq!(after.price, before.price, "depth must not move a price");
            }
        }
    }

    /// On a venue that quotes from a ladder, funding the vaults without widening the ladder leaves
    /// every quote exactly where it was.
    #[test]
    fn capital_alone_leaves_the_whole_ladder_untouched() {
        let scaled = scale_at(100.0, 1.0, 0.0);
        assert_eq!(scaled.before, scaled.after);
    }

    #[test]
    fn depth_raises_the_quote_ceiling_by_the_same_multiple() {
        let scaled = scale_at(1.0, 100.0, 0.0);
        assert_eq!(scaled.after[0].top_size(), SIZES[3] * 100);
    }

    #[test]
    fn the_balance_mirrors_move_with_the_vaults() {
        let scaled = scale_at(10.0, 1.0, 0.0);
        let data = scaled.account.data.decode().expect("decodes");
        for (mirror, amount) in layout().balance_mirrors.iter().zip(VAULTS) {
            assert_eq!(
                read_u128(&data, *mirror, U64).expect("in bounds"),
                u128::from(amount) * 10
            );
        }
    }

    #[rstest]
    #[case::side_a_lower(PRICES_A, PRICES_B)]
    #[case::side_b_lower(PRICES_B, PRICES_A)]
    fn the_spread_narrows_whichever_side_is_higher(
        #[case] first: [u128; 4],
        #[case] second: [u128; 4],
    ) {
        let ladder = |prices: [u128; 4]| Ladder {
            tiers: prices
                .into_iter()
                .zip(SIZES)
                .map(|(price, size)| Tier { price, size })
                .collect(),
        };
        let before = vec![ladder(first), ladder(second)];
        let after = tighten(before.clone(), 5.0);
        for i in 0..4 {
            let width = |l: &[Ladder]| l[0].tiers[i].price.abs_diff(l[1].tiers[i].price);
            assert!(
                width(&after) < width(&before),
                "tier {i} did not narrow: {} -> {}",
                width(&before),
                width(&after)
            );
        }
    }

    #[test]
    fn tightening_past_the_spread_crosses_nothing() {
        let scaled = scale_at(1.0, 1.0, 500.0);
        for i in 0..4 {
            assert_eq!(
                scaled.after[0].tiers[i].price, scaled.after[1].tiers[i].price,
                "an over-tightened tier must rest at the midpoint, not cross it"
            );
        }
    }

    #[test]
    fn a_zero_adjustment_rewrites_the_account_byte_for_byte() {
        let original = state(VAULTS);
        let scaled = scale_at(1.0, 1.0, 0.0);
        assert_eq!(
            scaled.account.data.decode().expect("decodes"),
            original.data,
            "the control arm must post exactly what it read, or it is not a control"
        );
    }

    #[test]
    fn an_account_of_the_wrong_length_is_refused() {
        let mut short = state(VAULTS);
        short.data.truncate(2384);
        let error = read_ladders(&short, &layout()).expect_err("a short account must be refused");
        assert!(error.to_string().contains("2384 bytes"), "{error}");
    }

    #[test]
    fn an_account_with_the_wrong_discriminator_is_refused() {
        let mut wrong = state(VAULTS);
        wrong.data[..8].copy_from_slice(b"pancake1");
        let error = read_ladders(&wrong, &layout()).expect_err("a foreign account must be refused");
        assert!(error.to_string().contains("pancake1"), "{error}");
    }

    #[test]
    fn a_tier_count_outside_the_declared_maximum_is_refused() {
        let mut wrong = state(VAULTS);
        write_u128(&mut wrong.data, 209, 16, 33).expect("in bounds");
        let error = read_ladders(&wrong, &layout()).expect_err("an impossible count is refused");
        assert!(error.to_string().contains("33 tiers"), "{error}");
    }

    /// A venue that keeps no copies of its vault balances is a documented plan shape.
    #[test]
    fn a_venue_that_mirrors_no_balances_still_scales_its_ladder() {
        let bare = StateLayout {
            balance_mirrors: Vec::new(),
            ..layout()
        };
        let scaled = scale(&state(VAULTS), &bare, &VAULTS, 10.0, 10.0, 0.0).expect("scales");
        assert_eq!(scaled.after[0].top_size(), SIZES[3] * 10);
    }

    #[test]
    fn a_spread_adjustment_without_two_sides_is_refused_rather_than_ignored() {
        let one = StateLayout {
            ladders: vec![layout().ladders[0].clone()],
            ..layout()
        };
        let error = scale(&state(VAULTS), &one, &VAULTS, 1.0, 1.0, 5.0)
            .expect_err("a one-sided book cannot be tightened");
        assert!(error.to_string().contains("exactly two"), "{error}");
        assert!(scale(&state(VAULTS), &one, &VAULTS, 1.0, 10.0, 0.0).is_ok());
    }

    #[test]
    fn a_mirror_that_disagrees_with_its_vault_is_refused() {
        let error = scale(&state(VAULTS), &layout(), &[1, 2], 2.0, 2.0, 0.0)
            .expect_err("a mismatched mirror must be refused");
        assert!(error.to_string().contains("mirror at byte"), "{error}");
    }

    #[rstest]
    #[case::zero(0.0)]
    #[case::negative(-1.0)]
    #[case::not_a_number(f64::NAN)]
    fn a_capital_multiple_that_is_not_a_positive_number_is_refused(#[case] capital: f64) {
        assert!(scale(&state(VAULTS), &layout(), &VAULTS, capital, 1.0, 0.0).is_err());
    }

    #[test]
    fn a_spread_adjustment_of_a_whole_unit_is_refused() {
        assert!(scale(&state(VAULTS), &layout(), &VAULTS, 1.0, 1.0, 10_000.0).is_err());
    }
}
