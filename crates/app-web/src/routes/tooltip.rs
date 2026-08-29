//! Item tooltips.
//!
//! Two paths to the same markup:
//!
//! * **Inline.** While rendering a page we read the tooltip cache and put the
//!   tooltip straight into the HTML next to the icon. Hovering then costs
//!   nothing -- no request, no flash of "Loading…". This is the normal case,
//!   because the cache holds a tooltip for a week and the collector warms it.
//! * **On hover.** For an icon whose tooltip was not cached at render time,
//!   the page carries `hx-get` and fetches this route the first time a pointer
//!   lands on it. That request is also what fills the cache for next time.
//!
//! Neither path ever lets a page render block on Battle.net: the inline read
//! is cache-only.

use std::collections::BTreeMap;

use app_core::market::{Catalog, CatalogItem, ItemId};
use app_core::repo::Store;
use app_core::service::{Freshness, ItemTooltipService};
use app_core::{AppError, Ports};
use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::response::Html;
use cluster_core::Millis;

use crate::error::WebResult;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::views::TooltipView;

#[derive(Template)]
#[template(path = "partials/tooltip.html")]
struct TooltipFragment {
    tip: TooltipView,
}

/// `GET /wow/item/{item_id}/tooltip` -> the tooltip body for one item.
///
/// Only catalogued items are served. That is not just tidiness: without the
/// check this route would be an open proxy that lets anyone spend our
/// Battle.net request budget on arbitrary item ids.
pub async fn tooltip<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Path(item_id): Path<u32>,
) -> WebResult<Html<String>> {
    let item = ItemId(item_id);
    let entry = env
        .catalogs()
        .index()
        .get(&item)
        .map(|(_, entry)| (*entry).clone())
        .ok_or(AppError::NotFound)?;

    let (tooltip, freshness) = service(&env)
        .lookup(prefs.region, prefs.locale, item, &entry.name, env.now())
        .await;

    page(
        &TooltipFragment {
            tip: TooltipView::new(
                &tooltip,
                rank_line(&entry, item),
                freshness != Freshness::Unavailable,
            ),
        },
        prefs.locale,
    )
}

/// The tooltip for one item, but only if it is already cached.
pub(crate) async fn cached_one<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    entry: &CatalogItem,
    item: ItemId,
    now: Millis,
) -> Option<TooltipView> {
    let tooltip = service(env).cached(prefs.locale, item, now).await?;
    Some(TooltipView::new(&tooltip, rank_line(entry, item), true))
}

/// Every cached tooltip in a catalog, keyed by item id.
///
/// One cache read per tracked item. They are point lookups on a small table,
/// and doing them here keeps the "which rank does the icon describe" decision
/// in the page that draws the icon rather than spreading it across two
/// modules.
pub(crate) async fn cached_all<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    catalog: &Catalog,
    now: Millis,
) -> BTreeMap<u32, TooltipView> {
    let mut map = BTreeMap::new();
    for entry in &catalog.items {
        for item in entry.item_ids() {
            if let Some(view) = cached_one(env, prefs, entry, item, now).await {
                map.insert(item.get(), view);
            }
        }
    }
    map
}

/// Which market this icon leads to. The game has no concept of our ranks being
/// separate markets, so this line is ours.
fn rank_line(entry: &CatalogItem, item: ItemId) -> Option<String> {
    match (entry.ranks.len(), entry.rank_of(item)) {
        (total, Some(rank)) if total > 1 => Some(format!("Rank {rank} of {total}")),
        _ => None,
    }
}

fn service<E: Ports>(env: &E) -> ItemTooltipService<'_, E::Items, <E::Store as Store>::Cache> {
    ItemTooltipService::new(
        env.items(),
        env.store().cache(),
        env.config().item_cache_ttl_ms,
    )
}
