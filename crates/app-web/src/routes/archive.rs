//! The archive: expansion -> patch -> raid tier -> market analysis.
//!
//! `docs/market-analysis.md` §8 draws the hierarchy and CLAUDE.md §16's
//! Phase 9 asks for it to be navigable after a patch or a tier has stopped
//! collecting. Four levels, four routes, and **no statistic of their own**.
//!
//! That last part is the design rather than an economy. Everything with a
//! price in it here is a component the app already has:
//!
//! * the patch level defers to `/partials/patches`, the same fragment the
//!   consumables page fetches, narrowed to one column;
//! * the tier level draws its bind-on-equip gear with the same `gear_group`
//!   macro the Gear page calls, from the same stored roll-ups;
//! * an item links to the item page, which is where market analysis lives.
//!
//! Phase 9's exit gate is that a tier rollover forks no route, template or
//! statistic. A tier page that reduced a history, or drew a card of its own,
//! would be the fork -- and it would drift from the pages it was copied from
//! within one patch (§7).
//!
//! ## The order things are validated in
//!
//! §16: "Validate current expansion first and patch second in
//! catalogue/navigation paths, and keep patch and raid/tier as independent
//! keys." So every handler below resolves the expansion, then looks for the
//! patch *inside it*, then the tier *inside that*. `/wow/archive/midnight/11.2`
//! is a 404 even though 11.2 is a real patch, because it is not Midnight's --
//! and a tier is found by its own id rather than by its position under a
//! patch.

use app_core::Ports;
use app_core::market::{ArchivedExpansion, ArchivedPatch, ArchivedTier, ItemKind, Realm, Scope};
use app_core::repo::{MarketEventRepository, ReadModelRepository, RealmPriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::Millis;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{
    ArchiveExpansionCard, ArchivePatchCard, ArchivePatchView, ArchiveTierLink, ArchiveTierView,
    ArchiveView, ExpansionView, GearGroup, Layout, TimelineRow,
};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Most events one archive page lists.
///
/// A patch runs for months and the weekly reset alone would fill a page. The
/// cap is generous enough that a real patch's real events all fit and small
/// enough that nothing can turn this into a feed.
const TIMELINE_LIMIT: usize = 40;

#[derive(Template)]
#[template(path = "archive.html")]
struct ArchivePage {
    layout: Layout,
    archive: ArchiveView,
}

#[derive(Template)]
#[template(path = "archive_expansion.html")]
struct ExpansionPage {
    layout: Layout,
    expansion: ExpansionView,
}

#[derive(Template)]
#[template(path = "archive_patch.html")]
struct PatchPage {
    layout: Layout,
    patch: ArchivePatchView,
}

#[derive(Template)]
#[template(path = "archive_tier.html")]
struct TierPage {
    layout: Layout,
    tier: ArchiveTierView,
}

/// `GET /wow/archive` -- every expansion a visitor may browse.
///
/// No database at all: the hierarchy is derived from the catalogues, which are
/// in the binary. §15's read path taken to its conclusion -- there is nothing
/// to read because there is nothing to calculate.
pub async fn index<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;

    let archive = ArchiveView {
        expansions: env
            .archive()
            .expansions
            .iter()
            .map(|expansion| ArchiveExpansionCard {
                name: expansion.name.clone(),
                href: format!("/wow/archive/{}", expansion.slug),
                span: span(prefs.locale, expansion.from, expansion.until),
                patches: expansion.patches.len(),
                tiers: expansion.tiers().len(),
                collecting: expansion.collecting,
                markets_href: markets_href(expansion),
            })
            .collect(),
    };

    page(
        &ArchivePage {
            layout: layout(&env, prefs, "Archive", &uri, user.as_ref(), &csrf),
            archive,
        },
        prefs.locale,
    )
}

/// `GET /wow/archive/{expansion}` -- one expansion's patches, newest first.
pub async fn expansion<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let now = env.now();
    let archive = env.archive();
    let Some(found) = archive.expansion(&slug) else {
        return Err(app_core::AppError::NotFound.into());
    };

    let view = ExpansionView {
        name: found.name.clone(),
        span: span(prefs.locale, found.from, found.until),
        collecting: found.collecting,
        markets_href: markets_href(found),
        tiers_total: found.tiers().len(),
        patches: found
            .patches
            .iter()
            .map(|patch| patch_card(found, patch, now))
            .collect(),
    };

    page(
        &ExpansionPage {
            layout: layout(&env, prefs, "Archive", &uri, user.as_ref(), &csrf),
            expansion: view,
        },
        prefs.locale,
    )
}

