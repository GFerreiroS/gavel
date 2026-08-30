//! What a region's worth of per-realm markets rolls up to.
//!
//! Gear and recipes are auctioned one at a time on each connected realm, so
//! both the card and the analysis page ask a question that spans realms: the
//! cheapest Veteran copy anywhere in EU, at what item levels, with how many
//! listings behind it. These tests pin what that roll-up says -- including the
//! three different meanings of "the dearest one", which is the distinction the
//! page would be lying about if they were merged.

use app_core::market::catalog::Track;
use app_core::market::materialise::{self, Scope};
use app_core::market::window::Window;
use app_core::market::{Catalog, CatalogSet, Copper, ItemId, RealmId, RealmSample, Region};
use cluster_core::Millis;

const AT: Millis = Millis(1_767_225_600_000);
const HOUR: u64 = 60 * 60 * 1000;
/// A gear item the shipped catalogue tracks.
const GEAR: ItemId = ItemId(271_438);

fn catalog() -> Catalog {
    CatalogSet::embedded()
        .shipped_active()
        .expect("an active catalogue")
        .clone()
}

fn sample(realm: u32, variant: &str, price: u64, listings: u32, at: Millis) -> RealmSample {
    RealmSample {
        item: GEAR,
        region: Region::Eu,
        realm: RealmId(realm),
        variant: variant.to_string(),
        observed_at: at,
        min_price: Copper(price),
        median_price: Copper(price),
        max_price: Copper(price),
        listings,
    }
}

fn roll(history: &[RealmSample]) -> Vec<app_core::market::MarketRollup> {
    materialise::rollups(history, &catalog(), &Window::Days(30))
}

/// Three realms, three prices. A realm's price is its cheapest copy, and the
/// spread *across* realms is which one to fly to.
#[test]
fn the_region_roll_up_is_the_spread_across_realms() {
    let history = [
        sample(1403, "12833,13333", 100, 3, AT),
        sample(1084, "12834,13333", 300, 2, AT),
        sample(1080, "12835,13333", 200, 5, AT),
    ];
    let rollups = roll(&history);

    let region = rollups
        .iter()
        .find(|r| r.scope == Scope::Region)
        .expect("a region roll-up");
    assert_eq!(region.track, Some(Track::Champion), "one track, one market");
    assert_eq!(region.cheapest_now, Some(Copper(100)));
    assert_eq!(region.cheapest_realm, Some(RealmId(1403)));
    assert_eq!(region.dearest_realm_now, Some(Copper(300)));
    assert_eq!(region.dearest_realm, Some(RealmId(1084)));
    assert_eq!(
        region.median_realm_now,
        Some(Copper(200)),
        "the middle realm, not the cheapest: one realm having a bad day is \
         not what the market costs"
    );
    assert_eq!(region.realms_listing, 3);
    assert_eq!(region.listings_now, 10);
}

/// "The dearest realm" and "the dearest listing" are different facts, and the
/// page shows each in the place it means something. Merging them would make a
/// card name a realm for a price nobody there is charging.
#[test]
fn the_dearest_realm_is_not_the_dearest_listing() {
    let mut dear = sample(1403, "12833,13333", 100, 3, AT);
    dear.max_price = Copper(9_000);
    let history = [dear, sample(1084, "12834,13333", 300, 2, AT)];
    let rollups = roll(&history);
    let region = rollups.iter().find(|r| r.scope == Scope::Region).unwrap();

    assert_eq!(region.dearest_realm_now, Some(Copper(300)));
    assert_eq!(
        region.highest_now,
        Some(Copper(9_000)),
        "the dearest listing is on the realm with the cheapest copy"
    );
    assert_eq!(region.dearest_realm, Some(RealmId(1084)));
}

