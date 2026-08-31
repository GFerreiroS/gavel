//! The public archive hierarchy: expansion -> patch -> raid tier.
//!
//! Phase 9's first bullet, and the half of its exit gate that needs no
//! database: a tier rollover is a catalogue edit, the expansion it belongs to
//! keeps every patch it ever had, and a `draft_ptr` catalogue contributes
//! nothing to any of it.

use app_core::market::archive::{Archive, slug};
use app_core::market::catalog::{CatalogSet, CatalogStatus};
use app_core::market::release::{self, ReleaseStates};

/// One expansion across two catalogues -- which is what a tier rollover
/// produces (§8: "New tiers introduce a new active catalogue") -- plus a
/// second expansion behind it and a PTR draft in front.
///
/// `midnight-s3` deliberately carries **the whole expansion's** patch and tier
/// list rather than only its own, which is the rule
/// [`Archive::problems`] exists to enforce.
fn catalogs() -> CatalogSet {
    CatalogSet::from_json(
        r#"{"catalogs":[
            {"id":"war-within","expansion":"The War Within","status":"archived",
             "patches":[{"patch":"11.0","name":"Launch","started":"2024-08-26"},
                        {"patch":"11.2","name":"Ghosts","started":"2025-08-11"}],
             "raid_tiers":[{"id":"nerubar","name":"Nerub-ar Palace","patch":"11.0",
                            "opened":"2024-09-10","season":1},
                           {"id":"manaforge","name":"Manaforge Omega","patch":"11.2",
                            "opened":"2025-08-12","season":3}],
             "items":[{"name":"Old flask","category":"flask","kind":"consumable",
                       "audience":"common","ranks":[{"item_id":100,"rank":1}]}]},
            {"id":"midnight-s2","expansion":"Midnight","status":"archived",
             "patches":[{"patch":"12.0","name":"Midnight launch","started":"2026-03-02"},
                        {"patch":"12.0.5","name":"Patch 12.0.5","started":"2026-04-21"},
                        {"patch":"12.1","name":"The Curse of Ula'tek","started":"2026-08-11"}],
             "raid_tiers":[{"id":"venomous-abyss","name":"The Venomous Abyss","patch":"12.1",
                            "opened":"2026-08-18","season":2}],
             "items":[{"name":"Season 2 chestpiece","category":"boe","kind":"boe",
                       "audience":"common","ranks":[{"item_id":200,"rank":1}]},
                      {"name":"Flask","category":"flask","kind":"consumable",
                       "audience":"common","ranks":[{"item_id":300,"rank":1}]}]},
            {"id":"midnight-s3","expansion":"Midnight","status":"active",
             "patches":[{"patch":"12.0","name":"Midnight launch","started":"2026-03-02"},
                        {"patch":"12.0.5","name":"Patch 12.0.5","started":"2026-04-21"},
                        {"patch":"12.1","name":"The Curse of Ula'tek","started":"2026-08-11"},
                        {"patch":"12.2","name":"The Sunless Reach","started":"2026-11-03"}],
             "raid_tiers":[{"id":"venomous-abyss","name":"The Venomous Abyss","patch":"12.1",
                            "opened":"2026-08-18","season":2},
                           {"id":"sunless-reach","name":"The Sunless Reach","patch":"12.2",
                            "opened":"2026-11-10","season":3}],
             "items":[{"name":"Season 3 chestpiece","category":"boe","kind":"boe",
                       "audience":"common","ranks":[{"item_id":400,"rank":1}]},
                      {"name":"Flask","category":"flask","kind":"consumable",
                       "audience":"common","ranks":[{"item_id":300,"rank":1}]}]},
            {"id":"midnight-s4","expansion":"Midnight","status":"draft_ptr",
             "patches":[{"patch":"12.3","name":"Unannounced","started":"2027-02-16"}],
             "raid_tiers":[{"id":"unannounced","name":"Unannounced","patch":"12.3",
                            "opened":"2027-02-23","season":4}],
             "items":[{"name":"Leaked chestpiece","category":"boe","kind":"boe",
                       "audience":"common","ranks":[{"item_id":500,"rank":1}]}]}]}"#,
    )
    .expect("test catalogues")
}

