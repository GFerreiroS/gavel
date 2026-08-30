//! Things that happened, and when.
//!
//! `docs/market-analysis.md` §9: correlating market movement with the
//! expansion needs explicit, timestamped events rather than labels inferred
//! later from the shape of a chart. This is the record, and only the record --
//! Phase 8 is where pre/post comparisons and heatmaps are built on it, and
//! §11 is explicit that an association is never described as a cause.
//!
//! Five things every event carries, and each of them is here because leaving
//! it out is a way of lying:
//!
//! * a **UTC interval**, open-ended where the thing is still going on;
//! * a **scope** -- which regions, which expansion, patch, tier, category,
//!   item or market it applies to, with "everything" spelled as an empty
//!   scope rather than as a special row;
//! * **provenance**: shipped catalogue data, an administrator's entry, or a
//!   deterministic calendar rule. A chart that marks an event should be able
//!   to say who says so;
//! * **validation**: whether anybody has checked it. An unverified event is
//!   not the same as a wrong one and not the same as a right one;
//! * **visibility**: PTR notes and operational annotations stay
//!   administrator-only until deliberately promoted.

use serde::{Deserialize, Serialize};

use cluster_core::Millis;

use super::catalog::{Catalog, ItemKind};
use super::key::MarketKey;
use super::{ItemId, Region};

/// What kind of thing happened.
///
/// The list is §9's candidates. It is an enum rather than free text because a
/// chart filters by it and an event study groups by it; "hotfix" and "Hotfix"
/// as two kinds is the failure that makes both useless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    PatchRelease,
    RaidOpening,
    SeasonStart,
    WeeklyReset,
    Hotfix,
    ProfessionChange,
    Holiday,
    /// A person writing down that something happened. The only kind whose
    /// truth rests on somebody's word, which is why it starts unvalidated.
    Annotation,
}

impl EventKind {
    pub const ALL: [EventKind; 8] = [
        EventKind::PatchRelease,
        EventKind::RaidOpening,
        EventKind::SeasonStart,
        EventKind::WeeklyReset,
        EventKind::Hotfix,
        EventKind::ProfessionChange,
        EventKind::Holiday,
        EventKind::Annotation,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            EventKind::PatchRelease => "patch_release",
            EventKind::RaidOpening => "raid_opening",
            EventKind::SeasonStart => "season_start",
            EventKind::WeeklyReset => "weekly_reset",
            EventKind::Hotfix => "hotfix",
            EventKind::ProfessionChange => "profession_change",
            EventKind::Holiday => "holiday",
            EventKind::Annotation => "annotation",
        }
    }

    /// The word a person reads, as a source string for translation.
    ///
    /// Separate from [`Self::as_str`], which is the machine word a form posts
    /// and a column stores. A label that doubled as a key would make renaming
    /// the label a migration.
    pub const fn label(self) -> &'static str {
        match self {
            EventKind::PatchRelease => "Patch release",
            EventKind::RaidOpening => "Raid opening",
            EventKind::SeasonStart => "Season start",
            EventKind::WeeklyReset => "Weekly reset",
            EventKind::Hotfix => "Hotfix",
            EventKind::ProfessionChange => "Profession change",
            EventKind::Holiday => "Holiday",
            EventKind::Annotation => "Note",
        }
    }

    pub fn parse(raw: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.as_str() == raw)
    }
}

/// Who says this happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// From the reviewed `catalogs.json`: a patch date, a tier opening.
    Catalogue,
    /// Generated from a rule -- a weekly reset -- rather than observed.
    Calendar,
    /// Typed in by an administrator.
    Administrator,
}

impl Provenance {
    pub const ALL: [Provenance; 3] = [
        Provenance::Catalogue,
        Provenance::Calendar,
        Provenance::Administrator,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Provenance::Catalogue => "catalogue",
            Provenance::Calendar => "calendar",
            Provenance::Administrator => "administrator",
        }
    }

    pub fn parse(raw: &str) -> Option<Provenance> {
        Provenance::ALL.into_iter().find(|p| p.as_str() == raw)
    }
}

/// Whether anybody has checked it.
///
/// Three states rather than a boolean, because "nobody has looked" and
/// "somebody looked and it is wrong" are different things to do with an event,
/// and collapsing them loses the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validation {
    #[default]
    Unvalidated,
    Validated,
    Rejected,
}

