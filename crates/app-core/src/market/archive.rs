//! The public archive: expansion -> patch -> raid tier -> market analysis.
//!
//! `docs/market-analysis.md` §8 gives the hierarchy and one rule about it that
//! is easy to lose:
//!
//! ```text
//! Expansion
//! └── Patch
//!     └── Raid / tier
//!         └── market and item analysis
//! ```
//!
//! > Patch and raid/tier are stored separately even when the current content
//! > maps one-to-one; that relationship must not be baked into keys.
//!
//! So a tier is looked up **by its own id**, and it *names* the patch it
//! opened in rather than being addressed as "that patch's second tier". A
//! patch that opened no raid is a patch with no tiers, not a missing row --
//! 12.0.5 is exactly that.
//!
//! ## Why this is derived rather than stored
//!
//! Everything here already exists: a [`Catalog`] carries its expansion, its
//! patches and its tiers, and [`crate::market::Window`] already materialises
//! `patch:12.1` and `tier:venomous-abyss` for every market. This module adds
//! no statistic and no table. It is the *navigation* over what Phases 1, 2 and
//! 6 already record, which is why Phase 9 can be a data operation: adding a
//! tier is an edit to `catalogs.json`, and the pages under it appear because
//! the hierarchy is read from the file rather than written by hand.
//!
//! ## A tier rollover ships a new catalogue
//!
//! §8: "New tiers introduce a new active catalogue; the former active BoE tier
//! stops collecting automatically and becomes a read-only archive." So one
//! expansion can span several catalogues, and the hierarchy is built across
//! all of them rather than out of one. That is what [`Archive::of`] takes a
//! list.
//!
//! It also creates the one trap in the arrangement, which [`Archive::problems`]
//! is here to catch. A tier's window runs until the *next tier its own
//! catalogue knows about*, so a catalogue that ships one tier and is then
//! superseded has a tier window with no end -- and it would go on absorbing
//! the successor's prices for ever. The fix is a data edit (the new catalogue
//! carries the expansion's whole tier list), which is why the check reads as a
//! sentence an administrator can act on rather than as an error.

use std::collections::BTreeMap;

use cluster_core::Millis;

use super::catalog::{Catalog, RaidTier};

/// Every expansion a visitor may browse, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Archive {
    pub expansions: Vec<ArchivedExpansion>,
}

/// One expansion, and the patches under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedExpansion {
    /// URL slug, derived from the name: "Midnight" -> `midnight`.
    pub slug: String,
    pub name: String,
    /// The catalogues that carry it, newest first. Several after a tier
    /// rollover.
    pub catalogs: Vec<String>,
    /// True while any of them is still being collected.
    pub collecting: bool,
    /// The first patch's start.
    pub from: Millis,
    /// `None` while it is the current expansion.
    pub until: Option<Millis>,
    /// Newest first, which is the order a reader wants: the archive is browsed
    /// backwards from what just happened.
    pub patches: Vec<ArchivedPatch>,
}

/// One patch, and the tiers it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedPatch {
    /// The key: "12.1". Independent of any tier's id (§8).
    pub patch: String,
    pub name: String,
    pub started: Millis,
    /// When the next patch shipped. `None` for the current one.
    pub until: Option<Millis>,
    /// The catalogues declaring it, newest first.
    pub catalogs: Vec<String>,
    /// Oldest first: a patch rarely opens more than one, and when it does they
    /// happened in an order.
    pub tiers: Vec<ArchivedTier>,
}

