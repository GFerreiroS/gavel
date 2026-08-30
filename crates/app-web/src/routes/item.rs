//! The single-item page: every statistic we hold, plus charts.

use app_core::market::materialise::MarketState;
use app_core::market::window::Window;
use app_core::market::{
    Catalog, CatalogItem, ItemId, ItemKind, analysis, analysis::WEEKDAY_NAMES, downsample,
};
use app_core::repo::{ReadModelRepository, Store, WatchRepository};
use app_core::{AppError, Ports};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::chart::{self, Series, Unit};
use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{ItemDetail, Layout, PatchStatRow, TrendView};

/// Points plotted. More than this and the lines are drawing sub-pixel noise.
const CHART_POINTS: usize = 140;

#[derive(Template)]
#[template(path = "item.html")]
struct ItemPage {
    layout: Layout,
    item: ItemDetail,
    /// `None` when nobody is signed in: the control is not offered, rather
    /// than offered and refused.
    following: Option<bool>,
}

pub async fn detail<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(item_id): Path<u32>,
) -> WebResult<Html<String>> {
    let item = ItemId(item_id);
    let (catalog, entry) = env
        .catalogs()
        .index()
        .get(&item)
        .map(|(c, i)| ((*c).clone(), (*i).clone()))
        .ok_or(AppError::NotFound)?;

    let detail = build(&env, prefs, &catalog, &entry, item).await?;
    let user = current_user(&env, &headers).await?;

    // Whether this reader already follows this item in the region they are
    // looking at. `None` for a visitor who is signed out: the control is not
    // offered at all rather than offered and then refused.
    let following = match user.as_ref() {
        None => None,
        Some(user) => Some(
            env.store()
                .watches()
                .watches(user.id)
                .await?
                .iter()
                .any(|w| w.item == item && w.region == prefs.region),
        ),
    };

    page(
        &ItemPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                &detail.name,
                "/wow/auctions",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            item: detail,
            following,
        },
        prefs.locale,
    )
}

