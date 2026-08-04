//! Unit tests for the parts of `run` that decide what a number means: the venue tally's
//! per-direction split, the shift schedule, and the price rewrite.

use super::*;
use crate::{
    cli::{ConnectionArgs, RangeArgs, RunArgs, shift_label},
    jsonl::CaptureRow,
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
use simulator_api::{BinaryEncoding, EncodedBinary};

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

fn conn(url: &str) -> ConnectionArgs {
    ConnectionArgs {
        url: url.to_string(),
        api_key: String::new(),
    }
}

#[test]
fn websocket_url_defaults_to_tls_and_honours_an_explicit_scheme() {
    for (url, expected) in [
        ("sim.example.com", "wss://sim.example.com/backtest"),
        ("ws://localhost:8900", "ws://localhost:8900/backtest"),
        (
            "ws://localhost:8900/backtest",
            "ws://localhost:8900/backtest",
        ),
        ("wss://sim.example.com/", "wss://sim.example.com/backtest"),
    ] {
        assert_eq!(conn(url).websocket_url(), expected, "{url}");
    }
}

#[test]
fn rpc_url_follows_the_websocket_scheme_and_passes_absolute_endpoints_through() {
    for (url, endpoint, expected) in [
        (
            "sim.example.com",
            "/backtest/s1",
            "https://sim.example.com/backtest/s1",
        ),
        (
            "ws://localhost:8900",
            "backtest/s1",
            "http://localhost:8900/backtest/s1",
        ),
        (
            "ws://localhost:8900/backtest",
            "backtest/s1",
            "http://localhost:8900/backtest/s1",
        ),
        ("sim.example.com", "https://other/rpc", "https://other/rpc"),
    ] {
        assert_eq!(conn(url).rpc_url(endpoint), expected, "{url} + {endpoint}");
    }
}

#[test]
fn positive_shift_runs_each_capture_forward_by_k() {
    let captured = captured_at(&[10, 12, 14]);
    let actions = build_shift_actions(2, account_key(), &captured, 10, 15);
    // Slot 10 sees the slot-12 capture, 12 sees 14; the tail carries the last capture.
    assert_eq!(schedule_of(&actions), vec![(10, 12), (12, 14), (14, 14)]);
}

#[test]
fn negative_shift_lags_each_capture_behind_by_k() {
    let captured = captured_at(&[10, 12, 14]);
    let actions = build_shift_actions(-2, account_key(), &captured, 10, 15);
    // Slot 12 sees the slot-10 capture, 14 sees 12; slot 10 has nothing older to post.
    assert_eq!(schedule_of(&actions), vec![(12, 10), (14, 12)]);
}

#[test]
fn empty_capture_builds_nothing() {
    assert!(build_shift_actions(1, account_key(), &BTreeMap::new(), 10, 15).is_empty());
}

fn rows_at(slots: &[u64]) -> Vec<CaptureRow> {
    slots
        .iter()
        .map(|slot| CaptureRow {
            slot: *slot,
            account: state(*slot),
            signature: Some(format!("sig{slot}")),
            transaction: Some(format!("tx{slot}")),
        })
        .collect()
}

/// (anchor slot, transaction) pairs the action fires, in wire order.
fn setup_schedule_of(action: &ScheduledAction) -> Vec<(u64, &str)> {
    let ActionAnchor::BeforeSlot { slots } = &action.anchor else {
        panic!("setup actions anchor before a slot");
    };
    slots
        .iter()
        .copied()
        .zip(action.transactions.iter().map(String::as_str))
        .collect()
}

#[rstest]
// Each update runs K slots after it really did; slot 14's lands past the range.
#[case::lag(-2, vec![(12, "tx10"), (14, "tx12")])]
// And K slots before; slot 10's would land before the range starts.
#[case::lead(2, vec![(10, "tx12"), (12, "tx14")])]
fn setup_transactions_anchor_the_shift_in_the_opposite_direction(
    #[case] shift: i64,
    #[case] expected: Vec<(u64, &str)>,
) {
    let rows = rows_at(&[10, 12, 14]);
    let action = build_setup_action(shift, account_key(), &rows, 10, 15).expect("action");
    assert_eq!(setup_schedule_of(&action), expected);
    assert!(action.feeds_reroute);
    assert_eq!(action.return_accounts, vec![account_key()]);
}

#[test]
fn setup_transactions_skip_captures_with_no_transaction() {
    let mut rows = rows_at(&[10, 12]);
    rows[0].transaction = None;
    let action = build_setup_action(-2, account_key(), &rows, 10, 15).expect("action");
    assert_eq!(setup_schedule_of(&action), vec![(14, "tx12")]);
}

#[test]
fn setup_transactions_build_nothing_without_a_resolved_capture() {
    assert!(build_setup_action(-2, account_key(), &[], 10, 15).is_none());
}

#[test]
fn shift_prefers_lag_and_labels_by_direction() {
    let args = |lag, lead| RunArgs {
        conn: conn("sim.example.com"),
        range: RangeArgs {
            start_slot: 1,
            slot_count: 1,
            no_replay: true,
        },
        account: account_key(),
        capture: None,
        lag,
        lead,
        program_id: None,
        filter_pair: Vec::new(),
        skip_l1_failures: false,
        circular_arbs: false,
        reroute_venues: None,
        setup_transactions: false,
        price_field: Vec::new(),
        price_shift_bps: None,
        out: PathBuf::from("out.jsonl"),
    };
    assert_eq!(args(Some(10), None).shift(), Some(-10));
    assert_eq!(args(None, Some(10)).shift(), Some(10));
    assert_eq!(args(None, None).shift(), None);
    assert_eq!(args(Some(0), None).shift(), Some(0));
    assert_eq!(shift_label(-10), "lag 10");
    assert_eq!(shift_label(0), "null shift");
    assert_eq!(shift_label(10), "lead 10");
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
    let (joined, zero_baseline) = join_legs(-4, &base, &modified);
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0].delta_bps, 1000.0);
    assert_eq!(joined[0].shift, -4);
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