fn states(pairs: &[(&str, CatalogStatus)]) -> ReleaseStates {
    let held = ReleaseStates::new();
    held.replace(pairs.iter().map(|(id, state)| (id.to_string(), *state)));
    held
}

/// The state after a rollover: season 3 collecting, season 2 frozen, season 4
/// still a draft.
fn after_rollover() -> ReleaseStates {
    states(&[
        ("war-within", CatalogStatus::Archived),
        ("midnight-s2", CatalogStatus::Archived),
        ("midnight-s3", CatalogStatus::Active),
        ("midnight-s4", CatalogStatus::DraftPtr),
    ])
}

fn built() -> Archive {
    release::archive(&catalogs(), &after_rollover())
}

/// The shape §8 draws, and the order a reader reads it in: the expansion being
/// collected first, then the archive backwards.
#[test]
fn the_hierarchy_is_expansion_then_patch_then_tier() {
    let archive = built();

    let names: Vec<&str> = archive.expansions.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Midnight", "The War Within"]);

    let midnight = archive.expansion("midnight").expect("Midnight");
    assert!(midnight.collecting, "one of its catalogues is active");
    // Newest first: the archive is browsed backwards from what just happened.
    let patches: Vec<&str> = midnight.patches.iter().map(|p| p.patch.as_str()).collect();
    assert_eq!(patches, ["12.2", "12.1", "12.0.5", "12.0"]);

    let curse = midnight.patch("12.1").expect("12.1");
    assert_eq!(
        curse
            .tiers
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["venomous-abyss"]
    );
    let abyss = curse.tier("venomous-abyss").expect("the tier");
    assert_eq!(abyss.season, Some(2));
    assert_eq!(abyss.patch, "12.1", "a tier names its patch (§8)");
}

/// §8: "Patch and raid/tier are stored separately even when the current
/// content maps one-to-one; that relationship must not be baked into keys."
///
/// So a patch that opened no raid is a patch with no tiers -- 12.0.5 is
/// exactly that, and it must still be a page rather than being skipped for
/// having nothing under it.
#[test]
fn a_patch_that_opened_no_raid_is_still_a_patch() {
    let archive = built();
    let midnight = archive.expansion("midnight").expect("Midnight");

    let quiet = midnight
        .patch("12.0.5")
        .expect("12.0.5 is in the hierarchy");
    assert!(quiet.tiers.is_empty(), "it opened no raid");
    assert_eq!(quiet.name, "Patch 12.0.5");

    // And a tier is addressed by its own id, never by its patch's position.
    let tiers: Vec<&str> = midnight.tiers().iter().map(|t| t.id.as_str()).collect();
    assert_eq!(tiers, ["sunless-reach", "venomous-abyss"]);
}

/// The expansion is validated first and the patch second (§16, Phase 9), so a
/// real patch key from *another* expansion is a 404 rather than a page about
/// the wrong thing.
#[test]
fn a_patch_is_only_found_inside_its_own_expansion() {
    let archive = built();

    assert!(archive.expansion("midnight").is_some());
    assert!(archive.expansion("the-war-within").is_some());
    assert!(archive.expansion("no-such-expansion").is_none());

    // 11.2 is a real patch. It is not Midnight's.
    assert!(
        archive
            .expansion("midnight")
            .unwrap()
            .patch("11.2")
            .is_none(),
        "a patch of another expansion must not resolve here"
    );
    assert!(
        archive
            .expansion("the-war-within")
            .unwrap()
            .patch("11.2")
            .is_some()
    );

    // Same for a tier: real, but not under this patch.
    let curse = archive
        .expansion("midnight")
        .unwrap()
        .patch("12.1")
        .unwrap();
    assert!(curse.tier("sunless-reach").is_none(), "that is 12.2's tier");
}

