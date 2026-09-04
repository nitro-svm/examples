//! Unit tests for the parts of `run` that decide what a number means: the venue tally's
//! per-direction split, the override schedule, and the price rewrite.

use super::*;
use simulator_api::{AccountData, AccountModifications};
use solana_address::Address;

use crate::{
    cli::RangeArgs,
    report::{VenueCounts, VenueTally, delta_bps, slimmed, slot_range, target_from_header},
    schedule::{build_overrides, reprice},
};
/// The totals are folded from the per-direction map, so a book whose two sides moved opposite
/// ways must still report each side — summing them is what hid the collapse.
#[test]
fn a_report_keeps_each_direction_apart() {
    let tally = VenueTally {
        by_direction: HashMap::from([
            (
                ("SOL".to_string(), "USDC".to_string()),
                VenueCounts {
                    legs: 29,
                    l1_legs: 515,
                    held: 20,
                    ..VenueCounts::default()
                },
            ),
            (
                ("USDC".to_string(), "SOL".to_string()),
                VenueCounts {
                    legs: 15581,
                    l1_legs: 741,
                    held: 300,
                    ..VenueCounts::default()
                },
            ),
        ]),
        ..VenueTally::default()
    };

    let report = tally.report();

    assert_eq!(report.total.legs, 15610);
    assert_eq!(report.total.l1_legs, 1256);
    let sides: Vec<_> = report
        .by_direction
        .iter()
        .map(|((input, _), counts)| (input.as_str(), counts.legs, counts.won(), counts.lost()))
        .collect();
    // Busiest first, and the collapsed side still visible with its own lost count.
    assert_eq!(
        sides,
        vec![("USDC", 15581, 15281, 441), ("SOL", 29, 9, 495)]
    );
}
use rstest::rstest;
use simulator_api::{BinaryEncoding, EncodedBinary, route_plan::RoutePlan};

fn account_key() -> Address {
    Address::from([9; 32])
}

fn state(slot: u64) -> AccountData {
    AccountData {
        data: EncodedBinary::new(String::new(), BinaryEncoding::Base64),
        executable: false,
        lamports: slot,
        owner: Address::from([8; 32]),
        space: 0,
    }
}

fn captured_at(slots: &[u64]) -> BTreeMap<u64, AccountData> {
    slots.iter().map(|slot| (*slot, state(*slot))).collect()
}

fn leg(quoted: u64) -> LegRecord {
    LegRecord {
        input_mint: "in".into(),
        output_mint: "out".into(),
        amount: 1,
        metis_quoted_out: quoted,
        original_quoted_out: quoted,
    }
}

/// (anchor slot, override payload lamports) per action.
fn schedule_of(actions: &[(u64, AccountModifications)]) -> Vec<(u64, u64)> {
    actions
        .iter()
        .map(|(slot, overrides)| (*slot, overrides.0[&account_key()].lamports))
        .collect()
}

#[test]
fn each_capture_is_published_at_its_own_slot() {
    let captured = captured_at(&[10, 12, 14]);
    let actions = build_overrides(account_key(), &captured, 10, 15);
    assert_eq!(schedule_of(&actions), vec![(10, 10), (12, 12), (14, 14)]);
}

/// Captures outside `[start, end]` belong to another run's range and must not be posted.
#[test]
fn captures_outside_the_range_are_dropped() {
    let captured = captured_at(&[8, 10, 12, 20]);
    let actions = build_overrides(account_key(), &captured, 10, 15);
    assert_eq!(schedule_of(&actions), vec![(10, 10), (12, 12)]);
}

#[test]
fn empty_capture_builds_nothing() {
    assert!(build_overrides(account_key(), &BTreeMap::new(), 10, 15).is_empty());
}