/// One row per realm as well, because "one realm" is the same question with
/// one market in it -- which is what stops the page having two
/// implementations of everything it shows.
#[test]
fn every_realm_gets_its_own_row() {
    let history = [
        sample(1403, "12833,13333", 100, 3, AT),
        sample(1084, "12834,13333", 300, 2, AT),
    ];
    let rollups = roll(&history);

    let mine = rollups
        .iter()
        .find(|r| r.scope == Scope::Realm(RealmId(1403)))
        .expect("a realm roll-up");
    assert_eq!(mine.cheapest_now, Some(Copper(100)));
    assert_eq!(mine.realms_listing, 1);
    assert_eq!(mine.listings_now, 3, "only this realm's listings");
    assert_eq!(rollups.len(), 3, "one region and two realms");
}

/// CLAUDE.md §8: the track is the market and the rank inside it is not. Three
/// tracks are three roll-ups; eight ranks are not eight markets.
#[test]
fn a_track_is_one_market_and_its_ranks_are_a_range() {
    let history = [
        sample(1403, "12825,13332", 90, 1, AT),
        sample(1403, "12826,13332", 209, 1, AT),
        sample(1403, "12833,13333", 120, 1, AT),
        sample(1403, "12834,13333", 150, 1, AT),
        sample(1403, "12835,13333", 200, 1, AT),
        sample(1403, "12841,13334", 1_200, 1, AT),
        sample(1403, "12842,13334", 1_350, 1, AT),
        sample(1403, "12843,13334", 3_300, 1, AT),
    ];
    let rollups = roll(&history);

    let mut tracks: Vec<Option<Track>> = rollups
        .iter()
        .filter(|r| r.scope == Scope::Region)
        .map(|r| r.track)
        .collect();
    tracks.sort();
    assert_eq!(
        tracks,
        [
            Some(Track::Veteran),
            Some(Track::Champion),
            Some(Track::Hero)
        ],
        "three tracks, three markets"
    );

    let hero = rollups
        .iter()
        .find(|r| r.scope == Scope::Region && r.track == Some(Track::Hero))
        .unwrap();
    assert_eq!(hero.level_range, "305\u{2013}311");
    assert_eq!(hero.levels.len(), 3, "and the levels inside it, broken out");
    assert_eq!(hero.levels[0].item_level, 305);
    assert_eq!(hero.levels[0].cheapest, Copper(1_200));
}

/// The track bonus is what groups, not the rank. The market carries rank
/// 12827 that no sync has resolved; its listings still land in Veteran,
/// because 13332 is right there in the same variant.
#[test]
fn an_unresolved_rank_still_lands_in_its_track() {
    let catalog = catalog();
    assert!(
        catalog.item_level(12827).is_none(),
        "12827 is deliberately not in the shipped catalogue"
    );
    let history = [sample(1403, "6652,10844,12827,13332,13662", 90, 1, AT)];
    let rollups = roll(&history);
    assert_eq!(
        rollups[0].track,
        Some(Track::Veteran),
        "13332 names the track even though the rank is unknown"
    );
}

/// Sockets and tertiary stats are counted inside a market rather than
/// splitting one: a socketed piece is the same piece, and pooling them keeps a
/// market thick enough to have a price.
#[test]
fn optional_bonuses_are_counted_and_not_split_out() {
    let history = [
        sample(1403, "6652,10844,12834,13333,13662,13696", 90, 5, AT),
        sample(1403, "41,10844,12834,13333,13662,13695", 150, 2, AT),
    ];
    let rollups = roll(&history);
    let region = rollups.iter().find(|r| r.scope == Scope::Region).unwrap();

    assert_eq!(rollups.len(), 2, "one region row and one realm row");
    let counted: Vec<(&str, u32, u32)> = region
        .modifiers
        .iter()
        .map(|m| (m.name.as_str(), m.now, m.seen))
        .collect();
    // 6652 and 13696 are absence markers -- "no tertiary", "no socket" -- and
    // the catalogue gives them no name, so they are not counted.
    assert_eq!(counted, [("Leech", 2, 2), ("Prismatic Socket", 2, 2)]);
}