/// A `draft_ptr` catalogue is administrator-only (§8). Not merely absent from
/// a picker: the whole branch it would add -- its patch, its tier, its
/// candidate items -- must not exist in the public hierarchy at all.
#[test]
fn a_ptr_draft_adds_no_branch_to_the_archive() {
    let archive = built();
    let midnight = archive.expansion("midnight").expect("Midnight");

    assert!(midnight.patch("12.3").is_none(), "an unannounced patch");
    assert!(
        midnight.tiers().iter().all(|t| t.id != "unannounced"),
        "an unannounced tier"
    );
    assert!(
        !midnight.catalogs.iter().any(|id| id == "midnight-s4"),
        "and the catalogue itself is not named"
    );

    // Activating it is what publishes it, and nothing else.
    let published = release::archive(
        &catalogs(),
        &states(&[
            ("war-within", CatalogStatus::Archived),
            ("midnight-s2", CatalogStatus::Archived),
            ("midnight-s3", CatalogStatus::Archived),
            ("midnight-s4", CatalogStatus::Active),
        ]),
    );
    assert!(
        published
            .expansion("midnight")
            .expect("Midnight")
            .patch("12.3")
            .is_some(),
        "the same data, once somebody decided to publish it"
    );
}

/// A rollover leaves one expansion spanning two catalogues, and the reader
/// must not be able to tell: 12.0 is still Midnight's first patch after
/// season 3 took over.
#[test]
fn one_expansion_survives_a_rollover_across_two_catalogues() {
    let archive = built();
    let midnight = archive.expansion("midnight").expect("Midnight");

    assert_eq!(
        midnight.catalogs,
        ["midnight-s3", "midnight-s2"],
        "newest first, and the draft is not among them"
    );
    assert_eq!(midnight.patches.len(), 4, "every patch it ever had");

    // The season 2 tier is filed under the catalogue that **opened** it, which
    // is what makes an archived tier's page show the gear that raid dropped.
    //
    // This was wrong once and it rendered perfectly: season 3 restates season
    // 2's tier -- it has to, or season 2's window never closes -- so both
    // catalogues declare it, and taking the newer one put the *new* season's
    // bind-on-equip pieces on the *archived* tier's page.
    let abyss = midnight
        .patch("12.1")
        .and_then(|p| p.tier("venomous-abyss"))
        .expect("the tier");
    assert_eq!(abyss.catalog, "midnight-s2", "whose gear this raid dropped");
    assert_eq!(
        abyss.declared_by,
        ["midnight-s3", "midnight-s2"],
        "and both catalogues know it happened, which is a different question"
    );
    let sunless_tier = midnight
        .patch("12.2")
        .and_then(|p| p.tier("sunless-reach"))
        .expect("the tier");
    assert_eq!(sunless_tier.catalog, "midnight-s3");

    // A patch both catalogues declare names both, newest first -- and the
    // newest is what a link to its prices carries, because that is the
    // catalogue whose windows are still being materialised.
    let curse = midnight.patch("12.1").expect("12.1");
    assert_eq!(curse.catalogs, ["midnight-s3", "midnight-s2"]);
    assert_eq!(curse.catalog(), "midnight-s3");
    assert_eq!(
        midnight.patch("12.2").expect("12.2").catalogs,
        ["midnight-s3"],
        "and a patch only one of them has names only that one"
    );

    // And it is closed at the tier that replaced it rather than running on.
    let sunless = midnight
        .patch("12.2")
        .and_then(|p| p.tier("sunless-reach"))
        .expect("the tier");
    assert_eq!(abyss.until, Some(sunless.opened));
    assert_eq!(sunless.until, None, "the current tier is open-ended");
}

/// Expansions close at the one that replaced them, and the current one does
/// not close at all.
#[test]
fn a_finished_expansion_is_told_when_it_ended() {
    let archive = built();
    let midnight = archive.expansion("midnight").expect("Midnight");
    let war = archive.expansion("the-war-within").expect("The War Within");

    assert_eq!(midnight.until, None, "still being collected");
    assert_eq!(
        war.until,
        Some(midnight.from),
        "it ended when Midnight started"
    );
    assert!(!war.collecting);
}

