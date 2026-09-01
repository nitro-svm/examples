use std::collections::BTreeMap;

use rstest::rstest;

use super::*;

fn arm(multiple: f64, built: u64, scored: u64, bps_total: i64) -> ArmRow {
    ArmRow {
        multiple,
        vaults: Vec::new(),
        matched: built,
        built,
        scored,
        bps_total,
        rejections: BTreeMap::new(),
        outcomes: BTreeMap::from([("filled".to_owned(), scored)]),
    }
}

#[test]
fn fill_rate_is_the_share_of_buildable_hops_that_filled() {
    assert_eq!(arm(1.0, 400, 100, 0).fill_rate(), 0.25);
}

#[test]
fn an_arm_that_built_nothing_has_a_zero_fill_rate_rather_than_a_division_by_zero() {
    assert_eq!(arm(1.0, 0, 0, 0).fill_rate(), 0.0);
}

#[test]
fn mean_bps_is_absent_rather_than_zero_when_nothing_scored() {
    assert_eq!(arm(1.0, 400, 0, 0).mean_bps(), None);
    assert_eq!(arm(1.0, 400, 4, -280).mean_bps(), Some(-70.0));
}

#[rstest]
#[case(1.0, "1x")]
#[case(0.1, "0.1x")]
#[case(25.0, "25x")]
fn a_multiple_renders_without_a_trailing_zero(#[case] multiple: f64, #[case] expected: &str) {
    assert_eq!(format_multiple(multiple), expected);
}

#[rstest]
#[case(812_440_000_000, 9, "812.44")]
#[case(20_311_000_000_000, 9, "20311")]
#[case(1_500_000_000, 9, "1.50")]
#[case(91_204_110_000, 6, "91204")]
fn an_amount_renders_in_its_mint_s_units(
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

mod arms {
    use clap::Parser;

    use crate::cli::Cli;

    /// `--multiple=<v>` rather than two tokens: a negative multiple is otherwise read as a flag.
    fn cli(multiples: &[&str]) -> Cli {
        let base = [
            "counterfactual_capital",
            "--api-key",
            "unused",
            "--start-slot",
            "438246141",
            "--market",
            "spec.json",
            "--vault",
            "4kHHmecPiRNJtAQug2cHFChovM4wqdBesWQckrXBWinD",
        ]
        .map(String::from);
        let extra = multiples.iter().map(|m| format!("--multiple={m}"));
        Cli::parse_from(base.into_iter().chain(extra))
    }

    #[test]
    fn the_default_ladder_carries_a_control_and_a_falsification_arm() {
        let arms = cli(&[]).arms().expect("defaults are valid");
        assert!(arms.contains(&1.0), "{arms:?}");
        assert!(arms.iter().any(|m| *m < 1.0), "{arms:?}");
    }

    #[test]
    fn arms_are_sorted_and_deduplicated() {
        assert_eq!(
            cli(&["5", "1", "2", "5"]).arms().expect("valid"),
            vec![1.0, 2.0, 5.0]
        );
    }

    #[test]
    fn a_ladder_without_the_control_is_rejected() {
        let error = cli(&["2", "5"]).arms().expect_err("no control");
        assert!(error.to_string().contains("must include 1.0"), "{error}");
    }

    #[test]
    fn a_ladder_may_include_a_multiple_below_one() {
        assert_eq!(cli(&["0.1", "1"]).arms().expect("valid"), vec![0.1, 1.0]);
    }

    #[rstest::rstest]
    #[case("0")]
    #[case("-1")]
    fn a_multiple_that_is_not_positive_is_rejected(#[case] multiple: &str) {
        assert!(cli(&[multiple, "1"]).arms().is_err());
    }
}
