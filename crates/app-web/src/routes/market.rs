//! Auction-house consumable tracker.
//!
//! One page per expansion. The active one shows live prices and alerts; an
//! archived one is the same page with the collection stopped -- history kept,
//! never added to.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::{
    ALL_AUDIENCES_LABELS, Catalog, CommodityProvider, ItemId, PriceSample, Region, WindowStats,
};
use app_core::repo::{PriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::Millis;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::format;
use crate::render::page;
use crate::session::current_user;
use crate::views::{
    AlertRow, CardGroup, CatalogLink, ItemCard, Layout, MarketView, PatchCell, PatchColumn,
    PatchRow, RankColumn,
};

/// Window the "vs usual" column compares against.
const BASELINE_DAYS: u64 = 7;
const ALERT_LIMIT: usize = 20;

#[derive(Template)]
#[template(path = "consumables.html")]
struct ConsumablesPage {
    layout: Layout,
    market: MarketView,
}

#[derive(Template)]
#[template(path = "partials/consumables.html")]
pub struct ConsumablesFragment {
    pub market: MarketView,
}

pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render_page(env, csrf, headers, None).await
}

pub async fn archived_page<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Html<String>> {
    render_page(env, csrf, headers, Some(id)).await
}

async fn render_page<E: Ports>(
    env: E,
    csrf: Csrf,
    headers: HeaderMap,
    id: Option<String>,
) -> WebResult<Html<String>> {
    let market = build(&env, id.as_deref()).await?;
    let user = current_user(&env, &headers).await?;
    page(&ConsumablesPage {
        layout: Layout::new(
            env.config(),
            "Consumables",
            "/wow/consumables",
            user.map(|u| u.username),
            csrf.0.clone(),
        ),
        market,
    })
}

pub async fn fragment<E: Ports>(State(env): State<E>) -> WebResult<Html<String>> {
    page(&ConsumablesFragment {
        market: build(&env, None).await?,
    })
}

/// Resolve which catalog the page is about: an explicit id, else the active
/// one, else the most recent archive so the page is never blank.
fn select<'a, E: Ports>(env: &'a E, id: Option<&str>) -> Option<&'a Catalog> {
    let catalogs = env.catalogs();
    match id {
        Some(id) => catalogs.by_id(id),
        None => catalogs
            .active()
            .or_else(|| catalogs.ordered().first().copied()),
    }
}

async fn build<E: Ports>(env: &E, id: Option<&str>) -> WebResult<MarketView> {
    let Some(catalog) = select(env, id) else {
        return Err(app_core::AppError::NotFound.into());
    };
    let market = env.market();
    // One region per page: commodity markets share nothing, so mixing them in
    // a single table would be meaningless.
    let region = market.regions.first().copied().unwrap_or(Region::Eu);

    let prices = env.store().prices();
    let now = env.now();

    let latest: BTreeMap<ItemId, PriceSample> = prices
        .latest(region)
        .await?
        .into_iter()
        .map(|s| (s.item, s))
        .collect();

    let recent_since = Millis(
        now.get()
            .saturating_sub(BASELINE_DAYS * 24 * 60 * 60 * 1000),
    );
    let recent: BTreeMap<ItemId, WindowStats> =
        index_stats(prices.window_stats(region, recent_since, None).await?);

    // Extremes are all-time, not windowed: "cheapest ever, and when" only
    // means something across the whole history.
    let all_time = index_stats(prices.window_stats(region, Millis::ZERO, None).await?);

    // --- one card per market, grouped the way the raid is grouped ----------
    let mut groups: Vec<CardGroup> = Vec::new();
    for (audience, label) in ALL_AUDIENCES_LABELS {
        let mut cards = Vec::new();
        for entry in catalog.by_audience(audience) {
            cards.push(card(entry, &latest, &recent, &all_time, now));
        }
        cards.sort_by(|a, b| a.category.cmp(b.category).then(a.name.cmp(&b.name)));
        groups.push(CardGroup {
            audience: audience.as_str(),
            label,
            cards,
        });
    }

    // --- patch-by-patch, plus the whole expansion --------------------------
    let windows = catalog.patch_windows();
    let mut columns = Vec::with_capacity(windows.len());
    let mut per_patch: Vec<BTreeMap<ItemId, WindowStats>> = Vec::with_capacity(windows.len());
    for (patch, from, until) in &windows {
        columns.push(PatchColumn {
            patch: patch.patch.clone(),
            label: patch.label(),
            started: patch.started.clone(),
        });
        per_patch.push(index_stats(
            prices.window_stats(region, *from, *until).await?,
        ));
    }
    let overall = index_stats(
        prices
            .window_stats(region, catalog.span_start(), None)
            .await?,
    );

    let mut patch_rows = Vec::new();
    for item in &catalog.items {
        for rank in &item.ranks {
            patch_rows.push(PatchRow {
                name: item.display_name(rank.item_id),
                audience: item.audience.as_str(),
                category: item.category.label(),
                cells: per_patch
                    .iter()
                    .map(|stats| cell(stats.get(&rank.item_id)))
                    .collect(),
                overall: cell(overall.get(&rank.item_id)),
            });
        }
    }
    patch_rows.sort_by(|a, b| a.category.cmp(b.category).then(a.name.cmp(&b.name)));

    // --- alerts ------------------------------------------------------------
    let all_items = env.catalogs().index();
    let alerts = prices
        .recent_alerts(ALERT_LIMIT)
        .await?
        .into_iter()
        .map(|alert| AlertRow {
            name: all_items
                .get(&alert.item)
                .map(|(_, item)| item.display_name(alert.item))
                .unwrap_or_else(|| alert.item.to_string()),
            region: alert.region.to_string().to_uppercase(),
            severity: alert.severity.as_str(),
            current: alert.current.to_string(),
            baseline: alert.baseline.to_string(),
            discount_percent: alert.discount_percent,
            quantity: alert.quantity,
            when: alert.observed_at.to_utc_string(),
        })
        .collect();

    let last_observed = prices.last_observed(region).await?;
    let selected = catalog.id.clone();

    Ok(MarketView {
        expansion: catalog.expansion.clone(),
        season: catalog.season.clone(),
        region: region.to_string().to_uppercase(),
        archived: !catalog.is_active(),
        configured: env.commodities().is_configured(),
        tracked_items: catalog.tracked_ids().len(),
        samples_held: latest.len(),
        last_observed: match last_observed {
            Some(at) => format::ago(now.since(at)),
            None => "never".to_string(),
        },
        catalogs: env
            .catalogs()
            .ordered()
            .into_iter()
            .map(|c| CatalogLink {
                id: c.id.clone(),
                label: c.expansion.clone(),
                collecting: c.is_active(),
                selected: c.id == selected,
            })
            .collect(),
        groups,
        patches: columns,
        patch_rows,
        alerts,
        baseline_days: BASELINE_DAYS,
    })
}

