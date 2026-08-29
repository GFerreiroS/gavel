//! One item at one item level: what it costs, and what it costs *with a
//! socket*.
//!
//! The gear grid answers "what is this worth right now"; this answers "is that
//! normal, and how often does a good one appear". Both questions are asked of
//! a single market — one item at one item level — because a Champion 2/6 helm
//! and a Hero 1/6 helm share an item id and nothing else that matters.
//!
//! Sockets and tertiary stats are counted rather than split out, for the
//! reason they are counted everywhere else: a socketed piece is the same
//! piece. What differs is how much of the market has one, which is exactly the
//! statistic this page exists to show.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::{Copper, ItemId, ItemKind, Point, Realm, RealmSample, Region};
use app_core::repo::{RealmPriceRepository, Store};
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
use crate::prefs::{MarketPrefs, RealmChoice, slug};
use crate::render::page;
use crate::session::current_user;
use crate::views::{GearLevelLink, GearModifierStat, GearStatsView, Layout, RealmOption};

/// How far back the page looks. Gear moves far more slowly than a commodity --
/// a raid tier is months -- so this is generous, and every figure on the page
/// says which window it came from.
const WINDOW_DAYS: u64 = 30;

/// Points per line. More than this and the SVG grows without the chart saying
/// anything further.
const CHART_POINTS: usize = 120;

#[derive(Template)]
#[template(path = "gear_stats.html")]
struct GearStatsPage {
    layout: Layout,
    stats: GearStatsView,
}

/// `GET /wow/gear/{item_id}/{item_level}` -- one item at one item level.
pub async fn stats<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    uri: OriginalUri,
    Path((item_id, item_level)): Path<(u32, u16)>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(
        item_id,
        Some(item_level),
        state,
        csrf,
        prefs,
        chosen,
        uri,
        headers,
    )
    .await
}

/// `GET /wow/recipe/{item_id}`
///
/// The same page without the ladder: a recipe has exactly one version of
/// itself, so there is no item level to choose between.
pub async fn recipe_stats<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    uri: OriginalUri,
    Path(item_id): Path<u32>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(item_id, None, state, csrf, prefs, chosen, uri, headers).await
}

#[allow(clippy::too_many_arguments)]
async fn render<E: Ports>(
    item_id: u32,
    wanted_level: Option<u16>,
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(chosen): Extension<RealmChoice>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let item = ItemId(item_id);
    let Some(catalog) = env.active_catalog() else {
        return Err(app_core::AppError::NotFound.into());
    };
    let Some(entry) = catalog.find(item).filter(|e| !e.kind.is_commodity()) else {
        return Err(app_core::AppError::NotFound.into());
    };

    // Which bonus id carries this item level. An item level nobody has
    // resolved is not a page: better a 404 than a graph of everything.
    let resolved = match wanted_level {
        Some(want) => {
            let found = catalog
                .item_levels
                .iter()
                .find(|(_, level)| level.item_level == want)
                .and_then(|(bonus, level)| Some((bonus.parse::<u32>().ok()?, level)));
            match found {
                Some((bonus, level)) => Some((bonus, level.item_level, level.upgrade.clone())),
                None => return Err(app_core::AppError::NotFound.into()),
            }
        }
        None => None,
    };

    let prices = env.store().realm_prices();
    let now = env.now();
    let since = Millis(now.get().saturating_sub(WINDOW_DAYS * 24 * 60 * 60 * 1000));

    let realms = prices.realms().await?;
    let selected: Option<&Realm> = chosen.0.as_deref().and_then(|want| {
        realms
            .iter()
            .find(|r| slug(&r.name).as_deref() == Some(want))
    });

    // Everything this item did over the window, then narrowed to the one item
    // level. Narrowing here rather than in SQL keeps the bonus grouping in one
    // place -- the store deals in variants and knows nothing about levels.
    let mut history: Vec<RealmSample> = Vec::new();
    match selected {
        Some(realm) => history.extend(prices.history(item, realm.region, realm.id, since).await?),
        None => {
            let mut regions: Vec<Region> = realms.iter().map(|r| r.region).collect();
            regions.sort();
            regions.dedup();
            for region in regions {
                history.extend(prices.history_in_region(item, region, since).await?);
            }
        }
    }
    // A recipe has one version of itself, so every sample of it belongs here.
    if let Some((upgrade_bonus, _, _)) = resolved {
        history.retain(|s| super::gear::has_bonus(&s.variant, upgrade_bonus));
    }

    let (title, section, section_href) = match entry.kind {
        ItemKind::Recipe => ("Recipes", "Recipes", "/wow/auctions/recipes"),
        _ => (
            "Bind-on-equip gear",
            "Bind-on-equip gear",
            "/wow/auctions/gear",
        ),
    };

    let user = current_user(&env, &headers).await?;
    let stats = build(
        BuildInput {
            item,
            item_level: resolved.as_ref().map(|(_, level, _)| *level).unwrap_or(0),
            upgrade: resolved
                .as_ref()
                .map(|(_, _, upgrade)| upgrade.clone())
                .unwrap_or_default(),
            history,
            selected,
            realms: &realms,
            // The ladder is only meaningful where there is one.
            section,
            section_href,
            catalog_levels: match resolved {
                Some(_) => catalog
                    .item_levels
                    .values()
                    .map(|l| (l.item_level, l.upgrade.clone()))
                    .collect(),
                None => Vec::new(),
            },
        },
        entry,
        prefs,
        &env,
        now,
    )
    .await;

    page(
        &GearStatsPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                title,
                "/wow/auctions",
                &uri,
                user.map(|u| u.username),
                csrf.0.clone(),
            ),
            stats,
        },
        prefs.locale,
    )
}