/// One raid tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedTier {
    /// The slug the catalogue gave it. Stable, and in the URL.
    pub id: String,
    pub name: String,
    /// The patch it opened in, by key. A field rather than a parent pointer,
    /// because §8 keeps the two independent.
    pub patch: String,
    pub opened: Millis,
    /// When the next tier of the same expansion opened. `None` for the current
    /// one.
    pub until: Option<Millis>,
    pub season: Option<u8>,
    /// The catalogue that **opened** this tier, and whose bind-on-equip list
    /// therefore *is* this tier.
    ///
    /// Not simply "a catalogue that declares it". A rollover catalogue restates
    /// its predecessor's tiers -- it has to, or the older one's window never
    /// closes -- so after one, two catalogues declare the archived tier and
    /// only one of them holds its gear. Taking the wrong one put the *new*
    /// season's bind-on-equip pieces on the *archived* tier's page, which is a
    /// page that renders perfectly and is about the wrong raid.
    pub catalog: String,
    /// Every catalogue declaring it, newest first.
    ///
    /// A different question from [`Self::catalog`], and [`Archive::problems`]
    /// is why the two are kept apart: what closes a tier's stored window is
    /// whether the catalogue whose tier list is being read declares the tier
    /// after it.
    pub declared_by: Vec<String>,
}

impl Archive {
    /// Build the hierarchy from the catalogues a visitor may see.
    ///
    /// The caller decides who may see what -- [`crate::Ports::public_catalogs`]
    /// is the gate, and a `draft_ptr` catalogue never reaches here. Passing one
    /// in would put an unreleased patch in the public navigation, which is the
    /// one thing §8 says this must not do.
    pub fn of<'a>(
        catalogs: impl IntoIterator<Item = &'a Catalog>,
        collecting: impl Fn(&Catalog) -> bool,
    ) -> Archive {
        // Grouped by the expansion's *name*, because that is what a reader
        // means by "Midnight" and a rollover gives it a second catalogue id.
        let mut by_expansion: BTreeMap<String, Vec<&Catalog>> = BTreeMap::new();
        for catalog in catalogs {
            by_expansion
                .entry(catalog.expansion.clone())
                .or_default()
                .push(catalog);
        }

        let mut expansions: Vec<ArchivedExpansion> = by_expansion
            .into_iter()
            .map(|(name, mut mine)| {
                // Newest catalogue first, everywhere a list of them is shown.
                // By its *last* patch rather than its first: a rollover
                // catalogue restates the expansion from 12.0, so they share a
                // span start and only the newest patch separates them.
                mine.sort_by(|a, b| {
                    latest_patch(b)
                        .cmp(&latest_patch(a))
                        .then_with(|| b.raid_tiers.len().cmp(&a.raid_tiers.len()))
                        .then_with(|| a.id.cmp(&b.id))
                });
                expansion(name, &mine, &collecting)
            })
            .collect();

        // Newest expansion first, and the one still collecting always at the
        // top: it is the one nearly every visitor came for.
        expansions.sort_by(|a, b| {
            b.collecting
                .cmp(&a.collecting)
                .then_with(|| b.from.cmp(&a.from))
                .then_with(|| a.slug.cmp(&b.slug))
        });

        // Close each finished expansion at the start of the one after it, so a
        // reader is told when it ended rather than left to infer it. Done over
        // the chronological order rather than the display order above.
        let mut order: Vec<usize> = (0..expansions.len()).collect();
        order.sort_by_key(|i| expansions[*i].from);
        for pair in order.windows(2) {
            let (earlier, later) = (pair[0], pair[1]);
            let ends = expansions[later].from;
            expansions[earlier].until = Some(ends);
        }

        Archive { expansions }
    }

    /// One expansion by slug.
    ///
    /// **The first half of every archive path.** §16's Phase 9 asks for the
    /// expansion to be validated first and the patch second, so a route
    /// resolves in that order and a patch is only ever looked for inside an
    /// expansion that exists. A patch key that belongs to another expansion is
    /// then a 404 rather than a page about the wrong thing.
    pub fn expansion(&self, slug: &str) -> Option<&ArchivedExpansion> {
        self.expansions.iter().find(|e| e.slug == slug)
    }

    /// What is wrong across the whole archive, as sentences.
    ///
    /// One check, and it is the one a tier rollover can get wrong silently.
    /// [`crate::market::Window::Tier`] ends a tier at the next tier **its own
    /// catalogue** declares, because that is the only tier list it has. So a
    /// catalogue shipping one tier and superseded by another has a tier window
    /// with no end, which goes on absorbing its successor's prices -- a
    /// statistic that is wrong rather than absent, and the kind nobody spots
    /// because it still renders.
    ///
    /// The fix is a data edit: the catalogue that opens a new tier carries the
    /// expansion's whole tier list. Hence a sentence rather than an error.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for expansion in &self.expansions {
            let mut tiers: Vec<&ArchivedTier> =
                expansion.patches.iter().flat_map(|p| &p.tiers).collect();
            tiers.sort_by_key(|t| t.opened);
            for pair in tiers.windows(2) {
                let (tier, next) = (pair[0], pair[1]);
                // The newest catalogue declaring this tier is the one whose
                // tier list a live market's window is computed from, so it is
                // the one that has to know what came next.
                let Some(reader) = tier.declared_by.first() else {
                    continue;
                };
                if next.declared_by.iter().any(|id| id == reader) {
                    continue;
                }
                problems.push(format!(
                    "catalogue {reader:?} declares raid tier {:?} but not the tier after it, \
                     {:?}: it has no end for {:?}, so the stored tier window runs on through \
                     {:?}. Add {:?} to {reader:?} as well.",
                    tier.id, next.id, tier.id, next.id, next.id,
                ));
            }
        }
        problems
    }
}

