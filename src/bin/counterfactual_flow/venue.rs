use anyhow::{Context, Result};
use serde::{Deserialize, de::DeserializeOwned};
use simulator_client::RerouteLegNotification;
use solana_address::Address;

const PROGRAM_ID_TO_LABEL_URL: &str = "https://lite-api.jup.ag/swap/v1/program-id-to-label";

/// A hop's `percent` is its share of the leg; a route that never split omits it.
pub const WHOLE_LEG: u8 = 100;

const fn whole_leg() -> u8 {
    WHOLE_LEG
}

/// Resolve `program_id`'s Jupiter/Metis route label via `program-id-to-label`.
pub async fn resolve_venue_label(program_id: &Address) -> Result<String> {
    let labels: std::collections::HashMap<String, String> = reqwest::get(PROGRAM_ID_TO_LABEL_URL)
        .await
        .context("fetch program-id-to-label")?
        .json()
        .await
        .context("parse program-id-to-label response")?;
    labels
        .get(&program_id.to_string())
        .cloned()
        .with_context(|| {
            format!("{program_id} has no Jupiter/Metis label — is it a routable venue?")
        })
}

/// One hop of metis's own `routePlan`, which names a pool and its display label.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetisHop {
    #[serde(default = "whole_leg")]
    percent: u8,
    swap_info: MetisSwapInfo,
}

#[derive(Deserialize)]
struct MetisSwapInfo {
    /// Required: a hop the server could not label must fail the parse so the caller falls back
    /// to `route_summary`. Accepting a missing label reads as "some other venue" instead.
    label: String,
}

/// One hop of the original's `routePlan`. Its events name the program that ran the hop, never a
/// pool, and its `label` is best-effort — matching on it would inherit the server's map.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OriginalHop {
    #[serde(default = "whole_leg")]
    percent: u8,
    program: Option<String>,
}

fn hops<T: DeserializeOwned>(plan: Option<&str>) -> Option<Vec<T>> {
    plan.and_then(|plan| serde_json::from_str(plan).ok())
}

/// A multi-hop route gives every hop the whole leg, so the venue's share is the largest hop it
/// holds rather than their sum; a split route is where the share drops below [`WHOLE_LEG`].
fn share_of(percents: impl Iterator<Item = u8>) -> u8 {
    percents.max().unwrap_or(0)
}

/// The venue's share of metis's chosen route for `leg`, `0` when metis routed around it.
/// Falls back to a substring check on `route_summary` — as the whole leg, since the summary
/// carries no percents — when `route_plan` is absent or fails to parse.
pub fn venue_share(leg: &RerouteLegNotification, venue_label: &str) -> u8 {
    match hops::<MetisHop>(leg.route_plan.as_deref()) {
        Some(hops) => share_of(
            hops.iter()
                .filter(|hop| hop.swap_info.label == venue_label)
                .map(|hop| hop.percent),
        ),
        None => match leg.route_summary.contains(venue_label) {
            true => WHOLE_LEG,
            false => 0,
        },
    }
}