/// "Now" is per realm, because realms are generated on their own schedules and
/// the newest overall would silently drop every realm that had not refreshed.
#[test]
fn now_is_taken_per_realm_not_across_the_region() {
    let older = Millis(AT.get() - 6 * HOUR);
    let history = [
        // Sargeras refreshed an hour ago and got dearer.
        sample(1403, "12833,13333", 500, 1, AT),
        sample(1403, "12833,13333", 100, 1, older),
        // Kazzak has not refreshed since; its price is still the one it has.
        sample(1084, "12833,13333", 200, 1, older),
    ];
    let rollups = roll(&history);
    let region = rollups.iter().find(|r| r.scope == Scope::Region).unwrap();

    assert_eq!(
        region.realms_listing, 2,
        "a realm that has not refreshed is still selling"
    );
    assert_eq!(region.cheapest_now, Some(Copper(200)));
    assert_eq!(region.cheapest_ever, Some(Copper(100)), "over the window");
    assert_eq!(region.snapshots, 2);
}

/// Both charts on the page come out of one stored series: the price line is
/// the median of what the realms charge, the listings line is their sum.
#[test]
fn one_series_carries_both_charts() {
    let older = Millis(AT.get() - HOUR);
    let history = [
        sample(1403, "12833,13333", 100, 3, older),
        sample(1084, "12833,13333", 300, 2, older),
        sample(1403, "12833,13333", 150, 1, AT),
    ];
    let rollups = roll(&history);
    let region = rollups.iter().find(|r| r.scope == Scope::Region).unwrap();

    assert_eq!(region.series.len(), 2, "one point per snapshot");
    assert_eq!(region.series[0].at, older);
    assert_eq!(
        region.series[0].price,
        Copper(300),
        "the median of two realms is the dearer of them"
    );
    assert_eq!(region.series[0].quantity, 5, "their listings, summed");
    assert_eq!(region.series[1].price, Copper(150));
}

/// **A behaviour change, on purpose, and the one thing this slice did not
/// merely move.**
///
/// The card page used to read the newest row per *(item, realm, variant)*,
/// which is what the store's "latest" query returns. A variant a realm has
/// stopped listing still has a newest row -- its last one -- so the page went
/// on counting listings that were gone. On the real archive that was 1,081 of
/// EU's 18,864 per-realm markets: nearly six per cent of what a card called
/// "on sale" was not.
///
/// The analysis page never had that problem, because it took the newest
/// snapshot *per realm* and dropped anything older. The two pages therefore
/// disagreed about what "now" meant, which is precisely what CLAUDE.md §7
/// forbids -- one name for one thing. Both now use the analysis page's rule.
///
/// A recipe has one version of itself, so the two rules coincide for it: the
/// recipes page came out byte-identical, and the gear page did not.
#[test]
fn a_variant_a_realm_has_stopped_listing_is_not_on_sale() {
    let stale = Millis(AT.get() - 6 * HOUR);
    let history = [
        // Sargeras still lists the ilvl 295 one, refreshed just now.
        sample(1403, "12834,13333", 150, 4, AT),
        // It stopped listing the ilvl 305 one six hours ago. The row survives,
        // because the archive never forgets; the listing does not.
        sample(1403, "12841,13334", 900, 7, stale),
    ];
    let rollups = roll(&history);

    let champion = rollups
        .iter()
        .find(|r| r.scope == Scope::Region && r.track == Some(Track::Champion))
        .expect("still listed");
    assert_eq!(champion.listings_now, 4);

    let hero = rollups
        .iter()
        .find(|r| r.scope == Scope::Region && r.track == Some(Track::Hero))
        .expect("the market still exists");
    assert_eq!(
        hero.listings_now, 0,
        "a delisted variant is not on sale, however recently it was"
    );
    assert_eq!(hero.cheapest_now, None, "and it has no price now");
    assert_eq!(
        hero.cheapest_ever,
        Some(Copper(900)),
        "but the window still remembers what it cost"
    );
    assert_eq!(hero.listings_seen, 7);
}
