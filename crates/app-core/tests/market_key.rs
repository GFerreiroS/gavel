//! What a market is, and that two spellings never name one.
//!
//! `MarketKey` is Phase 1's foundation: from Phase 2 onwards it is what a
//! read-model row, a cache key and a unit of remote work are filed under, so
//! its encoding is a contract rather than a convenience. These tests hold both
//! halves of that contract -- that every key survives a round trip, and that
//! nothing else decodes to a key at all.

use std::str::FromStr;

use app_core::market::catalog::Track;
use app_core::market::{Catalog, ItemId, MarketKey, PriceSample, RealmId, RealmSample, Region};
use cluster_core::Millis;

fn keys() -> Vec<MarketKey> {
    vec![
        MarketKey::commodity(Region::Eu, ItemId(212_265), 3),
        MarketKey::commodity(Region::Kr, ItemId(1), 1),
        MarketKey::recipe(Region::Eu, RealmId(1403), ItemId(271_441)),
        MarketKey::boe(
            Region::Us,
            RealmId(4),
            ItemId(271_441),
            Some(Track::Veteran),
        ),
        MarketKey::boe(Region::Tw, RealmId(963), ItemId(271_436), Some(Track::Myth)),
        // A track no catalogue has resolved. Still a market: the history is
        // real and a later sync can name it.
        MarketKey::boe(Region::Eu, RealmId(1403), ItemId(271_438), None),
    ]
}

#[test]
fn every_key_survives_the_round_trip() {
    for key in keys() {
        let encoded = key.to_string();
        assert_eq!(
            MarketKey::from_str(&encoded),
            Ok(key),
            "{encoded} did not decode to itself"
        );
    }
}

/// The exact spellings, written down. A change to any of these is a migration,
/// not an edit: they are what Phase 2's rows are keyed by.
#[test]
fn the_encoding_is_what_it_says_it_is() {
    assert_eq!(
        MarketKey::commodity(Region::Eu, ItemId(212_265), 3).to_string(),
        "c:eu:212265:3"
    );
    assert_eq!(
        MarketKey::recipe(Region::Eu, RealmId(1403), ItemId(271_441)).to_string(),
        "r:eu:1403:271441"
    );
    assert_eq!(
        MarketKey::boe(
            Region::Eu,
            RealmId(1403),
            ItemId(271_441),
            Some(Track::Hero)
        )
        .to_string(),
        "b:eu:1403:271441:hero"
    );
    assert_eq!(
        MarketKey::boe(Region::Eu, RealmId(1403), ItemId(271_441), None).to_string(),
        "b:eu:1403:271441:-"
    );
}

/// A decoder that accepts almost-a-key is a decoder that lets two strings name
/// one market, which is the whole thing this type exists to prevent.
#[test]
fn nothing_else_decodes() {
    for bad in [
        "",
        "c",
        "c:eu",
        "c:eu:212265",
        // A rank is 1-based; zero means the caller did not know, and a key
        // must not be able to say that.
        "c:eu:212265:0",
        // Trailing anything is a different key that starts the same.
        "c:eu:212265:3:",
        "c:eu:212265:3:x",
        "r:eu:1403:271441:extra",
        "x:eu:212265:3",
        "c:xx:212265:3",
        "c:eu:notanumber:3",
        // The forgiving spelling `Track::parse` accepts, which a key does not.
        "b:eu:1403:271441:Hero 2/6",
        "b:eu:1403:271441:HERO",
        "b:eu:1403:271441:",
        "b:eu:1403:271441:legendary",
    ] {
        assert!(
            MarketKey::from_str(bad).is_err(),
            "{bad:?} should not be a market key"
        );
    }
}

/// Two decoders for the same word, on purpose, and the strict one is the one
/// under a key.
#[test]
fn the_key_decoder_is_stricter_than_the_prose_one() {
    assert_eq!(Track::parse("Champion 2/6"), Some(Track::Champion));
    assert_eq!(Track::from_slug("Champion 2/6"), None);
    assert_eq!(Track::from_slug("champion"), Some(Track::Champion));
    assert_eq!(Track::from_slug("Champion"), None);
}

/// Sorting is part of the identity: a list of markets has one order, and
/// "whatever the database returned" is not it.
#[test]
fn markets_order_by_kind_then_place_then_item() {
    let mut sorted = keys();
    sorted.sort();
    let encoded: Vec<String> = sorted.iter().map(MarketKey::to_string).collect();
    assert_eq!(
        encoded,
        vec![
            "c:eu:212265:3",
            "c:kr:1:1",
            "r:eu:1403:271441",
            "b:eu:1403:271438:-",
            "b:us:4:271441:veteran",
            "b:tw:963:271436:myth",
        ]
    );
}

#[test]
fn a_commodity_market_knows_its_region_and_no_realm() {
    let key = MarketKey::commodity(Region::Eu, ItemId(212_265), 3);
    assert_eq!(key.region(), Region::Eu);
    assert_eq!(key.item(), ItemId(212_265));
    assert_eq!(key.realm(), None);
    assert!(key.is_commodity());

    let realm = MarketKey::recipe(Region::Eu, RealmId(1403), ItemId(1));
    assert_eq!(realm.realm(), Some(RealmId(1403)));
    assert!(!realm.is_commodity());
}

