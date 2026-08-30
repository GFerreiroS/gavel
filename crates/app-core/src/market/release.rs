//! What the deployment says each catalogue's state is.
//!
//! The catalogue's *content* -- items, tracks, bonus ids -- ships in
//! `catalogs.json`, reviewed in version control, which is where
//! `scripts/catalog-sync.py` writes it and where a patch's diff can be read.
//! Its *state* lives in the database, because that is the part a person
//! changes on a running instance: `docs/market-analysis.md` §8 has an
//! administrator activate a reviewed PTR catalogue explicitly, "rather than an
//! unattended calendar date, because PTR and release schedules can change" --
//! and a state compiled into the binary would mean a redeploy to follow one
//! that slipped.
//!
//! It is the same split [`crate::repo::SettingsRepository`] already makes:
//! what exists is reviewed code, what is switched on is a runtime decision.
//!
//! This type is the read side of that. It is loaded at startup and replaced
//! when an activation succeeds, so a page never asks the database where a
//! catalogue is in its life; it asks a map.

use std::collections::BTreeMap;
use std::sync::RwLock;

use super::catalog::CatalogStatus;

/// The states, as the database last reported them.
///
/// A plain `RwLock` rather than an async one: the critical section is a map
/// lookup with no `await` in it, and holding an async lock across nothing is
/// a scheduler round trip for no reason.
#[derive(Debug, Default)]
pub struct ReleaseStates {
    states: RwLock<BTreeMap<String, CatalogStatus>>,
}

impl ReleaseStates {
    pub fn new() -> ReleaseStates {
        ReleaseStates::default()
    }

    /// Replace the whole map. Called at startup and after an activation, both
    /// of which produce a complete picture rather than a delta.
    pub fn replace(&self, states: impl IntoIterator<Item = (String, CatalogStatus)>) {
        let fresh: BTreeMap<String, CatalogStatus> = states.into_iter().collect();
        match self.states.write() {
            Ok(mut held) => *held = fresh,
            // A poisoned lock means a thread panicked while holding it. The
            // map is a `BTreeMap` of copies with no invariant to break, so the
            // recovered value is sound; refusing to update it would leave the
            // whole instance stuck on a stale state for the sake of tidiness.
            Err(poisoned) => *poisoned.into_inner() = fresh,
        }
    }

    /// This catalogue's state, or `shipped` if the database has never seen it.
    ///
    /// The fallback matters on exactly one boot: the one where a release adds
    /// a catalogue, before the seed runs. After that there is always a row.
    pub fn state_of(&self, catalog: &str, shipped: CatalogStatus) -> CatalogStatus {
        let held = match self.states.read() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        held.get(catalog).copied().unwrap_or(shipped)
    }

    /// Whether anything has been loaded yet. False before the first read of
    /// the database, which is the only time `state_of` falls back.
    pub fn is_empty(&self) -> bool {
        match self.states.read() {
            Ok(held) => held.is_empty(),
            Err(poisoned) => poisoned.into_inner().is_empty(),
        }
    }
}

// --- resolving a catalogue under a state ------------------------------------
//
// Free functions rather than methods on [`crate::Ports`] so that the rules
// which decide what a visitor may see can be tested without standing up ten
// associated types. `Ports` delegates to them; nothing else should reimplement
// them.

use super::catalog::{Catalog, CatalogSet};

/// This catalogue's state: the deployment's answer, falling back to the file's.
pub fn state_of(states: &ReleaseStates, catalog: &Catalog) -> CatalogStatus {
    states.state_of(&catalog.id, catalog.shipped_status())
}

/// The catalogue currently being collected, if any.
///
/// `None` is a legal answer and not a broken instance: an expansion that has
/// ended while its successor is still a `draft_ptr` has nothing active.
pub fn active<'a>(catalogs: &'a CatalogSet, states: &ReleaseStates) -> Option<&'a Catalog> {
    catalogs
        .catalogs
        .iter()
        .find(|c| state_of(states, c).is_active())
}

/// A catalogue by id, for anybody.
///
/// `None` for a `draft_ptr` one. This is the single gate §8's
/// "administrator-only" rests on, which is why it is one function rather than
/// a check each route remembers -- the same reasoning §7 applies to the
/// operations pages, and for the same reason.
pub fn public<'a>(
    catalogs: &'a CatalogSet,
    states: &ReleaseStates,
    id: &str,
) -> Option<&'a Catalog> {
    catalogs
        .by_id(id)
        .filter(|c| state_of(states, c).is_public())
}

/// Every catalogue a visitor may see, in display order.
pub fn public_all<'a>(catalogs: &'a CatalogSet, states: &ReleaseStates) -> Vec<&'a Catalog> {
    catalogs
        .ordered_by(|c| state_of(states, c))
        .into_iter()
        .filter(|c| state_of(states, c).is_public())
        .collect()
}

/// Every catalogue, in display order. For `/admin`, which is the one place a
/// `draft_ptr` catalogue is visible at all.
pub fn all<'a>(catalogs: &'a CatalogSet, states: &ReleaseStates) -> Vec<&'a Catalog> {
    catalogs.ordered_by(|c| state_of(states, c))
}
