//! The single-item page: every statistic we hold, plus charts.

use app_core::market::{
    Catalog, CatalogItem, ItemId, Region, analysis, analysis::WEEKDAY_NAMES, downsample,
};
use app_core::repo::{PriceRepository, Store};
use app_core::{AppError, Ports};
use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::Millis;

use crate::chart::{self, Series};
use crate::csrf::Csrf;
use crate::error::WebResult;
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
}

pub async fn detail<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
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

    let detail = build(&env, &catalog, &entry, item).await?;
    let user = current_user(&env, &headers).await?;
    page(&ItemPage {
        layout: Layout::new(
            env.config(),
            &detail.name,
            "/wow/consumables",
            user.map(|u| u.username),
            csrf.0.clone(),
        ),
        item: detail,
    })
}

async fn build<E: Ports>(
    env: &E,
    catalog: &Catalog,
    entry: &CatalogItem,
    item: ItemId,
) -> WebResult<ItemDetail> {
    let region = env.market().regions.first().copied().unwrap_or(Region::Eu);
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

    Ok(ItemDetail {
        item_id: item.get(),
        name: entry.display_name(item),
        icon: entry.icon_url(),
        category: entry.category.label(),
        audience: entry.audience.as_str(),
        stat: entry.stat.as_str(),
        rank: entry.rank_of(item).unwrap_or(1),
        ranks_total: entry.ranks.len(),
        expansion: catalog.expansion.clone(),
        catalog_id: catalog.id.clone(),
        region: region.to_string().to_uppercase(),
        archived: !catalog.is_active(),

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
            .map(|(rank, _)| {
                if entry.ranks.len() > 1 {
                    format!("Rank {rank}")
                } else {
                    "Price".into()
                }
            })
            .collect(),
        patches,
    })
}
