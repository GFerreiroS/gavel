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
use app_core::market::{
    ALL_PROFESSIONS, Catalog, CatalogItem, Copper, ItemId, ItemKind, ItemLevel, Realm, RealmId,
    RealmSample, Region, Track,
};
use app_core::repo::{RealmPriceRepository, Store};
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
    let gear = build(
        &env,
        kind,
        prefs,
        &chosen,
        params.expansion.as_deref(),
        params.q.as_deref(),
        Detail::Shell,
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
    params: Query<SearchParams>,
) -> WebResult<Html<String>> {
    fragment_of(ItemKind::Boe, state, prefs, chosen, params).await
}

/// `GET /partials/recipes`
pub async fn recipes_fragment<E: Ports>(
    state: State<E>,
    prefs: Extension<MarketPrefs>,
    chosen: Extension<RealmChoice>,
    params: Query<SearchParams>,
) -> WebResult<Html<String>> {
    fragment_of(ItemKind::Recipe, state, prefs, chosen, params).await
}

async fn fragment_of<E: Ports>(
    kind: ItemKind,
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(chosen): Extension<RealmChoice>,
    Query(params): Query<SearchParams>,
) -> WebResult<Html<String>> {
    page(
        &GearFragment {
            gear: build(
                &env,
                kind,
                prefs,
                &chosen,
                params.expansion.as_deref(),
                params.q.as_deref(),
                Detail::Full,
            )
            .await?,
        },
        prefs.locale,
    )
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
        Some(id) => env.catalogs().by_id(id),
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

    // One query per region for the cross-realm view; one for the realm when
    // a realm is chosen. Either way it is the newest row per item, realm and
    // variant, which the store computes rather than us.
    let mut samples: Vec<RealmSample> = Vec::new();
    match (detail, selected) {
        // The shell asks for no prices at all: that is the whole point of it.
        (Detail::Shell, _) => {}
        (Detail::Full, Some(realm)) => samples.extend(prices.latest(realm.region, realm.id).await?),
        // One region, one query. This used to fan out over every collected
        // region and merge them, which was both slower and a worse page: the
        // reader already chose a region on the index, and four columns of
        // prices they cannot buy is four columns of noise.
        (Detail::Full, None) => samples.extend(prices.latest_in_region(prefs.region).await?),
    }

    let observed = samples.iter().map(|s| s.observed_at).max();

    // Grouped once, here, rather than by each card scanning the whole list for
    // its own item. A region holds ~18k markets and a page draws ~600 cards;
    // the scan was eleven million comparisons to answer six hundred questions,
    // and it was most of what this page cost.
    let by_item: HashMap<ItemId, Vec<&RealmSample>> = {
        // Read-model work, charged to the analysis stage: no statistic is
        // named here, but reducing a region's markets during a request is
        // exactly what Phase 2 moves to the write path.
        let _timing = timing::start(Stage::Analysis);
        let mut by_item: HashMap<ItemId, Vec<&RealmSample>> = HashMap::new();
        for sample in &samples {
            by_item.entry(sample.item).or_default().push(sample);
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
            .map(|entry| card(entry, &by_item, &named, selected, &tooltips, catalog))
            .filter(|card| matches(card, &needle))
            .collect();
        if cards.is_empty() {
            continue;
        }
        cards.sort_by(|a, b| a.name.cmp(&b.name));
        groups.push(GearGroup {
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
    Ok(GearView {
        realm_name: realm_name.clone(),
        region_label: prefs.region.to_string().to_uppercase(),
        kind: match kind {
            ItemKind::Recipe => "recipes",
            _ => "gear",
        },
        has_realms: !realms.is_empty(),
        fragment_href: format!(
            "{}?expansion={}&region={}&realm={}&q={}",
            text.fragment_path,
            query_value(&catalog.id),
            region_code,
            query_value(&realm_slug),
            query_value(needle.as_deref().unwrap_or_default()),
        ),
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
        archived: !catalog.is_active(),
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
    by_item: &HashMap<ItemId, Vec<&RealmSample>>,
    named: &BTreeMap<(Region, RealmId), (String, String)>,
    selected: Option<&Realm>,
    tooltips: &BTreeMap<u32, crate::views::TooltipView>,
    catalog: &Catalog,
) -> GearCard {
    let item = entry.ranks.first().map(|r| r.item_id).unwrap_or(ItemId(0));
    let mine: Vec<&RealmSample> = by_item.get(&item).cloned().unwrap_or_default();

    // One scope per column: the chosen realm, or every region side by side.
    // Regions are never merged -- a EU price is not something a US player can
    // act on.
    let scopes: Vec<(String, Vec<&RealmSample>)> = match selected {
        Some(realm) => vec![(realm.name.clone(), mine.clone())],
        None => {
            let mut regions: Vec<Region> = mine.iter().map(|s| s.region).collect();
            regions.sort();
            regions.dedup();
            regions
                .into_iter()
                .map(|region| {
                    (
                        region.to_string().to_uppercase(),
                        mine.iter()
                            .copied()
                            .filter(|s| s.region == region)
                            .collect(),
                    )
                })
                .collect()
        }
    };

    // Figures per scope, keyed by track, then transposed into rows. The
    // transpose is the point: a row holds the same track in every scope, so
    // one having an extra line of detail cannot push the other out of step.
    let per_scope: Vec<BTreeMap<Option<Track>, GearCell>> = scopes
        .iter()
        .map(|(_, samples)| match selected {
            Some(_) => realm_cells(samples, catalog),
            None => region_cells(samples, named, catalog),
        })
        .collect();

    // Which item levels each track actually holds, for the range on its row.
    let mut levels_seen: BTreeMap<Option<Track>, Vec<u16>> = BTreeMap::new();
    for sample in &mine {
        let level = rank_of(&sample.variant, catalog).map(|l| l.item_level);
        if let Some(level) = level {
            levels_seen
                .entry(track_of(&sample.variant, catalog))
                .or_default()
                .push(level);
        }
    }

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
            let cells: Vec<GearCell> = per_scope
                .iter()
                .map(|cells| cells.get(&track).cloned().unwrap_or_default())
                .collect();
            GearTrackRow {
                track: track.map(Track::as_str).unwrap_or(""),
                levels: level_range(levels_seen.get(&track).map(Vec::as_slice).unwrap_or(&[])),
                leveled: track.is_some(),
                href: match track {
                    Some(track) => format!("/wow/gear/{}/{}", item.get(), track.slug()),
                    None if entry.kind == ItemKind::Recipe => {
                        format!("/wow/recipe/{}", item.get())
                    }
                    None => String::new(),
                },
                listed: cells.iter().any(|c| c.listed),
                // A scope with nothing in this track still gets a cell, which
                // is what keeps the row square.
                cells,
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
        unlisted: tracks.iter().all(|t| t.cells.iter().all(|c| !c.listed)),
        scopes: scopes.into_iter().map(|(label, _)| label).collect(),
        tracks,
    }
}

/// One region's figures, per item level: the median across its realms, and
/// the extremes with the realm that holds them.
fn region_cells(
    samples: &[&RealmSample],
    named: &BTreeMap<(Region, RealmId), (String, String)>,
    catalog: &Catalog,
) -> BTreeMap<Option<Track>, GearCell> {
    let mut cells = BTreeMap::new();
    for (upgrade, group) in by_track(samples, catalog) {
        // A realm's price is its cheapest copy: that is what you would pay
        // there. One realm may list the same item level several times.
        let mut per_realm: BTreeMap<RealmId, (Copper, u32)> = BTreeMap::new();
        for sample in &group {
            let slot = per_realm
                .entry(sample.realm)
                .or_insert((sample.min_price, 0));
            slot.0 = slot.0.min(sample.min_price);
            slot.1 += sample.listings;
        }

        let mut priced: Vec<(RealmId, Copper)> = per_realm
            .iter()
            .map(|(id, (price, _))| (*id, *price))
            .collect();
        priced.sort_by_key(|(_, price)| price.get());
        let Some((cheapest_realm, cheapest_price)) = priced.first().copied() else {
            continue;
        };
        let (highest_realm, highest_price) = *priced.last().expect("non-empty");

        let region = group.first().map(|s| s.region);
        let name = |realm: RealmId| {
            region
                .and_then(|region| named.get(&(region, realm)).cloned())
                .unwrap_or_else(|| (realm.to_string(), String::new()))
        };
        cells.insert(
            upgrade,
            GearCell {
                listed: true,
                price: priced[priced.len() / 2].1.to_string(),
                cheapest: {
                    let (short, full) = name(cheapest_realm);
                    GearWhere {
                        realm: Some(short),
                        realm_full: full,
                        price: cheapest_price.to_string(),
                    }
                },
                highest: {
                    let (short, full) = name(highest_realm);
                    GearWhere {
                        realm: Some(short),
                        realm_full: full,
                        price: highest_price.to_string(),
                    }
                },
                listings: per_realm.values().map(|(_, n)| n).sum(),
                realms: priced.len(),
                extras: extras(&group, catalog),
            },
        );
    }
    cells
}

/// One realm's own figures, per item level.
///
/// Cheapest and highest stay, because the spread is the only comparison left
/// once there is no other realm -- but there is no realm to name.
fn realm_cells(samples: &[&RealmSample], catalog: &Catalog) -> BTreeMap<Option<Track>, GearCell> {
    let mut cells = BTreeMap::new();
    for (upgrade, group) in by_track(samples, catalog) {
        let Some(cheapest) = group.iter().map(|s| s.min_price).min() else {
            continue;
        };
        // Rows written before `max_price` existed carry zero, which is not a
        // price: fall back to the cheapest rather than reporting nothing.
        let highest = group
            .iter()
            .map(|s| s.max_price)
            .max()
            .filter(|p| p.get() > 0)
            .unwrap_or(cheapest);
        cells.insert(
            upgrade,
            GearCell {
                listed: true,
                price: cheapest.to_string(),
                cheapest: GearWhere {
                    realm: None,
                    realm_full: String::new(),
                    price: cheapest.to_string(),
                },
                highest: GearWhere {
                    realm: None,
                    realm_full: String::new(),
                    price: highest.to_string(),
                },
                listings: group.iter().map(|s| s.listings).sum(),
                realms: 1,
                extras: extras(&group, catalog),
            },
        );
    }
    cells
}

/// Group an item's variants by item level: one market per upgrade bonus.
///
/// Ordered by item level, so the ladder reads the same on every realm. A
/// variant carrying no upgrade bonus at all -- a recipe, or gear that never
/// had one -- sorts first as the base version.
/// Samples grouped by the track they belong to, weakest first.
///
/// One group per track, not per rank. The ranks inside a track are a range on
/// the row and a breakdown on the statistics page; they are not eight markets.
fn by_track<'a>(
    samples: &[&'a RealmSample],
    catalog: &Catalog,
) -> Vec<(Option<Track>, Vec<&'a RealmSample>)> {
    let mut grouped: BTreeMap<Option<Track>, Vec<&RealmSample>> = BTreeMap::new();
    for sample in samples {
        grouped
            .entry(track_of(&sample.variant, catalog))
            .or_default()
            .push(sample);
    }
    // `Track` orders weakest to strongest and `None` sorts first, which is the
    // order a card wants and the order a recipe's single row needs.
    grouped.into_iter().collect()
}

/// The resolved rank a variant carries, for other route modules.
pub(crate) fn rank_of_public<'a>(variant: &str, catalog: &'a Catalog) -> Option<&'a ItemLevel> {
    rank_of(variant, catalog)
}

/// The upgrade track a variant belongs to, for other route modules.
pub(crate) fn track_of_public(variant: &str, catalog: &Catalog) -> Option<Track> {
    track_of(variant, catalog)
}

/// The item levels a set of samples covers, as one range label.
pub(crate) fn level_range_of(samples: &[&RealmSample], catalog: &Catalog) -> String {
    let levels: Vec<u16> = samples
        .iter()
        .filter_map(|s| rank_of(&s.variant, catalog).map(|l| l.item_level))
        .collect();
    level_range(&levels)
}

/// The resolved rank a variant carries, if the catalog knows it.
fn rank_of<'a>(variant: &str, catalog: &'a Catalog) -> Option<&'a ItemLevel> {
    catalog.rank_in(variant)
}

/// The upgrade track a variant belongs to.
///
/// The track bonus first, because it is the reliable one: the market carries
/// rank 12827 that no sync has resolved, and its listings still land in
/// Veteran because 13332 is beside it in the same variant. The rank's own
/// wording is the fallback, for a catalog synced before tracks were recorded.
fn track_of(variant: &str, catalog: &Catalog) -> Option<Track> {
    catalog.track_in(variant)
}

/// The item levels a track holds, as one label: "285" or "279–285".
///
/// An en dash, not a hyphen: this is a range, and the hyphen is already doing
/// other work in "Veteran 1/6". Empty when nothing is listed, which is what
/// makes the row render as "no listings" rather than as a level of zero.
fn level_range(levels: &[u16]) -> String {
    let low = levels.iter().min();
    let high = levels.iter().max();
    match (low, high) {
        (Some(low), Some(high)) if low == high => low.to_string(),
        (Some(low), Some(high)) => format!("{low}\u{2013}{high}"),
        _ => String::new(),
    }
}

/// The names of the optional bonuses a variant carries, in catalog order.
pub(super) fn modifier_names<'a>(
    variant: &'a str,
    catalog: &'a Catalog,
) -> impl Iterator<Item = &'a str> + 'a {
    Catalog::bonuses(variant).filter_map(|id| catalog.modifier(id))
}

/// The optional bonuses in a market, counted by name.
///
/// A socketed piece is the same piece; it just sells for more. Pooling them
/// keeps a market thick enough to have a price, and the count says how much of
/// it is the plain version. Two bonus ids can share a name -- there is more
/// than one id for a Prismatic Socket -- so they are summed by name.
fn extras(samples: &[&RealmSample], catalog: &Catalog) -> Vec<GearExtra> {
    let mut counted: BTreeMap<&str, u32> = BTreeMap::new();
    for sample in samples {
        for id in Catalog::bonuses(&sample.variant) {
            if let Some(name) = catalog.modifier(id) {
                *counted.entry(name).or_default() += sample.listings;
            }
        }
    }
    counted
        .into_iter()
        .map(|(name, listings)| GearExtra {
            name: name.to_string(),
            listings,
        })
        .collect()
}

/// The same grouping `build` does, for tests that call `card` directly.
#[cfg(test)]
fn index_by_item(samples: &[RealmSample]) -> HashMap<ItemId, Vec<&RealmSample>> {
    let mut by_item: HashMap<ItemId, Vec<&RealmSample>> = HashMap::new();
    for sample in samples {
        by_item.entry(sample.item).or_default().push(sample);
    }
    by_item
}

#[cfg(test)]
mod tests {
    use cluster_core::Millis;

    use super::*;

    fn sample(realm: u32, variant: &str, price: u64, listings: u32) -> RealmSample {
        RealmSample {
            item: ItemId(271438),
            region: Region::Eu,
            realm: RealmId(realm),
            variant: variant.to_string(),
            observed_at: Millis(1_000),
            min_price: Copper(price),
            median_price: Copper(price),
            max_price: Copper(price),
            listings,
        }
    }

    fn catalog() -> Catalog {
        app_core::market::CatalogSet::embedded()
            .active()
            .expect("an active catalog")
            .clone()
    }

    /// The upgrade bonus is the market. It comes out of the stored bonus list
    /// by asking the catalog which id it knows an item level for, so an id
    /// nobody has resolved yet cannot silently become a market of its own.
    #[test]
    fn the_upgrade_bonus_identifies_the_item_level() {
        let catalog = catalog();
        let variant = "6652,10844,12834,13333,13662,13696";
        assert_eq!(catalog.upgrade_in(variant), Some(12834));
        let level = catalog
            .item_level(12834)
            .expect("resolved by the sync script");
        assert_eq!(level.item_level, 295);
        assert_eq!(level.upgrade, "Champion 2/6");
        assert_eq!(catalog.upgrade_in("40,10844,13662"), None);
    }

    /// A track is one market, and the ranks inside it are a range.
    ///
    /// The reverse of what this page used to do: eight rows, one per rank,
    /// which is eight markets nobody chooses between and a card too tall to
    /// compare with the one beside it. What a buyer picks is Veteran or Hero;
    /// which rung of Hero they end up with is a range and a breakdown on the
    /// statistics page.
    #[test]
    fn a_track_is_one_market_and_its_ranks_are_a_range() {
        let catalog = catalog();
        let samples = [
            sample(1403, "12825,13332", 90_000_000, 1),
            sample(1403, "12826,13332", 209_000_000, 1),
            sample(1403, "12833,13333", 120_000_000, 1),
            sample(1403, "12834,13333", 150_000_000, 1),
            sample(1403, "12835,13333", 200_000_000, 1),
            sample(1403, "12841,13334", 1_200_000_000, 1),
            sample(1403, "12842,13334", 1_350_000_000, 1),
            sample(1403, "12843,13334", 3_300_000_000, 1),
        ];
        let refs: Vec<&RealmSample> = samples.iter().collect();
        let cells = realm_cells(&refs, &catalog);

        assert_eq!(cells.len(), 3, "three tracks, three markets");
        let tracks: Vec<Option<Track>> = cells.keys().copied().collect();
        assert_eq!(
            tracks,
            [
                Some(Track::Veteran),
                Some(Track::Champion),
                Some(Track::Hero)
            ],
            "weakest first, which is the order a card lists them in"
        );

        // And each row says which levels it is made of.
        let hero: Vec<&RealmSample> = refs
            .iter()
            .copied()
            .filter(|s| track_of(&s.variant, &catalog) == Some(Track::Hero))
            .collect();
        assert_eq!(level_range_of(&hero, &catalog), "305\u{2013}311");
    }

    /// The track bonus is what groups, not the rank. The market carries rank
    /// 12827 that no sync has resolved; its listings must still land in
    /// Veteran, because 13332 is right there in the same variant.
    #[test]
    fn an_unresolved_rank_still_lands_in_its_track() {
        let catalog = catalog();
        assert!(
            catalog.item_level(12827).is_none(),
            "12827 is deliberately not in the shipped catalog"
        );
        assert_eq!(
            track_of("6652,10844,12827,13332,13662", &catalog),
            Some(Track::Veteran)
        );
    }

    /// A range of one level is that level, not "279-279".
    #[test]
    fn a_single_level_is_not_a_range() {
        assert_eq!(level_range(&[279]), "279");
        assert_eq!(level_range(&[279, 285, 282]), "279\u{2013}285");
        assert_eq!(level_range(&[]), "");
    }

    /// Sockets and tertiary stats are counted by name inside a market rather
    /// than splitting one: a socketed piece is the same piece.
    #[test]
    fn optional_bonuses_are_statistics_rather_than_markets() {
        let catalog = catalog();
        let samples = [
            sample(1403, "6652,10844,12834,13333,13662,13696", 90_000_000, 5),
            sample(1403, "41,10844,12834,13333,13662,13695", 150_000_000, 2),
        ];
        let refs: Vec<&RealmSample> = samples.iter().collect();
        let cells = realm_cells(&refs, &catalog);

        assert_eq!(cells.len(), 1, "one track is one market");
        let counted: Vec<(&str, u32)> = cells[&Some(Track::Champion)]
            .extras
            .iter()
            .map(|e| (e.name.as_str(), e.listings))
            .collect();
        // 6652 and 13696 are absence markers -- "no tertiary", "no socket" --
        // and the catalog gives them no name, so they are not counted.
        assert_eq!(counted, [("Leech", 2), ("Prismatic Socket", 2)]);
    }

    /// On one realm the spread is the only comparison there is, so cheapest
    /// and highest stay -- without a realm name, because there is one place.
    #[test]
    fn a_single_realm_keeps_the_spread_and_drops_the_name() {
        let catalog = catalog();
        let mut dear = sample(1403, "12834,13333", 90_000_000, 4);
        dear.max_price = Copper(500_000_000);
        let samples = [dear];
        let refs: Vec<&RealmSample> = samples.iter().collect();
        let cells = realm_cells(&refs, &catalog);

        let cell = &cells[&Some(Track::Champion)];
        assert_eq!(cell.cheapest.price, Copper(90_000_000).to_string());
        assert_eq!(cell.highest.price, Copper(500_000_000).to_string());
        assert!(cell.cheapest.realm.is_none(), "one realm needs no name");
        assert_eq!(cell.listings, 4, "only this realm's listings");
    }

    /// Tracked, but nobody is selling one. The card still has to appear and
    /// say so: "no listings" is an answer, and a different one from "we do
    /// not follow this item".
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
                item_id: ItemId(271434),
            }],
            floor_copper: None,
            icon: None,
        };
        // Samples exist, but for a different item.
        let elsewhere = [sample(1403, "12833,13333", 90_000_000, 3)];
        let by_item = index_by_item(&elsewhere);
        let card = card(
            &entry,
            &by_item,
            &BTreeMap::new(),
            None,
            &BTreeMap::new(),
            &catalog(),
        );
        assert!(card.unlisted);
        assert!(
            card.tracks
                .iter()
                .all(|t| t.cells.iter().all(|c| !c.listed)),
            "no track has anything to show"
        );
        assert_eq!(
            card.tracks.len(),
            4,
            "all four tracks, so this card lines up with the one beside it"
        );
    }
}
