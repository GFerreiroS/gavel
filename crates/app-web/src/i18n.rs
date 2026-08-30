//! Interface translation.
//!
//! Item text comes from Battle.net in every language (see
//! [`app_core::locale`]); this module covers the strings *we* wrote -- nav
//! labels, headings, column titles, hints.
//!
//! The catalogues are gettext PO files under `locales/`, which is the format
//! every community translation platform (Weblate, Crowdin, Transifex) reads
//! and writes. `build.rs` compiles them into sorted static tables, so at
//! runtime a lookup is a binary search over `&'static str` and a miss costs
//! nothing: the msgid *is* the English source string, so an untranslated
//! string renders as itself.
//!
//! Catalogues are per *language*, not per locale: `es_ES` and `es_MX` share
//! one Spanish interface even though their item text differs.

use std::any::Any;

use app_core::locale::Locale;
use askama::Values;

include!(concat!(env!("OUT_DIR"), "/catalogs.rs"));

/// The translation of `msgid`, or `msgid` itself when there is none.
pub fn translate(locale: Locale, msgid: &str) -> &str {
    let language = locale.language();
    let Some((_, entries)) = CATALOGS.iter().find(|(l, _)| *l == language) else {
        return msgid;
    };
    match entries.binary_search_by_key(&msgid, |(id, _)| id) {
        Ok(index) => entries[index].1,
        Err(_) => msgid,
    }
}

/// How much of the interface a catalogue must cover before the picker offers
/// it as a translation rather than as "item text only".
///
/// Presence is not enough: a catalogue somebody started yesterday with ten
/// strings in it would otherwise be advertised as a translation and deliver a
/// page that is 96% English. Below the threshold the language still works --
/// every translated string is used -- it is just not promised.
const INTERFACE_THRESHOLD_PERCENT: usize = 80;

/// Whether the interface itself is translated into this language, as opposed
/// to only the item text. Shown in the language picker, because promising a
/// translation that does not exist is worse than admitting it does not.
pub fn has_interface(locale: Locale) -> bool {
    let language = locale.language();
    if language == "en" {
        return true;
    }
    CATALOGS
        .iter()
        .find(|(l, _)| *l == language)
        .is_some_and(|(_, entries)| {
            entries.len() * 100 / TOTAL_STRINGS >= INTERFACE_THRESHOLD_PERCENT
        })
}

/// Translated share of the interface, for the "how complete is this" question
/// that a translation platform answers for its own languages but not for the
/// binary that is actually running.
pub fn interface_coverage(locale: Locale) -> usize {
    let language = locale.language();
    if language == "en" {
        return 100;
    }
    CATALOGS
        .iter()
        .find(|(l, _)| *l == language)
        .map_or(0, |(_, entries)| entries.len() * 100 / TOTAL_STRINGS)
}

