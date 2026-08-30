//! The intervals a market is described over.
//!
//! Precomputed rather than sliced during a request: `docs/market-analysis.md`
//! §6 lists the set, and CLAUDE.md §16's Phase 2 makes materialising them the
//! condition for a page never reducing a history again.
//!
//! Two families, and they are separate because they answer different
//! questions. A **rolling** window -- the last 24 hours, the last 30 days --
//! is what a card compares today's price against, and every card offers all
//! five because which one is used is the reader's own choice, remembered in a
//! cookie. A **named** window -- this patch, this tier, the expansion, all of
//! it -- is a period the game defines, and it is what an archive is browsed
//! by. Neither is derivable from the other: 30 days is not a patch, and a
//! patch that ran for six weeks is not 30 days.

use std::fmt;

use cluster_core::Millis;

use super::catalog::Catalog;

const HOUR_MS: u64 = 60 * 60 * 1000;
const DAY_MS: u64 = 24 * HOUR_MS;

/// One interval a market is summarised over.
///
/// `Patch` and `Tier` carry an id rather than a date range: §8 keeps them as
/// independent keys, and a window that stored "2026-08-11 to 2026-11-03" would
/// be silently wrong the moment a patch date was corrected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Window {
    /// A rolling window of whole days, anchored at `now`. The five the cards
    /// offer.
    Days(u64),
    /// This patch, by its `patch` string.
    Patch(String),
    /// This raid tier, by its slug.
    Tier(String),
    /// The expansion so far: from its first patch to now.
    Expansion,
    /// Everything ever recorded for this market, whatever expansion it was in.
    All,
}

/// The rolling windows every market gets, which are the choices a card offers
/// (`prefs::BASELINE_CHOICES`). Kept in step by a test rather than by memory.
pub const ROLLING_DAYS: [u64; 5] = [1, 3, 7, 14, 30];

impl Window {
    /// Every window that does not depend on a catalogue.
    pub fn universal() -> Vec<Window> {
        let mut all: Vec<Window> = ROLLING_DAYS.iter().map(|d| Window::Days(*d)).collect();
        all.push(Window::Expansion);
        all.push(Window::All);
        all
    }

    /// Every window a market in this catalogue is summarised over.
    ///
    /// One per patch and one per tier, because the analysis page shows a row
    /// for each and §16 forbids a handler calculating those columns.
    pub fn all_for(catalog: &Catalog) -> Vec<Window> {
        let mut all = Window::universal();
        all.extend(
            catalog
                .patches
                .iter()
                .map(|p| Window::Patch(p.patch.clone())),
        );
        all.extend(
            catalog
                .raid_tiers
                .iter()
                .map(|t| Window::Tier(t.id.clone())),
        );
        all
    }

    /// The half-open interval `[from, until)` this window covers.
    ///
    /// `until` is `None` for a window that runs up to now -- the rolling ones,
    /// the current patch, the expansion. A closed one belongs to a period the
    /// game has finished with.
    pub fn bounds(&self, catalog: &Catalog, now: Millis) -> Option<(Millis, Option<Millis>)> {
        match self {
            Window::Days(days) => Some((Millis(now.get().saturating_sub(days * DAY_MS)), None)),
            Window::Patch(patch) => catalog
                .patch_windows()
                .into_iter()
                .find(|(p, _, _)| &p.patch == patch)
                .map(|(_, from, until)| (from, until)),
            Window::Tier(id) => {
                let tier = catalog.raid_tiers.iter().find(|t| &t.id == id)?;
                // A tier runs until the next one opens, and the last one runs
                // on. Derived from the open dates for the same reason patch
                // windows are: contiguous by construction, and adding a tier
                // is one edit rather than two.
                let next = catalog
                    .raid_tiers
                    .iter()
                    .map(|t| t.opened_at())
                    .filter(|at| *at > tier.opened_at())
                    .min();
                Some((tier.opened_at(), next))
            }
            Window::Expansion => Some((catalog.span_start(), None)),
            Window::All => Some((Millis::ZERO, None)),
        }
    }

    /// How many observations a complete window would hold.
    ///
    /// Snapshots are generated hourly, so an hour is the bucket. `None` for a
    /// window with no start we can date -- there is nothing to be a fraction
    /// of. This is what makes coverage a number rather than an impression:
    /// §5.3 wants expected and observed buckets, not a sample count that
    /// cannot say what it is a count out of.
    pub fn expected_buckets(&self, catalog: &Catalog, now: Millis) -> Option<u32> {
        let (from, until) = self.bounds(catalog, now)?;
        if from == Millis::ZERO {
            return None;
        }
        let end = until.unwrap_or(now);
        Some((end.get().saturating_sub(from.get()) / HOUR_MS) as u32)
    }

    /// The stored form. Short, because it is a key column on every row.
    pub fn key(&self) -> String {
        self.to_string()
    }

    pub fn parse(raw: &str) -> Option<Window> {
        match raw {
            "expansion" => Some(Window::Expansion),
            "all" => Some(Window::All),
            _ => {
                if let Some(days) = raw.strip_suffix('d') {
                    return days.parse().ok().map(Window::Days);
                }
                if let Some(patch) = raw.strip_prefix("patch:") {
                    return (!patch.is_empty()).then(|| Window::Patch(patch.to_string()));
                }
                if let Some(tier) = raw.strip_prefix("tier:") {
                    return (!tier.is_empty()).then(|| Window::Tier(tier.to_string()));
                }
                None
            }
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Window::Days(days) => write!(f, "{days}d"),
            Window::Patch(patch) => write!(f, "patch:{patch}"),
            Window::Tier(tier) => write!(f, "tier:{tier}"),
            Window::Expansion => f.write_str("expansion"),
            Window::All => f.write_str("all"),
        }
    }
}
