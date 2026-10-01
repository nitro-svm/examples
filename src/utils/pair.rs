//! The `<base>,<quote>` pair flag the reroute examples filter on.

use simulator_api::MintPair;
use solana_address::Address;

/// Parse `<base>,<quote>`, two base58 mints.
pub fn parse_pair(value: &str) -> Result<MintPair, String> {
    let (base, quote) = value
        .split_once(',')
        .ok_or("expected two base58 mints separated by a comma")?;
    let parse = |mint: &str| mint.trim().parse::<Address>().map_err(|e| e.to_string());
    Ok(MintPair::new(parse(base)?, parse(quote)?))
}