/// `GET /wow/archive/{expansion}/{patch}` -- what happened during one patch.
pub async fn patch<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path((slug, key)): Path<(String, String)>,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let now = env.now();
    let archive = env.archive();
    // Expansion first, patch second, and the patch only inside the expansion
    // that was found. A patch key from another expansion resolves to nothing.
    let Some(expansion) = archive.expansion(&slug) else {
        return Err(app_core::AppError::NotFound.into());
    };
    let Some(found) = expansion.patch(&key) else {
        return Err(app_core::AppError::NotFound.into());
    };

    let card = patch_card(expansion, found, now);
    let view = ArchivePatchView {
        expansion: expansion.name.clone(),
        expansion_href: format!("/wow/archive/{}", expansion.slug),
        patch: found.patch.clone(),
        name: found.name.clone(),
        started: found.started.to_date_string(),
        until: card.until.clone(),
        ran_days: card.ran_days,
        current: card.current,
        tiers: card.tiers.clone(),
        timeline: timeline(&env, prefs, found.started, found.until, now).await?,
        // The consumables page's own fragment, narrowed to this patch. Not a
        // second table: §7's shared components, and the reason this page can
        // exist without a statistic in it.
        table_href: format!(
            "/partials/patches?expansion={}&patch={}",
            super::gear::query_value(found.catalog()),
            super::gear::query_value(&found.patch),
        ),
        region: prefs.region.to_string().to_uppercase(),
    };

    page(
        &PatchPage {
            layout: layout(&env, prefs, "Archive", &uri, user.as_ref(), &csrf),
            patch: view,
        },
        prefs.locale,
    )
}

/// `GET /wow/archive/{expansion}/{patch}/{tier}` -- one raid tier's market.
///
/// The leaf of the hierarchy, and the one page here that reads prices: a raid
/// tier *is* its bind-on-equip list (§8), so the cards are the answer rather
/// than a link to it. They are the stored roll-ups the Gear page reads and the
/// macro the Gear page draws, region-wide -- the realm picker stays on the
/// Gear page, because §7 does not let a control migrate.
///
/// An archived tier keeps the last roll-ups that were published for it. The
/// 30-day window that feeds them holds nothing once collection has stopped, so
/// nothing is staged for those markets and `publish` leaves the rows it did
/// not recalculate exactly where they were. The freshness line then says how
/// old they are, which is the honest answer to "what did this cost".
pub async fn tier<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path((slug, key, id)): Path<(String, String, String)>,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let now = env.now();
    let archive = env.archive();
    let Some(expansion) = archive.expansion(&slug) else {
        return Err(app_core::AppError::NotFound.into());
    };
    let Some(patch) = expansion.patch(&key) else {
        return Err(app_core::AppError::NotFound.into());
    };
    // By its own id, within the patch it names. §8 keeps the two independent,
    // so this is a lookup rather than an index into the patch's list.
    let Some(found) = patch.tier(&id) else {
        return Err(app_core::AppError::NotFound.into());
    };
    // The catalogue whose bind-on-equip list is this tier. `public_catalog`
    // even though the archive is built from public catalogues already: the
    // gate is one function, and a second way in is a second way to forget.
    let Some(catalog) = env.public_catalog(&found.catalog) else {
        return Err(app_core::AppError::NotFound.into());
    };

    let entries: Vec<&app_core::market::CatalogItem> = catalog.of_kind(ItemKind::Boe).collect();

    // One query, the same one the Gear page makes: the region's stored
    // bind-on-equip roll-ups. No reduction, and no per-card lookup.
    let rollups = env
        .store()
        .read_model()
        .rollups(prefs.region, ItemKind::Boe, Scope::Region)
        .await?;
    let observed = rollups.iter().filter_map(|r| r.observed_at).max();
    let mut by_item: std::collections::HashMap<
        app_core::market::ItemId,
        Vec<&app_core::market::MarketRollup>,
    > = std::collections::HashMap::new();
    for rollup in &rollups {
        by_item.entry(rollup.item).or_default().push(rollup);
    }

    let realms: Vec<Realm> = env
        .store()
        .realm_prices()
        .realms()
        .await?
        .into_iter()
        .filter(|r| r.region == prefs.region)
        .collect();
    let named = super::gear::realm_names(&realms);
    let tooltips = super::tooltip::cached_all(&env, prefs, catalog, now).await;

    let mut cards: Vec<crate::views::GearCard> = entries
        .iter()
        .map(|entry| super::gear::card(entry, &by_item, &named, None, &tooltips))
        .collect();
    cards.sort_by(|a, b| a.name.cmp(&b.name));

    let groups = if cards.is_empty() {
        Vec::new()
    } else {
        vec![GearGroup {
            deferred: false,
            href: String::new(),
            label: "",
            anchor: "tier",
            cards,
        }]
    };

    let view = ArchiveTierView {
        expansion: expansion.name.clone(),
        expansion_href: format!("/wow/archive/{}", expansion.slug),
        patch: patch.patch.clone(),
        patch_href: format!("/wow/archive/{}/{}", expansion.slug, patch.patch),
        name: found.name.clone(),
        opened: found.opened.to_date_string(),
        until: found
            .until
            .map(|at| at.to_date_string())
            .unwrap_or_else(|| "—".to_string()),
        ran_days: days_between(found.opened, found.until, now),
        season: found.season.unwrap_or(0),
        current: found.until.is_none() && expansion.collecting,
        region: prefs.region.to_string().to_uppercase(),
        region_code: prefs.region.as_str(),
        gear_href: format!(
            "/wow/auctions/gear?expansion={}",
            super::gear::query_value(&catalog.id)
        ),
        pieces: entries.len(),
        observed: super::market::observed(prefs, now, observed),
        groups,
        timeline: timeline(&env, prefs, found.opened, found.until, now).await?,
    };

    page(
        &TierPage {
            layout: layout(&env, prefs, "Archive", &uri, user.as_ref(), &csrf),
            tier: view,
        },
        prefs.locale,
    )
}