/// The venue's share of the route the swap took on L1 — what the re-quote displaced. Matched on
/// the program rather than the label, since the original's events name the program and the label
/// is only as good as the server's map.
///
/// `None` when no route was recovered at all, which is a third state: scoring it as "not this
/// venue" would turn every unresolved leg into one the venue won, and a server that does not
/// send `originalRoutePlan` would report a clean sweep rather than nothing.
pub fn original_venue_share(leg: &RerouteLegNotification, program_id: &Address) -> Option<u8> {
    let program = program_id.to_string();
    let hops = hops::<OriginalHop>(leg.original_route_plan.as_deref())?;
    Some(share_of(
        hops.iter()
            .filter(|hop| hop.program.as_deref() == Some(&program))
            .map(|hop| hop.percent),
    ))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const VENUE_LABEL: &str = "Whirlpool";
    const VENUE_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

    fn leg(
        route_plan: Option<String>,
        original_route_plan: Option<String>,
        route_summary: &str,
    ) -> RerouteLegNotification {
        RerouteLegNotification {
            input_mint: "in".to_string(),
            output_mint: "out".to_string(),
            amount: 1,
            swap_mode: "ExactIn".to_string(),
            original_quoted_out: 1,
            metis_quoted_out: 1,
            route_summary: route_summary.to_string(),
            route_plan,
            original_route_plan,
        }
    }

    /// `None` omits `percent` entirely, the shape a route that never split has.
    fn percent_field(percent: Option<u8>) -> String {
        percent.map_or(String::new(), |percent| format!(r#""percent":{percent},"#))
    }

    fn json_or_null(value: Option<&str>) -> String {
        value.map_or("null".to_string(), |value| format!(r#""{value}""#))
    }

    fn json_hops(hops: impl Iterator<Item = String>) -> Option<String> {
        Some(format!("[{}]", hops.collect::<Vec<_>>().join(",")))
    }

    /// A `None` label writes JSON `null`: a hop the server could not name.
    fn metis_plan(hops: &[(Option<&str>, Option<u8>)]) -> Option<String> {
        json_hops(hops.iter().map(|(label, percent)| {
            format!(
                r#"{{{}"swapInfo":{{"ammKey":"pool","label":{}}}}}"#,
                percent_field(*percent),
                json_or_null(*label)
            )
        }))
    }

    fn original_plan(hops: &[(Option<&str>, Option<u8>)]) -> Option<String> {
        json_hops(hops.iter().map(|(program, percent)| {
            format!(
                r#"{{{}"program":{}}}"#,
                percent_field(*percent),
                json_or_null(*program)
            )
        }))
    }

    #[rstest]
    #[case::hop_label_matches(metis_plan(&[(Some(VENUE_LABEL), Some(100))]), "", 100)]
    #[case::split_hop_carries_its_percent(
        metis_plan(&[(Some(VENUE_LABEL), Some(14)), (Some("Raydium"), Some(86))]),
        "",
        14
    )]
    #[case::multi_hop_takes_the_largest_share(
        metis_plan(&[(Some(VENUE_LABEL), Some(100)), (Some(VENUE_LABEL), Some(100))]),
        "",
        100
    )]
    #[case::absent_percent_is_the_whole_leg(metis_plan(&[(Some(VENUE_LABEL), None)]), "", 100)]
    #[case::other_venue(metis_plan(&[(Some("Raydium"), Some(100))]), "", 0)]
    // The original's shape has no `swapInfo`, so it never parses as a metis plan and the summary
    // decides — the same path a null label takes.
    #[case::original_shape_falls_back_to_the_summary(
        original_plan(&[(Some(VENUE_PROGRAM), Some(100))]),
        VENUE_LABEL,
        100
    )]
    #[case::null_label_falls_back_to_the_summary(metis_plan(&[(None, Some(100))]), VENUE_LABEL, 100)]
    #[case::null_label_without_a_summary_hit(metis_plan(&[(None, Some(100))]), "Raydium", 0)]
    #[case::malformed_json_falls_back_to_the_summary(
        Some("{not json".to_string()),
        VENUE_LABEL,
        100
    )]
    #[case::absent_plan_falls_back_to_the_summary(None, VENUE_LABEL, 100)]
    #[case::absent_plan_and_no_summary_hit(None, "Raydium", 0)]
    fn venue_share_reads_metis_hops_and_falls_back_to_the_summary(
        #[case] route_plan: Option<String>,
        #[case] route_summary: &str,
        #[case] expected: u8,
    ) {
        let leg = leg(route_plan, None, route_summary);
        assert_eq!(venue_share(&leg, VENUE_LABEL), expected);
    }

    #[rstest]
    #[case::hop_program_matches(original_plan(&[(Some(VENUE_PROGRAM), Some(100))]), Some(100))]
    #[case::split_hop_carries_its_percent(
        original_plan(&[(Some(VENUE_PROGRAM), Some(30)), (Some("other"), Some(70))]),
        Some(30)
    )]
    #[case::absent_percent_is_the_whole_leg(original_plan(&[(Some(VENUE_PROGRAM), None)]), Some(100))]
    // The label is the server's guess; only the program attributes a hop.
    #[case::label_alone_does_not_match(metis_plan(&[(Some(VENUE_LABEL), Some(100))]), Some(0))]
    #[case::null_program(original_plan(&[(None, Some(100))]), Some(0))]
    #[case::other_program(original_plan(&[(Some("other"), Some(100))]), Some(0))]
    // Unrecoverable, not "some other venue": there is no fallback on this side.
    #[case::malformed_json_is_unresolved(Some("{not json".to_string()), None)]
    #[case::absent_plan_is_unresolved(None, None)]
    fn original_venue_share_matches_on_the_program_only(
        #[case] original_route_plan: Option<String>,
        #[case] expected: Option<u8>,
    ) {
        let leg = leg(None, original_route_plan, VENUE_LABEL);
        let program = VENUE_PROGRAM.parse::<Address>().expect("venue program");
        assert_eq!(original_venue_share(&leg, &program), expected);
    }
}