// --- resolving a market from an observation ---------------------------------

fn commodity(item: ItemId) -> PriceSample {
    PriceSample {
        item,
        region: Region::Eu,
        observed_at: Millis(1_767_225_600_000),
        min_unit_price: app_core::market::Copper(1),
        p05_unit_price: app_core::market::Copper(1),
        median_unit_price: app_core::market::Copper(1),
        quantity: 1,
        listings: 1,
    }
}

fn per_realm(item: ItemId, variant: &str) -> RealmSample {
    RealmSample {
        item,
        region: Region::Eu,
        realm: RealmId(1403),
        variant: variant.to_string(),
        observed_at: Millis(1_767_225_600_000),
        min_price: app_core::market::Copper(1),
        median_price: app_core::market::Copper(1),
        max_price: app_core::market::Copper(1),
        listings: 1,
    }
}

fn test_catalog() -> Catalog {
    Catalog::from_json(
        r#"{"id":"t","expansion":"T","season":"t","items":[
            {"name":"Potion","category":"flask","audience":"common","stat":"haste",
             "ranks":[{"rank":1,"item_id":10},{"rank":2,"item_id":11},{"rank":3,"item_id":12}]},
            {"name":"Pattern","category":"recipe","kind":"recipe","audience":"common",
             "ranks":[{"rank":1,"item_id":50}]},
            {"name":"Cloak","category":"boe","kind":"boe","audience":"common",
             "ranks":[{"rank":1,"item_id":60}]}],
          "tracks":{"13332":"veteran","13333":"champion"},
          "item_levels":{"12834":{"item_level":285,"upgrade":"Hero 2/6"}}}"#,
    )
    .expect("test catalog")
}

#[test]
fn a_commodity_observation_carries_its_rank_from_the_catalogue() {
    let catalog = test_catalog();
    assert_eq!(
        catalog.market_of(&commodity(ItemId(12))),
        MarketKey::commodity(Region::Eu, ItemId(12), 3)
    );
    assert_eq!(
        catalog.market_of(&commodity(ItemId(10))),
        MarketKey::commodity(Region::Eu, ItemId(10), 1)
    );
}

/// History collected under a catalogue that has since been archived is still
/// addressable. Refusing it would make an item that left the catalogue lose
/// the archive it is the whole point of keeping.
#[test]
fn an_untracked_item_still_gets_a_market() {
    let catalog = test_catalog();
    assert_eq!(
        catalog.market_of(&commodity(ItemId(999))),
        MarketKey::commodity(Region::Eu, ItemId(999), 1)
    );
}

#[test]
fn a_recipe_has_one_market_per_realm_and_no_track() {
    let catalog = test_catalog();
    assert_eq!(
        catalog.market_of_realm(&per_realm(ItemId(50), "")),
        MarketKey::recipe(Region::Eu, RealmId(1403), ItemId(50))
    );
}

/// CLAUDE.md §8, and the bug it came from: the market carries a rank no sync
/// has resolved, and its listings still belong to the track whose bonus id is
/// right there in the same variant. Grouping on the rank would have made it a
/// market of its own, named after nothing.
#[test]
fn a_boe_groups_on_the_track_bonus_not_on_the_rank() {
    let catalog = test_catalog();
    let unresolved_rank = per_realm(ItemId(60), "6652,10844,12827,13332,13662");
    assert_eq!(
        catalog.market_of_realm(&unresolved_rank),
        MarketKey::boe(Region::Eu, RealmId(1403), ItemId(60), Some(Track::Veteran)),
        "13332 names the track even though 12827 is unknown"
    );

    // With no track bonus at all, the rank's own wording is the fallback.
    let by_wording = per_realm(ItemId(60), "6652,10844,12834");
    assert_eq!(
        catalog.market_of_realm(&by_wording),
        MarketKey::boe(Region::Eu, RealmId(1403), ItemId(60), Some(Track::Hero))
    );

    // And with neither, the market exists and says it does not know.
    let unknown = per_realm(ItemId(60), "6652,10844");
    assert_eq!(
        catalog.market_of_realm(&unknown),
        MarketKey::boe(Region::Eu, RealmId(1403), ItemId(60), None)
    );
}

/// The shipped catalogue resolves to keys that survive a round trip, which is
/// the check that the encoding holds against real data rather than fixtures.
#[test]
fn the_shipped_catalogue_produces_decodable_keys() {
    let catalogs = app_core::market::CatalogSet::embedded();
    let catalog = catalogs.ordered().first().copied().expect("a catalogue");

    let mut seen = 0;
    for item in catalog.tracked_ids() {
        let key = catalog.market_of(&commodity(item));
        assert_eq!(MarketKey::from_str(&key.to_string()), Ok(key));
        seen += 1;
    }
    assert!(seen > 100, "only {seen} commodity markets in the catalogue");

    for item in catalog.realm_tracked_ids() {
        let key = catalog.market_of_realm(&per_realm(item, "6652,10844,12833,13333,13662"));
        assert_eq!(MarketKey::from_str(&key.to_string()), Ok(key));
    }
}
