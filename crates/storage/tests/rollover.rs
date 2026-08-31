//! A complete tier rollover, simulated end to end.
//!
//! CLAUDE.md §16, Phase 9's exit gate:
//!
//! > a complete simulated tier rollover requires catalogue/admin data changes
//! > only; old analysis remains reachable and frozen, new collection starts
//! > once, and no route/template/statistic is forked for the tier.
//!
//! The last clause is asserted structurally in `app_web::routes`; the first
//! three are here, because they need the real activation transaction and the
//! real read model. **Nothing in this file writes code to make the rollover
//! happen.** It edits a catalogue and presses the button an administrator
//! presses, and then asks what the rules say.

use app_core::market::catalog::CatalogSet;
use app_core::market::materialise::{MarketState, Materialised};
use app_core::market::release::{self, ReleaseStates};
use app_core::market::window::Window;
use app_core::market::{Copper, ItemId, MarketKey, Region};
use app_core::repo::{ReadModelRepository, ReleaseRepository, Store};
use cluster_core::Millis;
use storage::{SqliteConfig, SqliteStore};

/// Midnight, mid-rollover: season 2 is what the file ships as active, season 3
/// is prepared against the PTR and carries **the expansion's whole** patch and
/// tier list.
///
/// That last part is the rule `Archive::problems` enforces, and it is here
/// rather than in a comment because it is the difference between the archived
/// tier's window closing when the next raid opened and it running on for ever.
fn catalogs() -> CatalogSet {
    CatalogSet::from_json(
        r#"{"catalogs":[
            {"id":"midnight-s2","expansion":"Midnight","status":"active",
             "catalog_version":4,
             "patches":[{"patch":"12.1","name":"The Curse of Ula'tek","started":"2026-08-11"}],
             "raid_tiers":[{"id":"venomous-abyss","name":"The Venomous Abyss","patch":"12.1",
                            "opened":"2026-08-18","season":2}],
             "items":[{"name":"Season 2 chestpiece","category":"boe","kind":"boe",
                       "audience":"common","ranks":[{"item_id":200,"rank":1}]},
                      {"name":"Flask of Alacrity","category":"flask","kind":"consumable",
                       "audience":"common","ranks":[{"item_id":300,"rank":1}]}]},
            {"id":"midnight-s3","expansion":"Midnight","status":"draft_ptr",
             "catalog_version":5,
             "notes":["Myth track bonus ids are read off the PTR build and are not final."],
             "patches":[{"patch":"12.1","name":"The Curse of Ula'tek","started":"2026-08-11"},
                        {"patch":"12.2","name":"The Sunless Reach","started":"2026-11-03"}],
             "raid_tiers":[{"id":"venomous-abyss","name":"The Venomous Abyss","patch":"12.1",
                            "opened":"2026-08-18","season":2},
                           {"id":"sunless-reach","name":"The Sunless Reach","patch":"12.2",
                            "opened":"2026-11-10","season":3}],
             "items":[{"name":"Season 3 chestpiece","category":"boe","kind":"boe",
                       "audience":"common","ranks":[{"item_id":400,"rank":1}]},
                      {"name":"Flask of Alacrity","category":"flask","kind":"consumable",
                       "audience":"common","ranks":[{"item_id":300,"rank":1}]}]}]}"#,
    )
    .expect("test catalogues")
}

async fn store() -> SqliteStore {
    SqliteStore::connect(&SqliteConfig::in_memory())
        .await
        .expect("in-memory database")
}

/// The states, read back from the database the way startup and every
/// activation read them.
async fn states(store: &SqliteStore) -> ReleaseStates {
    let held = ReleaseStates::new();
    held.replace(
        store
            .releases()
            .releases()
            .await
            .expect("releases")
            .into_iter()
            .map(|r| (r.catalog, r.state)),
    );
    held
}