fn layout<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    title: &'static str,
    uri: &axum::http::Uri,
    user: Option<&app_core::model::User>,
    csrf: &Csrf,
) -> Layout {
    Layout::new(
        env.config(),
        prefs.locale,
        title,
        // The archive lives under the Auction House tab: every tracking
        // category does, and the archive is how you reach the ones that
        // stopped. A nav entry of its own would be a second front door to the
        // same thing.
        "/wow/auctions",
        uri,
        user,
        csrf.masked(),
    )
}

fn patch_card(
    expansion: &ArchivedExpansion,
    patch: &ArchivedPatch,
    now: Millis,
) -> ArchivePatchCard {
    ArchivePatchCard {
        patch: patch.patch.clone(),
        name: patch.name.clone(),
        href: format!("/wow/archive/{}/{}", expansion.slug, patch.patch),
        started: patch.started.to_date_string(),
        until: patch
            .until
            .map(|at| at.to_date_string())
            .unwrap_or_else(|| "—".to_string()),
        ran_days: days_between(patch.started, patch.until, now),
        current: patch.until.is_none() && expansion.collecting,
        tiers: patch
            .tiers
            .iter()
            .map(|tier| tier_link(expansion, patch, tier))
            .collect(),
    }
}

fn tier_link(
    expansion: &ArchivedExpansion,
    patch: &ArchivedPatch,
    tier: &ArchivedTier,
) -> ArchiveTierLink {
    ArchiveTierLink {
        name: tier.name.clone(),
        href: format!(
            "/wow/archive/{}/{}/{}",
            expansion.slug, patch.patch, tier.id
        ),
        opened: tier.opened.to_date_string(),
        season: tier.season.unwrap_or(0),
        current: tier.until.is_none() && expansion.collecting,
    }
}

/// What happened inside an interval, as a list.
///
/// **Public and validated only.** `between`'s third argument is the audience,
/// and passing it rather than filtering afterwards is what keeps an
/// administrator's unchecked note off a page nobody meant to publish it to
/// (§10). Phase 8 made that decision deliberate and this page does not get to
/// make it again.
async fn timeline<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    from: Millis,
    until: Option<Millis>,
    now: Millis,
) -> WebResult<Vec<TimelineRow>> {
    let found = env
        .store()
        .market_events()
        .between(from, until.unwrap_or(now), true)
        .await?;
    Ok(found
        .into_iter()
        .rev()
        .take(TIMELINE_LIMIT)
        .map(|event| TimelineRow {
            // `label`, not `as_str`: the machine word is the form's and the
            // column's, and a reader was being shown `raid_opening` in
            // both languages. Every label here is already in
            // `EXTERNAL_STRINGS` and already translated (§13).
            kind: event.kind.label(),
            title: event.title.clone(),
            when: event.starts_at.to_utc_string(),
            scope: super::item::scope_text(&event.scope, prefs.locale),
            notes: event.notes.clone(),
        })
        .collect())
}

/// "2026-03-02 — present", or "2026-03-02 — 2027-01-05".
///
/// The wording is translated and the dates are not, which is `format::ago`'s
/// arrangement and for the same reason: the sentence wraps the values, so the
/// two cannot be concatenated in the template. An open-ended span said only
/// its start date before, which reads as a date rather than as a period.
fn span(locale: app_core::locale::Locale, from: Millis, until: Option<Millis>) -> String {
    match until {
        Some(until) => format!("{} — {}", from.to_date_string(), until.to_date_string()),
        None => format!(
            "{} — {}",
            from.to_date_string(),
            crate::i18n::translate(locale, "present")
        ),
    }
}

/// Whole days between two instants, counting an open-ended interval up to now.
fn days_between(from: Millis, until: Option<Millis>, now: Millis) -> u64 {
    let end = until.unwrap_or(now);
    end.get().saturating_sub(from.get()) / DAY_MS
}

/// Where an expansion's prices are, which is the Auction House index with its
/// newest catalogue selected. The index owns the region and the comparison
/// window for everything beneath it (§7), so the link carries neither.
fn markets_href(expansion: &ArchivedExpansion) -> String {
    match expansion.catalogs.first() {
        Some(id) => format!("/wow/auctions?expansion={}", super::gear::query_value(id)),
        None => "/wow/auctions".to_string(),
    }
}
