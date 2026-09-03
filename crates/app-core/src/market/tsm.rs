//! TradeSkillMaster observations kept separate from Blizzard auction samples.
//!
//! TSM's values are independently calculated and must never be substituted for
//! ours implicitly.  These types deliberately carry their source's fields
//! without trying to make them fit [`super::PriceSample`].

use cluster_core::Millis;

use super::{Copper, ItemId, Region};

/// One daily regional sales observation published by TSM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsmRegionDaily {
    pub item: ItemId,
    pub region: Region,
    /// Midnight UTC for the upstream observation's calendar day.
    pub day: Millis,
    pub market_value: Copper,
    pub historical: Copper,
    pub avg_sale_price: Copper,
    /// Sale rate expressed in basis points, not a rendered float.
    pub sale_rate_bp: u16,
    pub sold_per_day: u64,
    pub updated_at: Millis,
}

/// One region-wide commodity observation published by TSM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsmCommoditySample {
    pub item: ItemId,
    pub region: Region,
    pub observed_at: Millis,
    pub market_value: Copper,
    pub min_buyout: Copper,
    pub recent: Copper,
    pub historical: Copper,
    pub updated_at: Millis,
}

/// An internal-only comparison against a stable local alignment window.
///
/// `market_value_ratio_bp` is `TSM marketValue / our median`, in basis
/// points. `min_buyout_matches` is intentionally exact: it measures the same
/// quantity on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsmContrast {
    pub item: ItemId,
    pub region: Region,
    pub observed_at: Millis,
    pub own_observed_at: Millis,
    pub min_buyout_matches: bool,
    pub market_value_ratio_bp: Option<u32>,
}
