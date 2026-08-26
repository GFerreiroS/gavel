//! Turning a pile of listings into one observation.
//!
//! Pure and allocation-light on purpose: this is the part that would run on a
//! node, and it is the only place that decides what "the price" means.

use cluster_core::Millis;

use super::{Copper, ItemId, Listing, PriceSample, Region};

/// Summary of one item's listings at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceStats {
    pub min: Copper,
    pub p05: Copper,
    pub median: Copper,
    pub quantity: u64,
    pub listings: u32,
}

/// Reduce every listing for one item into a single sample.
///
/// `listings` is sorted in place, so the caller must pass a mutable slice it
/// owns; that avoids allocating a copy per item when sweeping a snapshot that
/// contains tens of thousands of them.
///
/// The percentile is **supply-weighted**, not listing-weighted: p05 is the
/// price you pay for the cheapest 5% of *units*, which is what a buyer
/// actually experiences. A single 1-copper listing of one unit moves `min` but
/// barely moves `p05`, which is why alerting uses the latter.
pub fn summarise(listings: &mut [Listing]) -> Option<PriceStats> {
    if listings.is_empty() {
        return None;
    }
    listings.sort_unstable_by_key(|l| l.unit_price.get());

    let total: u64 = listings.iter().map(|l| l.quantity).sum();
    if total == 0 {
        return None;
    }

    Some(PriceStats {
        min: listings[0].unit_price,
        p05: quantile_price(listings, total, 5),
        median: quantile_price(listings, total, 50),
        quantity: total,
        listings: listings.len() as u32,
    })
}

/// Price at which the cheapest `percent` of supply has been consumed.
/// Expects `listings` already sorted ascending by unit price.
fn quantile_price(listings: &[Listing], total_quantity: u64, percent: u64) -> Copper {
    // Ceiling division so a tiny market still targets at least one unit.
    let target = (total_quantity * percent).div_ceil(100).max(1);
    let mut seen = 0u64;
    for listing in listings {
        seen += listing.quantity;
        if seen >= target {
            return listing.unit_price;
        }
    }
    listings
        .last()
        .map(|l| l.unit_price)
        .unwrap_or(Copper::ZERO)
}

impl PriceStats {
    pub fn into_sample(self, item: ItemId, region: Region, observed_at: Millis) -> PriceSample {
        PriceSample {
            item,
            region,
            observed_at,
            min_unit_price: self.min,
            p05_unit_price: self.p05,
            median_unit_price: self.median,
            quantity: self.quantity,
            listings: self.listings,
        }
    }
}