fn priced(fields: &[(usize, i64)]) -> AccountData {
    let mut raw = vec![0u8; 2048];
    for &(offset, value) in fields {
        raw[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    AccountData {
        data: EncodedBinary::from_bytes(&raw, BinaryEncoding::Base64),
        executable: false,
        lamports: 1,
        owner: Address::from([8; 32]),
        space: 2048,
    }
}

fn field_at(account: &AccountData, offset: usize) -> i64 {
    let raw = account.data.decode().expect("valid base64");
    i64::from_le_bytes(raw[offset..offset + 8].try_into().expect("eight bytes"))
}

/// Every named field moves by the same relative amount, whatever its fixed-point scale —
/// a venue storing one price twice must not end up disagreeing with itself.
#[rstest]
#[case::worse(-5.0)]
#[case::better(5.0)]
fn repricing_moves_each_field_by_the_same_bps(#[case] shift: f64) {
    // Deliberately different scales: Q32.32 and a decimal-ish one.
    let mut account = priced(&[(839, 323_315_220_502), (895, 1_059_430_871_808)]);
    let written = reprice(&mut account, &[839, 895], shift).expect("in range");
    assert_eq!(written, vec![839, 895]);
    for offset in [839, 895] {
        let before = field_at(
            &priced(&[(839, 323_315_220_502), (895, 1_059_430_871_808)]),
            offset,
        );
        let moved = (field_at(&account, offset) as f64 / before as f64 - 1.0) * 10_000.0;
        assert!((moved - shift).abs() < 0.01, "{offset}: {moved} vs {shift}");
    }
}

/// A mis-aimed offset lands on padding far more often than on a price; writing there would
/// read downstream as a re-priced venue that was never re-priced.
#[test]
fn repricing_leaves_non_positive_fields_alone() {
    let mut account = priced(&[(839, 0), (895, -7)]);
    let written = reprice(&mut account, &[839, 895], -5.0).expect("in range");
    assert!(written.is_empty(), "{written:?}");
    assert_eq!(field_at(&account, 839), 0);
    assert_eq!(field_at(&account, 895), -7);
}

#[test]
fn repricing_past_the_account_is_rejected() {
    let mut account = priced(&[(0, 1)]);
    assert!(reprice(&mut account, &[2044], -5.0).is_err());
}

#[test]
fn delta_bps_is_signed_scaled_and_undefined_on_zero_baseline() {
    assert_eq!(delta_bps(10_000, 10_100), Some(100.0));
    assert_eq!(delta_bps(10_000, 9_900), Some(-100.0));
    assert_eq!(delta_bps(10_000, 10_000), Some(0.0));
    assert_eq!(delta_bps(0, 42), None);
    // Sub-bps moves must survive: the router jitters well below one basis point.
    let sub_bps = delta_bps(10_000_000, 10_000_001).unwrap();
    assert!((sub_bps - 0.001).abs() < 1e-9, "{sub_bps}");
}

#[test]
fn join_legs_pairs_common_keys_and_excludes_zero_baselines() {
    let base = BTreeMap::from([
        (("sig1".into(), 0), leg(100)),
        (("sig2".into(), 0), leg(200)),
        (("sig4".into(), 0), leg(0)),
    ]);
    let modified = BTreeMap::from([
        (("sig1".into(), 0), leg(110)),
        (("sig3".into(), 0), leg(999)),
        (("sig4".into(), 0), leg(7)),
    ]);
    let (joined, zero_baseline) = join_legs(&base, &modified);
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0].delta_bps, 1000.0);
    assert_eq!(zero_baseline, 1);
}

#[test]
fn summary_percentiles_over_abs_deltas() {
    // abs-sorted [0, 50, 100, 200]: round-index median = idx round(1.5) = 100,
    // p90 = idx round(2.7) = 200.
    let stats = delta_summary(&[0.0, -100.0, 50.0, 200.0]);
    assert_eq!(stats.matched, 4);
    assert_eq!(stats.median_abs_bps, 100.0);
    assert_eq!(stats.p90_abs_bps, 200.0);
    assert_eq!(stats.mean_abs_bps, 87.5);
}

/// Both arms of one comparison land beside each other, neither overwriting the other.
#[rstest]
#[case::keeps_the_extension("reroute-out.jsonl", "control", "reroute-out-control.jsonl")]
#[case::without_one("reroute-out", "modified", "reroute-out-modified")]
#[case::keeps_the_directory("runs/arm.jsonl", "control", "runs/arm-control.jsonl")]
fn an_arm_path_suffixes_the_stem(#[case] base: &str, #[case] arm: &str, #[case] expected: &str) {
    assert_eq!(arm_path(Path::new(base), arm), PathBuf::from(expected));
}