/// A published market, minimally. Enough to be a row that either survives a
/// later publication or does not.
fn materialised(item: u32, price: u64) -> Materialised {
    let key = MarketKey::commodity(Region::Eu, ItemId(item), 1);
    Materialised {
        state: MarketState {
            min_price: Copper(price),
            observed_at: Some(Millis(1_000)),
            quantity: 10,
            listings: 4,
            ..MarketState::empty(key)
        },
        // No windows: what this test is about is whether the *current* row of
        // an archived market survives a later publication, and a window would
        // be a second thing to build and assert without a second question
        // behind it. `repositories.rs` holds the window round-trip.
        windows: Vec::new(),
    }
}

/// The whole rollover, in the order it happens.
#[tokio::test]
async fn a_tier_rollover_is_a_data_change_and_nothing_else() {
    let store = store().await;
    let catalogs = catalogs();
    let releases = store.releases();
    let model = store.read_model();
    let opened = Millis::from_utc_date(2026, 11, 10);

    // --- 1. the shipped states seed a database that has never seen them -----
    let seeded = releases
        .seed(&catalogs.shipped_states(), Millis(1_000))
        .await
        .expect("seed");
    assert_eq!(seeded, 2);

    // --- 2. the draft is invisible to everybody -----------------------------
    let before = states(&store).await;
    assert_eq!(
        release::active(&catalogs, &before).map(|c| c.id.as_str()),
        Some("midnight-s2")
    );
    assert!(
        release::public(&catalogs, &before, "midnight-s3").is_none(),
        "a guessed catalogue id is a 404, not a page"
    );
    assert!(
        release::public_item(&catalogs, &before, ItemId(400)).is_none(),
        "and neither is its candidate item list reachable one level down"
    );
    let archive = release::archive(&catalogs, &before);
    let midnight = archive.expansion("midnight").expect("Midnight");
    assert!(
        midnight.patch("12.2").is_none(),
        "an unannounced patch is not in the public hierarchy"
    );
    assert_eq!(midnight.patches.len(), 1);

    // --- 3. season 2 has published analysis ---------------------------------
    let first = model.begin(2, Millis(1_000)).await.unwrap();
    model
        .stage(
            first,
            &[materialised(300, 5_000), materialised(200, 90_000)],
        )
        .await
        .unwrap();
    model
        .publish(first, (Some(Millis(0)), Some(Millis(1_000))), Millis(1_100))
        .await
        .unwrap();
    let season_two = model
        .market(MarketKey::commodity(Region::Eu, ItemId(200), 1))
        .await
        .unwrap()
        .expect("a published season 2 market");

    // --- 4. an administrator activates the reviewed catalogue ---------------
    //
    // The same call `/admin/release` makes. No code changed between step 2 and
    // here; the catalogue was already in the binary and the button is what
    // published it.
    let done = releases
        .activate("midnight-s3", opened)
        .await
        .expect("activate");
    assert_eq!(done.activated, "midnight-s3");
    assert_eq!(
        done.archived.as_deref(),
        Some("midnight-s2"),
        "activating archives its predecessor in the same transaction (§8)"
    );

    let after = states(&store).await;

    // --- 5. new collection starts once, and only once -----------------------
    let active = release::active(&catalogs, &after).expect("something is collecting");
    assert_eq!(active.id, "midnight-s3");
    assert_eq!(
        catalogs
            .catalogs
            .iter()
            .filter(|c| release::state_of(&after, c).is_collected())
            .count(),
        1,
        "exactly one catalogue is collected, never two"
    );
    assert!(
        active.realm_tracked_ids().contains(&ItemId(400)),
        "the new tier's gear is what is fetched now"
    );
    assert!(
        !active.realm_tracked_ids().contains(&ItemId(200)),
        "and last tier's is not: an archived tier stops collecting"
    );
    // Pressing the button a second time is not a second rollover. §8's
    // transaction is idempotent, which is what "starts once" means when the
    // control is a form somebody can submit twice.
    let again = releases
        .activate("midnight-s3", Millis(opened.get() + 1_000))
        .await
        .expect("activate again");
    assert_eq!(again.archived, None, "nothing further is archived");
    let twice = states(&store).await;
    assert_eq!(
        release::active(&catalogs, &twice).map(|c| c.id.as_str()),
        Some("midnight-s3")
    );

    // --- 6. the old analysis is still reachable, and frozen -----------------
    assert!(
        release::public(&catalogs, &after, "midnight-s2").is_some(),
        "an archived catalogue stays browsable for ever"
    );
    let (owner, entry) =
        release::public_item(&catalogs, &after, ItemId(200)).expect("last tier's gear");
    assert_eq!(owner.id, "midnight-s2");
    assert_eq!(entry.name, "Season 2 chestpiece");

    // A publication that recalculates only the live catalogue's markets. The
    // archived one has no new observations, so nothing is staged for it -- and
    // `publish` leaves rows it did not recalculate exactly where they were.
    // That is what "frozen" is: not a rule about archived rows, but the
    // absence of anything to overwrite them with.
    let second = model.begin(2, Millis(2_000)).await.unwrap();
    model
        .stage(second, &[materialised(300, 4_100)])
        .await
        .unwrap();
    model
        .publish(
            second,
            (Some(Millis(0)), Some(Millis(2_000))),
            Millis(2_100),
        )
        .await
        .unwrap();
    let still_there = model
        .market(MarketKey::commodity(Region::Eu, ItemId(200), 1))
        .await
        .unwrap()
        .expect("season 2's market is still published");
    assert_eq!(
        still_there, season_two,
        "an archived tier's analysis is the last one published for it, unchanged"
    );
    assert_eq!(
        model
            .market(MarketKey::commodity(Region::Eu, ItemId(300), 1))
            .await
            .unwrap()
            .unwrap()
            .min_price,
        Copper(4_100),
        "while the market that is still collected moved"
    );

    // --- 7. the archive gained a branch and lost nothing --------------------
    let archive = release::archive(&catalogs, &after);
    let midnight = archive.expansion("midnight").expect("Midnight");
    assert_eq!(
        midnight.catalogs,
        ["midnight-s3", "midnight-s2"],
        "one expansion, two catalogues, newest first"
    );
    assert_eq!(
        midnight
            .patches
            .iter()
            .map(|p| p.patch.as_str())
            .collect::<Vec<_>>(),
        ["12.2", "12.1"],
        "the patch that came with the rollover, and the one before it"
    );
    let abyss = midnight
        .patch("12.1")
        .and_then(|p| p.tier("venomous-abyss"))
        .expect("last tier is still in the hierarchy");
    let reach = midnight
        .patch("12.2")
        .and_then(|p| p.tier("sunless-reach"))
        .expect("and the new one is in it");
    assert_eq!(abyss.until, Some(reach.opened), "the old tier closed");
    // And it still points at the catalogue that *opened* it, which is where
    // its bind-on-equip list is. Season 3 restates the tier -- it has to, or
    // the window above never closes -- and taking that one would put the new
    // raid's gear on the archived raid's page.
    assert_eq!(abyss.catalog, "midnight-s2");
    assert_eq!(reach.catalog, "midnight-s3");
    assert!(archive.problems().is_empty(), "{:#?}", archive.problems());

    // --- 8. and the stored window agrees with the page ----------------------
    //
    // The archive draws the tier's end from every catalogue of the expansion;
    // `Window::Tier` draws it from the catalogue that owns the market. They
    // have to be the same instant, or the page would name a period the
    // statistics under it do not cover. This is exactly what
    // `Archive::problems` refuses to let drift.
    let now = Millis::from_utc_date(2026, 12, 1);
    let bounds = Window::Tier("venomous-abyss".to_string())
        .bounds(active, now)
        .expect("the archived tier has a window");
    assert_eq!(bounds.0, abyss.opened);
    assert_eq!(bounds.1, abyss.until);
}