impl ArchivedExpansion {
    /// One patch by key, **within this expansion**.
    ///
    /// The second half of the rule above: `/wow/archive/midnight/13.0` is a
    /// 404 even when 13.0 is a real patch, because it is not this expansion's.
    pub fn patch(&self, key: &str) -> Option<&ArchivedPatch> {
        self.patches.iter().find(|p| p.patch == key)
    }

    /// Every tier of the expansion, newest first. For a picker that does not
    /// care which patch opened them.
    pub fn tiers(&self) -> Vec<&ArchivedTier> {
        let mut all: Vec<&ArchivedTier> = self.patches.iter().flat_map(|p| &p.tiers).collect();
        all.sort_by_key(|t| std::cmp::Reverse(t.opened));
        all
    }
}

impl ArchivedPatch {
    /// One tier by its own id, within this patch.
    pub fn tier(&self, id: &str) -> Option<&ArchivedTier> {
        self.tiers.iter().find(|t| t.id == id)
    }

    /// The catalogue to open the market pages under: the newest that declares
    /// this patch.
    pub fn catalog(&self) -> &str {
        self.catalogs
            .first()
            .map(String::as_str)
            .unwrap_or_default()
    }
}

/// One expansion's patches and tiers, merged across its catalogues.
fn expansion(
    name: String,
    catalogs: &[&Catalog],
    collecting: &impl Fn(&Catalog) -> bool,
) -> ArchivedExpansion {
    // Patches merged by key, over a newest-first list of catalogues, so the
    // newest declaration wins for the name and the date. They should not
    // disagree -- a rollover catalogue restating a shipped patch is repeating
    // it, not correcting it -- and where one does, the live catalogue is the
    // one whose windows are being materialised, so it is the one the page
    // should agree with.
    let mut patches: BTreeMap<String, ArchivedPatch> = BTreeMap::new();
    for catalog in catalogs {
        for patch in &catalog.patches {
            let entry = patches
                .entry(patch.patch.clone())
                .or_insert_with(|| ArchivedPatch {
                    patch: patch.patch.clone(),
                    name: patch.name.clone(),
                    started: patch.started_at(),
                    until: None,
                    catalogs: Vec::new(),
                    tiers: Vec::new(),
                });
            if !entry.catalogs.iter().any(|id| id == &catalog.id) {
                entry.catalogs.push(catalog.id.clone());
            }
        }
    }

    // Tiers, filed under the patch they name. A tier naming a patch this
    // expansion does not have is dropped here and reported by
    // `Catalog::problems`, which is the place that says *why*.
    let mut tiers: Vec<(&RaidTier, &str)> = catalogs
        .iter()
        .flat_map(|c| c.raid_tiers.iter().map(move |t| (t, c.id.as_str())))
        .collect();
    // Stable, over a newest-first list of catalogues, so a tier restated by a
    // rollover catalogue keeps the newest declaration -- which is the one
    // whose `Window::Tier` bound closes at the right moment.
    tiers.sort_by_key(|(t, _)| t.opened_at());
    {
        let mut seen: Vec<&str> = Vec::new();
        tiers.retain(|(t, _)| {
            let first = !seen.contains(&t.id.as_str());
            if first {
                seen.push(t.id.as_str());
            }
            first
        });
    }

    // Every tier of the expansion in order, so each one can be closed at the
    // next -- across catalogues, because that is when the raid actually
    // stopped being current. Deduplicated first: a tier restated by two
    // catalogues would otherwise close itself on its own opening day.
    let opens: Vec<Millis> = tiers.iter().map(|(t, _)| t.opened_at()).collect();
    for (i, (tier, _)) in tiers.iter().enumerate() {
        let Some(patch) = patches.get_mut(&tier.patch) else {
            continue;
        };
        // Newest first, matching the order the catalogues arrived in.
        let declared_by: Vec<String> = catalogs
            .iter()
            .filter(|c| c.raid_tiers.iter().any(|t| t.id == tier.id))
            .map(|c| c.id.clone())
            .collect();
        patch.tiers.push(ArchivedTier {
            id: tier.id.clone(),
            name: tier.name.clone(),
            patch: tier.patch.clone(),
            opened: tier.opened_at(),
            until: opens.get(i + 1).copied(),
            season: tier.season,
            catalog: opener(catalogs, tier),
            declared_by,
        });
    }

    let mut patches: Vec<ArchivedPatch> = patches.into_values().collect();
    patches.sort_by_key(|p| p.started);
    // Contiguous by construction, exactly as `Catalog::patch_windows` builds
    // them -- and for the same reason: a stored end date would be wrong the
    // moment a start date was corrected.
    for i in 0..patches.len() {
        patches[i].until = patches.get(i + 1).map(|p| p.started);
    }
    for patch in &mut patches {
        patch.tiers.sort_by_key(|t| t.opened);
    }
    let from = patches.first().map(|p| p.started).unwrap_or(Millis::ZERO);
    patches.reverse();

    ArchivedExpansion {
        slug: slug(&name),
        name,
        catalogs: catalogs.iter().map(|c| c.id.clone()).collect(),
        collecting: catalogs.iter().any(|c| collecting(c)),
        from,
        until: None,
        patches,
    }
}

