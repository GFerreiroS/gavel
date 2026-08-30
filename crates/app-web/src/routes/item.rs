//! The single-item page: a shell, and the analysis it loads into it.
//!
//! **Split in two, and the split is the point.** CLAUDE.md §16's Phase 6 asks
//! to "keep personalised watch controls separate from the otherwise cacheable
//! market analysis body", and the two halves genuinely are different kinds of
//! thing. The shell carries the nav, the CSRF token and whether *you* follow
//! this item, so it keeps `no-store` without exception (§10). The body is a
//! pure function of the published version, the item, the region, the
//! comparison window and the locale -- so it carries an ETag, lives in the
//! fragment cache, and is the same bytes for everybody who asks for it.
//!
//! That is also why this page pays a second round trip where the Gear page
//! does not. Phase 3's rule is that a second request has to buy something: the
//! gear cards are stored rows and arrive no sooner for being fetched
//! separately, but this body builds five SVGs, and after the first reader
//! since a publication it is a cache hit instead.

use app_core::market::materialise::{MarketState, MarketWindow};
use app_core::market::window::Window;
use app_core::market::{
    Catalog, CatalogItem, Copper, ItemId, ItemKind, analysis, analysis::WEEKDAY_NAMES,
};
use app_core::repo::{ReadModelRepository, Store, WatchRepository};
use app_core::{AppError, Ports};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::chart;
use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{ItemAnalysis, ItemDetail, Layout, PanelHead, PatchStatRow, TrendView};

#[derive(Template)]
#[template(path = "item.html")]
struct ItemPage {
    layout: Layout,
    item: ItemDetail,
    /// `None` when nobody is signed in: the control is not offered, rather
    /// than offered and refused.
    following: Option<bool>,
}

#[derive(Template)]
#[template(path = "partials/item.html")]
struct AnalysisFragment {
    analysis: ItemAnalysis,
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
    let (catalog, entry) = lookup(&env, item)?;
    let detail = head(&env, prefs, &catalog, &entry, item).await?;
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

/// The analysis body: cacheable, revalidated, and identical for every reader.
pub async fn analysis<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(cache): Extension<std::sync::Arc<crate::FragmentCache>>,
    headers: HeaderMap,
    Path(item_id): Path<u32>,
) -> WebResult<axum::response::Response> {
    let item = ItemId(item_id);
    let (catalog, entry) = lookup(&env, item)?;

    let key = crate::fragment_cache::FragmentKey::new(
        "item",
        env.store()
            .read_model()
            .published()
            .await?
            .map(|v| v.version),
        &catalog.id,
        prefs.region.as_str(),
        prefs.baseline_days,
        "",
        prefs.locale.code(),
        // The item is what makes one of these fragments different from the
        // next, so it goes in the slot the group occupies elsewhere. Leaving
        // it out would serve one item's charts under another's name.
        Some(&item_id.to_string()),
    );

    crate::fragment_cache::respond(&cache, &headers, key, async {
        let analysis = build(&env, prefs, &catalog, &entry, item).await?;
        Ok(page(&AnalysisFragment { analysis }, prefs.locale)?.0)
    })
    .await
}

fn lookup<E: Ports>(env: &E, item: ItemId) -> Result<(Catalog, CatalogItem), AppError> {
    env.catalogs()
        .index()
        .get(&item)
        .map(|(c, i)| ((*c).clone(), (*i).clone()))
        .ok_or(AppError::NotFound)
}

/// The shell's half: who this item is, and nothing about what it costs.
async fn head<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    catalog: &Catalog,
    entry: &CatalogItem,
    item: ItemId,
) -> WebResult<ItemDetail> {
    let tooltip = super::tooltip::cached_one(env, prefs, entry, item, env.now()).await;
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
        region: prefs.region.to_string().to_uppercase(),
        region_code: prefs.region.as_str(),
        archived: !env.catalog_state(catalog).is_collected(),
    })
}

