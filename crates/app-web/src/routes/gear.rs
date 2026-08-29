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

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::{
    ALL_PROFESSIONS, Catalog, CatalogItem, Copper, ItemId, ItemKind, Realm, RealmId, RealmSample,
    Region,
};
use app_core::repo::{RealmPriceRepository, Store};
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
    GearCard, GearCell, GearExtra, GearGroup, GearLevelRow, GearView, GearWhere, Layout,
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
    let gear = build(&env, kind, prefs, &chosen, params.expansion.as_deref()).await?;
    let user = current_user(&env, &headers).await?;
    page(
        &GearPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                gear.title,
                "/wow/auctions",
                &uri,
                user.map(|u| u.username),
                csrf.0.clone(),
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
            gear: build(&env, kind, prefs, &chosen, params.expansion.as_deref()).await?,
        },
        prefs.locale,
    )
}

async fn build<E: Ports>(
    env: &E,
    kind: ItemKind,
    prefs: MarketPrefs,
    chosen: &RealmChoice,
    expansion: Option<&str>,
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
    let realms = prices.realms().await?;
    let named: BTreeMap<(Region, RealmId), String> = realms
        .iter()
        .map(|r| ((r.region, r.id), r.name.clone()))
        .collect();

    // Resolve the slug against the realms we actually collect. Slugs are
    // unique across the configured set, so the region in the URL is there for
    // the reader rather than to disambiguate; an unknown one is "all realms".
    let selected: Option<&Realm> = chosen.0.as_deref().and_then(|want| {
        realms
            .iter()
            .find(|r| slug(&r.name).as_deref() == Some(want))
    });

    // One query per region for the cross-realm view; one for the realm when
    // a realm is chosen. Either way it is the newest row per item, realm and
    // variant, which the store computes rather than us.
    let mut samples: Vec<RealmSample> = Vec::new();
    match selected {
        Some(realm) => samples.extend(prices.latest(realm.region, realm.id).await?),
        None => {
            for region in regions_of(&realms) {
                samples.extend(prices.latest_in_region(region).await?);
            }
        }
    }

    let observed = samples.iter().map(|s| s.observed_at).max();
    let tooltips = super::tooltip::cached_all(env, prefs, catalog, now).await;

    let mut groups = Vec::new();
    for (label, anchor, entries) in sections(catalog, kind) {
        let mut cards: Vec<GearCard> = entries
            .into_iter()
            .map(|entry| card(entry, &samples, &named, selected, &tooltips, catalog))
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

    let text = Text::of(kind);
    Ok(GearView {
        title: text.title,
        blurb: text.blurb,
        path: text.path,
        fragment_path: text.fragment_path,
        leveled: !matches!(kind, ItemKind::Recipe),
        compact: selected.is_some(),
        expansion: catalog.expansion.clone(),
        expansion_id: catalog.id.clone(),
        archived: !catalog.is_active(),
        observed: super::market::observed(prefs, now, observed),
        realm_label: selected.map(|r| r.name.clone()),
        region: selected.map_or(prefs.region, |r| r.region).as_str(),
        realm_slug: selected.and_then(|r| slug(&r.name)).unwrap_or_default(),
        realms: realms
            .iter()
            .map(|realm| RealmOption {
                value: slug(&realm.name).unwrap_or_default(),
                name: realm.name.clone(),
                region: realm.region.to_string().to_uppercase(),
                selected: selected.is_some_and(|r| r.id == realm.id && r.region == realm.region),
            })
            .collect(),
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

/// Which regions have collected realms, in a stable order.
fn regions_of(realms: &[Realm]) -> Vec<Region> {
    let mut regions: Vec<Region> = realms.iter().map(|r| r.region).collect();
    regions.sort();
    regions.dedup();
    regions
}

fn card(
    entry: &CatalogItem,
    samples: &[RealmSample],
    named: &BTreeMap<(Region, RealmId), String>,
    selected: Option<&Realm>,
    tooltips: &BTreeMap<u32, crate::views::TooltipView>,
    catalog: &Catalog,
) -> GearCard {
    let item = entry.ranks.first().map(|r| r.item_id).unwrap_or(ItemId(0));
    let mine: Vec<&RealmSample> = samples.iter().filter(|s| s.item == item).collect();

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

    // Figures per scope, keyed by item level, then transposed into rows. The
    // transpose is the point: a row holds the same item level in every region,
    // so one region having an extra line of detail cannot push the other out
    // of step.
    let per_scope: Vec<BTreeMap<Option<u32>, GearCell>> = scopes
        .iter()
        .map(|(_, samples)| match selected {
            Some(_) => realm_cells(samples, catalog),
            None => region_cells(samples, named, catalog),
        })
        .collect();

    let mut upgrades: Vec<Option<u32>> = per_scope.iter().flat_map(|c| c.keys().copied()).collect();
    upgrades.sort_by_key(|bonus| {
        bonus
            .and_then(|b| catalog.item_level(b))
            .map(|l| l.item_level)
            .unwrap_or(0)
    });
    upgrades.dedup();

    let levels: Vec<GearLevelRow> = upgrades
        .into_iter()
        .map(|bonus| {
            let known = bonus.and_then(|b| catalog.item_level(b));
            let item_level = known.map(|l| l.item_level).unwrap_or_default();
            GearLevelRow {
                item_level,
                upgrade: known.map(|l| l.upgrade.clone()).unwrap_or_default(),
                leveled: known.is_some(),
                href: match known {
                    Some(_) => format!("/wow/gear/{}/{item_level}", item.get()),
                    // A recipe has no item level, but it has statistics.
                    None if entry.kind == ItemKind::Recipe => {
                        format!("/wow/recipe/{}", item.get())
                    }
                    None => String::new(),
                },
                // A scope with nothing at this item level still gets a cell,
                // which is what keeps the row square.
                cells: per_scope
                    .iter()
                    .map(|cells| cells.get(&bonus).cloned().unwrap_or_default())
                    .collect(),
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
        unlisted: levels.is_empty(),
        scopes: scopes.into_iter().map(|(label, _)| label).collect(),
        levels,
    }
}

/// One region's figures, per item level: the median across its realms, and
/// the extremes with the realm that holds them.
fn region_cells(
    samples: &[&RealmSample],
    named: &BTreeMap<(Region, RealmId), String>,
    catalog: &Catalog,
) -> BTreeMap<Option<u32>, GearCell> {
    let mut cells = BTreeMap::new();
    for (upgrade, group) in by_level(samples, catalog) {
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
                .unwrap_or_else(|| realm.to_string())
        };
        cells.insert(
            upgrade,
            GearCell {
                listed: true,
                price: priced[priced.len() / 2].1.to_string(),
                cheapest: GearWhere {
                    realm: Some(name(cheapest_realm)),
                    price: cheapest_price.to_string(),
                },
                highest: GearWhere {
                    realm: Some(name(highest_realm)),
                    price: highest_price.to_string(),
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
fn realm_cells(samples: &[&RealmSample], catalog: &Catalog) -> BTreeMap<Option<u32>, GearCell> {
    let mut cells = BTreeMap::new();
    for (upgrade, group) in by_level(samples, catalog) {
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
                    price: cheapest.to_string(),
                },
                highest: GearWhere {
                    realm: None,
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
fn by_level<'a>(
    samples: &[&'a RealmSample],
    catalog: &Catalog,
) -> Vec<(Option<u32>, Vec<&'a RealmSample>)> {
    let mut grouped: BTreeMap<Option<u32>, Vec<&RealmSample>> = BTreeMap::new();
    for sample in samples {
        grouped
            .entry(upgrade_of(&sample.variant, catalog))
            .or_default()
            .push(sample);
    }
    let mut levels: Vec<(Option<u32>, Vec<&RealmSample>)> = grouped.into_iter().collect();
    levels.sort_by_key(|(bonus, _)| {
        bonus
            .and_then(|b| catalog.item_level(b))
            .map(|l| l.item_level)
            .unwrap_or(0)
    });
    levels
}

/// The upgrade bonus in a variant: the one id the catalog knows an item level
/// for. Anything else it carries is optional and counted, not grouped on.
fn upgrade_of(variant: &str, catalog: &Catalog) -> Option<u32> {
    bonuses(variant).find(|id| catalog.item_level(*id).is_some())
}

/// Whether a stored variant carries a particular bonus id.
pub(super) fn has_bonus(variant: &str, bonus: u32) -> bool {
    bonuses(variant).any(|id| id == bonus)
}

/// The names of the optional bonuses a variant carries, in catalog order.
pub(super) fn modifier_names<'a>(
    variant: &'a str,
    catalog: &'a Catalog,
) -> impl Iterator<Item = &'a str> + 'a {
    bonuses(variant).filter_map(|id| catalog.modifier(id))
}

fn bonuses(variant: &str) -> impl Iterator<Item = u32> + '_ {
    variant.split(',').filter_map(|id| id.parse::<u32>().ok())
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
        for id in bonuses(&sample.variant) {
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
        assert_eq!(upgrade_of(variant, &catalog), Some(12834));
        let level = catalog
            .item_level(12834)
            .expect("resolved by the sync script");
        assert_eq!(level.item_level, 295);
        assert_eq!(level.upgrade, "Champion 2/6");
        assert_eq!(upgrade_of("40,10844,13662", &catalog), None);
    }

    /// Eight item levels trade, and each is its own market. Pooling them --
    /// which an earlier version did, grouping by track -- reported one price
    /// for ilvl 292 and 298 together, which is a price for neither.
    #[test]
    fn every_item_level_is_its_own_market() {
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

        assert_eq!(cells.len(), 8, "eight item levels, eight markets");
        let mut levels: Vec<u16> = cells
            .keys()
            .filter_map(|bonus| catalog.item_level((*bonus)?).map(|l| l.item_level))
            .collect();
        levels.sort_unstable();
        assert_eq!(levels, [279, 282, 292, 295, 298, 305, 308, 311]);
        assert_eq!(
            catalog.item_level(12834).map(|l| l.upgrade.as_str()),
            Some("Champion 2/6")
        );
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

        assert_eq!(cells.len(), 1, "one item level is one market");
        let counted: Vec<(&str, u32)> = cells[&Some(12834)]
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

        let cell = &cells[&Some(12834)];
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
        let card = card(
            &entry,
            &elsewhere,
            &BTreeMap::new(),
            None,
            &BTreeMap::new(),
            &catalog(),
        );
        assert!(card.unlisted);
        assert!(card.levels.is_empty(), "no region has anything to show");
    }
}