/// Strings that reach the templates from other crates.
///
/// They cannot be found by scanning the templates -- a template only sees
/// `{{ node.status|t }}` -- so they are listed here for the extractor, and a
/// test asserts that every variant of the enums behind them is present. That
/// test is the thing that stops a new `Role` or `Category` from quietly
/// rendering in English for ever.
#[cfg_attr(not(test), allow(dead_code))]
pub const EXTERNAL_STRINGS: &[&str] = &[
    // Auction-house categories (views::AuctionCategory). Their names and
    // summaries live in Rust, so a template scan cannot see them.
    "Consumables",
    "Flasks, potions, food and runes -- what a raid night costs.",
    "Reagents",
    "Every crafting material of the current expansion, by profession.",
    "Region-wide market",
    "Enchants",
    "Every enchantment on the auction house, by the slot it applies to.",
    "Gems",
    "Bind-on-equip gear",
    "Raid BoEs, with a price on every realm and an upgrade ladder.",
    "Per connected realm",
    "Recipes",
    "Every recipe trading this expansion, by the profession that reads it.",
    // The two per-realm pages (routes::gear::Text).
    "Raid bind-on-equip pieces from {}. Gear is not a commodity: every connected realm has its own price, and one item id trades at several upgrade levels.",
    "Every recipe of {} trading on the auction house, by the profession that reads it. Recipes are per realm, like gear, and have no upgrade levels.",
    "The rare-quality cuts -- what a raider actually sockets.",
    // The enchants and gems pages (routes::enhancements::Text). Their wording
    // lives in Rust because one module serves both pages.
    "Every enchantment sold on the auction house in {}, by the slot it applies to. Prices are per scroll; each quality rank is its own market.",
    "Every rare-quality cut gem of {}. Uncommon cuts and the handful of epic gems are not tracked; each quality rank is its own market.",
    "{} enchants tracked.",
    "{} of {} enchants match.",
    "{} gems tracked.",
    "{} of {} gems match.",
    // Catalogue release states (routes::admin::state_label). The `/admin`
    // panel shows the word, not the machine state: `draft_ptr` goes in the
    // form, "PTR draft" goes to the reader.
    "PTR draft",
    "Collecting",
    "Archived",
    // Gear modifiers. These reach a template from the catalog's `modifiers`
    // map, which the sync script fills from a rendered English tooltip, so
    // they are listed here to be translatable like any other label.
    "Avoidance",
    "Leech",
    "Speed",
    "Indestructible",
    "Prismatic Socket",
    // Upgrade tracks (market::Track). The game's own word for each, and the
    // axis that actually separates gear prices.
    "Veteran",
    "Champion",
    "Hero",
    "Myth",
    // A single-rank card's column label (cards::card). The multi-rank labels
    // are rank numbers prefixed with an R, which are not words and are left
    // alone; this one is a word, and it rendered in English on an otherwise
    // Spanish page until it was listed here. (Keep quoted tokens out of the
    // comments in this block: the extractor reads them as entries.)
    "Price",
    // The valuation bands (market::engine::Valuation). One word decides how a
    // reader reads every figure beside it, and it reaches the template as
    // `{{ band|t }}` from a stored row, so a template scan cannot see it. A
    // test asserts every variant of the enum is here.
    "Very cheap",
    "Cheap",
    "Typical",
    "Expensive",
    "Very expensive",
    // Why there is no band (market::engine::Insufficient). §5.3 wants the
    // refusal said out loud rather than a card going quiet, and a refusal
    // nobody translated is a card going quiet in Spanish.
    "Not enough history",
    "Too many gaps",
    // The card sparkline's accessible name (chart::sparkline). Built in Rust
    // because the SVG is, so it is listed here like any other label that does
    // not pass through a template literal.
    "Price over the comparison window",
    // Equipment slots (market::Slot), the enchant grouping.
    "Head",
    "Neck",
    "Shoulder",
    "Cloak",
    "Chest",
    "Wrist",
    "Hands",
    "Waist",
    "Legs",
    "Feet",
    "Finger",
    "Weapon",
    "Two-handed weapon",
    // Professions (market::Profession), the reagent grouping.
    "Alchemy",
    "Blacksmithing",
    "Cooking",
    "Enchanting",
    "Engineering",
    "Fishing",
    "Herbalism",
    "Inscription",
    "Jewelcrafting",
    "Leatherworking",
    "Mining",
    "Skinning",
    "Tailoring",
    "Shared reagents",
    // Navigation (views::Layout).
    "Alerts",
    "Dashboard",
    "Cluster",
    "Nodes",
    "Jobs",
    "WoW",
    "Consumables",
    "Account",
    // Cluster roles.
    "gateway",
    "frontend",
    "backend",
    "compute",
    "storage",
    "coordinator",
    // Node health and cluster status.
    "starting",
    "healthy",
    "suspect",
    "offline",
    "degraded",
    "down",
    // Job and task states.
    "queued",
    "assigned",
    "running",
    "completed",
    "failed",
    "cancelled",
    // Cluster event messages: the sentence is translated, the node and job
    // identifiers substituted into it are not.
    "{} joined",
    "{} left",
    "{} heartbeat lost",
    "{} recovered",
    "{} assigned to {}",
    "{} removed from {}",
    "{} elected coordinator",
    "{} lost coordinator role",
    "{} created",
    "{} completed",
    "{} failed",
    "{} completed on {}",
    "{} failed on {} ({})",
    "{} failed ({})",
    "{} requeued",
    // Cluster events.
    "node_joined",
    "node_left",
    "node_unhealthy",
    "node_recovered",
    "node_offline",
    "leader_elected",
    "leader_lost",
    "role_assigned",
    "role_removed",
    "job_created",
    "job_completed",
    "job_failed",
    "task_assigned",
    "task_completed",
    "task_failed",
    "task_requeued",
    // Failure reasons.
    "execution_error",
    "timeout",
    "injected",
    "host",
    // Consumable categories, plus the two generated ones.
    "Flasks",
    "Combat potions",
    "Healing potions",
    "Mana potions",
    "Food",
    "Feasts",
    "Weapon oils",
    "Weapon stones",
    "Augment runes",
    "Vantus runes",
    "Utility",
    // Audiences.
    "Everyone",
    "Melee — tanks and melee DPS",
    "Caster — caster DPS and healers",
    "common",
    "melee",
    "caster",
    // Secondary stats.
    "haste",
    "crit",
    "mastery",
    "versatility",
    "primary",
    "stamina",
    // Never rendered (the templates hide it) but listed so the coverage test
    // can stay exhaustive rather than carry an exception.
    "none",
    // Baseline windows (prefs::BASELINE_CHOICES): the comparison window behind
    // every price percentage, offered on the Auction House index.
    "Last 24 hours",
    "Last 3 days",
    "Last 7 days",
    "Last 14 days",
    "Last 30 days",
    // Error messages (app_core::error::text). Every one of them is a sentence
    // a visitor reads at the moment something went wrong, which is the worst
    // moment to be handed English. `error_sentences_are_all_translatable`
    // walks `text::ALL` against this list.
    "not found",
    "invalid username or password",
    "not permitted",
    "that already exists",
    "that request was not valid: {}",
    "Something went wrong on our side.",
    "username must be {}-{} characters",
    "username may contain letters, digits, '_' and '-' only",
    "password must be {}-{} characters",
    "username already taken",
    "too many sign-in attempts; try again in {} minutes",
    "too many new accounts just now; try again in {} minutes",
    "task count must be between 1 and {}",
    "sleep duration must be between 1 and {} ms",
    "prime bound must be between 2 and {}",
    "unknown job kind '{}'",
    "unknown role '{}'",
    "a region is required",
    "that region name contains characters that are not allowed",
    "a realm is required",
    "that realm name contains characters that are not allowed",
    "a character name is required",
    "that character name contains characters that are not allowed",
    // Relative times (format::ago).
    "{} ago",
    "just now",
    // Price trends.
    "24h",
    "7d",
    "30d",
];

