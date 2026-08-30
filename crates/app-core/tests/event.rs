//! The event record: what it carries, and what it refuses to claim.
//!
//! Phase 1 records events. Nothing correlates against them yet -- that is
//! Phase 8 -- so what is worth testing now is the record's honesty: that its
//! scope means what it says, that an unchecked event cannot reach a visitor,
//! and that the events derived from the catalogue are derived rather than
//! guessed.

use app_core::market::catalog::Catalog;
use app_core::market::event::{
    self, EventKind, EventScope, Provenance, RESET_RULES, ResetRule, Validation, Visibility,
};
use app_core::market::{MarketEvent, Region};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

fn catalog() -> Catalog {
    Catalog::from_json(
        r#"{"id":"midnight","expansion":"Midnight",
            "patches":[
              {"patch":"12.0","name":"Midnight launch","started":"2026-03-02"},
              {"patch":"12.0.5","name":"Patch 12.0.5","started":"2026-04-21"},
              {"patch":"12.1","name":"The Curse","started":"2026-08-11"}],
            "raid_tiers":[
              {"id":"venomous-abyss","name":"The Venomous Abyss","patch":"12.1",
               "opened":"2026-08-18","season":2}],
            "items":[]}"#,
    )
    .unwrap()
}

/// Every catalogue event is a date somebody wrote down and a reviewer read,
/// which is why these come out validated and public. Nothing here is inferred.
#[test]
fn the_catalogue_states_its_own_timeline() {
    let events = event::from_catalogue(&catalog());
    assert_eq!(events.len(), 4, "three patches and one raid opening");

    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::PatchRelease,
            EventKind::PatchRelease,
            EventKind::PatchRelease,
            EventKind::RaidOpening,
        ],
        "oldest first, so the raid opening lands after the patch that carried it"
    );

    for e in &events {
        assert_eq!(e.provenance, Provenance::Catalogue);
        assert_eq!(e.validation, Validation::Validated);
        assert_eq!(e.visibility, Visibility::Public);
        assert!(e.is_public());
        assert_eq!(e.scope.expansion.as_deref(), Some("Midnight"));
        assert!(e.ends_at.is_none(), "a release does not end");
    }

    // The raid opened a week after its patch shipped, and the record keeps the
    // two apart -- which is the whole reason §8 stores them as separate keys.
    let patch = events
        .iter()
        .find(|e| e.scope.patch.as_deref() == Some("12.1"))
        .unwrap();
    let raid = events.last().unwrap();
    assert_eq!(raid.scope.tier.as_deref(), Some("venomous-abyss"));
    assert_eq!(raid.scope.patch.as_deref(), Some("12.1"));
    assert_eq!(raid.starts_at.get() - patch.starts_at.get(), 7 * DAY);
}

/// Re-deriving the same catalogue produces the same ids, which is what makes
/// recording them at every start idempotent.
#[test]
fn the_same_catalogue_derives_the_same_ids() {
    let first: Vec<String> = event::from_catalogue(&catalog())
        .into_iter()
        .map(|e| e.id)
        .collect();
    let again: Vec<String> = event::from_catalogue(&catalog())
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(first, again);
    assert!(first.contains(&"catalogue:midnight:patch:12.1".to_string()));
    assert!(first.contains(&"catalogue:midnight:tier:venomous-abyss".to_string()));
}

/// An empty region list is every region, not none. Backwards, this would
/// silently drop every global event from every chart.
#[test]
fn an_empty_scope_means_everything() {
    let everywhere = EventScope::default();
    for region in [Region::Eu, Region::Us, Region::Kr, Region::Tw] {
        assert!(everywhere.covers_region(region));
    }

    let eu_only = EventScope {
        regions: vec![Region::Eu],
        ..EventScope::default()
    };
    assert!(eu_only.covers_region(Region::Eu));
    assert!(!eu_only.covers_region(Region::Us));
}

/// Two separate gates, and a chart needs both: an internal note must not leak,
/// and neither must an event nobody has checked. Marking a chart with an
/// unverified event is making a claim on its behalf.
#[test]
fn an_unchecked_event_is_not_public_even_when_it_is_visible() {
    let mut event = MarketEvent {
        id: "x".into(),
        kind: EventKind::Annotation,
        title: "Something happened".into(),
        notes: None,
        starts_at: Millis(1_000),
        ends_at: None,
        scope: EventScope::default(),
        provenance: Provenance::Administrator,
        validation: Validation::Unvalidated,
        visibility: Visibility::Public,
    };
    assert!(!event.is_public(), "visible but unchecked is not public");

    event.validation = Validation::Validated;
    assert!(event.is_public());

    event.visibility = Visibility::Internal;
    assert!(
        !event.is_public(),
        "checked but internal is not public either"
    );

    event.visibility = Visibility::Public;
    event.validation = Validation::Rejected;
    assert!(
        !event.is_public(),
        "rejected is not the same as unchecked, and neither of them ships"
    );
}

/// The reset rule works, and says so about the part it does not know.
#[test]
fn a_weekly_reset_is_a_rule_and_not_a_row() {
    // 2026-01-01 was a Thursday. Monday = 0, so Thursday = 3.
    let thursday = Millis::from_utc_date(2026, 1, 1);
    let rule = ResetRule {
        region: Region::Eu,
        weekday: 2,
        hour_utc: 3,
    };

    // The next Wednesday at 03:00 UTC is six days later.
    let next = rule.first_at_or_after(thursday);
    assert_eq!(next, Millis(thursday.get() + 6 * DAY + 3 * HOUR));

    // Asking again from a moment just after it lands a week further on, not on
    // the same instant again.
    let after = rule.first_at_or_after(Millis(next.get() + 1));
    assert_eq!(after, Millis(next.get() + 7 * DAY));

    // Four weeks holds four resets, however the window is aligned.
    let events = rule.events(thursday, Millis(thursday.get() + 28 * DAY));
    assert_eq!(events.len(), 4);
    assert!(
        events
            .windows(2)
            .all(|p| p[1].starts_at.get() - p[0].starts_at.get() == 7 * DAY)
    );
}

/// §2, applied to a timestamp: an unavailable fact is rendered as unavailable
/// and never estimated to fill a gap. The weekday is not in doubt; the hour is,
/// because the resets are defined in local time and this rule has never heard
/// of daylight saving. So the events it produces stay off every public chart
/// until somebody checks them.
#[test]
fn the_reset_hours_are_not_claimed_to_be_right() {
    let from = Millis::from_utc_date(2026, 1, 1);
    let events = event::weekly_resets(from, Millis(from.get() + 14 * DAY));

    assert_eq!(
        events.len(),
        RESET_RULES.len() * 2,
        "two weeks, every region"
    );
    for e in &events {
        assert_eq!(e.provenance, Provenance::Calendar);
        assert_eq!(e.validation, Validation::Unvalidated);
        assert_eq!(e.visibility, Visibility::Internal);
        assert!(!e.is_public());
        assert_eq!(
            e.scope.regions.len(),
            1,
            "a reset is region-scoped, not global"
        );
    }

    // And every region has one: §9 is explicit that these are region-scoped
    // events rather than one global timestamp.
    let mut regions: Vec<Region> = events.iter().map(|e| e.scope.regions[0]).collect();
    regions.sort();
    regions.dedup();
    assert_eq!(regions.len(), RESET_RULES.len());
}