/// The header round-trips, and stays distinguishable from the notifications after it.
#[test]
fn a_run_header_round_trips_and_names_itself() {
    let config = RunConfig {
        range: RangeArgs {
            start_slot: 100,
            slot_count: 50,
            no_replay: true,
        },
        schedule: Schedule::default(),
        filter: None,
        venue: None,
        jsonl_out: None,
        detect_failed_l1_swaps: false,
        circular_arbs: true,
        reroute_aggregators: None,
        price_shift_bps: Some(-0.4),
        record_full: false,
    };

    let line = serde_json::to_string(&config.header()).expect("header encodes");
    let read_back: RunHeader = serde_json::from_str(&line).expect("header decodes");

    assert_eq!(read_back.format_version, FORMAT_VERSION);
    assert_eq!(read_back.kind, HeaderKind::CounterfactualFlowRun);
    assert_eq!((read_back.start_slot, read_back.end_slot), (100, 150));
    assert_eq!(read_back.price_shift_bps, Some(-0.4));
    // Slim is the default, and the header says so rather than leaving it to be inferred.
    assert!(read_back.slim);
    // `--no-replay` is the negative of what the session is asked for.
    assert!(!read_back.replay_account_state);
    assert!(read_back.circular_arbs);

    // A notification must not parse as a header, or a reader would take the first row as one.
    assert!(serde_json::from_str::<RunHeader>(r#"{"slot":1,"legs":[]}"#).is_err());
}

/// Slim drops the two fields the analysis never reads, and nothing else. It empties rather than
/// removes them so every row still reads back as the wire type — which is the property the whole
/// format rests on.
#[test]
fn a_slim_row_keeps_everything_the_analysis_reads_and_still_round_trips() {
    let wire = serde_json::json!({
        "context": {"slot": 42},
        "slot": 42,
        "batchIndex": 3,
        "originalSignature": "sig",
        "legs": [{
            "inputMint": "in", "outputMint": "out", "amount": 1_000, "swapMode": "ExactIn",
            "originalQuotedOut": 990, "metisQuotedOut": 1_010, "routeSummary": "SolFi",
            "routePlan": [], "originalRoutePlan": [],
        }],
        "routedTransaction": {"data": "bGFyZ2UgYmxvYg==", "encoding": "base64"},
        "err": serde_json::Value::Null,
        "logs": ["Program log: one", "Program log: two"],
        "computeUnitsConsumed": 123_456,
        "fee": 5_000,
        "realizedOutputAmount": 1_009,
        "originalRealizedOutputAmount": 991,
    });
    let full: RerouteNotification = serde_json::from_value(wire).expect("wire decodes");

    let slim = slimmed(&full);
    assert!(slim.logs.is_empty());
    assert!(slim.routed_transaction.data.is_empty());

    // Everything the report reads survives.
    assert_eq!(slim.batch_index, 3);
    assert_eq!(slim.original_signature, "sig");
    assert_eq!(slim.legs[0].swap_mode, "ExactIn");
    assert_eq!(slim.legs[0].route_summary, "SolFi");
    assert!(
        slim.legs[0]
            .route_plan
            .as_ref()
            .is_some_and(RoutePlan::is_empty)
    );
    assert!(
        slim.legs[0]
            .original_route_plan
            .as_ref()
            .is_some_and(RoutePlan::is_empty)
    );
    assert_eq!(slim.original_realized_output_amount, Some(991));
    assert_eq!(slim.realized_output_amount, Some(1_009));

    // And the row still reads back as the wire type, which removing the keys would break.
    let line = serde_json::to_string(&slim).expect("slim encodes");
    let read_back: RerouteNotification = serde_json::from_str(&line).expect("slim decodes");
    assert_eq!(read_back.legs[0].amount, 1_000);
}

/// A recording whose header names no venue has no default, and must say so rather than guess.
#[test]
fn no_selector_measures_the_venue_the_run_named() {
    let header = |venue: &str| -> RunHeader {
        serde_json::from_str(&format!(
            r#"{{"formatVersion":1,"kind":"counterfactualFlowRun","startSlot":1,"endSlot":2,
                 {venue}"overrideSlots":0,"slim":true,
                 "rerouteVenues":null,"filterPairs":[],"circularArbs":false,
                 "detectFailedL1Swaps":true,"replayAccountState":true}}"#
        ))
        .expect("header decodes")
    };

    let named =
        header(r#""programId":"QuaNtZsgYRe5Z9Bk4LZ4cTD9tbkVoyCNf1R2BN9bBDv","label":"Quantum","#);
    let target = target_from_header(&named).expect("the run named a venue");
    assert_eq!(target.label(), Some("Quantum"));
    assert!(
        target.program().is_some(),
        "both keys, so both columns match"
    );

    assert!(target_from_header(&header("")).is_none());
    // The range is the recording's to supply; the report cannot know it.
    assert_eq!(
        slot_range(&Some(named)).as_deref(),
        Some("slots 1–2"),
        "the subtitle names the range the run covered"
    );
}