/// The one way a tier rollover goes wrong silently.
///
/// `Window::Tier` ends a tier at the next tier **its own catalogue** declares.
/// A catalogue that ships one tier and is superseded therefore has a tier
/// window with no end, which goes on absorbing the successor's prices -- a
/// statistic that is wrong rather than absent. The check reads as a sentence
/// because the fix is a data edit.
#[test]
fn a_tier_left_open_ended_by_a_rollover_is_reported() {
    // The correct arrangement: season 3 carries the whole tier list.
    assert!(
        built().problems().is_empty(),
        "a catalogue carrying its expansion's whole tier list is coherent"
    );

    // The mistake: season 3 knows only its own tier, so season 2's window
    // never closes.
    let forgetful = CatalogSet::from_json(
        r#"{"catalogs":[
            {"id":"s2","expansion":"Midnight","status":"archived",
             "patches":[{"patch":"12.1","name":"Curse","started":"2026-08-11"}],
             "raid_tiers":[{"id":"abyss","name":"Abyss","patch":"12.1",
                            "opened":"2026-08-18","season":2}],
             "items":[]},
            {"id":"s3","expansion":"Midnight","status":"active",
             "patches":[{"patch":"12.1","name":"Curse","started":"2026-08-11"},
                        {"patch":"12.2","name":"Reach","started":"2026-11-03"}],
             "raid_tiers":[{"id":"reach","name":"Reach","patch":"12.2",
                            "opened":"2026-11-10","season":3}],
             "items":[]}]}"#,
    )
    .expect("test catalogues");
    let problems = release::archive(
        &forgetful,
        &states(&[
            ("s2", CatalogStatus::Archived),
            ("s3", CatalogStatus::Active),
        ]),
    )
    .problems();

    assert_eq!(problems.len(), 1, "{problems:#?}");
    let said = &problems[0];
    assert!(
        said.contains("\"s2\""),
        "it names the catalogue to fix: {said}"
    );
    assert!(said.contains("\"abyss\""), "{said}");
    assert!(said.contains("\"reach\""), "{said}");
    assert!(said.contains("Add"), "it says what to do: {said}");
}

/// An item that trades across a rollover is listed by both catalogues, and the
/// live definition is the one a page reads -- its patch list is the longer one,
/// so the windows it materialises are the ones that close at the right moment.
#[test]
fn an_item_that_survives_a_rollover_is_owned_by_the_live_catalogue() {
    let catalogs = catalogs();
    let states = after_rollover();
    let owners = release::public_owners(&catalogs, &states);

    use app_core::market::ItemId;
    assert_eq!(
        owners.get(&ItemId(300)).map(|c| c.id.as_str()),
        Some("midnight-s3")
    );
    // One that did not survive keeps the catalogue that has it.
    assert_eq!(
        owners.get(&ItemId(200)).map(|c| c.id.as_str()),
        Some("midnight-s2")
    );
    assert_eq!(
        owners.get(&ItemId(100)).map(|c| c.id.as_str()),
        Some("war-within")
    );
    // And the draft's candidate items belong to nobody the public can reach.
    assert!(!owners.contains_key(&ItemId(500)));
}

/// The item gate, one level below the catalogue gate: an id that exists only
/// in a `draft_ptr` catalogue resolves to nothing, so its page is a 404 rather
/// than an announcement of what the next tier will carry.
#[test]
fn a_draft_items_page_does_not_exist() {
    let catalogs = catalogs();
    let states = after_rollover();
    use app_core::market::ItemId;

    assert!(release::public_item(&catalogs, &states, ItemId(500)).is_none());
    // An archived tier's gear, by contrast, stays reachable for ever. That is
    // what an archive is.
    let (catalog, entry) =
        release::public_item(&catalogs, &states, ItemId(200)).expect("archived gear");
    assert_eq!(catalog.id, "midnight-s2");
    assert_eq!(entry.name, "Season 2 chestpiece");
}

#[test]
fn slugs_are_derived_from_the_name() {
    assert_eq!(slug("Midnight"), "midnight");
    assert_eq!(slug("The War Within"), "the-war-within");
    assert_eq!(slug("Warlords of Draenor"), "warlords-of-draenor");
    assert_eq!(slug("  Legion  "), "legion");
    assert_eq!(slug("12.0.5"), "12-0-5");
}
