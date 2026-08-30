//! The release lifecycle: draft_ptr -> active -> archived.
//!
//! Phase 1's exit gate, in the half that does not need a database: exactly one
//! catalogue is active, the one it replaced stays browsable, and a `draft_ptr`
//! catalogue is invisible to everything except the administrator's own view.
//!
//! The invariants are tested against the *rules*, not against the templates.
//! A page that happened to render the right thing today is not the same as a
//! rule that cannot render the wrong one.

use app_core::market::catalog::{CatalogSet, CatalogStatus};
use app_core::market::release::{self, ReleaseStates};

/// Three catalogues: last tier, this tier, and one being prepared against a
/// PTR. The shipped states are deliberately *stale* -- the file says the old
/// tier is active -- so that every assertion below is about the database's
/// answer rather than the file's.
fn catalogs() -> CatalogSet {
    CatalogSet::from_json(
        r#"{"catalogs":[
            {"id":"old","expansion":"Old","status":"active",
             "patches":[{"patch":"11.2","name":"Last","started":"2025-08-11"}],
             "raid_tiers":[{"id":"last-raid","name":"The Last Raid","patch":"11.2",
                            "opened":"2025-08-18","season":4}],
             "items":[]},
            {"id":"now","expansion":"Now","status":"archived",
             "patches":[{"patch":"12.1","name":"This","started":"2026-08-11"}],
             "raid_tiers":[{"id":"abyss","name":"The Venomous Abyss","patch":"12.1",
                            "opened":"2026-08-18","season":2}],
             "items":[]},
            {"id":"next","expansion":"Next","status":"archived",
             "patches":[{"patch":"12.2","name":"Next","started":"2026-11-03"}],
             "raid_tiers":[{"id":"next-raid","name":"The Next Raid","patch":"12.2",
                            "opened":"2026-11-10","season":3}],
             "items":[]}]}"#,
    )
    .expect("test catalogues")
}

fn states(pairs: &[(&str, CatalogStatus)]) -> ReleaseStates {
    let held = ReleaseStates::new();
    held.replace(pairs.iter().map(|(id, state)| (id.to_string(), *state)));
    held
}

fn ids(catalogs: Vec<&app_core::market::Catalog>) -> Vec<&str> {
    catalogs.into_iter().map(|c| c.id.as_str()).collect()
}

/// The whole point of moving the state out of the JSON: a person, not a build,
/// decides what is being collected.
#[test]
fn the_database_outranks_the_shipped_file() {
    let catalogs = catalogs();
    let states = states(&[
        ("old", CatalogStatus::Archived),
        ("now", CatalogStatus::Active),
        ("next", CatalogStatus::DraftPtr),
    ]);

    // The file still says "old" is the active one.
    assert_eq!(
        catalogs.shipped_active().map(|c| c.id.as_str()),
        Some("old")
    );
    // The deployment says otherwise, and the deployment wins.
    assert_eq!(
        release::active(&catalogs, &states).map(|c| c.id.as_str()),
        Some("now")
    );
}

/// §8: a `draft_ptr` catalogue is administrator-only. Not "hidden from the
/// nav" -- unreachable, including by guessing its id.
#[test]
fn a_ptr_draft_is_invisible_to_everybody_but_the_administrator() {
    let catalogs = catalogs();
    let states = states(&[
        ("old", CatalogStatus::Archived),
        ("now", CatalogStatus::Active),
        ("next", CatalogStatus::DraftPtr),
    ]);

    assert!(
        release::public(&catalogs, &states, "next").is_none(),
        "a bookmarked or guessed PTR id must be a 404, not a page"
    );
    assert_eq!(ids(release::public_all(&catalogs, &states)), ["now", "old"]);

    // The administrator's view is the one place it exists.
    assert_eq!(
        ids(release::all(&catalogs, &states)),
        ["now", "next", "old"]
    );
    assert!(release::public(&catalogs, &states, "now").is_some());
    assert!(release::public(&catalogs, &states, "old").is_some());
}

/// Archiving is not deleting. §8: archived is "public, frozen, and never
/// collected again", and its analysis stays browsable.
#[test]
fn an_archived_tier_stays_public_and_stops_collecting() {
    let catalogs = catalogs();
    let states = states(&[
        ("old", CatalogStatus::Archived),
        ("now", CatalogStatus::Active),
        ("next", CatalogStatus::DraftPtr),
    ]);

    let old = release::public(&catalogs, &states, "old").expect("still browsable");
    let state = release::state_of(&states, old);
    assert!(state.is_public());
    assert!(!state.is_collected(), "an archived tier is never collected");

    let now = release::public(&catalogs, &states, "now").unwrap();
    assert!(release::state_of(&states, now).is_collected());

    let next = catalogs.by_id("next").unwrap();
    let drafted = release::state_of(&states, next);
    assert!(!drafted.is_public());
    assert!(
        !drafted.is_collected(),
        "a PTR draft is never collected, which is why it has no prices to leak"
    );
}

/// An expansion that has ended while its successor is still on the PTR has
/// nothing active. That is a legal state, and it must not look like a fault.
#[test]
fn nothing_active_is_a_state_and_not_a_failure() {
    let catalogs = catalogs();
    let states = states(&[
        ("old", CatalogStatus::Archived),
        ("now", CatalogStatus::Archived),
        ("next", CatalogStatus::DraftPtr),
    ]);

    assert!(release::active(&catalogs, &states).is_none());
    // And the pages still have somewhere to go.
    assert_eq!(ids(release::public_all(&catalogs, &states)), ["now", "old"]);
}

/// Before the first read of the database -- one boot, on the release that adds
/// a catalogue -- the file's state is all there is.
#[test]
fn an_unseeded_catalogue_falls_back_to_what_it_shipped_with() {
    let catalogs = catalogs();
    let empty = ReleaseStates::new();
    assert!(empty.is_empty());
    assert_eq!(
        release::active(&catalogs, &empty).map(|c| c.id.as_str()),
        Some("old"),
        "the file's own status, until the database has one"
    );
}

/// Ordering is part of what a visitor sees: the collected one first, then the
/// archive newest first.
#[test]
fn the_collected_catalogue_leads_and_the_archive_follows_by_age() {
    let catalogs = catalogs();
    let states = states(&[
        ("old", CatalogStatus::Archived),
        ("now", CatalogStatus::Archived),
        ("next", CatalogStatus::Active),
    ]);
    assert_eq!(
        ids(release::public_all(&catalogs, &states)),
        ["next", "now", "old"]
    );
}
