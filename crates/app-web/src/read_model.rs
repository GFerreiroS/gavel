//! What a commodity page asks the read model for.
//!
//! Four pages -- consumables, reagents, enchants, gems -- draw the same card
//! from the same three facts: the market's current state, its comparison
//! window, and its all-time extremes. Before Phase 2 each of them reduced
//! those from the archive during the request. Now each of them reads three
//! sets of rows, and this is the one place that spells out which three.
//!
//! One helper rather than four copies, for the reason §7 gives about shared
//! components: two copies drift, and the drift lands on the reader as two
//! pages calling different numbers by the same name.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::materialise::{MarketSummary, MarketWindow};
use app_core::market::window::Window;
use app_core::market::{ItemId, Region};
use app_core::repo::{ReadModelRepository, Store};

use crate::error::WebResult;

/// The three sets of rows a category page draws from, keyed by item.
#[derive(Debug, Default)]
pub(crate) struct CommodityPage {
    /// Current state, one entry per market.
    pub current: BTreeMap<ItemId, MarketSummary>,
    /// The reader's chosen comparison window -- what "vs usual" is measured
    /// against.
    pub recent: BTreeMap<ItemId, MarketWindow>,
    /// Everything ever recorded. "Cheapest ever, and when" only means
    /// something across the whole history.
    pub all_time: BTreeMap<ItemId, MarketWindow>,
}

/// Read one region's commodity markets at the reader's comparison window.
///
/// Three queries for a whole page, whatever the page holds -- and none of them
/// reduces anything. `baseline_days` is the reader's own choice, remembered in
/// a cookie; every one of the five is materialised, so choosing another is a
/// different row rather than a different calculation.
pub(crate) async fn commodity_page<E: Ports>(
    env: &E,
    region: Region,
    baseline_days: u64,
) -> WebResult<CommodityPage> {
    let model = env.store().read_model();
    let current = model.commodities(region).await?;
    let recent = model
        .commodity_windows(region, &Window::Days(baseline_days))
        .await?;
    let all_time = model.commodity_windows(region, &Window::All).await?;

    Ok(CommodityPage {
        current: current.into_iter().map(|s| (s.key.item(), s)).collect(),
        recent: recent.into_iter().map(|w| (w.key.item(), w)).collect(),
        all_time: all_time.into_iter().map(|w| (w.key.item(), w)).collect(),
    })
}
