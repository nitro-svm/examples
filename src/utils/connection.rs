//! Reaching a deployment: the URL and key every example takes.

use clap::Args;

#[derive(Args, Clone)]
pub struct ConnectionArgs {
    /// Simulator URL: a bare host for the hosted deployment, or an explicit
    /// `ws://host:port` for a locally run stack.
    #[arg(long, default_value = "simulator.termina.technology")]
    pub url: String,

    /// API key sent as the `X-API-Key` header.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    pub api_key: String,
}