async fn build<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    catalog: &Catalog,
    entry: &CatalogItem,
    item: ItemId,
) -> WebResult<ItemDetail> {
    let region = prefs.region;
    let model = env.store().read_model();
    let now = env.now();

    // The published state of this market. Everything below is read from it:
    // this handler no longer calls `analyse`, and no longer reads a single
    // observation. CLAUDE.md §16's Phase 2, at the one page that used to
    // reduce a whole history twice over.
    let key = catalog.market_of_key(region, item);
    let stats = model
        .market(key)
        .await?
        .unwrap_or_else(|| MarketState::empty(key));

    // Plot every rank of the same consumable together: "is R1 worth it" is the
    // question the chart should answer at a glance. Each rank is its own
    // market, so each is its own stored row and its own stored series.
    let mut all_ranks = Vec::new();
    for rank in &entry.ranks {
        let series = if rank.item_id == item {
            stats.series.clone()
        } else {
            model
                .market(catalog.market_of_key(region, rank.item_id))
                .await?
                .map(|s| s.series)
                .unwrap_or_default()
        };
        all_ranks.push((rank.rank, series));
    }
    all_ranks.sort_by_key(|(rank, _)| *rank);

    // Labels are owned here and borrowed by the series below. An earlier
    // version reached for `Box::leak` to satisfy the lifetime, which would
    // have leaked a string on every page view.
    let labels: Vec<String> = all_ranks
        .iter()
        .map(|(rank, _)| {
            if entry.ranks.len() > 1 {
                format!("R{rank}")
            } else {
                "price".to_string()
            }
        })
        .collect();
    let series: Vec<Series<'_>> = all_ranks
        .iter()
        .zip(&labels)
        .enumerate()
        .map(|(slot, ((_, points), label))| Series {
            label,
            points,
            slot,
        })
        .collect();

    let price_chart = chart::line_chart(
        &series,
        Unit::Gold,
        "Not enough history yet — the chart appears after a few collections.",
    );

    // Stock is a different measure on a different scale, so it gets its own
    // chart rather than a second y-axis.
    let stock_points: Vec<app_core::market::Point> = downsample(&stats.series, CHART_POINTS)
        .into_iter()
        .map(|p| app_core::market::Point {
            at: p.at,
            price: app_core::market::Copper(p.quantity),
            quantity: p.quantity,
        })
        .collect();
    let stock_chart = chart::line_chart(
        &[Series {
            label: "units listed",
            points: &stock_points,
            slot: 0,
        }],
        Unit::Count,
        "Not enough history yet.",
    );

    let hour_chart = chart::bar_chart(
        &stats.by_hour,
        &|b| format!("{b:02}:00"),
        "Needs a full day of observations.",
    );
    let weekday_chart = chart::bar_chart(
        &stats.by_weekday,
        &|b| WEEKDAY_NAMES[(b as usize).min(6)].to_string(),
        "Needs a full week of observations.",
    );

    // Per-patch, for this one market: one read of every stored window rather
    // than one reduction of the archive per patch column.
    let windows = model.windows_of(key).await?;
    let mut patches = Vec::new();
    for (patch, _, _) in catalog.patch_windows() {
        let window = Window::Patch(patch.patch.clone());
        let stats = windows.iter().find(|w| w.window == window);
        patches.push(match stats {
            Some(w) if w.samples > 0 => PatchStatRow {
                patch: patch.patch.clone(),
                label: patch.label(),
                mean: w.mean.to_string(),
                low: w.low.to_string(),
                high: w.high.to_string(),
                samples: w.samples,
                has_data: true,
            },
            _ => PatchStatRow {
                patch: patch.patch.clone(),
                label: patch.label(),
                mean: "—".into(),
                low: "—".into(),
                high: "—".into(),
                samples: 0,
                has_data: false,
            },
        });
    }

    let trend = |label: &'static str, t: analysis::Trend| TrendView {
        label,
        percent: t.percent,
        known: t.known,
        cheaper: t.percent < 0,
    };

    let tooltip = super::tooltip::cached_one(env, prefs, entry, item, now).await;
    let (section, section_path) = match entry.kind {
        ItemKind::Consumable => ("Consumables", "/wow/consumables"),
        ItemKind::Reagent => ("Reagents", "/wow/auctions/reagents"),
        ItemKind::Enchant => ("Enchants", "/wow/auctions/enchants"),
        ItemKind::Gem => ("Gems", "/wow/auctions/gems"),
        ItemKind::Boe => ("Bind-on-equip gear", "/wow/auctions/gear"),
        ItemKind::Recipe => ("Recipes", "/wow/auctions/recipes"),
    };

    Ok(ItemDetail {
        item_id: item.get(),
        // The localised name when the tooltip cache has it; the catalog's
        // English otherwise. The rank suffix stays ours either way.
        name: match (&tooltip, entry.ranks.len(), entry.rank_of(item)) {
            (Some(tip), total, Some(rank)) if total > 1 => format!("{} (R{rank})", tip.name),
            (Some(tip), _, _) => tip.name.clone(),
            (None, _, _) => entry.display_name(item),
        },
        icon: entry.icon_url(),
        tooltip,
        category: entry.category.label(),
        audience: entry.audience.as_str(),
        stat: entry.stat.as_str(),
        rank: entry.rank_of(item).unwrap_or(1),
        ranks_total: entry.ranks.len(),
        expansion: catalog.expansion.clone(),
        section,
        section_href: format!("{section_path}?expansion={}", catalog.id),
        expansion_href: format!("/wow/auctions?expansion={}", catalog.id),
        region: region.to_string().to_uppercase(),
        region_code: region.as_str(),
        archived: !env.catalog_state(catalog).is_collected(),

        // A market with no history renders every figure as unavailable rather
        // than as zero, which is §2's rule and the reason the empty state has
        // a shape of its own.
        has_data: stats.has_data(),
        current: dash(stats.has_data().then(|| stats.price.to_string())),
        mean: stats.mean.to_string(),
        median: stats.median.to_string(),
        low: dash(stats.has_data().then(|| stats.low.to_string())),
        low_when: dash(stats.has_data().then(|| stats.low_at.to_utc_string())),
        high: dash(stats.has_data().then(|| stats.high.to_string())),
        high_when: dash(stats.has_data().then(|| stats.high_at.to_utc_string())),
        quantity: stats.quantity,
        samples: stats.samples as usize,
        first_seen: dash(stats.first_seen.map(|at| at.to_date_string())),
        volatility_percent: stats.volatility_percent,
        trends: vec![
            trend("24 hours", stats.day),
            trend("7 days", stats.week),
            trend("30 days", stats.month),
        ],
        best_hour: stats.best_hour.map(|h| format!("{h:02}:00 UTC")),
        best_weekday: stats
            .best_weekday
            .map(|d| WEEKDAY_NAMES[(d as usize).min(6)].to_string()),
        price_chart,
        stock_chart,
        hour_chart,
        weekday_chart,
        series_labels: all_ranks
            .iter()
            .enumerate()
            .map(|(slot, (rank, _))| crate::views::SeriesKey {
                colour: crate::chart::series_colour(slot),
                label: if entry.ranks.len() > 1 {
                    format!("Rank {rank}")
                } else {
                    "Price".into()
                },
            })
            .collect(),
        patches,
    })
}

/// An unavailable figure, spelled the one way the whole app spells it.
fn dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "\u{2014}".into())
}