struct BuildInput<'a> {
    item: ItemId,
    item_level: u16,
    upgrade: String,
    section: &'static str,
    section_href: &'static str,
    history: Vec<RealmSample>,
    selected: Option<&'a Realm>,
    realms: &'a [Realm],
    catalog_levels: Vec<(u16, String)>,
}

async fn build<E: Ports>(
    input: BuildInput<'_>,
    entry: &app_core::market::CatalogItem,
    prefs: MarketPrefs,
    env: &E,
    now: Millis,
) -> GearStatsView {
    let BuildInput {
        item,
        item_level,
        upgrade,
        section,
        section_href,
        history,
        selected,
        realms,
        catalog_levels,
    } = input;

    let tooltip = super::tooltip::cached_one(env, prefs, entry, item, now).await;
    let observed = history.iter().map(|s| s.observed_at).max();

    // The newest snapshot is "now". Taken per realm, because realms are
    // generated on their own schedules and the newest overall would silently
    // drop every realm that had not refreshed yet.
    let mut newest: BTreeMap<(Region, u32), Millis> = BTreeMap::new();
    for sample in &history {
        let at = newest
            .entry((sample.region, sample.realm.get()))
            .or_insert(sample.observed_at);
        *at = (*at).max(sample.observed_at);
    }
    let current: Vec<&RealmSample> = history
        .iter()
        .filter(|s| newest.get(&(s.region, s.realm.get())) == Some(&s.observed_at))
        .collect();

    let cheapest_now = current.iter().map(|s| s.min_price).min();
    let highest_now = current
        .iter()
        .map(|s| s.max_price)
        .max()
        .filter(|p| p.get() > 0)
        .or(cheapest_now);
    let cheapest_ever = history.iter().map(|s| s.min_price).min();
    let highest_ever = history
        .iter()
        .map(|s| s.max_price)
        .max()
        .filter(|p| p.get() > 0)
        .or(cheapest_ever);

    let gold = |price: Option<Copper>| price.map(|p| p.to_string()).unwrap_or_else(|| "—".into());

    GearStatsView {
        item_id: item.get(),
        name: tooltip
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| entry.name.clone()),
        icon: entry.icon_url(),
        slot: entry.slot.map(|s| s.label()).unwrap_or(""),
        item_level,
        upgrade,
        section,
        section_href,
        siblings: catalog_levels
            .into_iter()
            .map(|(level, upgrade)| GearLevelLink {
                item_level: level,
                upgrade,
                href: format!("/wow/gear/{}/{level}", item.get()),
                current: level == item_level,
            })
            .collect(),
        scope: selected.map(|r| r.name.clone()),
        realm_slug: selected.and_then(|r| slug(&r.name)).unwrap_or_default(),
        region: selected.map_or(prefs.region, |r| r.region).as_str(),
        realms: realms
            .iter()
            .map(|realm| RealmOption {
                value: slug(&realm.name).unwrap_or_default(),
                name: realm.name.clone(),
                region: realm.region.to_string().to_uppercase(),
                selected: selected.is_some_and(|r| r.id == realm.id && r.region == realm.region),
            })
            .collect(),
        observed: super::market::observed(prefs, now, observed),
        window_days: WINDOW_DAYS,
        snapshots: history
            .iter()
            .map(|s| s.observed_at)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        cheapest_now: gold(cheapest_now),
        highest_now: gold(highest_now),
        cheapest_ever: gold(cheapest_ever),
        highest_ever: gold(highest_ever),
        listings_now: current.iter().map(|s| s.listings).sum(),
        modifiers: modifier_stats(&history, &current, env),
        price_chart: price_chart(&history, selected),
        listings_chart: listings_chart(&history, selected),
        unlisted: history.is_empty(),
        tooltip,
    }
}

