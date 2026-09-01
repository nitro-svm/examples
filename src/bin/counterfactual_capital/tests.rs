use std::collections::BTreeMap;

use rstest::rstest;

use super::*;

fn arm(multiple: f64, built: u64, outcomes: &[(&str, u64)]) -> ArmRow {
    let scored = outcomes
        .iter()
        .find(|(key, _)| *key == FILLED)
        .map_or(0, |(_, count)| *count);
    ArmRow {
        multiple,
        tighten_bps: 0.0,
        scale: "all".to_owned(),
        frozen: true,
        vaults: Vec::new(),
        tiers: Vec::new(),
        ceiling: None,
        matched: built,
        built,
        scored,
        bps_total: 0,
        rejections: BTreeMap::new(),
        outcomes: outcomes
            .iter()
            .map(|(key, count)| ((*key).to_owned(), *count))
            .collect(),
    }
}

#[test]
fn fill_rate_is_the_share_of_buildable_hops_that_filled() {
    assert_eq!(arm(1.0, 400, &[(FILLED, 100)]).fill_rate(), 0.25);
}

#[test]
fn an_arm_that_built_nothing_has_a_zero_fill_rate_rather_than_a_division_by_zero() {
    assert_eq!(arm(1.0, 0, &[]).fill_rate(), 0.0);
}

#[test]
fn mean_bps_is_absent_rather_than_zero_when_nothing_scored() {
    assert_eq!(arm(1.0, 400, &[]).mean_bps(), None);
    let mut scored = arm(1.0, 400, &[(FILLED, 4)]);
    scored.bps_total = -280;
    assert_eq!(scored.mean_bps(), Some(-70.0));
}

#[test]
fn refusals_are_every_outcome_but_a_fill_largest_first() {
    let row = arm(
        1.0,
        500,
        &[(FILLED, 400), ("reverted", 80), ("unattributed", 20)],
    );
    assert_eq!(row.refusals(), vec![("reverted", 80), ("unattributed", 20)]);
    assert_eq!(row.outcome(FILLED), 400);
    assert_eq!(row.outcome("absent"), 0);
}

#[test]
fn an_arm_that_filled_everything_it_was_offered_has_no_refusals() {
    assert!(arm(1.0, 400, &[(FILLED, 400)]).refusals().is_empty());
}

#[rstest]
#[case(812_440_000_000, 9, "812.44")]
#[case(20_311_000_000_000, 9, "20311")]
#[case(1_500_000_000, 9, "1.50")]
#[case(91_204_110_000, 6, "91204")]
fn an_amount_renders_in_its_mints_units(
    #[case] base_units: u64,
    #[case] decimals: u8,
    #[case] expected: &str,
) {
    assert_eq!(format_amount(base_units, decimals), expected);
}

#[test]
fn an_address_shortens_to_its_ends_so_two_venues_stay_distinguishable() {
    assert_eq!(
        short("So11111111111111111111111111111111111111112"),
        "So1111..1112"
    );
    assert_eq!(short("short"), "short");
}
