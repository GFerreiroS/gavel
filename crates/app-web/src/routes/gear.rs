//! Bind-on-equip gear, priced per connected realm.
//!
//! The page every other market page is not. A commodity has one price for a
//! whole region; a BoE has a different price on every realm, at several
//! upgrade levels that all share one item id. So this page answers two
//! questions rather than one:
//!
//! * **No realm chosen** — what does this cost *generally*, and where is it
//!   worth going? Each region summarises its realms: the median of what each
//!   realm's cheapest copy costs, plus the cheapest and dearest realm by name.
//!   Regions sit side by side and are never mixed, because nobody can buy
//!   across one.
//! * **A realm chosen** — what does it cost *here*. That choice is
//!   remembered, so coming back lands where you left off.
//!
//! The median is taken over per-realm *minima*: a realm's price is what its
//! cheapest copy costs, because that is what you would actually pay there.
//! Averaging every listing on every realm would report a number nobody can
//! buy at.

use std::collections::{BTreeMap, HashMap};

use app_core::Ports;
use app_core::market::materialise::{MarketRollup, Scope};
use app_core::market::{
    ALL_PROFESSIONS, Catalog, CatalogItem, ItemId, ItemKind, Realm, RealmId, Region, Track,
};
use app_core::repo::{ReadModelRepository, RealmPriceRepository, Store};
use app_core::timing::{self, Stage};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Query, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::{MarketPrefs, RealmChoice, slug};
use crate::render::page;
use crate::session::current_user;
use crate::views::{
    GearCard, GearCell, GearExtra, GearGroup, GearTrackRow, GearView, GearWhere, Layout,
    RealmOption,
};

use super::reagents::SearchParams;

// The market is one **item level**: one upgrade bonus id, which belongs to
// exactly one track, so the id alone identifies it. The catalog carries what
// each one means -- ilvl 295, "Champion 2/6" -- resolved once by
// `scripts/catalog-sync.py` from SimulationCraft and Wowhead and committed.
//
// Everything else a listing carries is optional: sockets and tertiary stats
// change what a piece is worth without changing what it is, so they are
// counted inside a market rather than splitting it.
//
// Samples are stored with their full bonus list, so all of this is a display
// rule. A patch that renumbers bonus ids costs a re-run of the sync script,
// never any history.

#[derive(Template)]
#[template(path = "gear.html")]
struct GearPage {
    layout: Layout,
    gear: GearView,
}

#[derive(Template)]
#[template(path = "partials/gear.html")]
pub struct GearFragment {
    pub gear: GearView,
}

/// One group of gear cards, for a deferred section fetching itself.
#[derive(Template)]
#[template(path = "partials/gear_group.html")]
pub struct GearGroupFragment {
    pub group: crate::views::GearGroup,
    pub region: &'static str,
    pub realm_slug: String,
    pub compact: bool,
}

/// `GET /wow/auctions/gear`
pub async fn page_handler<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    uri: OriginalUri,
    params: Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(
        ItemKind::Boe,
        state,
        csrf,
        prefs,
        chosen,
        uri,
        params,
        headers,
    )
    .await
}

/// `GET /wow/auctions/recipes`
pub async fn recipes_page<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    uri: OriginalUri,
    params: Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(
        ItemKind::Recipe,
        state,
        csrf,
        prefs,
        chosen,
        uri,
        params,
        headers,
    )
    .await
}

/// Axum hands each extractor in separately, so the count is the framework's
/// rather than a design to simplify.
#[allow(clippy::too_many_arguments)]
async fn render<E: Ports>(
    kind: ItemKind,
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(chosen): Extension<RealmChoice>,
    OriginalUri(uri): OriginalUri,
    Query(params): Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    // `Full`, not `Shell`: the cards are inlined into the first response.
    // They are stored rows now -- nine of them for gear, and a bounded first
    // group for recipes -- so the round trip that used to buy the paint now
    // only costs one.
    let gear = build(
        &env,
        kind,
        prefs,
        &chosen,
        params.expansion.as_deref(),
        params.q.as_deref(),
        Detail::Full,
    )
    .await?;
    let user = current_user(&env, &headers).await?;
    page(
        &GearPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                gear.title,
                "/wow/auctions",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            gear,
        },
        prefs.locale,
    )
}

