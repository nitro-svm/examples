//! Reaching a deployment: the URL and key every example takes, and the two derivations that turn
//! one `--url` into both endpoints a session needs.

use clap::Args;

#[derive(Args, Clone)]
pub struct ConnectionArgs {
    /// Simulator URL: a bare host for the hosted deployment, or an explicit
    /// `ws://host:port` for a locally run stack.
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    pub url: String,

    /// API key sent as the `X-API-Key` header.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    pub api_key: String,
}

impl ConnectionArgs {
    /// A bare host is the hosted deployment over TLS; an explicit `ws(s)://` is used as
    /// given, so a local stack (plaintext) is reachable as `ws://localhost:8900`.
    pub fn websocket_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        let base = match base.split_once("://") {
            Some(("ws" | "wss", _)) => base.to_string(),
            _ => format!("wss://{base}"),
        };
        if base.ends_with("/backtest") {
            base
        } else {
            format!("{base}/backtest")
        }
    }

    /// Deployments report the session's RPC endpoint either absolute or as a path. A
    /// plaintext websocket implies a plaintext RPC endpoint on the same host.
    pub fn rpc_url(&self, endpoint: &str) -> String {
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            return endpoint.to_string();
        }
        let (scheme, host) = match self.url.trim_end_matches('/').split_once("://") {
            Some(("ws", host)) => ("http", host),
            Some((_, host)) => ("https", host),
            None => ("https", self.url.trim_end_matches('/')),
        };
        let host = host.trim_end_matches("/backtest");
        format!("{scheme}://{host}/{}", endpoint.trim_start_matches('/'))
    }
}
