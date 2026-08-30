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

use app_core::Ports;
use app_core::market::materialise::{MarketRollup, Scope};
use app_core::market::{Catalog, Copper, ItemId, ItemKind, Point, Realm, Track};
use app_core::repo::{ReadModelRepository, RealmPriceRepository, Store};
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
use crate::prefs::{MarketPrefs, RealmChoice};
use crate::render::page;
use crate::session::current_user;
use crate::views::{GearLevelLink, GearModifierStat, GearStatsView, Layout};

/// How far back the page looks. Gear moves far more slowly than a commodity --
/// a raid tier is months -- so this is generous, and every figure on the page
/// says which window it came from.
const WINDOW_DAYS: u64 = 30;

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
    Path((item_id, track)): Path<(u32, String)>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(
        item_id,
        Track::parse(&track),
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
    wanted_track: Option<Track>,
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

    // Gear is asked for by track; a recipe has no track and is asked for by
    // item alone. A slug that is not a track at all is a 404 rather than a
    // graph of everything.
    if entry.kind != ItemKind::Recipe && wanted_track.is_none() {
        return Err(app_core::AppError::NotFound.into());
    }

    let prices = env.store().realm_prices();
    let now = env.now();

    // The reader's region, chosen once on the Auction House index. Regions are
    // separate markets and were never merged; this stops the page offering the
    // other three as though they were an option here.
    let realms: Vec<Realm> = prices
        .realms()
        .await?
        .into_iter()
        .filter(|r| r.region == prefs.region)
        .collect();
    let want_realm = chosen.0.clone();
    let selected: Option<&Realm> = want_realm
        .as_deref()
        .and_then(|want| realms.iter().find(|r| super::gear::realm_matches(r, want)));

    // One stored row. Everything this page shows -- the figures, the level
    // breakdown, the modifier counts and both charts -- was rolled up on the
    // write path, for the region or for the chosen realm as the reader asked.
    // It used to be one query per realm's history and a reduction of all of it
    // inside the request.
    let scope = selected.map_or(Scope::Region, |realm| Scope::Realm(realm.id));
    let rollup = env
        .store()
        .read_model()
        .rollup(prefs.region, item, wanted_track, scope)
        .await?;

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
            track: wanted_track,
            rollup,
            selected,
            want_realm: want_realm.clone(),
            // The ladder is only meaningful where there is one.
            section,
            section_href,
            catalog,
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
                user.as_ref(),
                csrf.masked(),
            ),
            stats,
        },
        prefs.locale,
    )
}