/// `GET /partials/gear` -- the cards alone, for the realm picker.
pub async fn fragment<E: Ports>(
    state: State<E>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    cache: Extension<std::sync::Arc<crate::FragmentCache>>,
    params: Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<axum::response::Response> {
    fragment_of(ItemKind::Boe, state, prefs, chosen, cache, params, headers).await
}

/// `GET /partials/recipes`
pub async fn recipes_fragment<E: Ports>(
    state: State<E>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    cache: Extension<std::sync::Arc<crate::FragmentCache>>,
    params: Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<axum::response::Response> {
    fragment_of(
        ItemKind::Recipe,
        state,
        prefs,
        chosen,
        cache,
        params,
        headers,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fragment_of<E: Ports>(
    kind: ItemKind,
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(chosen): Extension<RealmChoice>,
    Extension(cache): Extension<std::sync::Arc<crate::FragmentCache>>,
    Query(params): Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<axum::response::Response> {
    if params.q.as_deref().is_some_and(|q| !q.trim().is_empty()) {
        return Ok(axum::response::IntoResponse::into_response(
            render_fragment(kind, &env, prefs, &chosen, &params).await?,
        ));
    }

    let key = crate::fragment_cache::FragmentKey::new(
        match kind {
            ItemKind::Recipe => "recipes",
            _ => "gear",
        },
        env.store()
            .read_model()
            .published()
            .await?
            .map(|v| v.version),
        params.expansion.as_deref().unwrap_or(""),
        prefs.region.as_str(),
        prefs.baseline_days,
        // The realm is part of what a per-realm fragment says, so it is part
        // of what it is filed under.
        chosen.0.as_deref().unwrap_or(""),
        prefs.locale.code(),
        params.group.as_deref(),
    );
    crate::fragment_cache::respond(&cache, &headers, key, async {
        Ok(render_fragment(kind, &env, prefs, &chosen, &params)
            .await?
            .0)
    })
    .await
}

async fn render_fragment<E: Ports>(
    kind: ItemKind,
    env: &E,
    prefs: MarketPrefs,
    chosen: &RealmChoice,
    params: &SearchParams,
) -> WebResult<Html<String>> {
    let gear = build(
        env,
        kind,
        prefs,
        chosen,
        params.expansion.as_deref(),
        params.q.as_deref(),
        Detail::Full,
    )
    .await?;

    // One group, for a deferred section fetching itself.
    if let Some(wanted) = params.group.as_deref() {
        let (region, realm_slug, compact) = (gear.region, gear.realm_slug.clone(), gear.compact);
        let Some(group) = crate::groups::only(gear.groups, wanted) else {
            return Err(app_core::AppError::NotFound.into());
        };
        return page(
            &GearGroupFragment {
                group,
                region,
                realm_slug,
                compact,
            },
            prefs.locale,
        );
    }

    page(&GearFragment { gear }, prefs.locale)
}

/// Percent-encode one query-string value.
///
/// The search box is the reason: a reader may type a space, an ampersand or a
/// plus, and any of the three would otherwise change which parameters the
/// fragment endpoint sees. Unreserved characters pass through, everything else
/// goes out as `%XX`.
pub(crate) fn query_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// How much of a page to build.
///
/// The shell of a category page -- its title, its controls, the realm
/// picker -- costs one small query. The cards cost a scan of every market in
/// every collected region, and on the per-realm pages that is most of the
/// page's time. Building them separately is what lets the browser paint the
/// controls while the cards are still being counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// Everything except the prices. The cards arrive in their own request.
    Shell,
    /// The cards, which is what the fragment endpoint answers with.
    Full,
}

async fn build<E: Ports>(
    env: &E,
    kind: ItemKind,
    prefs: MarketPrefs,
    chosen: &RealmChoice,
    expansion: Option<&str>,
    query: Option<&str>,
    detail: Detail,
) -> WebResult<GearView> {
    let catalog = match expansion.filter(|id| !id.is_empty()) {
        Some(id) => env.public_catalog(id),
        None => env.active_catalog(),
    };
    let Some(catalog) = catalog else {
        return Err(app_core::AppError::NotFound.into());
    };
    let prices = env.store().realm_prices();
    let now = env.now();

    // Realm names come from our own table rather than the upstream: they were
    // recorded when the realm was configured, so a realm since dropped still
    // has a name against its history.
    // Only the region the reader chose on the Auction House index. Regions
    // were never merged -- a EU price is not something a US player can act
    // on -- but showing all four side by side made the reader do the choosing
    // again on every card, and contradicted the index, which owns that choice
    // for every category beneath it (CLAUDE.md §7).
    let realms: Vec<Realm> = prices
        .realms()
        .await?
        .into_iter()
        .filter(|r| r.region == prefs.region)
        .collect();
    // One name per auction house, and the whole list beside it.
    //
    // The joined name is unreadable and, worse, ragged: "Die Arguswacht, Die
    // ewige Wacht, Die Todeskrallen, Das Syndikat, …" wraps to three lines
    // while "Howling Fjord" takes one, and the card next to it no longer lines
    // up. Cards sit in a grid and their rows have to agree (§7). So: one realm
    // names the market -- any of them does, they share it -- and the rest are
    // in the line's tooltip.
    let named: BTreeMap<(Region, RealmId), (String, String)> = realms
        .iter()
        .map(|r| {
            let mut members = r.members.clone();
            members.sort();
            let short = members.first().cloned().unwrap_or_else(|| r.name.clone());
            let full = if members.len() > 1 {
                members.join(", ")
            } else {
                String::new()
            };
            ((r.region, r.id), (short, full))
        })
        .collect();

    // Resolve the slug against the realms we collect in this region. A slug
    // may name any one of the realms sharing an auction house -- "sargeras"
    // and "ner-zhul" are the same market -- because that is how a player
    // thinks of it, and asking them to know which name the connected realm was
    // filed under is asking them to know an implementation detail.
    let selected: Option<&Realm> = chosen
        .0
        .as_deref()
        .and_then(|want| realms.iter().find(|r| realm_matches(r, want)));

    // One query: the stored roll-ups for this kind, in this region, at the
    // scope the reader chose. A few hundred rows rather than the eighteen
    // thousand markets behind them, and no reduction at all -- this page used
    // to rebuild every market in the region from the archive to draw nine
    // cards.
    let scope = selected.map_or(Scope::Region, |realm| Scope::Realm(realm.id));
    let rollups: Vec<MarketRollup> = match detail {
        // The shell asks for no prices at all: that is the whole point of it.
        Detail::Shell => Vec::new(),
        Detail::Full => {
            env.store()
                .read_model()
                .rollups(prefs.region, kind, scope)
                .await?
        }
    };

    let observed = rollups.iter().filter_map(|r| r.observed_at).max();

    let by_item: HashMap<ItemId, Vec<&MarketRollup>> = {
        let _timing = timing::start(Stage::Analysis);
        let mut by_item: HashMap<ItemId, Vec<&MarketRollup>> = HashMap::new();
        for rollup in &rollups {
            by_item.entry(rollup.item).or_default().push(rollup);
        }
        by_item
    };

    let tooltips = match detail {
        Detail::Shell => Default::default(),
        Detail::Full => super::tooltip::cached_all(env, prefs, catalog, now).await,
    };

    let needle = super::reagents::normalise(query);
    let mut groups = Vec::new();
    let cards_timing = timing::start(Stage::Analysis);
    for (label, anchor, entries) in sections(catalog, kind) {
        if detail == Detail::Shell {
            break;
        }
        let mut cards: Vec<GearCard> = entries
            .into_iter()
            .map(|entry| card(entry, &by_item, &named, selected, &tooltips))
            .filter(|card| matches(card, &needle))
            .collect();
        if cards.is_empty() {
            continue;
        }
        cards.sort_by(|a, b| a.name.cmp(&b.name));
        groups.push(GearGroup {
            // Filled in by `groups::defer` once the page's size is known.
            deferred: false,
            href: String::new(),
            label,
            anchor,
            cards,
        });
    }
    drop(cards_timing);

    let text = Text::of(kind);
    // The name the reader picked, not the connected realm's joined name: they
    // typed "Sargeras" and the box should still say Sargeras.
    let realm_name = match (selected, chosen.0.as_deref()) {
        (Some(realm), Some(want)) => member_named(realm, want),
        _ => String::new(),
    };
    let realm_slug = chosen.0.clone().unwrap_or_default();
    let region_code = selected.map_or(prefs.region, |r| r.region).as_str();

    // Gear is nine cards and recipes are a hundred and thirty-four, so the
    // threshold decides rather than the page.
    let defer_href = |group: &str| {
        format!(
            "{}?expansion={}&region={}&realm={}&group={}",
            text.fragment_path,
            query_value(&catalog.id),
            region_code,
            query_value(&realm_slug),
            query_value(group),
        )
    };
    crate::groups::defer(&mut groups, needle.is_some(), defer_href);

    Ok(GearView {
        realm_name: realm_name.clone(),
        region_label: prefs.region.to_string().to_uppercase(),
        kind: match kind {
            ItemKind::Recipe => "recipes",
            _ => "gear",
        },
        has_realms: !realms.is_empty(),
        title: text.title,
        blurb: text.blurb,
        path: text.path,
        fragment_path: text.fragment_path,
        leveled: !matches!(kind, ItemKind::Recipe),
        compact: selected.is_some(),
        searchable: matches!(kind, ItemKind::Recipe),
        query: needle.unwrap_or_default(),
        expansion: catalog.expansion.clone(),
        expansion_id: catalog.id.clone(),
        archived: !env.catalog_state(catalog).is_collected(),
        observed: super::market::observed(prefs, now, observed),
        realm_label: selected.map(|realm| shared_label(realm, &realm_name)),
        region: region_code,
        realm_slug,
        groups,
    })
}

/// The cards in the order they are shown, as (heading, anchor, entries).
///
/// Gear is one nameless run: nine items do not need dividing. Recipes divide
/// by profession, which for once costs no judgement -- Blizzard's own item
/// subclass *is* the profession, unlike reagents where it is a material type.
type Section<'a> = (&'static str, &'static str, Vec<&'a CatalogItem>);

fn sections(catalog: &Catalog, kind: ItemKind) -> Vec<Section<'_>> {
    match kind {
        ItemKind::Recipe => ALL_PROFESSIONS
            .into_iter()
            .map(|p| (p.label(), p.as_str(), catalog.recipes_for(p).collect()))
            .collect(),
        _ => vec![("", "all", catalog.of_kind(kind).collect())],
    }
}

/// The wording that differs between the two per-realm pages.
pub(crate) struct Text {
    pub title: &'static str,
    pub blurb: &'static str,
    pub path: &'static str,
    pub fragment_path: &'static str,
}

impl Text {
    pub(crate) const GEAR: Text = Text {
        title: "Bind-on-equip gear",
        blurb: "Raid bind-on-equip pieces from {}. Gear is not a commodity: every connected \
                realm has its own price, and one item id trades at several upgrade levels.",
        path: "/wow/auctions/gear",
        fragment_path: "/partials/gear",
    };

    pub(crate) const RECIPES: Text = Text {
        title: "Recipes",
        blurb: "Every recipe of {} trading on the auction house, by the profession that reads \
                it. Recipes are per realm, like gear, and have no upgrade levels.",
        path: "/wow/auctions/recipes",
        fragment_path: "/partials/recipes",
    };

    const fn of(kind: ItemKind) -> Text {
        match kind {
            ItemKind::Recipe => Text::RECIPES,
            _ => Text::GEAR,
        }
    }
}

/// Whether a card's name contains the search term.
///
/// The same rule as the reagents page, on the localised name: a search matches
/// what the visitor can actually read on the page.
fn matches(card: &GearCard, needle: &Option<String>) -> bool {
    match needle {
        None => true,
        Some(needle) => card.name.to_lowercase().contains(needle.as_str()),
    }
}

/// Which regions have collected realms, in a stable order.
/// Which of a connected realm's names matches this slug.
///
/// Falls back to the joined name, which is what a realm recorded before the
/// members column existed still has.
pub(crate) fn member_named(realm: &Realm, want: &str) -> String {
    realm
        .members
        .iter()
        .find(|member| slug(member).as_deref() == Some(want))
        .cloned()
        .unwrap_or_else(|| realm.name.clone())
}

/// How to describe the chosen realm: its own name, and who it shares the
/// auction house with.
///
/// Naming the others matters. A player on Sargeras is looking at Garona's and
/// Ner'zhul's listings too, and a page that quietly merged three realms
/// without saying so would be lying by omission.
fn shared_label(realm: &Realm, chosen_name: &str) -> String {
    let others: Vec<&str> = realm
        .members
        .iter()
        .map(String::as_str)
        .filter(|member| *member != chosen_name)
        .collect();
    if others.is_empty() {
        chosen_name.to_string()
    } else {
        format!("{chosen_name} ({})", others.join(", "))
    }
}

/// One entry per *realm*, not per auction house.
///
/// EU alone has 92 connected realms and rather more realms inside them. The
/// picker used to list the joined names -- "Arak-arahm, Rashgarroth,
/// Kael'thas, Throk'Feroth" -- which is unreadable, made the control wide
/// enough to push the page into a horizontal scroll, and asked the reader to
/// know which of the four names their market was filed under.
///
/// Sorted by name, because that is how they are looked for.
pub(crate) fn realm_options(realms: &[Realm]) -> Vec<RealmOption> {
    let mut options: Vec<RealmOption> = Vec::new();
    for realm in realms {
        // A connected realm recorded before the members column existed has
        // none, and falls back to its joined name so the picker still works.
        let names: Vec<&String> = if realm.members.is_empty() {
            vec![&realm.name]
        } else {
            realm.members.iter().collect()
        };
        for name in &names {
            options.push(RealmOption {
                name: (*name).clone(),
            });
        }
    }
    options.sort_by(|a, b| a.name.cmp(&b.name));
    options
}

/// Whether a slug names this connected realm, by any of the realms in it.
///
/// A connected realm is several realms sharing one auction house. A player
/// looking for Sargeras should find it under Sargeras, not under
/// "Garona, Sargeras, Ner'zhul" -- which is the joined name and an
/// implementation detail of how Blizzard filed them.
pub(crate) fn realm_matches(realm: &Realm, want: &str) -> bool {
    if slug(&realm.name).as_deref() == Some(want) {
        return true;
    }
    realm
        .members
        .iter()
        .any(|member| slug(member).as_deref() == Some(want))
}

fn card(
    entry: &CatalogItem,
    by_item: &HashMap<ItemId, Vec<&MarketRollup>>,
    named: &BTreeMap<(Region, RealmId), (String, String)>,
    selected: Option<&Realm>,
    tooltips: &BTreeMap<u32, crate::views::TooltipView>,
) -> GearCard {
    let item = entry.ranks.first().map(|r| r.item_id).unwrap_or(ItemId(0));
    let mine: Vec<&MarketRollup> = by_item.get(&item).cloned().unwrap_or_default();

    // One column. The reader chose a region on the Auction House index and a
    // realm here, and §7 does not let this page offer the other three regions
    // as though they were an option -- a EU price is not something a US player
    // can act on.
    let label = match selected {
        Some(realm) => realm.name.clone(),
        None => mine
            .first()
            .map(|r| r.region.to_string().to_uppercase())
            .unwrap_or_default(),
    };

    // A recipe has one version of itself and no track; gear always shows all
    // four, so a card with nothing listed at Myth still lines up with the card
    // beside it that has one.
    let wanted: Vec<Option<Track>> = if entry.kind == ItemKind::Recipe {
        vec![None]
    } else {
        Track::ALL.into_iter().map(Some).collect()
    };

    let tracks: Vec<GearTrackRow> = wanted
        .into_iter()
        .map(|track| {
            let rollup = mine.iter().copied().find(|r| r.track == track);
            let cell = rollup
                .map(|r| cell(r, named, selected.is_some()))
                .unwrap_or_default();
            GearTrackRow {
                track: track.map(Track::as_str).unwrap_or(""),
                levels: rollup.map(|r| r.level_range.clone()).unwrap_or_default(),
                leveled: track.is_some(),
                href: match track {
                    Some(track) => format!("/wow/gear/{}/{}", item.get(), track.slug()),
                    None if entry.kind == ItemKind::Recipe => {
                        format!("/wow/recipe/{}", item.get())
                    }
                    None => String::new(),
                },
                listed: cell.listed,
                // A track with nothing in it still gets a cell, which is what
                // keeps the row square.
                cells: vec![cell],
            }
        })
        .collect();

    let tooltip_item_id = item.get();
    GearCard {
        name: tooltips
            .get(&tooltip_item_id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| entry.name.clone()),
        icon: entry.icon_url(),
        tooltip_item_id,
        tooltip: tooltips.get(&tooltip_item_id).cloned(),
        slot: entry.slot.map(|s| s.label()).unwrap_or(""),
        // A recipe has no slot; Blizzard's subclass is its profession, and the
        // upstream localises it.
        material: match entry.slot {
            Some(_) => None,
            None => tooltips
                .get(&tooltip_item_id)
                .and_then(|t| t.material.clone()),
        },
        unlisted: tracks.iter().all(|t| !t.listed),
        scopes: vec![label],
        tracks,
    }
}

/// One stored roll-up as the cell a card draws.
///
/// The two scopes read different figures out of the same row, and the reason
/// is in the row's own comment: across realms the useful spread is between
/// realms -- the cheapest one, the median one, the dearest one, and which they
/// are -- while on a single realm there is no other realm to compare with, so
/// what is left is the spread between that realm's own listings.
fn cell(
    rollup: &MarketRollup,
    named: &BTreeMap<(Region, RealmId), (String, String)>,
    one_realm: bool,
) -> GearCell {
    let Some(cheapest) = rollup.cheapest_now else {
        return GearCell::default();
    };
    let name = |realm: Option<RealmId>| {
        realm
            .and_then(|realm| named.get(&(rollup.region, realm)).cloned())
            .unwrap_or_else(|| {
                (
                    realm.map(|r| r.to_string()).unwrap_or_default(),
                    String::new(),
                )
            })
    };

    let (price, high, high_realm) = if one_realm {
        (cheapest, rollup.highest_now.unwrap_or(cheapest), None)
    } else {
        (
            rollup.median_realm_now.unwrap_or(cheapest),
            rollup.dearest_realm_now.unwrap_or(cheapest),
            rollup.dearest_realm,
        )
    };

    GearCell {
        listed: true,
        price: price.to_string(),
        cheapest: GearWhere {
            realm: (!one_realm).then(|| name(rollup.cheapest_realm).0),
            realm_full: if one_realm {
                String::new()
            } else {
                name(rollup.cheapest_realm).1
            },
            price: cheapest.to_string(),
        },
        highest: GearWhere {
            realm: (!one_realm).then(|| name(high_realm).0),
            realm_full: if one_realm {
                String::new()
            } else {
                name(high_realm).1
            },
            price: high.to_string(),
        },
        listings: rollup.listings_now,
        realms: rollup.realms_listing as usize,
        // The stored position, not one worked out here. It was materialised by
        // the same engine that placed the flask on the consumables page, over
        // the same equal-duration buckets -- which is the whole reason a gear
        // card may print the word at all.
        band: rollup
            .position
            .and_then(|p| p.valuation)
            .map(|v| v.as_str()),
        band_slug: rollup
            .position
            .and_then(|p| p.valuation)
            .map(|v| v.slug())
            .unwrap_or("none"),
        rank_percent: rollup.position.and_then(|p| p.rank),
        extras: rollup
            .modifiers
            .iter()
            .filter(|m| m.now > 0)
            .map(|m| GearExtra {
                name: m.name.clone(),
                listings: m.now,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use app_core::market::materialise::{MarketRollup, ModifierStat, Scope};
    use app_core::market::window::Window;

    use super::*;

    fn rollup(track: Option<Track>) -> MarketRollup {
        MarketRollup {
            region: Region::Eu,
            item: ItemId(271_438),
            kind: ItemKind::Boe,
            track,
            scope: Scope::Region,
            window: Window::Days(30),
            observed_at: Some(cluster_core::Millis(1_000)),
            snapshots: 4,
            realms_listing: 3,
            cheapest_now: Some(app_core::market::Copper(90_000_000)),
            cheapest_realm: Some(RealmId(1403)),
            dearest_realm_now: Some(app_core::market::Copper(200_000_000)),
            dearest_realm: Some(RealmId(1084)),
            median_realm_now: Some(app_core::market::Copper(150_000_000)),
            highest_now: Some(app_core::market::Copper(500_000_000)),
            cheapest_ever: Some(app_core::market::Copper(80_000_000)),
            highest_ever: Some(app_core::market::Copper(600_000_000)),
            listings_now: 7,
            listings_seen: 20,
            level_range: "305\u{2013}311".into(),
            levels: Vec::new(),
            modifiers: vec![
                ModifierStat {
                    name: "Leech".into(),
                    now: 2,
                    seen: 5,
                },
                // Seen in the window but not now: a card shows what is on sale.
                ModifierStat {
                    name: "Speed".into(),
                    now: 0,
                    seen: 3,
                },
            ],
            series: Vec::new(),
            distribution: None,
            position: None,
            swing: app_core::market::engine::Swing(0),
            realms_collected: 0,
            realm_spread: None,
        }
    }

    fn named() -> BTreeMap<(Region, RealmId), (String, String)> {
        BTreeMap::from([
            (
                (Region::Eu, RealmId(1403)),
                ("Sargeras".to_string(), "Sargeras".to_string()),
            ),
            (
                (Region::Eu, RealmId(1084)),
                ("Kazzak".to_string(), "Kazzak".to_string()),
            ),
        ])
    }

    /// Across realms, the useful spread is between realms: which one is
    /// cheapest, what the middle one charges, which one is dearest. The
    /// headline is the median, not the minimum -- one realm having a bad day
    /// is not what the market costs.
    #[test]
    fn the_cross_realm_cell_names_where_to_go() {
        let cell = cell(&rollup(Some(Track::Champion)), &named(), false);

        assert!(cell.listed);
        assert_eq!(
            cell.price,
            app_core::market::Copper(150_000_000).to_string()
        );
        assert_eq!(cell.cheapest.realm.as_deref(), Some("Sargeras"));
        assert_eq!(
            cell.cheapest.price,
            app_core::market::Copper(90_000_000).to_string()
        );
        assert_eq!(cell.highest.realm.as_deref(), Some("Kazzak"));
        assert_eq!(
            cell.highest.price,
            app_core::market::Copper(200_000_000).to_string(),
            "the dearest realm, not the dearest listing"
        );
        assert_eq!(cell.realms, 3);
    }

    /// On one realm there is no other realm to compare with, so what is left
    /// is the spread between that realm's own listings -- and there is no
    /// realm to name.
    #[test]
    fn a_single_realm_keeps_the_spread_and_drops_the_name() {
        let cell = cell(&rollup(Some(Track::Champion)), &named(), true);

        assert_eq!(
            cell.cheapest.price,
            app_core::market::Copper(90_000_000).to_string()
        );
        assert_eq!(
            cell.highest.price,
            app_core::market::Copper(500_000_000).to_string(),
            "the dearest listing, which is the only spread there is"
        );
        assert!(cell.cheapest.realm.is_none(), "one realm needs no name");
        assert!(cell.highest.realm.is_none());
    }

    /// Sockets and tertiary stats are counted inside a market rather than
    /// splitting one: a socketed piece is the same piece. A card counts what
    /// is on sale now, not what was seen last month.
    #[test]
    fn optional_bonuses_are_counted_and_only_the_listed_ones() {
        let cell = cell(&rollup(Some(Track::Champion)), &named(), false);
        let counted: Vec<(&str, u32)> = cell
            .extras
            .iter()
            .map(|e| (e.name.as_str(), e.listings))
            .collect();
        assert_eq!(counted, [("Leech", 2)]);
    }

    /// Gear always shows all four tracks, so a card with nothing listed at
    /// Myth still lines up with the card beside it that has one.
    #[test]
    fn every_track_gets_a_row_whether_or_not_anybody_is_selling() {
        let entry = CatalogItem {
            name: "Venom Rite Mantle".into(),
            category: app_core::market::Category::Boe,
            kind: ItemKind::Boe,
            profession: None,
            slot: Some(app_core::market::Slot::Shoulder),
            audience: app_core::market::Audience::Common,
            stat: app_core::market::Stat::None,
            ranks: vec![app_core::market::ItemRank {
                rank: 1,
                item_id: ItemId(271_434),
            }],
            floor_copper: None,
            icon: None,
            target_quantity: None,
        };

        // One track listed out of four.
        let mut only_hero = rollup(Some(Track::Hero));
        only_hero.item = ItemId(271_434);
        let by_item = HashMap::from([(ItemId(271_434), vec![&only_hero])]);
        let card = card(&entry, &by_item, &named(), None, &BTreeMap::new());

        assert_eq!(card.tracks.len(), 4);
        assert_eq!(
            card.tracks.iter().filter(|t| t.listed).count(),
            1,
            "the other three still get their row, saying so"
        );
        assert!(!card.unlisted);
        assert_eq!(card.tracks[2].levels, "305\u{2013}311");
    }

    /// Tracked, but nobody is selling one. The card still has to appear and
    /// say so: "no listings" is an answer, and a different one from "we do not
    /// follow this item".
    #[test]
    fn an_item_nobody_is_selling_is_marked_unlisted() {
        let entry = CatalogItem {
            name: "Venom Rite Mantle".into(),
            category: app_core::market::Category::Boe,
            kind: ItemKind::Boe,
            profession: None,
            slot: Some(app_core::market::Slot::Shoulder),
            audience: app_core::market::Audience::Common,
            stat: app_core::market::Stat::None,
            ranks: vec![app_core::market::ItemRank {
                rank: 1,
                item_id: ItemId(271_434),
            }],
            floor_copper: None,
            icon: None,
            target_quantity: None,
        };
        // Roll-ups exist, but for a different item.
        let elsewhere = rollup(Some(Track::Champion));
        let by_item = HashMap::from([(ItemId(271_438), vec![&elsewhere])]);
        let card = card(&entry, &by_item, &named(), None, &BTreeMap::new());

        assert!(card.unlisted);
        assert!(
            card.tracks.iter().all(|t| !t.listed),
            "no track has anything to show"
        );
    }
}
