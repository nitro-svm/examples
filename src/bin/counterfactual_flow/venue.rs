use anyhow::{Context, Result};
use simulator_client::{
    FULL_PERCENT, RerouteLegNotification, RouteHop, RoutePlan, reroute_report::Target,
};
use solana_address::Address;

const PROGRAM_ID_TO_LABEL_URL: &str = "https://lite-api.jup.ag/swap/v1/program-id-to-label";

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

/// A multi-hop route gives every hop the whole leg, so the venue's share is the largest hop it
/// holds rather than their sum; a split route is where the share drops below [`FULL_PERCENT`].
fn share_of<'a>(hops: impl Iterator<Item = &'a RouteHop>) -> u64 {
    hops.map(|hop| hop.percent).max().unwrap_or(0)
}

/// The venue's share of metis's chosen route for `leg`, `0` when metis routed around it.
///
/// Falls back to a substring check on `route_summary` — as the whole leg, since the summary
/// carries no percents — when the plan is absent or carries an unlabelled hop, which is not
/// evidence of another venue.
pub fn venue_share(leg: &RerouteLegNotification, venue: &Target) -> u64 {
    let labelled = leg
        .route_plan
        .as_ref()
        .map(RoutePlan::hops)
        .filter(|hops| hops.iter().all(|hop| hop.label().is_some()));
    match labelled {
        Some(hops) => share_of(hops.iter().filter(|hop| venue.claims_quoted(hop))),
        None => match venue
            .label()
            .is_some_and(|label| leg.route_summary.contains(label))
        {
            true => FULL_PERCENT,
            false => 0,
        },
    }
}

/// The venue's share of the route the swap took on L1 — what the re-quote displaced. Matched by
/// [`Target::claims_fill`], so a target naming a program matches on that rather than on the
/// label, which is only as good as the server's map.
///
/// `None` when no route was recovered, which is a third state: scoring it as "not this venue"
/// would turn every unresolved leg into one the venue won.
pub fn original_venue_share(leg: &RerouteLegNotification, venue: &Target) -> Option<u64> {
    let hops = leg.original_route_plan.as_ref().map(RoutePlan::hops)?;
    Some(share_of(hops.iter().filter(|hop| venue.claims_fill(hop))))
}
#[cfg(test)]
mod tests {
    use rstest::rstest;
    use simulator_api::route_plan::RouteHopSwapInfo;

    use super::*;

    const VENUE_LABEL: &str = "Whirlpool";
    const VENUE_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

    fn leg(
        route_plan: Option<RoutePlan>,
        original_route_plan: Option<RoutePlan>,
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
            realized_route_plan: None,
        }
    }

    /// `None` for the percent is the shape a route that never split has.
    const fn percent(percent: Option<u64>) -> u64 {
        match percent {
            Some(percent) => percent,
            None => FULL_PERCENT,
        }
    }

    /// A quote's hops: a pool and a label, and no program. A `None` label is one the server
    /// could not name.
    fn metis_plan(hops: &[(Option<&str>, Option<u64>)]) -> Option<RoutePlan> {
        RoutePlan::new(
            hops.iter()
                .map(|(label, split)| RouteHop {
                    percent: percent(*split),
                    swap_info: RouteHopSwapInfo {
                        label: label.map(str::to_string),
                        amm_key: Some("pool".to_string()),
                        ..RouteHopSwapInfo::default()
                    },
                    ..RouteHop::default()
                })
                .collect(),
        )
    }

    /// A fill's hops: a program, and none of the router's naming.
    fn original_plan(hops: &[(Option<&str>, Option<u64>)]) -> Option<RoutePlan> {
        RoutePlan::new(
            hops.iter()
                .map(|(program, split)| RouteHop {
                    percent: percent(*split),
                    program: program.map(str::to_string),
                    ..RouteHop::default()
                })
                .collect(),
        )
    }

    /// Both keys, as `venue_of` resolves them for a real run.
    fn venue() -> Target {
        Target::new(
            Some(VENUE_LABEL.to_string()),
            Some(VENUE_PROGRAM.parse().expect("venue program")),
        )
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
    // The original's shape carries no label, so the summary decides, as for a null label.
    #[case::original_shape_falls_back_to_the_summary(
        original_plan(&[(Some(VENUE_PROGRAM), Some(100))]),
        VENUE_LABEL,
        100
    )]
    #[case::null_label_falls_back_to_the_summary(
        metis_plan(&[(None, Some(100))]),
        VENUE_LABEL,
        100
    )]
    #[case::null_label_without_a_summary_hit(metis_plan(&[(None, Some(100))]), "Raydium", 0)]
    #[case::absent_plan_falls_back_to_the_summary(None, VENUE_LABEL, 100)]
    #[case::absent_plan_and_no_summary_hit(None, "Raydium", 0)]
    fn venue_share_reads_metis_hops_and_falls_back_to_the_summary(
        #[case] route_plan: Option<RoutePlan>,
        #[case] route_summary: &str,
        #[case] expected: u64,
    ) {
        let leg = leg(route_plan, None, route_summary);
        assert_eq!(venue_share(&leg, &venue()), expected);
    }

    #[rstest]
    #[case::hop_program_matches(original_plan(&[(Some(VENUE_PROGRAM), Some(100))]), Some(100))]
    #[case::split_hop_carries_its_percent(
        original_plan(&[(Some(VENUE_PROGRAM), Some(30)), (Some("other"), Some(70))]),
        Some(30)
    )]
    #[case::absent_percent_is_the_whole_leg(
        original_plan(&[(Some(VENUE_PROGRAM), None)]),
        Some(100)
    )]
    // The label is the server's guess; only the program attributes a hop.
    #[case::label_alone_does_not_match(metis_plan(&[(Some(VENUE_LABEL), Some(100))]), Some(0))]
    #[case::null_program(original_plan(&[(None, Some(100))]), Some(0))]
    #[case::other_program(original_plan(&[(Some("other"), Some(100))]), Some(0))]
    // Unrecoverable, not "some other venue": there is no fallback on this side.
    #[case::absent_plan_is_unresolved(None, None)]
    fn original_venue_share_matches_on_the_program_only(
        #[case] original_route_plan: Option<RoutePlan>,
        #[case] expected: Option<u64>,
    ) {
        let leg = leg(None, original_route_plan, VENUE_LABEL);
        assert_eq!(original_venue_share(&leg, &venue()), expected);
    }
}
