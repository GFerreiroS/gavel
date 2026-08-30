//! How much of a category arrives in the first response.
//!
//! `docs/market-analysis.md` §15: "Use it according to the amount of useful
//! content. A small page such as the nine-card Gear view should inline its
//! already materialised cards in the first response. A large category should
//! render a small useful first group and load later groups as they approach
//! the viewport."
//!
//! So the rule is size, and it lives here rather than being decided per page,
//! because a reader who learns a page's behaviour should find the same one
//! next door. The threshold is the whole of the policy: below it, the page is
//! one response; above it, the first group is one response and the rest arrive
//! as they are scrolled to. Search is always complete either way -- it runs
//! against the read model, so a search that matched a deferred group would
//! otherwise show nothing.

use askama::Template;

use crate::i18n::filters;

use crate::views::{CardGroup, GearGroup};

/// Cards above which a category defers its later groups.
///
/// Chosen from what the fragments actually weigh on the real archive: the
/// pages below it are 6 to 54 KB, the pages above it are 142 to 423 KB. It is
/// a round number in that gap rather than a tuned one, and moving it is a
/// judgement about page weight rather than a bug fix.
const INLINE_LIMIT: usize = 40;

/// A section of a category page that can arrive on its own.
///
/// Two shapes of group -- the commodity card's and the gear card's -- and one
/// rule about when they arrive. The trait is what keeps the rule from being
/// written twice and drifting, which §7 is emphatic about.
pub(crate) trait Deferrable {
    fn card_count(&self) -> usize;
    fn slug(&self) -> &str;
    fn defer_to(&mut self, href: String);
}

impl Deferrable for CardGroup {
    fn card_count(&self) -> usize {
        self.cards.len()
    }
    fn slug(&self) -> &str {
        self.audience
    }
    fn defer_to(&mut self, href: String) {
        self.deferred = true;
        self.href = href;
    }
}

impl Deferrable for GearGroup {
    fn card_count(&self) -> usize {
        self.cards.len()
    }
    fn slug(&self) -> &str {
        self.anchor
    }
    fn defer_to(&mut self, href: String) {
        self.deferred = true;
        self.href = href;
    }
}

/// Inline groups until the budget is spent, and defer the rest.
///
/// A *budget* rather than "the first group", because the first group is
/// whatever the alphabet put there. On the reagents page it is Alchemy, which
/// has two cards: deferring everything after it would have delivered a first
/// response that answers nothing and thirteen sections that all have to be
/// scrolled to. §15 asks for a small *useful* first group, and useful is a
/// number of cards rather than a number of headings.
///
/// `href` builds the URL a group fetches itself from: the same fragment
/// endpoint the page already has, plus the group's own slug.
pub(crate) fn defer<G: Deferrable>(
    groups: &mut [G],
    searching: bool,
    href: impl Fn(&str) -> String,
) {
    // A search result is small and is the thing the reader asked for. Making
    // them scroll to see whether their match was in the third group would be
    // a page that hid the answer to its own question.
    if searching {
        return;
    }
    if groups.iter().map(G::card_count).sum::<usize>() <= INLINE_LIMIT {
        return;
    }

    let mut inlined = 0;
    for group in groups.iter_mut() {
        // The first group is always inlined whatever its size, or a page whose
        // opening section is larger than the budget would arrive empty.
        if inlined == 0 || inlined + group.card_count() <= INLINE_LIMIT {
            inlined += group.card_count();
            continue;
        }
        let target = href(group.slug());
        group.defer_to(target);
    }
}

/// Keep one group and drop the cards from the rest.
///
/// What the single-group endpoint answers with. The other groups are still
/// built -- they are rows from the read model, not a reduction -- and throwing
/// their cards away here rather than not building them keeps one code path
/// producing the page.
pub(crate) fn only<G: Deferrable>(groups: Vec<G>, wanted: &str) -> Option<G> {
    groups.into_iter().find(|g| g.slug() == wanted)
}

/// One group, rendered alone.
///
/// The response a deferred group's own request gets. It renders through the
/// same macro the whole-category fragment does, so a group looks the same
/// whichever way it arrived.
#[derive(Template)]
#[template(path = "partials/card_group.html")]
pub(crate) struct CardGroupFragment {
    pub group: CardGroup,
    pub baseline_days: u64,
    /// The prefix the page's section anchors use: `profession`, `slot`,
    /// `group`. It has to match, or a deferred group would arrive with a
    /// different id from the one the page linked to.
    pub heading_id: &'static str,
    /// The page's own sentence under the heading, already translated. Empty
    /// where the page has none.
    pub note: String,
}