struct BuildInput<'a> {
    item: ItemId,
    /// `None` for a recipe, which has one version of itself.
    track: Option<Track>,
    section: &'static str,
    section_href: &'static str,
    /// `None` for a market nothing has ever been listed on.
    rollup: Option<MarketRollup>,
    selected: Option<&'a Realm>,
    /// The slug the reader asked for, so the picker can echo their own realm's
    /// name back rather than the connected realm's joined one.
    want_realm: Option<String>,
    catalog: &'a Catalog,
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
        track,
        section,
        section_href,
        rollup,
        selected,
        want_realm,
        catalog,
    } = input;

    let tooltip = super::tooltip::cached_one(env, prefs, entry, item, now).await;
    let _ = catalog;

    // A market nothing has ever been listed on renders as unlisted rather than
    // as a page of zeroes. §2: an unavailable fact is rendered unavailable.
    let empty = MarketRollup::empty(prefs.region, item, entry.kind, track);
    let stats = rollup.as_ref().unwrap_or(&empty);

    let gold = |price: Option<Copper>| {
        price
            .map(|p| p.to_string())
            .unwrap_or_else(|| "\u{2014}".into())
    };

    // The cross-realm spread, and only across realms: on a single realm it
    // would be a five-number summary of one number.
    let spread = selected.is_none().then_some(stats.realm_spread).flatten();

    GearStatsView {
        item_id: item.get(),
        name: tooltip
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| entry.name.clone()),
        icon: entry.icon_url(),
        slot: entry.slot.map(|s| s.label()).unwrap_or(""),
        track: track.map(Track::as_str).unwrap_or(""),
        level_range: stats.level_range.clone(),
        section,
        section_href,
        // The ladder is the other tracks, one click wide. Only for gear: a
        // recipe has nothing to climb.
        siblings: match track {
            None => Vec::new(),
            Some(_) => Track::ALL
                .into_iter()
                .map(|other| GearLevelLink {
                    track: other.as_str(),
                    href: format!("/wow/gear/{}/{}", item.get(), other.slug()),
                    current: Some(other) == track,
                })
                .collect(),
        },
        levels: stats
            .levels
            .iter()
            .map(|level| crate::views::GearLevelStat {
                item_level: level.item_level,
                upgrade: level.upgrade.clone(),
                cheapest: level.cheapest.to_string(),
                highest: level.highest.to_string(),
                listings: level.listings,
                realms: level.realms as usize,
            })
            .collect(),
        kind: match entry.kind {
            ItemKind::Recipe => "recipes",
            _ => "gear",
        },
        realm_name: match (selected, want_realm.as_deref()) {
            (Some(realm), Some(want)) => super::gear::member_named(realm, want),
            _ => String::new(),
        },
        scope: selected.map(|r| r.name.clone()),
        realm_slug: want_realm.clone().unwrap_or_default(),
        region: selected.map_or(prefs.region, |r| r.region).as_str(),
        observed: super::market::observed(prefs, now, stats.observed_at),
        window_days: WINDOW_DAYS,
        snapshots: stats.snapshots as usize,
        cheapest_now: gold(stats.cheapest_now),
        highest_now: gold(stats.highest_now),
        cheapest_ever: gold(stats.cheapest_ever),
        highest_ever: gold(stats.highest_ever),
        listings_now: stats.listings_now,
        // Only across realms. On one realm the fraction is one out of one and
        // the spread is a summary of a single number, so both are absent
        // rather than rendered as a tautology.
        realms_listing: (selected.is_none()).then_some(stats.realms_listing),
        realms_collected: stats.realms_collected,
        spread_cheapest: spread.map(|d| gold(Some(d.p05))),
        spread_median: spread.map(|d| gold(Some(d.median))),
        spread_dearest: spread.map(|d| gold(Some(d.p95))),
        spread_percent: spread.and_then(|d| {
            // From the cheapest realm up to the median one: what a reader
            // saves by not simply buying at home. Measured from the median
            // rather than from the dearest, because the dearest realm is one
            // seller having a bad day and nobody is choosing to fly there.
            let (cheap, median) = (d.p05.get(), d.median.get());
            (median > 0 && cheap > 0 && median > cheap)
                .then(|| ((median - cheap) * 100 / median) as u32)
        }),
        modifiers: stats
            .modifiers
            .iter()
            .map(|modifier| GearModifierStat {
                name: modifier.name.clone(),
                now: modifier.now,
                seen: modifier.seen,
                // Derived rather than stored: it is two stored numbers divided,
                // and a stored ratio is a third thing that can disagree with
                // them.
                share: if stats.listings_seen == 0 {
                    0
                } else {
                    (modifier.seen * 100).div_ceil(stats.listings_seen).min(100)
                },
            })
            .collect(),
        // One stored series, two charts: the price line is what the realms in
        // scope charge for the cheapest copy, the listings line is how many
        // are behind it.
        price_chart: draw_series(
            &stats.series,
            series_label(selected, prefs),
            |p| p.price,
            Unit::Gold,
        ),
        listings_chart: draw_series(
            &stats.series,
            series_label(selected, prefs),
            |p| Copper(p.quantity),
            Unit::Count,
        ),
        unlisted: rollup.is_none(),
        tooltip,
    }
}

/// What the chart's one line is called: the realm, or the region.
fn series_label(selected: Option<&Realm>, prefs: MarketPrefs) -> String {
    match selected {
        Some(realm) => realm.name.clone(),
        None => prefs.region.to_string().to_uppercase(),
    }
}

/// Draw one stored series, reading one of its two axes.
fn draw_series(
    series: &[Point],
    label: String,
    value: impl Fn(&Point) -> Copper,
    unit: Unit,
) -> String {
    let points: Vec<Point> = series
        .iter()
        .map(|p| Point {
            at: p.at,
            price: value(p),
            quantity: p.quantity,
        })
        .collect();
    chart::line_chart(
        &[Series {
            label: &label,
            points: &points,
            slot: 0,
        }],
        unit,
        "Not enough history yet \u{2014} the chart appears after a few collections.",
    )
}