async fn build<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    catalog: &Catalog,
    entry: &CatalogItem,
    item: ItemId,
) -> WebResult<ItemAnalysis> {
    let region = prefs.region;
    let model = env.store().read_model();
    let now = env.now();
    let locale = prefs.locale;

    // The published state of this market, and every window of it. Two reads,
    // and neither reduces anything: this handler does not call `analyse`, does
    // not read an observation, and since Phase 6 does not downsample either.
    let key = catalog.market_of_key(region, item);
    let state = model
        .market(key)
        .await?
        .unwrap_or_else(|| MarketState::empty(key));
    let windows = model.windows_of(key).await?;

    // The reader's comparison window, which the Auction House index owns for
    // every page beneath it (§7). Every figure on this page describes it, and
    // the panels say so rather than leaving the reader to assume.
    let chosen = Window::Days(prefs.baseline_days);
    let window = windows.iter().find(|w| w.window == chosen);
    let window_label = crate::i18n::translate(locale, "the last {} days").replacen(
        "{}",
        &prefs.baseline_days.to_string(),
        1,
    );

    let money = |value: Copper| value.to_string();
    let dash = |value: Option<String>| value.unwrap_or_else(|| "\u{2014}".into());

    // --- the price panel ---------------------------------------------------
    // Every rank of the same consumable on one chart: "is R1 worth it" is the
    // question it should answer at a glance. Each rank is its own market and
    // its own stored series, so this is a read per rank and no arithmetic.
    let mut ranks: Vec<&app_core::market::ItemRank> = entry.ranks.iter().collect();
    ranks.sort_by_key(|r| r.rank);
    let mut rank_series = Vec::new();
    for rank in &ranks {
        let series = if rank.item_id == item {
            window.map(|w| w.series.clone()).unwrap_or_default()
        } else {
            model
                .windows_of(catalog.market_of_key(region, rank.item_id))
                .await?
                .into_iter()
                .find(|w| w.window == chosen)
                .map(|w| w.series)
                .unwrap_or_default()
        };
        rank_series.push((rank.rank, series));
    }

    // The band chart draws this item; the other ranks ride along as plain
    // lines, because two bands overlapping is two clouds and no information.
    let mine = rank_series
        .iter()
        .find(|(rank, _)| Some(*rank) == entry.rank_of(item))
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let price_chart = chart::band_chart(
        &mine,
        state.has_data().then_some(state.price),
        crate::i18n::translate(
            locale,
            "Not enough history yet — the chart appears after a few collections.",
        ),
    );

    let distribution_chart = chart::histogram_chart(
        window
            .and_then(|w| w.histogram.as_ref())
            .unwrap_or(&app_core::market::Histogram {
                lo: Copper::ZERO,
                hi: Copper::ZERO,
                bins: Vec::new(),
            }),
        state.has_data().then_some(state.price),
        crate::i18n::translate(locale, "Not enough history to show a distribution yet."),
    );

    let stock_chart = chart::stock_chart(
        &mine,
        crate::i18n::translate(locale, "Not enough history yet."),
    );

    let hour_chart = chart::bar_chart(
        &state.by_hour,
        &|b| format!("{b:02}:00"),
        crate::i18n::translate(locale, "Needs a full day of observations."),
    );
    let weekday_chart = chart::bar_chart(
        &state.by_weekday,
        &|b| WEEKDAY_NAMES[(b as usize).min(6)].to_string(),
        crate::i18n::translate(locale, "Needs a full week of observations."),
    );

    // --- per patch ---------------------------------------------------------
    let mut patches = Vec::new();
    for (patch, _, _) in catalog.patch_windows() {
        let stats = windows
            .iter()
            .find(|w| w.window == Window::Patch(patch.patch.clone()));
        patches.push(match stats {
            Some(w) if w.samples > 0 => PatchStatRow {
                patch: patch.patch.clone(),
                label: patch.label(),
                // The engine's median, so a patch row and the panel above it
                // mean the same thing by the word. It was the mean before
                // Phase 5, which is the measure one spike moves.
                mean: money(w.distribution.median),
                low: money(w.low),
                high: money(w.high),
                samples: w.samples,
                has_data: true,
            },
            _ => PatchStatRow {
                patch: patch.patch.clone(),
                label: patch.label(),
                mean: "\u{2014}".into(),
                low: "\u{2014}".into(),
                high: "\u{2014}".into(),
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

    let freshness = state
        .observed_at
        .map(|at| crate::format::ago(locale, now.since(at)));
    let coverage_text = window.and_then(coverage_of);

    // A panel head with the terms that panel actually answers in. Phase 6's
    // exit gate is that each names its question, window, units, coverage and
    // freshness -- so they are built here, together, where a missing one is
    // visible rather than in five places in a template.
    let panel = |question: &'static str,
                 units: &'static str,
                 coverage: Option<String>,
                 fresh: Option<String>| PanelHead {
        question,
        window: window_label.clone(),
        units,
        coverage,
        freshness: fresh,
    };

    let position = window.map(|w| w.position);
    let insufficient = position.and_then(|p| p.insufficient);

    Ok(ItemAnalysis {
        has_data: state.has_data(),

        price_panel: panel(
            "What has this been worth, and how tightly?",
            "gold",
            coverage_text.clone(),
            freshness.clone(),
        ),
        current: dash(state.has_data().then(|| money(state.price))),
        band: position.and_then(|p| p.valuation).map(|v| v.as_str()),
        band_slug: position
            .and_then(|p| p.valuation)
            .map(|v| v.slug())
            .unwrap_or("none"),
        rank_percent: position.and_then(|p| p.rank),
        from_median_percent: position.and_then(|p| p.from_median_percent),
        insufficient: insufficient.map(|reason| match reason {
            app_core::market::Insufficient::NotEnoughHistory { .. } => "Not enough history",
            app_core::market::Insufficient::TooManyGaps { .. } => "Too many gaps",
        }),
        insufficient_have: match insufficient {
            Some(app_core::market::Insufficient::NotEnoughHistory { have, .. }) => have,
            Some(app_core::market::Insufficient::TooManyGaps { coverage, .. }) => coverage,
            None => 0,
        },
        insufficient_need: match insufficient {
            Some(app_core::market::Insufficient::NotEnoughHistory { need, .. })
            | Some(app_core::market::Insufficient::TooManyGaps { need, .. }) => need,
            None => 0,
        },
        anomaly: position
            .map(|p| p.anomaly.as_str())
            .unwrap_or(app_core::market::Anomaly::Ordinary.as_str()),
        anomaly_slug: match position.map(|p| p.anomaly) {
            Some(app_core::market::Anomaly::Extreme) => "extreme",
            Some(app_core::market::Anomaly::Mild) => "mild",
            _ => "ordinary",
        },
        median: dash(window.map(|w| money(w.distribution.median))),
        p25: dash(window.map(|w| money(w.distribution.p25))),
        p75: dash(window.map(|w| money(w.distribution.p75))),
        iqr: dash(window.map(|w| money(w.distribution.iqr))),
        mad: dash(window.map(|w| money(w.distribution.mad))),

        distribution_panel: panel(
            "What prices has this market spent its time at?",
            "hours",
            coverage_text.clone(),
            freshness.clone(),
        ),
        stock_panel: panel(
            "Is there enough of it to buy?",
            "units listed",
            coverage_text.clone(),
            freshness.clone(),
        ),
        quantity: state.quantity,
        listings: state.listings,

        quality_panel: panel(
            "How much of this window did we actually see?",
            "hours",
            coverage_text,
            freshness.clone(),
        ),
        samples: state.samples as usize,
        observed_buckets: window.map(|w| w.observed_buckets).unwrap_or(0),
        expected_buckets: window.and_then(|w| w.expected_buckets),
        coverage_percent: window.and_then(|w| w.coverage_percent()),
        largest_gap: dash(
            window
                .filter(|w| w.largest_gap_ms > 0)
                .map(|w| crate::format::duration_ms(w.largest_gap_ms)),
        ),
        first_seen: dash(state.first_seen.map(|at| at.to_date_string())),
        observed_at: dash(freshness),

        trends: vec![
            trend("24 hours", state.day),
            trend("7 days", state.week),
            trend("30 days", state.month),
        ],
        swing_percent: window.map(|w| w.swing.0).unwrap_or(0),

        best_hour: state.best_hour.map(|h| format!("{h:02}:00 UTC")),
        best_weekday: state
            .best_weekday
            .map(|d| WEEKDAY_NAMES[(d as usize).min(6)].to_string()),
        // The cycle charts are over the whole history rather than the chosen
        // window -- an hour-of-day average from one day is not an average --
        // so they say so instead of borrowing the window above.
        cycle_panel: PanelHead {
            question: "When in the week is it cheapest?",
            window: crate::i18n::translate(locale, "the whole history").to_string(),
            units: "gold",
            coverage: None,
            freshness: None,
        },

        price_chart,
        distribution_chart,
        stock_chart,
        hour_chart,
        weekday_chart,
        series_labels: rank_series
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

        patch_panel: PanelHead {
            question: "How has each patch priced it?",
            window: crate::i18n::translate(locale, "one row per patch").to_string(),
            units: "gold",
            coverage: None,
            freshness: None,
        },
        patches,
    })
}

/// "57 of 336 hours (17%)", or nothing to say.
///
/// Both numbers, not just the percentage: 17% of a fortnight and 17% of a day
/// are the same fraction and very different evidence, and §2 is that the
/// reader is told what a figure is a figure of.
fn coverage_of(window: &MarketWindow) -> Option<String> {
    let expected = window.expected_buckets?;
    let percent = window.coverage_percent()?;
    Some(format!(
        "{} / {} ({}%)",
        window.observed_buckets, expected, percent
    ))
}