fn index_stats(stats: Vec<WindowStats>) -> BTreeMap<ItemId, WindowStats> {
    stats.into_iter().map(|w| (w.item, w)).collect()
}

fn cell(stats: Option<&WindowStats>) -> PatchCell {
    match stats {
        Some(w) if w.samples > 0 => PatchCell {
            low: w.low.to_string(),
            mean: w.mean.to_string(),
            high: w.high.to_string(),
            samples: w.samples,
            has_data: true,
        },
        _ => PatchCell::empty(),
    }
}

/// One consumable as a card, with a column per quality rank.
fn card(
    entry: &app_core::market::CatalogItem,
    latest: &BTreeMap<ItemId, PriceSample>,
    recent: &BTreeMap<ItemId, WindowStats>,
    all_time: &BTreeMap<ItemId, WindowStats>,
    now: Millis,
) -> ItemCard {
    let multi_rank = entry.ranks.len() > 1;
    let mut ranks: Vec<&app_core::market::ItemRank> = entry.ranks.iter().collect();
    ranks.sort_by_key(|r| r.rank);

    let columns: Vec<RankColumn> = ranks
        .iter()
        .map(|rank| {
            column(
                rank.item_id,
                if multi_rank {
                    format!("R{}", rank.rank)
                } else {
                    "Price".to_string()
                },
                latest.get(&rank.item_id),
                recent.get(&rank.item_id),
                all_time.get(&rank.item_id),
            )
        })
        .collect();

    // Every rank comes from the same snapshot, so one timestamp covers the card.
    let observed = ranks
        .iter()
        .filter_map(|r| latest.get(&r.item_id))
        .map(|s| s.observed_at)
        .max()
        .map(|at| format::ago(now.since(at)))
        .unwrap_or_else(|| "never".into());

    ItemCard {
        name: entry.name.clone(),
        icon: entry.icon_url(),
        category: entry.category.label(),
        stat: entry.stat.as_str(),
        any_data: columns.iter().any(|c| c.has_data),
        observed,
        columns,
    }
}

fn column(
    id: ItemId,
    label: String,
    sample: Option<&PriceSample>,
    recent: Option<&WindowStats>,
    all_time: Option<&WindowStats>,
) -> RankColumn {
    let base = RankColumn {
        item_id: id.get(),
        label,
        has_data: false,
        current: "\u{2014}".into(),
        mean: "\u{2014}".into(),
        low: "\u{2014}".into(),
        low_when: "\u{2014}".into(),
        high: "\u{2014}".into(),
        high_when: "\u{2014}".into(),
        quantity: 0,
        delta_percent: 0,
        cheap: false,
        dear: false,
    };

    let Some(sample) = sample else {
        // Tracked but never seen: collection has not run, or this rank has no
        // listings at all.
        return base;
    };

    // "vs usual" compares against the recent window, not all time: a price
    // that is normal for this month should not read as cheap because it was
    // cheaper at launch.
    let delta = match recent {
        Some(w) if w.samples > 1 && w.mean.get() > 0 => {
            let current = sample.p05_unit_price.get() as i128;
            let mean = w.mean.get() as i128;
            ((current - mean) * 100 / mean) as i32
        }
        _ => 0,
    };
    let dated = all_time.filter(|w| w.samples > 0);

    RankColumn {
        has_data: true,
        current: sample.p05_unit_price.to_string(),
        mean: dated
            .map(|w| w.mean.to_string())
            .unwrap_or_else(|| base.mean.clone()),
        low: dated
            .map(|w| w.low.to_string())
            .unwrap_or_else(|| base.low.clone()),
        low_when: dated
            .map(|w| w.low_at.to_date_string())
            .unwrap_or_else(|| base.low_when.clone()),
        high: dated
            .map(|w| w.high.to_string())
            .unwrap_or_else(|| base.high.clone()),
        high_when: dated
            .map(|w| w.high_at.to_date_string())
            .unwrap_or_else(|| base.high_when.clone()),
        quantity: sample.quantity,
        delta_percent: delta,
        cheap: delta <= -15,
        dear: delta >= 15,
        ..base
    }
}
