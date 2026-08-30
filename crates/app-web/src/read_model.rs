//! What a commodity page asks the read model for.
//!
//! Four pages -- consumables, reagents, enchants, gems -- draw the same card
//! from the same two facts: the market's current state and its comparison
//! window. Before Phase 2 each of them reduced those from the archive during
//! the request. Now each of them reads two sets of rows, and this is the one
//! place that spells out which two.
//!
//! **It was three until Phase 5.** The third was every market's all-time
//! window, read on every category page to print an Avg, a Low with its date
//! and a High with its date. Those figures answered a question about the
//! archive rather than about buying one now, and the analysis page -- one
//! click away from every column -- answers it better. Dropping them dropped
//! the query with them: a whole read of `market_windows` per page, gone
//! because nothing on the page wanted it any more.
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

/// The two sets of rows a category page draws from, keyed by item.
#[derive(Debug, Default)]
pub(crate) struct CommodityPage {
    /// Current state, one entry per market: the price, the depth behind it,
    /// and when it was last seen.
    pub current: BTreeMap<ItemId, MarketSummary>,
    /// The reader's chosen comparison window. Every figure on a card that is
    /// not the price now comes from here -- the median, the band, the rank and
    /// the sparkline -- so that they all describe the same interval.
    pub recent: BTreeMap<ItemId, MarketWindow>,
}

/// Read one region's commodity markets at the reader's comparison window.
///
/// Two queries for a whole page, whatever the page holds -- and neither of
/// them reduces anything. `baseline_days` is the reader's own choice,
/// remembered in a cookie; every one of the five is materialised, so choosing
/// another is a different row rather than a different calculation.
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

    Ok(CommodityPage {
        current: current.into_iter().map(|s| (s.key.item(), s)).collect(),
        recent: recent.into_iter().map(|w| (w.key.item(), w)).collect(),
    })
}