/// How common each socket or tertiary is: in the newest snapshot, and across
/// the window.
fn modifier_stats<E: Ports>(
    history: &[RealmSample],
    current: &[&RealmSample],
    env: &E,
) -> Vec<GearModifierStat> {
    let Some(catalog) = env.active_catalog() else {
        return Vec::new();
    };
    let total_seen: u32 = history.iter().map(|s| s.listings).sum();

    let mut now: BTreeMap<&str, u32> = BTreeMap::new();
    for sample in current {
        for name in super::gear::modifier_names(&sample.variant, catalog) {
            *now.entry(name).or_default() += sample.listings;
        }
    }
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    for sample in history {
        for name in super::gear::modifier_names(&sample.variant, catalog) {
            *seen.entry(name).or_default() += sample.listings;
        }
    }

    seen.into_iter()
        .map(|(name, count)| GearModifierStat {
            name: name.to_string(),
            now: now.get(name).copied().unwrap_or(0),
            seen: count,
            share: if total_seen == 0 {
                0
            } else {
                (count * 100).div_ceil(total_seen).min(100)
            },
        })
        .collect()
}

/// What it has cost: one line per region, or one for the chosen realm.
///
/// A line per *realm* would be six, and the palette is two -- but more to the
/// point, six overlapping lines answer no question. Each region's line is the
/// median of what its realms' cheapest copies cost, the same figure the grid
/// shows.
fn price_chart(history: &[RealmSample], selected: Option<&Realm>) -> String {
    let grouped = series_points(history, selected, |samples| {
        let mut cheapest: Vec<Copper> = samples.iter().map(|s| s.min_price).collect();
        cheapest.sort_unstable();
        cheapest
            .get(cheapest.len() / 2)
            .copied()
            .unwrap_or_default()
    });
    draw(&grouped, Unit::Gold)
}

/// How many are for sale. A price with two listings behind it is a different
/// fact from the same price with forty.
fn listings_chart(history: &[RealmSample], selected: Option<&Realm>) -> String {
    let grouped = series_points(history, selected, |samples| {
        Copper(samples.iter().map(|s| s.listings as u64).sum())
    });
    draw(&grouped, Unit::Count)
}

/// Collapse the history into one series per region (or one for a realm),
/// applying `value` to the samples sharing a timestamp.
fn series_points(
    history: &[RealmSample],
    selected: Option<&Realm>,
    value: impl Fn(&[&RealmSample]) -> Copper,
) -> Vec<(String, Vec<Point>)> {
    let mut by_series: BTreeMap<String, BTreeMap<Millis, Vec<&RealmSample>>> = BTreeMap::new();
    for sample in history {
        let label = match selected {
            Some(realm) => realm.name.clone(),
            None => sample.region.to_string().to_uppercase(),
        };
        by_series
            .entry(label)
            .or_default()
            .entry(sample.observed_at)
            .or_default()
            .push(sample);
    }

    by_series
        .into_iter()
        .map(|(label, at)| {
            let points: Vec<Point> = at
                .into_iter()
                .map(|(at, samples)| Point {
                    at,
                    price: value(&samples),
                    quantity: samples.iter().map(|s| s.listings as u64).sum(),
                })
                .collect();
            (label, thin(points))
        })
        .collect()
}

/// Keep the series inside [`CHART_POINTS`], evenly.
fn thin(points: Vec<Point>) -> Vec<Point> {
    if points.len() <= CHART_POINTS {
        return points;
    }
    let step = points.len().div_ceil(CHART_POINTS);
    points.into_iter().step_by(step).collect()
}

fn draw(series: &[(String, Vec<Point>)], unit: Unit) -> String {
    let lines: Vec<Series<'_>> = series
        .iter()
        .enumerate()
        .map(|(slot, (label, points))| Series {
            label,
            points,
            slot,
        })
        .collect();
    chart::line_chart(
        &lines,
        unit,
        "Not enough history yet — the chart appears after a few collections.",
    )
}