/// The render-time locale, handed to templates through Askama's value store.
///
/// A value rather than a field on every template struct: the alternative is
/// threading a locale through fifteen structs and their partials, and
/// forgetting one is a silently-English page.
pub struct Ctx {
    pub locale: Locale,
}

impl Values for Ctx {
    fn get_value<'a>(&'a self, key: &str) -> Option<&'a dyn Any> {
        (key == LOCALE_KEY).then_some(&self.locale as &dyn Any)
    }
}

const LOCALE_KEY: &str = "locale";

/// Template filters. Askama resolves `{{ "x"|t }}` to `filters::t(...)` in the
/// module where the template struct lives, so route modules import this as
/// `use crate::i18n::filters;`.
pub mod filters {
    use app_core::locale::{DEFAULT_LOCALE, Locale};
    use askama::Values;

    /// `{{ "Raid consumables"|t }}` -- translate a source string.
    pub fn t<'a>(msgid: &'a str, values: &dyn Values) -> askama::Result<&'a str> {
        Ok(super::translate(locale(values), msgid))
    }

    /// `{{ "every realm in {} sees it"|t|fill(region) }}` -- substitute the
    /// next `{}`.
    ///
    /// Translated sentences cannot always keep the English word order, so the
    /// variable has to travel inside the string rather than be concatenated
    /// around it.
    pub fn fill(
        text: impl std::fmt::Display,
        _values: &dyn Values,
        value: impl std::fmt::Display,
    ) -> askama::Result<String> {
        Ok(text.to_string().replacen("{}", &value.to_string(), 1))
    }

    fn locale(values: &dyn Values) -> Locale {
        askama::get_value::<Locale>(values, super::LOCALE_KEY)
            .copied()
            .unwrap_or(DEFAULT_LOCALE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untranslated_string_renders_as_its_source() {
        // Italian has no interface catalogue, so the English shows through
        // rather than a key or a blank.
        assert_eq!(
            translate(Locale::ItIt, "Raid consumables"),
            "Raid consumables"
        );
        assert!(!has_interface(Locale::ItIt));
    }

    #[test]
    fn spanish_is_translated_and_shared_by_both_variants() {
        assert_eq!(translate(Locale::EsEs, "Nodes"), "Nodos");
        assert_eq!(translate(Locale::EsMx, "Nodes"), "Nodos");
        assert!(has_interface(Locale::EsEs));
        assert!(has_interface(Locale::EsMx));
    }

    /// A `.po` whose name is not a language any `Locale` speaks would compile
    /// into the binary and never be reachable. Better to fail here than to
    /// have someone translate 250 strings into a file nobody can select.
    #[test]
    fn every_catalogue_is_reachable() {
        use app_core::locale::ALL_LOCALES;

        for (language, _) in CATALOGS {
            assert!(
                ALL_LOCALES.into_iter().any(|l| l.language() == *language),
                "locales/{language}.po does not match any supported locale"
            );
        }
    }

    #[test]
    fn a_barely_started_catalogue_is_not_advertised_as_a_translation() {
        // Spanish is complete; anything below the threshold must still read as
        // "item text only" in the picker.
        assert!(interface_coverage(Locale::EsEs) >= INTERFACE_THRESHOLD_PERCENT);
        assert_eq!(interface_coverage(Locale::ItIt), 0);
        assert!(!has_interface(Locale::ItIt));
    }

    #[test]
    fn english_needs_no_catalogue() {
        assert_eq!(translate(Locale::EnGb, "Nodes"), "Nodes");
        assert!(has_interface(Locale::EnGb));
    }

    /// Every event message pattern must be translatable, or the event log
    /// stays English in a translated page.
    #[test]
    fn every_event_message_is_listed() {
        use cluster_core::job::FailureReason;
        use cluster_core::{ClusterEvent, JobId, NodeId, Role, TaskId};

        let node = NodeId(1);
        let job = JobId(1);
        let task = TaskId(1);
        let events = [
            ClusterEvent::NodeJoined { node },
            ClusterEvent::NodeLeft { node },
            ClusterEvent::NodeUnhealthy { node },
            ClusterEvent::NodeRecovered { node },
            ClusterEvent::RoleAssigned {
                node,
                role: Role::Compute,
            },
            ClusterEvent::RoleRemoved {
                node,
                role: Role::Compute,
            },
            ClusterEvent::LeaderElected { node },
            ClusterEvent::LeaderLost { node },
            ClusterEvent::JobCreated { job },
            ClusterEvent::JobCompleted { job },
            ClusterEvent::JobFailed { job },
            ClusterEvent::TaskAssigned { task, node },
            ClusterEvent::TaskCompleted { task, node },
            ClusterEvent::TaskFailed {
                task,
                node: Some(node),
                reason: FailureReason::Timeout,
            },
            ClusterEvent::TaskFailed {
                task,
                node: None,
                reason: FailureReason::Timeout,
            },
            ClusterEvent::TaskRequeued { task },
        ];

        for event in events {
            let (pattern, _) = event.message_parts();
            assert!(
                EXTERNAL_STRINGS.contains(&pattern),
                "event pattern not listed for translation: {pattern:?}"
            );
        }
    }

    /// Every string another crate can put in front of a visitor must be in
    /// [`EXTERNAL_STRINGS`], or it can never be translated.
    #[test]
    fn no_domain_label_escapes_the_extractor() {
        use app_core::market::{
            ALL_AUDIENCES, ALL_AUDIENCES_LABELS, ALL_PROFESSIONS, Category, Stat,
        };
        use cluster_core::{ALL_ROLES, JobState, NodeStatus, TaskState};

        let mut missing: Vec<String> = Vec::new();
        let mut check = |label: &str| {
            if !EXTERNAL_STRINGS.contains(&label) {
                missing.push(label.to_string());
            }
        };

        // Every catalogue state the `/admin` panel can show. A fourth one
        // added to the lifecycle has to be translatable before it ships.
        for state in app_core::market::catalog::CatalogStatus::ALL {
            check(crate::routes::admin::state_label(state));
        }

        for role in ALL_ROLES {
            check(role.as_str());
        }
        for status in NodeStatus::ALL {
            check(status.as_str());
        }
        for state in JobState::ALL {
            check(state.as_str());
        }
        for state in TaskState::ALL {
            check(state.as_str());
        }
        for category in Category::ALL {
            check(category.label());
        }
        for (_, label) in ALL_AUDIENCES_LABELS {
            check(label);
        }
        for audience in ALL_AUDIENCES {
            check(audience.as_str());
        }
        for stat in Stat::ALL {
            check(stat.as_str());
        }
        for profession in ALL_PROFESSIONS {
            check(profession.label());
        }
        for (_, label) in crate::prefs::BASELINE_CHOICES {
            check(label);
        }
        for slot in app_core::market::ALL_SLOTS {
            check(slot.label());
        }
        // The enchants and gems pages carry their wording in Rust, so a typo
        // between the source string and this list would render English on an
        // otherwise translated page.
        for text in [
            crate::routes::enhancements::Text::ENCHANTS,
            crate::routes::enhancements::Text::GEMS,
        ] {
            check(text.title);
            check(text.blurb);
            check(text.counted);
            check(text.matched_of);
        }
        for text in [
            crate::routes::gear::Text::GEAR,
            crate::routes::gear::Text::RECIPES,
        ] {
            check(text.title);
            check(text.blurb);
        }

        assert!(
            missing.is_empty(),
            "not listed for translation: {missing:?}"
        );
    }

    /// Every upgrade track must be translatable: they are the row labels on
    /// every gear card, and an untranslated one is the loudest English on an
    /// otherwise Spanish page.
    #[test]
    fn every_upgrade_track_is_listed() {
        for track in app_core::market::Track::ALL {
            assert!(
                EXTERNAL_STRINGS.contains(&track.as_str()),
                "track not listed for translation: {}",
                track.as_str()
            );
        }
    }

    /// Every valuation band must be translatable.
    ///
    /// It is one word and it decides how a reader reads every figure beside
    /// it. It also reaches the template as `{{ band|t }}` from a stored row,
    /// so nothing that scans templates can find it -- which is exactly the
    /// shape of bug this test exists for.
    #[test]
    fn every_valuation_band_is_listed() {
        for band in app_core::market::Valuation::ALL {
            assert!(
                EXTERNAL_STRINGS.contains(&band.as_str()),
                "band not listed for translation: {}",
                band.as_str()
            );
        }
    }

    /// So must every reason a band was refused.
    ///
    /// §5.3 wants the refusal said out loud rather than a card going quiet,
    /// and a refusal nobody translated is a card going quiet in Spanish.
    /// Listed by hand because the reasons carry values and the enum has no
    /// `ALL`; the match is what fails when a variant is added.
    #[test]
    fn every_reason_for_no_band_is_listed() {
        use app_core::market::Insufficient;
        let reasons = [
            Insufficient::NotEnoughHistory { have: 0, need: 0 },
            Insufficient::TooManyGaps {
                coverage: 0,
                need: 0,
            },
        ];
        for reason in reasons {
            let label = match reason {
                Insufficient::NotEnoughHistory { .. } => "Not enough history",
                Insufficient::TooManyGaps { .. } => "Too many gaps",
            };
            assert!(
                EXTERNAL_STRINGS.contains(&label),
                "reason not listed for translation: {label}"
            );
        }
    }

    /// An error is the sentence a visitor is most likely to read and least
    /// likely to expect, so none of them may fall through to English.
    #[test]
    fn error_sentences_are_all_translatable() {
        for source in app_core::error::text::ALL {
            assert!(
                EXTERNAL_STRINGS.contains(source),
                "error source not listed for translation: {source:?}"
            );
            assert_ne!(
                translate(Locale::EsEs, source),
                *source,
                "error source has no Spanish translation: {source:?}"
            );
        }
    }

    /// A translation may reorder the sentence, but it cannot drop a value: a
    /// message with two placeholders and a translation with one renders the
    /// second number nowhere.
    #[test]
    fn translations_keep_every_placeholder() {
        for source in app_core::error::text::ALL {
            let spanish = translate(Locale::EsEs, source);
            assert_eq!(
                source.matches("{}").count(),
                spanish.matches("{}").count(),
                "placeholder count differs between {source:?} and {spanish:?}"
            );
        }
    }

    /// The generated tables are binary-searched, so their order is load
    /// bearing rather than cosmetic.
    #[test]
    fn catalogues_are_sorted() {
        for (language, entries) in CATALOGS {
            assert!(
                entries.windows(2).all(|w| w[0].0 < w[1].0),
                "{language} catalogue is not sorted by msgid"
            );
        }
    }
}