impl Validation {
    pub const ALL: [Validation; 3] = [
        Validation::Unvalidated,
        Validation::Validated,
        Validation::Rejected,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Validation::Unvalidated => "unvalidated",
            Validation::Validated => "validated",
            Validation::Rejected => "rejected",
        }
    }

    pub fn parse(raw: &str) -> Option<Validation> {
        Validation::ALL.into_iter().find(|v| v.as_str() == raw)
    }
}

/// Who may see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Administrator-only. Where PTR notes and operational annotations start,
    /// and where they stay until somebody promotes them (§9).
    #[default]
    Internal,
    Public,
}

impl Visibility {
    pub const ALL: [Visibility; 2] = [Visibility::Internal, Visibility::Public];

    pub const fn as_str(self) -> &'static str {
        match self {
            Visibility::Internal => "internal",
            Visibility::Public => "public",
        }
    }

    pub fn parse(raw: &str) -> Option<Visibility> {
        Visibility::ALL.into_iter().find(|v| v.as_str() == raw)
    }
}

/// What an event applies to.
///
/// Every field empty means "everything", which is the common case and needs no
/// special spelling. A region list is empty for a global event and populated
/// for a regional one: §9 is explicit that region reset times are
/// region-scoped events and not one global timestamp.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScope {
    #[serde(default)]
    pub regions: Vec<Region>,
    #[serde(default)]
    pub expansion: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
    /// The raid tier's id. Separate from `patch` even where they map one to
    /// one, for §8's reason.
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub category: Option<ItemKind>,
    #[serde(default)]
    pub item: Option<ItemId>,
    /// The narrowest scope there is: one market.
    #[serde(default)]
    pub market: Option<MarketKey>,
}

impl EventScope {
    /// Whether this event applies in a region.
    ///
    /// An empty region list is every region, not none. Getting that backwards
    /// would silently drop every global event from every chart.
    pub fn covers_region(&self, region: Region) -> bool {
        self.regions.is_empty() || self.regions.contains(&region)
    }
}

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketEvent {
    /// Stable and deterministic where it can be: a patch release's id is
    /// derived from the catalogue and the patch, so re-seeding the same
    /// catalogue writes the same row rather than a second copy of it.
    pub id: String,
    pub kind: EventKind,
    pub title: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// UTC. Everything here is UTC; a local time in this table would be a
    /// different instant depending on who read it.
    pub starts_at: Millis,
    /// Open-ended for something still going on.
    #[serde(default)]
    pub ends_at: Option<Millis>,
    #[serde(default)]
    pub scope: EventScope,
    pub provenance: Provenance,
    #[serde(default)]
    pub validation: Validation,
    #[serde(default)]
    pub visibility: Visibility,
}

impl MarketEvent {
    /// Whether a visitor may see it.
    ///
    /// Both halves, and both matter: an internal note must not leak, and
    /// neither must an event nobody has checked or one somebody has rejected.
    /// A chart that marks an unverified event is making a claim on its behalf.
    pub fn is_public(&self) -> bool {
        self.visibility == Visibility::Public && self.validation == Validation::Validated
    }
}

// --- events that are already data -------------------------------------------

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const WEEK_MS: u64 = 7 * DAY_MS;

/// Monday = 0. 1970-01-01 was a Thursday, hence the offset.
fn weekday(at: Millis) -> u64 {
    ((at.get() / DAY_MS) + 3) % 7
}

