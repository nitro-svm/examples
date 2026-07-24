//! CLI wrapper around [`backtest_example::amm_liquidity`]. All measurement logic
//! lives in the library so it can also be driven programmatically; this binary
//! only parses flags into a [`MeasurementConfig`] and runs it.

use anyhow::Result;
use clap::Parser;

use backtest_example::amm_liquidity::{MeasurementConfig, TitanVenueDiscriminant, run_measurement};
use backtest_example::utils::parse::{USDC_MINT, WSOL_MINT};

#[derive(Parser)]
#[command(about = "Measure spread and depth for a single prop AMM venue across blocks")]
struct Cli {
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    #[arg(long, default_value_t = 422_818_048)]
    start_slot: u64,

    #[arg(long, default_value_t = 422_818_148)]
    end_slot: u64,

    /// Spread CSV path. Defaults to `spread_<venue>.csv` so runs don't clobber each other.
    #[arg(long)]
    spread_output: Option<String>,

    /// Depth CSV path. Defaults to `depth_<venue>.csv` so runs don't clobber each other.
    #[arg(long)]
    depth_output: Option<String>,

    #[arg(long, default_value_t = false)]
    enable_intra_block_inspection: bool,

    #[arg(long, default_value = USDC_MINT)]
    quote_mint: String,

    #[arg(long, default_value = WSOL_MINT)]
    base_mint: String,

    /// Spread measurement size in base native units.
    #[arg(long, default_value_t = 50_000_000_000)]
    spread_size: u64,

    /// Smallest size for the depth sweep (quote-mint native units).
    #[arg(long, default_value_t = 1_000_000_000)]
    depth_min: u64,

    /// Stop the depth sweep once price impact exceeds this many bps.
    #[arg(long, default_value_t = 1000)]
    max_impact_bps: u64,

    /// Titan Venue discriminant(s) to isolate, comma-separated or repeated
    /// All listed venues are measured in the same simulator session.
    /// 55 = BisonFi, 13 = ZeroFi, 28 = HumidiFi, 57 = GoonFiV2, 23 = Tessera.
    /// See the Venue enum in the Titan IDL.
    #[arg(long = "venue-discriminant", value_delimiter = ',', default_value = "55", num_args = 1..)]
    venue_discriminants: Vec<u8>,
}

impl From<Cli> for MeasurementConfig {
    fn from(cli: Cli) -> Self {
        MeasurementConfig {
            url: cli.url,
            api_key: cli.api_key,
            start_slot: cli.start_slot,
            end_slot: cli.end_slot,
            spread_output: cli.spread_output,
            depth_output: cli.depth_output,
            enable_intra_block_inspection: cli.enable_intra_block_inspection,
            quote_mint: cli.quote_mint,
            base_mint: cli.base_mint,
            spread_size: cli.spread_size,
            depth_min: cli.depth_min,
            max_impact_bps: cli.max_impact_bps,
            venue_discriminants: cli.venue_discriminants,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();
    // Validate discriminants up front so a bad value fails before opening a session.
    for &disc in &cli.venue_discriminants {
        TitanVenueDiscriminant::from_u8(disc)?;
    }

    run_measurement(&MeasurementConfig::from(cli)).await
}
