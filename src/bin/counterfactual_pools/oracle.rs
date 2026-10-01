//! The SNDK pool's oracle, repriced every slot from Binance's SNDKUSDT perpetual.

use anyhow::{Context, Result};
use simulator_api::{AccountData, EncodedBinary};

use crate::{SLOT_COUNT, START_SLOT};

/// Unix milliseconds of the first and last slot's blocks, which every slot's update time is spread
/// between.
const START_MS: i64 = 1790703321000;
const END_MS: i64 = 1790706003000;

/// Binance SNDKUSDT 1m closes over the range, from the slot each minute closed at.
const PRICES: &[(u64, f64)] = &[
    (451710501, 1712.22),
    (451710646, 1713.8),
    (451710870, 1713.04),
    (451711094, 1712.54),
    (451711318, 1712.97),
    (451711541, 1712.44),
    (451711765, 1711.98),
    (451711989, 1713.04),
    (451712212, 1712.33),
    (451712436, 1712.7),
    (451712660, 1712.66),
    (451712884, 1712.86),
    (451713107, 1713.6),
    (451713331, 1714.88),
    (451713555, 1714.29),
    (451713778, 1712.33),
    (451714002, 1712.27),
    (451714226, 1711.06),
    (451714450, 1710.71),
    (451714673, 1710.21),
    (451714897, 1710.79),
    (451715121, 1709.59),
    (451715344, 1709.56),
    (451715568, 1709.62),
    (451715792, 1710.04),
    (451716016, 1712.16),
    (451716239, 1715.63),
    (451716463, 1714.83),
    (451716687, 1715.84),
    (451716910, 1714.13),
    (451717134, 1715.38),
    (451717358, 1714.19),
    (451717582, 1712.89),
    (451717805, 1715.17),
    (451718029, 1715.05),
    (451718253, 1713.95),
    (451718476, 1714.81),
    (451718700, 1712.96),
    (451718924, 1713.5),
    (451719148, 1714.73),
    (451719371, 1715.27),
    (451719595, 1715.06),
    (451719819, 1715.09),
    (451720042, 1716.56),
    (451720266, 1716.75),
    (451720490, 1716.33),
];

/// TaurusFi oracle layout: 48-byte entries led by an f64 price, then the update's slot (twice) and
/// unix milliseconds.
const ENTRY_LEN: usize = 48;
const UPDATE_SLOTS: [usize; 2] = [480, 488];
const UPDATE_MILLIS: usize = 496;

/// One state per slot, with the price at `entry` and a fresh update time so it never reads stale.
pub fn schedule(oracle: &AccountData, entry: usize) -> Result<Vec<(u64, AccountData)>> {
    let template = oracle.data.decode()?;
    (START_SLOT..=START_SLOT + SLOT_COUNT)
        .map(|slot| {
            let (_, price) = PRICES
                .iter()
                .rev()
                .find(|(from, _)| *from <= slot)
                .context("no price before the range")?;
            let millis =
                START_MS + (END_MS - START_MS) * (slot - START_SLOT) as i64 / SLOT_COUNT as i64;
            let mut data = template.clone();
            data[entry * ENTRY_LEN..entry * ENTRY_LEN + 8].copy_from_slice(&price.to_le_bytes());
            for at in UPDATE_SLOTS {
                data[at..at + 8].copy_from_slice(&slot.to_le_bytes());
            }
            data[UPDATE_MILLIS..UPDATE_MILLIS + 8].copy_from_slice(&millis.to_le_bytes());
            Ok((
                slot,
                AccountData {
                    data: EncodedBinary::from_bytes(&data, oracle.data.encoding),
                    ..oracle.clone()
                },
            ))
        })
        .collect()
}