/// Which catalogue opened this tier -- and therefore whose bind-on-equip list
/// it is.
///
/// The one whose *latest* tier this is. A rollover catalogue restates its
/// predecessor's tiers, so several may declare it, and only the one that
/// opened it holds the gear the raid dropped. The fallback is the oldest that
/// declares it, which is the same answer whenever a catalogue was superseded
/// without its successor restating anything.
fn opener(catalogs: &[&Catalog], tier: &RaidTier) -> String {
    let declaring = || {
        catalogs
            .iter()
            .filter(|c| c.raid_tiers.iter().any(|t| t.id == tier.id))
    };
    declaring()
        .find(|c| c.current_tier().is_some_and(|latest| latest.id == tier.id))
        .or_else(|| declaring().next_back())
        .map(|c| c.id.clone())
        .unwrap_or_default()
}

/// The start of the last patch a catalogue declares.
///
/// What "newest catalogue" means once one expansion spans several of them:
/// they all begin at the expansion's launch, so the first patch cannot tell
/// them apart and the last one always can.
fn latest_patch(catalog: &Catalog) -> Millis {
    catalog
        .patches
        .iter()
        .map(|p| p.started_at())
        .max()
        .unwrap_or(Millis::ZERO)
}

/// A URL slug from a display name.
///
/// ASCII-folded and hyphenated: "Midnight" -> `midnight`, "The War Within" ->
/// `the-war-within`. Non-ASCII is dropped rather than transliterated, and a
/// name that leaves nothing behind keeps its catalogue reachable through the
/// expansion picker instead -- guessing a slug for it would be worse than
/// having none.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}
