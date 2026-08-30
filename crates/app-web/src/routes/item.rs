//! The single-item page: every statistic we hold, plus charts.

use app_core::market::{
    Catalog, CatalogItem, ItemId, ItemKind, analysis, analysis::WEEKDAY_NAMES, downsample,
};
use app_core::repo::{PriceRepository, Store, WatchRepository};
use app_core::{AppError, Ports};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::Millis;

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
    let prices = env.store().prices();
    let now = env.now();

    // Everything ever recorded for this market.
    let history = prices.history(item, region, Millis::ZERO).await?;
    let stats = analysis::analyse(&history, now);

    // Plot every rank of the same consumable together: "is R1 worth it" is the
    // question the chart should answer at a glance.
    let mut all_ranks = Vec::new();
    for rank in &entry.ranks {
        let samples = if rank.item_id == item {
            history.clone()
        } else {
            prices.history(rank.item_id, region, Millis::ZERO).await?
        };
        let analysed = analysis::analyse(&samples, now);
        all_ranks.push((rank.rank, downsample(&analysed.series, CHART_POINTS)));
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

    // Per-patch, for this one market.
    let mut patches = Vec::new();
    for (patch, from, until) in catalog.patch_windows() {
        let stats = prices
            .window_stats(region, from, until)
            .await?
            .into_iter()
            .find(|w| w.item == item);
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

        has_data: stats.samples > 0,
        current: stats
            .current
            .map(|p| p.price.to_string())
            .unwrap_or_else(|| "—".into()),
        mean: stats.mean.to_string(),
        median: stats.median.to_string(),
        low: stats
            .low
            .map(|p| p.price.to_string())
            .unwrap_or_else(|| "—".into()),
        low_when: stats
            .low
            .map(|p| p.at.to_utc_string())
            .unwrap_or_else(|| "—".into()),
        high: stats
            .high
            .map(|p| p.price.to_string())
            .unwrap_or_else(|| "—".into()),
        high_when: stats
            .high
            .map(|p| p.at.to_utc_string())
            .unwrap_or_else(|| "—".into()),
        quantity: stats.current.map(|p| p.quantity).unwrap_or(0),
        samples: stats.samples,
        first_seen: stats
            .first_seen
            .map(|at| at.to_date_string())
            .unwrap_or_else(|| "—".into()),
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