/// The events a reviewed catalogue already states: when each patch shipped,
/// and when each raid opened.
///
/// Nothing is inferred here. Every one of these is a date somebody wrote down
/// in `catalogs.json` and a reviewer read, which is why they come out
/// validated and public while the calendar rule below does not.
pub fn from_catalogue(catalog: &Catalog) -> Vec<MarketEvent> {
    let mut events = Vec::new();

    for patch in &catalog.patches {
        events.push(MarketEvent {
            id: format!("catalogue:{}:patch:{}", catalog.id, patch.patch),
            kind: EventKind::PatchRelease,
            title: patch.label(),
            notes: None,
            starts_at: patch.started_at(),
            ends_at: None,
            scope: EventScope {
                expansion: Some(catalog.expansion.clone()),
                patch: Some(patch.patch.clone()),
                ..EventScope::default()
            },
            provenance: Provenance::Catalogue,
            validation: Validation::Validated,
            visibility: Visibility::Public,
        });
    }

    for tier in &catalog.raid_tiers {
        events.push(MarketEvent {
            id: format!("catalogue:{}:tier:{}", catalog.id, tier.id),
            kind: EventKind::RaidOpening,
            title: tier.name.clone(),
            notes: None,
            starts_at: tier.opened_at(),
            ends_at: None,
            scope: EventScope {
                expansion: Some(catalog.expansion.clone()),
                // Both, and separately: the tier belongs to a patch and is not
                // the same key as one.
                patch: Some(tier.patch.clone()),
                tier: Some(tier.id.clone()),
                ..EventScope::default()
            },
            provenance: Provenance::Catalogue,
            validation: Validation::Validated,
            visibility: Visibility::Public,
        });
    }

    events.sort_by(|a, b| a.starts_at.cmp(&b.starts_at).then(a.id.cmp(&b.id)));
    events
}

/// When a region's week turns over.
///
/// A rule rather than rows, because a reset happens every week for ever and
/// storing that as a row per week is a table that grows for no reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetRule {
    pub region: Region,
    /// Monday = 0.
    pub weekday: u64,
    /// Hour of the day, UTC.
    pub hour_utc: u64,
}

/// The reset rules as this codebase currently believes them.
///
/// **Deliberately unvalidated.** The weekday is not in doubt; the hour is,
/// because the regional resets are defined in local time and this rule has no
/// idea that daylight saving exists -- for half the year an hour here is an
/// hour wrong. Rather than encode a precision the codebase does not have, the
/// events these produce are [`Validation::Unvalidated`] and
/// [`Visibility::Internal`], so nothing marks a chart with them until somebody
/// has checked. §2's rule, applied to a timestamp: an unavailable fact is
/// rendered as unavailable and never estimated to fill a gap.
pub const RESET_RULES: [ResetRule; 4] = [
    ResetRule {
        region: Region::Us,
        weekday: 1,
        hour_utc: 15,
    },
    ResetRule {
        region: Region::Eu,
        weekday: 2,
        hour_utc: 3,
    },
    ResetRule {
        region: Region::Kr,
        weekday: 2,
        hour_utc: 22,
    },
    ResetRule {
        region: Region::Tw,
        weekday: 2,
        hour_utc: 22,
    },
];

impl ResetRule {
    /// The first reset at or after `from`.
    pub fn first_at_or_after(&self, from: Millis) -> Millis {
        let day = Millis((from.get() / DAY_MS) * DAY_MS);
        let ahead = (7 + self.weekday - weekday(day)) % 7;
        let candidate = Millis(day.get() + ahead * DAY_MS + self.hour_utc * 60 * 60 * 1000);
        if candidate.get() >= from.get() {
            candidate
        } else {
            Millis(candidate.get() + WEEK_MS)
        }
    }

    /// Every reset in `[from, until)`, as events.
    ///
    /// Generated on demand rather than stored: the rule is the fact, and a row
    /// per week for ever is a table that says the same thing repeatedly.
    pub fn events(&self, from: Millis, until: Millis) -> Vec<MarketEvent> {
        let mut events = Vec::new();
        let mut at = self.first_at_or_after(from);
        while at.get() < until.get() {
            events.push(MarketEvent {
                id: format!("calendar:reset:{}:{}", self.region.as_str(), at.get()),
                kind: EventKind::WeeklyReset,
                title: format!("{} weekly reset", self.region.as_str().to_uppercase()),
                notes: None,
                starts_at: at,
                ends_at: None,
                scope: EventScope {
                    regions: vec![self.region],
                    ..EventScope::default()
                },
                provenance: Provenance::Calendar,
                // See RESET_RULES: the hour is not something this codebase
                // knows well enough to publish.
                validation: Validation::Unvalidated,
                visibility: Visibility::Internal,
            });
            at = Millis(at.get() + WEEK_MS);
        }
        events
    }
}

/// Every region's resets in a window.
pub fn weekly_resets(from: Millis, until: Millis) -> Vec<MarketEvent> {
    let mut events: Vec<MarketEvent> = RESET_RULES
        .iter()
        .flat_map(|rule| rule.events(from, until))
        .collect();
    events.sort_by(|a, b| a.starts_at.cmp(&b.starts_at).then(a.id.cmp(&b.id)));
    events
}
