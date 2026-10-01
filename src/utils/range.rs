//! The slot range every example replays.

use clap::Args;

#[derive(Args, Clone)]
pub struct RangeArgs {
    /// First slot (inclusive) to replay.
    #[arg(long)]
    pub start_slot: u64,

    /// Slots to cover, as the inclusive range `[start, start + count]`.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..))]
    pub slot_count: u64,
}

impl RangeArgs {
    pub const fn end_slot(&self) -> u64 {
        self.start_slot + self.slot_count
    }
}
