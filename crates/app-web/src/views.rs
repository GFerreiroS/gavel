//! View models.
//!
//! The templates never see a `Node`, a `Job` or a Raider.IO response. Every
//! value here is already formatted, so the presentation layer cannot drift
//! into depending on internal cluster structures.

use app_core::WebConfig;
use app_core::item::ItemTooltip;
use app_core::locale::{ALL_LOCALES, Locale};
use app_core::market::Region;
use app_core::model::User;
use app_core::wow::Character;
use axum::http::Uri;
use cluster_core::{
    ALL_ROLES, ClusterSnapshot, EventRecord, Job, JobDetail, Millis, Node, RolePolicies, Task,
    TaskAttempt,
};

use crate::format;

#[derive(Debug, Clone)]
pub struct NavItem {
    pub label: &'static str,
    pub href: &'static str,
    pub active: bool,
}

/// Everything `base.html` needs. Every page view embeds one.
#[derive(Debug, Clone)]
pub struct Layout {
    /// BCP-47 tag for the `<html lang>` attribute: the language the interface
    /// is being rendered in.
    pub lang: &'static str,
    pub title: String,
    pub app_name: String,
    pub nav: Vec<NavItem>,
    pub signed_in: bool,
    pub username: String,
    /// Slow safety net for when the SSE stream is unavailable. Updates
    /// normally arrive as a `cluster-changed` event pushed over SSE.
    pub fallback_poll_ms: u64,
    pub csrf: String,
    pub debug_controls: bool,
    /// Content hashes, so a rebuilt asset is a new URL rather than a stale
    /// cache entry.
    pub pico_version: &'static str,
    pub css_version: &'static str,
    pub htmx_version: &'static str,
    pub live_version: &'static str,
    /// The language menu in the top bar. Language is a site-wide setting --
    /// unlike region, which only means anything on the market pages.
    pub languages: Vec<LanguageLink>,
    pub language_label: &'static str,
}

/// The Auction House index: choose an expansion and a region once, see what
/// that combination holds, then pick a category.
///
/// The choice lives here rather than on each category page because it is the
/// same choice for all of them -- picking a region separately per category is
/// how you end up comparing two different markets without noticing.
#[derive(Debug, Clone)]
pub struct AuctionsView {
    /// Expansion and region, in that order.
    pub picker: MarketPicker,
    pub expansion: String,
    pub region: String,
    /// True when viewing a finished expansion: read-only, never updated again.
    pub archived: bool,
    /// Items tracked across every category of the selected expansion.
    pub tracked_items: usize,
    /// How many of them currently have a price recorded for this region.
    pub samples_held: usize,
    pub last_observed: String,
    /// The "vs usual" window, and the alternatives on offer. The choice lives
    /// here because it applies to every category below, exactly like the
    /// expansion and the region do.
    pub baseline_days: u64,
    pub baselines: Vec<BaselineOption>,
    pub categories: Vec<AuctionCategory>,
}

/// One window on the baseline picker.
#[derive(Debug, Clone)]
pub struct BaselineOption {
    pub days: u64,
    /// A source string, translated by the template.
    pub label: &'static str,
    pub selected: bool,
}

/// One tracking category on the auction-house index.
///
/// Every category lives under the one Auction House tab rather than getting a
/// nav entry of its own: the nav does not survive one tab per category.
#[derive(Debug, Clone)]
pub struct AuctionCategory {
    pub href: String,
    /// Source strings, translated in the template.
    pub name: &'static str,
    pub summary: &'static str,
    /// What the market is dimensioned by -- region-wide, or per realm.
    pub scope: &'static str,
    pub tracked_items: usize,
    pub live: bool,
    /// A key into the `macros.html` `icon` macro, not a display string --
    /// matching on `name` instead would tie the icon to translated text.
    pub icon: &'static str,
}

/// The reagents page: every tracked crafting material, by profession.
///
/// Same cards as the consumables page -- an item is an item, and the figures
/// worth showing are the same -- grouped by profession instead of raid role,
/// and searchable because there are 223 of them rather than 26.
#[derive(Debug, Clone)]
pub struct ReagentsView {
    /// Where the cards come from, with the current choices already in it.
    pub fragment_href: String,
    pub expansion: String,
    /// The catalog id behind `expansion`, so the page's own form can carry the
    /// choice back rather than silently falling back to the live expansion.
    pub expansion_id: String,
    pub archived: bool,
    /// What the visitor typed, echoed back into the search box.
    pub query: String,
    pub total: usize,
    pub matched: usize,
    /// Age of the snapshot every card on the page was priced from. One line
    /// for the page: the figures all come from the same collection cycle, so
    /// repeating it on each card was the same sentence a hundred times.
    pub observed: String,
    /// The window the +/- percentage compares against, chosen on the index.
    pub baseline_days: u64,
    pub groups: Vec<CardGroup>,
}

/// The enchants and gems pages: one grid of cards, optionally under headings.
///
/// Shared by both because the two pages differ only in their wording and in
/// whether the cards divide into sections. See `routes::enhancements`.
#[derive(Debug, Clone)]
pub struct EnhancementsView {
    /// Where the cards come from, with the current choices already in it.
    pub fragment_href: String,
    /// Source strings, translated by the template.
    pub title: &'static str,
    /// Takes the expansion name.
    pub blurb: &'static str,
    /// Takes the tracked count; used when nothing has been searched for.
    pub counted: &'static str,
    /// Takes the matched count and the tracked count.
    pub matched_of: &'static str,
    /// Where the page's own form submits, and where the search box fetches.
    pub path: &'static str,
    pub fragment_path: &'static str,
    /// False renders one flat grid with no headings, which is what a page of
    /// sixteen gems wants.
    pub grouped: bool,
    pub expansion: String,
    pub expansion_id: String,
    pub archived: bool,
    pub query: String,
    pub total: usize,
    pub matched: usize,
    pub observed: String,
    pub baseline_days: u64,
    pub groups: Vec<CardGroup>,
}

/// The gear page: bind-on-equip items, priced per connected realm.
///
/// Two modes in one view model. With no realm chosen it shows every region
/// side by side, each summarising its realms; with one chosen it shows that
/// realm alone. The card and its tiers are the same either way, which is why
/// this is one page rather than two.
#[derive(Debug, Clone)]
pub struct GearView {
    /// The chosen realm's own name, echoed into the picker. Empty for the
    /// cross-realm view.
    pub realm_name: String,
    /// The region these prices are from, as the reader would write it: "EU".
    pub region_label: String,
    /// `gear` or `recipes`: which page the realm picker's links lead back to.
    pub kind: &'static str,
    /// False when this region has no connected realms configured at all,
    /// which is a deployment to fix rather than an empty page to explain.
    pub has_realms: bool,
    pub expansion: String,
    pub expansion_id: String,
    pub archived: bool,
    /// Age of the newest realm snapshot on the page.
    pub observed: String,
    /// Every collected realm, for the picker. Empty when none is configured.
    /// What the picker currently reads. `None` is "all realms".
    pub realm_label: Option<String>,
    /// Goes in the URL beside the realm slug, so a link reads
    /// `?region=eu&realm=draenor`.
    pub region: &'static str,
    /// The chosen realm as a slug, carried into links off this page.
    pub realm_slug: String,
    /// Page wording, which differs between gear and recipes.
    pub title: &'static str,
    pub blurb: &'static str,
    pub path: &'static str,
    pub fragment_path: &'static str,
    /// Whether the items on this page have upgrade levels at all. Recipes do
    /// not, and explaining a ladder they do not have would be noise.
    pub leveled: bool,
    /// Whether this page offers a search box. A grid of nine gear cards does
    /// not need one; a hundred and thirty recipes do.
    pub searchable: bool,
    /// What the visitor typed, echoed back into the box.
    pub query: String,
    /// One realm chosen means one column of figures per card, which fits the
    /// ordinary card width -- the same grid the consumable and reagent pages
    /// use. Two regions side by side need the wider one.
    pub compact: bool,
    /// One nameless group on the gear page; one per profession on recipes.
    pub groups: Vec<GearGroup>,
}

/// A headed run of cards. The heading is empty when the page does not divide.
#[derive(Debug, Clone)]
pub struct GearGroup {
    pub label: &'static str,
    pub anchor: &'static str,
    pub cards: Vec<GearCard>,
    /// Rendered as a heading and a count, with its cards fetched when the
    /// reader scrolls to it. See [`CardGroup::deferred`].
    pub deferred: bool,
    pub href: String,
}

/// One connected realm on the picker.
#[derive(Debug, Clone)]
pub struct RealmOption {
    /// One realm's own name -- "Sargeras", not "Garona, Sargeras, Ner'zhul".
    ///
    /// Several of these share an auction house and all resolve to the same
    /// market. Listing them separately is what lets a player find the realm
    /// they play on instead of the joined name Blizzard filed it under; which
    /// others come with it is said in the note under the picker, once a choice
    /// has been made, rather than in every line of the list.
    pub name: String,
}

/// One tracked item: a row per item level, a cell per region (or the chosen
/// realm).
///
/// Rows rather than columns, so the same item level is the same row across
/// every region and the card cannot drift out of alignment when one side has
/// an extra line of detail. The level is named once above its cells instead of
/// repeated in each of them.
#[derive(Debug, Clone)]
pub struct GearCard {
    pub name: String,
    pub icon: Option<String>,
    pub tooltip_item_id: u32,
    pub tooltip: Option<TooltipView>,
    /// The equipment slot, as its own line under the name.
    pub slot: &'static str,
    /// Blizzard's own subclass -- "Alchemy" on a recipe -- shown where there
    /// is no slot, so every card in the app has a line under its name.
    pub material: Option<String>,
    /// Column headings: "EU" and "US", or one realm's name.
    pub scopes: Vec<String>,
    pub tracks: Vec<GearTrackRow>,
    /// True when nothing is listed anywhere we look. The card still appears:
    /// "nobody is selling one" is an answer, and it is a different answer
    /// from "we do not track this".
    pub unlisted: bool,
}

/// One upgrade track of an item.
///
/// A row per *track*, not per item level. A card used to carry one row for
/// every rank the market held -- eight of them, "ilvl 279 · Veteran 1/6" and
/// so on down -- which is eight markets nobody prices separately and a card
/// too tall to compare with its neighbour. The track is the choice a buyer
/// actually makes; the ranks inside it are a range, and the statistics page
/// is where they are worth breaking apart.
#[derive(Debug, Clone)]
pub struct GearTrackRow {
    /// The track's English name, translated in the template. Empty for a
    /// recipe, which has one version of itself and no track at all.
    pub track: &'static str,
    /// The item levels listed in this track: "279" or "279–285". Empty when
    /// the market holds none, or when the catalog cannot name them yet.
    pub levels: String,
    /// False for recipes, which would be lied about by a track heading.
    pub leveled: bool,
    /// Its own statistics page. Empty when there is nothing to show there.
    pub href: String,
    /// Whether any scope has this track listed. A track nobody is selling
    /// still gets its row -- that is what keeps the grid square -- but it
    /// must not offer a button to a page with nothing on it.
    pub listed: bool,
    pub cells: Vec<GearCell>,
}

/// One item level's figures in one region, or on one realm.
#[derive(Debug, Clone, Default)]
pub struct GearCell {
    /// False renders a placeholder, which keeps the row aligned and says
    /// plainly that this region has none.
    pub listed: bool,
    /// The headline: across realms, the median of what each realm's cheapest
    /// copy costs; on one realm, that realm's cheapest.
    pub price: String,
    /// The cheapest and dearest copy. Across realms these name a realm;
    /// on one realm they are the spread within it and name nothing.
    pub cheapest: GearWhere,
    pub highest: GearWhere,
    pub listings: u32,
    /// How many realms had one at all, for the cross-realm view.
    pub realms: usize,
    /// The valuation band and its percentile, from the same engine and the
    /// same estimator a commodity card uses. §7's "one name for one thing",
    /// applied to the word that matters most: `Cheap` on a gear card is the
    /// same claim about the same kind of evidence as `Cheap` on a flask.
    ///
    /// No sparkline beside it, and that is the deliberate half. A gear card
    /// is four fixed track rows of cells (§7's "level the grid"), and four
    /// lines inside one card fight that geometry rather than serving it; the
    /// track's own analysis page draws the line.
    pub band: Option<&'static str>,
    pub band_slug: &'static str,
    pub rank_percent: Option<u8>,
    /// Sockets and tertiary stats, as counts rather than as separate markets:
    /// they change what a piece is worth without changing what it is.
    pub extras: Vec<GearExtra>,
}

#[derive(Debug, Clone, Default)]
pub struct GearWhere {
    /// Every realm sharing this auction house, for the line's `title`. Empty
    /// where there is nothing more to say than `realm` already says.
    pub realm_full: String,
    /// `None` on a single realm: there is only one place, and naming it three
    /// times a card is noise.
    pub realm: Option<String>,
    pub price: String,
}

/// One optional bonus and how many listings carry it.
///
/// The name comes from the catalog, which got it by asking what the bonus adds
/// to a rendered tooltip -- so "Leech" is what the game says, not a guess.
#[derive(Debug, Clone)]
pub struct GearExtra {
    /// "Prismatic Socket", "Leech" -- resolved from the bonus id by the sync
    /// script and stored in the catalog.
    pub name: String,
    pub listings: u32,
}

/// The statistics page for one item at one item level.
///
/// One page per (item, item level), because that is the market: a Champion 2/6
/// helm and a Hero 1/6 helm share an item id and nothing else that matters.
#[derive(Debug, Clone)]
pub struct GearStatsView {
    /// The chosen realm's own name, echoed into the picker.
    pub realm_name: String,
    /// `gear` or `recipes`: which page the realm picker's links lead to.
    pub kind: &'static str,
    pub item_id: u32,
    pub name: String,
    pub icon: Option<String>,
    pub tooltip: Option<TooltipView>,
    pub slot: &'static str,
    /// The track this page is about -- "Hero". Empty for a recipe, which has
    /// one version of itself and no track.
    pub track: &'static str,
    /// The item levels this track holds in the market: "305–311".
    pub level_range: String,
    /// The category this belongs to, for the breadcrumb.
    pub section: &'static str,
    pub section_href: &'static str,
    /// The other tracks of the same item, so the ladder is one click wide
    /// rather than a trip back to the grid.
    pub siblings: Vec<GearLevelLink>,
    /// What each item level inside this track is doing, which is the reason
    /// to open this page: the card gave the range, this gives the breakdown.
    pub levels: Vec<GearLevelStat>,
    /// What a sweep of this market costs, on the chosen realm.
    ///
    /// `None` with no realm chosen, and the page says why rather than going
    /// quiet: a sweep happens in one auction house, so a figure pooled across
    /// ninety realms would price an order nobody can fill. `None` on a realm
    /// too until ladder collection has run for it.
    pub depth: Option<DepthView>,

    /// Whose prices these are: a realm's name, or every realm.
    pub scope: Option<String>,
    /// The same realm as a slug, for links out of this page.
    pub realm_slug: String,
    pub region: &'static str,
    pub observed: String,
    /// How far back the figures reach, in days.
    pub window_days: u64,
    pub snapshots: usize,
    /// Headline prices: now, and across the window.
    pub cheapest_now: String,
    pub highest_now: String,
    pub cheapest_ever: String,
    pub highest_ever: String,
    pub listings_now: u32,
    /// **Availability**: realms with one listed, out of realms collected
    /// (§16, Phase 8). A fraction, because "listed on 40 realms" means one
    /// thing out of 45 and another out of 184 -- and without the denominator
    /// the numerator is a number the reader has to go and look up. `None` on a
    /// single realm, where the fraction is one out of one and says nothing.
    pub realms_listing: Option<u32>,
    pub realms_collected: u32,
    /// **Dispersion**: how far apart the realms are on price right now.
    /// `None` on a single realm, and on a market too thin to summarise.
    pub spread_cheapest: Option<String>,
    pub spread_median: Option<String>,
    pub spread_dearest: Option<String>,
    /// The gap between the cheapest realm and the median one, as a percentage
    /// -- what flying somewhere is worth.
    pub spread_percent: Option<u32>,
    /// Sockets and tertiaries: how many are listed now, and how many listing
    /// observations carried them across the window.
    pub modifiers: Vec<GearModifierStat>,
    pub price_chart: String,
    pub listings_chart: String,
    pub unlisted: bool,
}

/// One item level of an item, as a link from another.
#[derive(Debug, Clone)]
pub struct GearLevelLink {
    /// The track's English name, translated in the template.
    pub track: &'static str,
    pub href: String,
    pub current: bool,
}

/// One item level inside a track, and what it is doing right now.
///
/// The card shows a track as a range because that is the choice a buyer
/// makes. This is what the range is made of -- and an ilvl 311 selling for
/// less than an ilvl 305 is exactly the kind of thing worth seeing.
#[derive(Debug, Clone)]
pub struct GearLevelStat {
    pub item_level: u16,
    /// "Hero 3/6" -- the rank inside the track, as the game words it.
    pub upgrade: String,
    pub cheapest: String,
    pub highest: String,
    pub listings: u32,
    /// How many realms had one at all.
    pub realms: usize,
}

/// How common an optional bonus is, now and over the window.
#[derive(Debug, Clone)]
pub struct GearModifierStat {
    pub name: String,
    /// Listings carrying it in the newest snapshot.
    pub now: u32,
    /// Listing observations carrying it across the window. The same auction
    /// sitting unsold for six hours is six observations, so this is a measure
    /// of how common the bonus is, not of how many were sold.
    pub seen: u32,
    /// Share of all observations, as a percentage.
    pub share: u32,
}

/// The collection settings page.
#[derive(Debug, Clone)]
pub struct AdminView {
    pub releases: Vec<AdminRelease>,
    pub categories: Vec<AdminCategory>,
    pub regions: Vec<AdminRegion>,
    pub realms_enabled: usize,
    pub realms_total: usize,
    /// Every event, including the internal and the unchecked. This is the one
    /// place they are visible at all -- reviewing them is what the
    /// administrator is here to do (§16, Phase 8).
    pub events: Vec<AdminEvent>,
    /// The kinds an annotation may be filed under, for the form.
    pub event_kinds: Vec<(&'static str, &'static str)>,
    /// What is wrong *across* the catalogues, which no single one can see.
    ///
    /// One catalogue's `problems` are about its own rows; these are about the
    /// arrangement -- a tier left with no successor by a rollover, whose
    /// stored window then runs on through the tier that replaced it. Shown at
    /// the top of the release panel because the fix is an edit to a catalogue
    /// that looks perfectly coherent on its own.
    pub archive_problems: Vec<String>,
}

/// One event, as the administrator sees it.
#[derive(Debug, Clone)]
pub struct AdminEvent {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
    pub when: String,
    pub scope: String,
    pub provenance: &'static str,
    pub validation: &'static str,
    pub visibility: &'static str,
    /// Whether a reader outside this page can see it: public *and* validated,
    /// both, which is the check `MarketEvent::is_public` makes.
    pub live: bool,
    /// Only an administrator's own annotations can be removed. A patch release
    /// is re-derived from the catalogue at every start, so a delete button on
    /// one would appear not to work.
    pub removable: bool,
}

/// One catalogue and where it is in its life.
///
/// The only place a `draft_ptr` catalogue is visible at all: it is
/// administrator-only, it has no prices, and it is here so that somebody can
/// review it and decide (`docs/market-analysis.md` §8).
#[derive(Debug, Clone)]
pub struct AdminRelease {
    pub id: String,
    pub expansion: String,
    /// "Midnight 12.1 — Season 2 (The Venomous Abyss)". Derived, not typed.
    pub season: String,
    /// `draft_ptr`, `active`, `archived`. The machine word, for the form.
    pub state: &'static str,
    /// The word a person reads.
    pub state_label: &'static str,
    pub patch: String,
    pub tier: String,
    pub items: usize,
    pub catalog_version: u32,
    /// Whether the Activate button is offered. False for the catalogue that
    /// is already active, and false for one whose data does not hold
    /// together -- §8 activates a catalogue *after reviewing it*, and a
    /// review that cannot fail is not one.
    pub activatable: bool,
    /// What is wrong with it, in the reader's own words. Empty is the normal
    /// case and renders nothing.
    pub problems: Vec<String>,
    /// The catalogue this one will archive if it is activated.
    ///
    /// §8 makes activation and archiving one transaction, so pressing the
    /// button ends the season that is running. A button that does two things
    /// and names one of them is a button somebody presses by accident.
    pub archives: Option<String>,
    /// The patches and tiers a reviewer is being asked to approve.
    ///
    /// §16's Phase 9 wants a PTR catalogue reviewed before it is activated,
    /// and a review needs something to look at. The item count alone is not
    /// it: what an activation changes is which patch and which raid tier the
    /// archive starts filing prices under.
    pub patches: Vec<AdminPatch>,
    /// What a reviewer should know before pressing the button, from the
    /// catalogue's own `notes`. Administrator-only: a PTR note is
    /// unconfirmed research (§9).
    pub notes: Vec<String>,
    /// How many items of each kind, which is what a PTR draft is mostly made
    /// of and the figure a reviewer checks against the patch notes.
    pub kinds: Vec<(&'static str, usize)>,
}

/// One patch of a catalogue under review, with the tiers it declares.
#[derive(Debug, Clone)]
pub struct AdminPatch {
    pub patch: String,
    pub name: String,
    pub started: String,
    /// Rendered whether or not the patch opened a raid: "opened no raid" is a
    /// thing a reviewer has to be told, not an absence to be inferred.
    pub tiers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AdminCategory {
    /// The switch's name, as the form submits it.
    pub key: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    /// Per-realm categories are the expensive ones: a cycle costs one fetch
    /// per realm rather than one per region.
    pub per_realm: bool,
}

#[derive(Debug, Clone)]
pub struct AdminRegion {
    pub code: &'static str,
    pub label: String,
    /// Realms grouped by the language they are played in. EU is seven
    /// languages sharing a region, and someone looking for their own realm
    /// among ninety-two is looking for their language first.
    pub languages: Vec<AdminLanguage>,
    pub enabled: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub struct AdminLanguage {
    /// "Deutsch", or the raw locale tag when we have no name for it.
    pub label: &'static str,
    pub markets: Vec<AdminMarket>,
    /// Counted by market rather than by name: three realms sharing one
    /// auction house are one thing being collected.
    pub enabled: usize,
}

/// One auction house, and the realms that share it.
///
/// The box is the unit, because the market is: clicking any name inside it
/// switches the whole thing. Drawing them together says that without a
/// sentence under every name, and gives the page a shape -- every market is
/// one box, whether it holds one realm or ten.
#[derive(Debug, Clone)]
pub struct AdminMarket {
    pub id: u32,
    /// Every realm on this auction house, alphabetically.
    pub names: Vec<String>,
    pub enabled: bool,
}

/// One entry in the language menu.
#[derive(Debug, Clone)]
pub struct LanguageLink {
    /// `?lang=es_ES` -- relative, so it keeps whatever page you are on.
    pub href: String,
    pub label: &'static str,
    pub selected: bool,
    /// False when only item text is translated into this language and the
    /// interface still renders in English. Saying so is better than letting
    /// someone pick Italian and wonder why half the page did not change.
    pub interface: bool,
    /// Translated share of the interface, for a catalogue that exists but is
    /// not finished. Shown rather than hidden: this is a community
    /// translation, and "34% translated" is an invitation.
    pub coverage_percent: usize,
}

impl Layout {
    pub fn new(
        config: &WebConfig,
        locale: Locale,
        title: impl Into<String>,
        current: &'static str,
        request_uri: &Uri,
        user: Option<&User>,
        csrf: String,
    ) -> Self {
        // What the app is for. Everyone sees these.
        const NAV: [(&str, &str); 2] =
            [("Auction House", "/wow/auctions"), ("Account", "/account")];
        // Only means anything with an account behind it: the page is a list of
        // what *you* follow. Offering it to a signed-out visitor is offering a
        // page that can only say "sign in".
        const SIGNED_IN_NAV: [(&str, &str); 1] = [("Alerts", "/wow/alerts")];
        // How the app is running. Only an administrator has any use for them,
        // and a nav full of pages most visitors cannot open is a worse
        // greeting than a short one.
        const ADMIN_NAV: [(&str, &str); 6] = [
            ("Dashboard", "/"),
            ("Cluster", "/cluster"),
            ("Nodes", "/nodes"),
            ("Jobs", "/jobs"),
            ("Collection", "/admin"),
            ("WoW", "/wow"),
        ];
        let is_admin = user.is_some_and(|u| u.is_admin);
        let title = title.into();
        Self {
            lang: locale.bcp47(),
            // Page titles are source strings like "Dashboard"; a dynamic one
            // (an item name, a job id) simply will not match and passes
            // through unchanged.
            title: crate::i18n::translate(locale, &title).to_string(),
            app_name: config.app_name.clone(),
            languages: ALL_LOCALES
                .into_iter()
                .map(|l| LanguageLink {
                    href: language_href(request_uri, l),
                    label: l.label(),
                    selected: l == locale,
                    interface: crate::i18n::has_interface(l),
                    coverage_percent: crate::i18n::interface_coverage(l),
                })
                .collect(),
            language_label: locale.label(),
            nav: NAV
                .iter()
                .chain(if user.is_some() {
                    &SIGNED_IN_NAV[..]
                } else {
                    &[]
                })
                .chain(if is_admin { &ADMIN_NAV[..] } else { &[] })
                .map(|(label, href)| NavItem {
                    label,
                    href,
                    active: *href == current,
                })
                .collect(),
            signed_in: user.is_some(),
            username: user.map(|u| u.username.clone()).unwrap_or_default(),
            fallback_poll_ms: config.poll_interval_ms * 10,
            csrf,
            debug_controls: config.debug_controls,
            pico_version: &crate::assets::PICO_VERSION,
            css_version: &crate::assets::STYLE_VERSION,
            htmx_version: &crate::assets::HTMX_VERSION,
            live_version: &crate::assets::LIVE_VERSION,
        }
    }
}

/// Keep the current page state when changing language. Replacing the entire
/// query silently reset expansion and search choices on market pages.
fn language_href(uri: &Uri, locale: Locale) -> String {
    let mut href = uri.path().to_string();
    let mut separator = '?';

    if let Some(query) = uri.query() {
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key);
            if key == "lang" {
                continue;
            }
            href.push(separator);
            href.push_str(pair);
            separator = '&';
        }
    }

    href.push(separator);
    href.push_str("lang=");
    href.push_str(locale.code());
    href
}

#[derive(Debug, Clone)]
pub struct RoleCountView {
    pub role: &'static str,
    pub count: usize,
    pub min: usize,
    pub met: bool,
}

#[derive(Debug, Clone)]
pub struct MetricsView {
    pub requests_total: u64,
    pub mean_latency: String,
    pub in_flight: u64,
    pub peak_in_flight: u64,
    pub client_errors: u64,
    pub server_errors: u64,
}

impl MetricsView {
    pub fn new(snapshot: &app_core::MetricsSnapshot) -> Self {
        Self {
            requests_total: snapshot.requests_total,
            mean_latency: format!("{:.2} ms", snapshot.mean_latency_ms()),
            in_flight: snapshot.in_flight,
            peak_in_flight: snapshot.peak_in_flight,
            client_errors: snapshot.client_errors,
            server_errors: snapshot.server_errors,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub status: &'static str,
    pub nodes_online: usize,
    pub nodes_total: usize,
    pub roles: Vec<RoleCountView>,
    pub jobs_running: usize,
    pub jobs_queued: usize,
    pub jobs_completed: usize,
    pub jobs_failed: usize,
    pub tasks_running: usize,
    pub tasks_queued: usize,
    pub leader: String,
    pub gateway: String,
}

impl Stats {
    pub fn from_snapshot(snapshot: &ClusterSnapshot) -> Self {
        let policies: RolePolicies = snapshot.policies;
        Self {
            status: snapshot.status_label(),
            nodes_online: snapshot.nodes_online,
            nodes_total: snapshot.nodes_total,
            roles: ALL_ROLES
                .iter()
                .map(|role| {
                    let count = snapshot.roles.get(*role);
                    let min = policies.get(*role).min_replicas;
                    RoleCountView {
                        role: role.as_str(),
                        count,
                        min,
                        met: count >= min,
                    }
                })
                .collect(),
            jobs_running: snapshot.jobs.running,
            jobs_queued: snapshot.jobs.queued,
            jobs_completed: snapshot.jobs.completed,
            jobs_failed: snapshot.jobs.failed,
            tasks_running: snapshot.tasks_running,
            tasks_queued: snapshot.tasks_queued,
            leader: snapshot
                .leader
                .map(|n| n.to_string())
                .unwrap_or_else(|| "none".into()),
            gateway: snapshot
                .gateway
                .map(|n| n.to_string())
                .unwrap_or_else(|| "none".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoleChip {
    pub role: &'static str,
    pub held: bool,
}

#[derive(Debug, Clone)]
pub struct NodeView {
    pub id: String,
    pub raw_id: u16,
    pub status: &'static str,
    pub online: bool,
    pub roles: Vec<RoleChip>,
    pub cores: u8,
    pub memory: String,
    pub load_percent: u8,
    pub running_tasks: u16,
    pub free_memory: String,
    pub simulated: bool,
    pub last_seen: String,
    pub is_leader: bool,
    pub is_gateway: bool,
}

impl NodeView {
    pub fn new(node: &Node, now: Millis, snapshot: &ClusterSnapshot, locale: Locale) -> Self {
        Self {
            id: node.id.to_string(),
            raw_id: node.id.get(),
            status: node.status.as_str(),
            online: node.status.accepts_work(),
            roles: ALL_ROLES
                .iter()
                .map(|role| RoleChip {
                    role: role.as_str(),
                    held: node.roles.contains(*role),
                })
                .collect(),
            cores: node.capabilities.cores,
            memory: if node.capabilities.memory_bytes > 0 {
                format::bytes(node.capabilities.memory_bytes)
            } else {
                // A worker that did not report a figure. Saying so beats
                // printing "0 B" as though it had none.
                "—".into()
            },
            load_percent: node.load.load_percent,
            running_tasks: node.load.running_tasks,
            free_memory: format::bytes(node.load.free_memory_bytes),
            simulated: node.load.simulated,
            last_seen: format::ago(locale, node.age_ms(now)),
            is_leader: snapshot.leader == Some(node.id),
            is_gateway: snapshot.gateway == Some(node.id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventView {
    pub time: String,
    pub message: String,
    pub kind: &'static str,
    pub severity: &'static str,
}

impl EventView {
    /// Composed here rather than in the template: an event message is a
    /// sentence with identifiers in it, and only the sentence may be
    /// translated. `{{ "..."|t }}` cannot do the substitution for a varying
    /// number of arguments, so the view does both steps.
    pub fn new(record: &EventRecord, locale: Locale) -> Self {
        let (pattern, args) = record.event.message_parts();
        let mut message = crate::i18n::translate(locale, pattern).to_string();
        for arg in args {
            if let Some(at) = message.find("{}") {
                message.replace_range(at..at + 2, &arg);
            }
        }
        Self {
            time: record.at.to_clock_string(),
            message,
            kind: record.event.kind(),
            severity: record.event.severity().as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub raw_id: u64,
    pub description: String,
    pub state: &'static str,
    pub progress: u8,
    pub tasks_total: u16,
    pub tasks_done: u16,
    pub tasks_failed: u16,
    pub created: String,
    pub duration: String,
    pub finished: bool,
}

impl JobRow {
    pub fn new(job: &Job, now: Millis) -> Self {
        Self {
            id: job.id.to_string(),
            raw_id: job.id.get(),
            description: job.spec.describe(),
            state: job.state.as_str(),
            progress: job.progress_percent(),
            tasks_total: job.task_count,
            tasks_done: job.tasks_completed,
            tasks_failed: job.tasks_failed,
            created: job.created_at.to_utc_string(),
            duration: format::duration_ms(job.duration_ms(now)),
            finished: job.state.is_terminal(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskView {
    pub id: String,
    pub index: u16,
    pub description: String,
    pub state: &'static str,
    pub node: String,
    pub attempt: u16,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct FailureView {
    pub time: String,
    pub task: String,
    pub node: String,
    pub attempt: u16,
    pub reason: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct JobDetailView {
    pub job: JobRow,
    pub tasks: Vec<TaskView>,
    pub failures: Vec<FailureView>,
}

impl JobDetailView {
    pub fn new(detail: &JobDetail, now: Millis) -> Self {
        Self {
            job: JobRow::new(&detail.job, now),
            tasks: detail.tasks.iter().map(task_view).collect(),
            failures: detail.failures.iter().map(failure_view).collect(),
        }
    }
}

fn task_view(task: &Task) -> TaskView {
    TaskView {
        id: task.id.to_string(),
        index: task.index,
        description: task.spec.describe(),
        state: task.state.as_str(),
        node: task
            .assigned_to
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into()),
        attempt: task.attempt,
        output: task.output.clone().unwrap_or_default(),
    }
}

fn failure_view(failure: &TaskAttempt) -> FailureView {
    FailureView {
        time: failure.at.to_utc_string(),
        task: failure.task_id.to_string(),
        node: failure
            .node_id
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into()),
        attempt: failure.attempt,
        reason: failure.reason.to_string(),
        detail: failure.detail.clone(),
    }
}

#[derive(Debug, Clone)]
pub struct CharacterView {
    pub name: String,
    pub realm: String,
    pub region: String,
    pub class: String,
    pub race: String,
    pub spec: String,
    pub faction: String,
    pub item_level: String,
    pub mythic_plus_score: String,
    pub thumbnail_url: Option<String>,
    pub profile_url: Option<String>,
    pub source: String,
    pub cached: bool,
    pub fetched_at: String,
}

impl CharacterView {
    pub fn new(character: &Character, source: &str, cached: bool) -> Self {
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
        Self {
            name: character.name.clone(),
            realm: character.realm.clone(),
            region: character.region.to_uppercase(),
            class: character.class.clone(),
            race: character.race.clone(),
            spec: dash(&character.spec),
            faction: dash(&character.faction),
            item_level: format::optional_f32(character.item_level),
            mythic_plus_score: format::optional_f32(character.mythic_plus_score),
            thumbnail_url: character.thumbnail_url.clone(),
            profile_url: character.profile_url.clone(),
            source: source.to_string(),
            cached,
            fetched_at: character.fetched_at.to_utc_string(),
        }
    }
}

// --- auction house -------------------------------------------------------

/// One quality rank within a card. Each rank is its own market, so each gets
/// its own column of figures.
///
/// **Phase 5 replaced what these figures were.** The column used to print an
/// all-time Avg, Low with its date and High with its date -- three prices and
/// two dates that answered "what has this ever cost", which is a question
/// about the archive rather than about buying one now. It answers the market
/// question instead: where today's price sits in this market's own history
/// (`band`, `rank_percent`), what the middle of that history is (`median`),
/// how much of it there is to buy (`quantity`, `listings`), how recently that
/// was true (`freshness`), and the shape it got here by (`spark`).
///
/// The all-time extremes did not move to another line of the card; they moved
/// to the analysis page, which is where a question about the whole archive
/// belongs and where the link at the bottom of every column goes.
#[derive(Debug, Clone)]
pub struct RankColumn {
    pub item_id: u32,
    pub label: String,
    pub has_data: bool,
    pub current: String,
    /// The comparison window's median -- the engine's, so this is the same
    /// number the analysis page and the alert rule call the median.
    pub median: String,
    /// Signed percentage from that median; negative is cheaper.
    pub delta_percent: i32,
    pub cheap: bool,
    pub dear: bool,
    /// The valuation band's source string and its CSS slug, or `None` where
    /// the evidence gate refused one. Never shown without `rank_percent`
    /// beside it: §5.2 is explicit that the label is not shown alone.
    pub band: Option<&'static str>,
    pub band_slug: &'static str,
    pub rank_percent: Option<u8>,
    /// Why there is no band, as a source string and its two numbers, so the
    /// card can say "34 hours of 72" rather than going quiet. §5.3's
    /// `Not enough history` and its reason.
    pub insufficient: Option<&'static str>,
    pub insufficient_have: u32,
    pub insufficient_need: u32,
    pub quantity: u64,
    pub listings: u32,
    /// How long ago this market was last observed, already worded in the
    /// reader's language.
    pub freshness: String,
    /// Older than the page's own snapshot: this market did not refresh when
    /// the rest of the page did, so its figures are about a different moment.
    pub stale: bool,
    /// The sparkline, rendered. Empty where there is no shape to draw, and the
    /// template then draws nothing rather than an empty box.
    pub spark: String,
}

/// One consumable as a card, with a column per rank.
///
/// R1 and R2 are separate markets but the same item, and the interesting
/// question is the comparison between them -- so they belong side by side on
/// one card rather than as two cards that happen to sort next to each other.
#[derive(Debug, Clone)]
pub struct ItemCard {
    pub name: String,
    pub icon: Option<String>,
    /// Set on reagents: the localised material type, shown instead of the
    /// category. `None` on consumables, which show category and stat.
    pub material: Option<String>,
    /// Which rank the icon's tooltip describes: the highest, because that is
    /// the current version of a crafted consumable and the one people mean
    /// when they name it. The tooltip says which rank it is showing.
    pub tooltip_item_id: u32,
    /// Rendered into the page when the tooltip was already cached, which
    /// makes hovering free. `None` falls back to fetching on first hover.
    pub tooltip: Option<TooltipView>,
    pub category: &'static str,
    pub stat: &'static str,
    /// Item quality as a number. Cards sort rarest first: within a group, the
    /// rare cut and the epic reagent are what someone came to look at, and an
    /// alphabetical list buries them among the commons.
    pub rarity: u8,
    /// The catalog's own (English) name. Only used for sorting, where the
    /// displayed name would move the cards about from one language to the
    /// next; never rendered.
    pub sort_name: String,
    pub any_data: bool,
    pub columns: Vec<RankColumn>,
}

#[derive(Debug, Clone)]
pub struct CardGroup {
    pub audience: &'static str,
    pub label: &'static str,
    pub cards: Vec<ItemCard>,
    /// Rendered as a heading and a count, with its cards fetched when the
    /// reader scrolls to it.
    ///
    /// `docs/market-analysis.md` §15: a small page inlines its cards, a large
    /// category renders a useful first group and loads the rest as they
    /// approach the viewport. The count is still here either way, so the page
    /// says how much there is before it says what it is -- and so the anchors
    /// the headings carry keep working.
    pub deferred: bool,
    /// Where this group's cards come from when it is deferred.
    pub href: String,
}

/// A signed change, pre-rendered.
#[derive(Debug, Clone)]
pub struct TrendView {
    pub label: &'static str,
    pub percent: i32,
    pub known: bool,
    pub cheaper: bool,
}

#[derive(Debug, Clone)]
pub struct PatchStatRow {
    pub patch: String,
    pub label: String,
    pub mean: String,
    pub low: String,
    pub high: String,
    pub samples: u32,
    pub has_data: bool,
}

/// Everything the single-item page shows.
#[derive(Debug, Clone)]
pub struct ItemDetail {
    pub item_id: u32,
    pub name: String,
    pub icon: Option<String>,
    /// As on [`ItemCard`]: inlined when cached, fetched on hover when not.
    pub tooltip: Option<TooltipView>,
    pub category: &'static str,
    pub audience: &'static str,
    pub stat: &'static str,
    pub rank: u8,
    pub ranks_total: usize,
    pub expansion: String,
    /// Category-aware breadcrumb target. Reagents must not link back to the
    /// consumables page simply because both use the same detail template.
    pub section: &'static str,
    pub section_href: String,
    pub expansion_href: String,
    pub region: String,
    /// The same region as the code a form posts back, lowercase.
    pub region_code: &'static str,
    pub archived: bool,
}

/// One event, and what the market did around it.
///
/// **Never a cause.** §16: the wording is `observed after`, and this struct
/// carries no field that could be rendered as anything else. A comparison that
/// does not clear its evidence gate renders as unsupported rather than being
/// drawn smaller, because a reader does not weigh a figure by its font size.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub kind: &'static str,
    pub title: String,
    pub when: String,
    /// The scope this event claims -- which regions, which patch, which
    /// category. Shown because §16's gate is that "every correlation exposes
    /// its scope", and an event that applied to one region is not evidence
    /// about another.
    pub scope: String,
    /// `None` where there is no comparison to make at all: nothing recorded on
    /// one side of it.
    pub before: Option<String>,
    pub after: Option<String>,
    pub change_percent: i32,
    pub before_samples: u32,
    pub after_samples: u32,
    /// False when either side is too thin. The row still appears -- the event
    /// happened -- but it says it cannot compare rather than comparing badly.
    pub supported: bool,
    /// Whether there was anything at all on *both* sides.
    ///
    /// Distinct from `supported`, and the distinction is the difference
    /// between two honest sentences. An event that predates the whole archive
    /// has nothing before it -- not "0 observations before", which reads as a
    /// count somebody took, but no overlap at all. Collapsing the two reported
    /// "(0 before, 0 after)" for an event with ten observations after it,
    /// which is a figure that is simply wrong.
    pub comparable: bool,
}

/// What buying the target quantity actually costs.
///
/// Every figure here is about *one snapshot*: what is on the shelf now, not
/// what has sold. §15's "listed stock is not sales volume" applies to all of
/// it, and the panel says so in words rather than leaving it to be inferred.
#[derive(Debug, Clone)]
pub struct DepthView {
    /// Rungs and units, which decide whether the rest is worth reading.
    pub levels: u32,
    pub total: u64,
    pub cheapest: String,
    /// True when the ladder is too thin to be a distribution -- a BoE with
    /// four auctions. The percentiles and the liquidity proxies are absent
    /// rather than guessed, which is what `sparse` is announcing.
    pub sparse: bool,
    pub p25: Option<String>,
    pub p50: Option<String>,
    pub within_5: Option<u64>,
    pub within_20: Option<u64>,
    /// The catalogue's target quantity, and what a sweep for it costs.
    pub target: u64,
    pub filled: u64,
    pub complete: bool,
    pub total_cost: String,
    pub average_unit: String,
    pub clearing_price: String,
    /// How much dearer the sweep is than the sticker price.
    pub impact_percent: u32,
    pub walls: Vec<WallView>,
    /// The ladder, drawn.
    pub chart: String,
}

#[derive(Debug, Clone)]
pub struct WallView {
    pub price: String,
    pub quantity: u64,
    pub share_percent: u32,
}

/// One panel's header: the question it answers, and the terms it answers in.
///
/// Phase 6's exit gate is that "every panel names its question, window, units,
/// coverage, and freshness". Making that a value rather than a paragraph per
/// panel in the template is what stops the next panel being added without
/// them -- there is nowhere to put a panel that does not have them, and a
/// reviewer can see at a glance which of the five each one carries.
///
/// `coverage` and `freshness` are `None` where a panel genuinely has no such
/// thing -- an hour-of-day chart is over the whole history and is not fresh or
/// stale -- rather than being filled with a plausible-looking value.
#[derive(Debug, Clone)]
pub struct PanelHead {
    /// The question, as a source string. Not a title: "Price over time" names
    /// a chart, "What has this been worth, and how tightly?" names what the
    /// reader came to find out.
    pub question: &'static str,
    /// The interval, already worded.
    pub window: String,
    /// Gold, units, auctions, hours. Named because a chart's y-axis is the one
    /// place a reader guesses.
    pub units: &'static str,
    /// "57 of 336 hours (17%)", or `None` where the panel is not a fraction of
    /// anything.
    pub coverage: Option<String>,
    /// How old the newest observation behind it is.
    pub freshness: Option<String>,
}

/// The cacheable half of the item page.
///
/// Everything here is a pure function of the published version, the item, the
/// region, the comparison window and the locale -- which is what lets it carry
/// an ETag and live in the fragment cache. The personalised half, which is the
/// follow control and the nav, stays in the shell and stays `no-store`; §16's
/// Phase 6 asks for that split by name, and §10 is why it is not optional.
#[derive(Debug, Clone)]
pub struct ItemAnalysis {
    pub has_data: bool,

    // --- what is it worth now, and where does that sit ---------------------
    pub price_panel: PanelHead,
    pub current: String,
    pub band: Option<&'static str>,
    pub band_slug: &'static str,
    pub rank_percent: Option<u8>,
    pub from_median_percent: Option<i32>,
    pub insufficient: Option<&'static str>,
    pub insufficient_have: u32,
    pub insufficient_need: u32,
    /// Shown beside the band and never folded into it: §5.4 keeps "unusually
    /// far from the body of the distribution" apart from "low in it".
    pub anomaly: &'static str,
    pub anomaly_slug: &'static str,
    pub median: String,
    pub p25: String,
    pub p75: String,
    pub iqr: String,
    pub mad: String,

    pub distribution_panel: PanelHead,
    pub stock_panel: PanelHead,
    pub quantity: u64,
    pub listings: u32,

    // --- what it costs to buy what you need --------------------------------
    /// `None` until ladder collection has run for this market. An archive
    /// gathered before Phase 7 has no ladders and cannot be given any, so the
    /// panel says that rather than drawing an empty market.
    pub depth: Option<DepthView>,
    pub depth_panel: PanelHead,

    // --- how good is the evidence -----------------------------------------
    pub quality_panel: PanelHead,
    pub samples: usize,
    pub observed_buckets: u32,
    pub expected_buckets: Option<u32>,
    pub coverage_percent: Option<u32>,
    pub largest_gap: String,
    pub first_seen: String,
    pub observed_at: String,

    pub trends: Vec<TrendView>,
    pub swing_percent: u32,

    /// The cheapest hour of the week, named as an hour *and* a day -- which is
    /// what the grid can say and two separate charts could not.
    pub cheapest_cell: Option<String>,
    pub cycle_panel: PanelHead,

    // --- how it moves, and what with (Phase 8) -----------------------------
    pub movement_panel: PanelHead,
    /// Already worded by `correlate::Association::wording`, which is the one
    /// place that phrasing lives so it cannot drift into a claim of causation.
    pub association: Option<&'static str>,
    pub association_rho: i32,
    pub association_pairs: u32,
    pub association_strength: &'static str,
    pub drawdown_percent: u32,
    pub rise_percent: u32,
    pub typical_move_percent: Option<u32>,
    pub stability_changes: u32,

    // --- what happened, and when (Phase 8) ---------------------------------
    pub events_panel: PanelHead,
    pub events: Vec<EventRow>,

    /// Pre-rendered inline SVG.
    pub price_chart: String,
    pub distribution_chart: String,
    pub stock_chart: String,
    pub heatmap_chart: String,
    /// Legend entries: the label, and the colour the chart draws that slot in.
    ///
    /// The colour travels with the label rather than being looked up in CSS,
    /// so a legend swatch cannot lose its colour to a stylesheet edit the way
    /// it did when this was a `.swatch.s1` class.
    pub series_labels: Vec<SeriesKey>,

    pub patch_panel: PanelHead,
    pub patches: Vec<PatchStatRow>,
}

#[derive(Debug, Clone)]
pub struct AlertRow {
    pub item_id: u32,
    pub name: String,
    pub region: String,
    pub severity: &'static str,
    pub current: String,
    pub baseline: String,
    pub discount_percent: u8,
    pub quantity: u64,
    pub when: String,
}

/// Today's alerts for the items one person follows.
///
/// An alert nobody asked for is a feed. This is the whole reason the view has
/// a shape of its own: `visible` is false for a visitor who is signed out or
/// who follows nothing, and the summary then renders nothing at all rather
/// than an empty box explaining itself.
#[derive(Debug, Clone, Default)]
pub struct AlertsView {
    pub visible: bool,
    pub rows: Vec<AlertRow>,
}

impl AlertsView {
    pub fn count(&self) -> usize {
        self.rows.len()
    }
}

/// One followed item, on the alerts page.
#[derive(Debug, Clone)]
pub struct WatchRow {
    pub item_id: u32,
    pub name: String,
    pub region: String,
    /// The region code the form posts back, lowercase.
    pub region_code: &'static str,
    pub icon: Option<String>,
    /// What it costs right now, or `None` when nothing has been recorded.
    pub current: Option<String>,
    pub href: String,
}

/// The alerts page: what you follow.
///
/// Today's alerts are *not* in here. They are the same fragment the auction
/// house index shows, and a shared component with two owners drifts (§7); the
/// page holds it in its own field so both render the same markup.
#[derive(Debug, Clone, Default)]
pub struct WatchlistView {
    pub signed_in: bool,
    pub watches: Vec<WatchRow>,
}

/// One expansion in the selector.
#[derive(Debug, Clone)]
pub struct CatalogLink {
    pub id: String,
    pub label: String,
    /// The expansion currently being collected.
    pub collecting: bool,
    /// The one being viewed.
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct PatchColumn {
    pub patch: String,
    pub label: String,
    pub started: String,
}

/// One item's prices within one patch window.
#[derive(Debug, Clone)]
pub struct PatchCell {
    pub low: String,
    pub mean: String,
    pub high: String,
    pub samples: u32,
    pub has_data: bool,
}

impl PatchCell {
    pub fn empty() -> Self {
        Self {
            low: "—".into(),
            mean: "—".into(),
            high: "—".into(),
            samples: 0,
            has_data: false,
        }
    }
}

/// A row of the patch-by-patch comparison: how one item moved across the
/// expansion.
#[derive(Debug, Clone)]
pub struct PatchRow {
    pub name: String,
    pub audience: &'static str,
    pub category: &'static str,
    pub cells: Vec<PatchCell>,
    /// Across every patch -- the "overall cost" view.
    pub overall: PatchCell,
}

#[derive(Debug, Clone)]
pub struct MarketView {
    pub expansion: String,
    pub season: String,
    /// True when viewing a finished expansion: read-only, never updated again.
    pub archived: bool,
    /// False when Battle.net credentials are missing -- the page says so
    /// rather than silently showing an empty table.
    pub configured: bool,
    pub groups: Vec<CardGroup>,
    /// Age of the snapshot every card on the page was priced from.
    pub observed: String,
    pub baseline_days: u64,
}

/// The expansion's price history, patch by patch.
///
/// Its own view, and its own request. It is 659 rows of every item at every
/// rank across the expansion -- 85% of what the consumables fragment used to
/// weigh -- and most visits never scroll to it. A reader who does scroll gets
/// exactly the same table; a reader who does not stops paying for it.
#[derive(Debug, Default)]
pub struct PatchesView {
    pub expansion: String,
    /// Set when the table has been narrowed to one patch, which is what the
    /// archive's patch page fetches. The same table, one column -- not a
    /// second one (§16, Phase 9: nothing is forked for a patch or a tier).
    pub only: Option<String>,
    pub patches: Vec<PatchColumn>,
    pub rows: Vec<PatchRow>,
}

/// What the visitor is looking at: which expansion, and whose prices.
///
/// Not the language: that is a site-wide setting and lives in the top bar,
/// because every regional host returns every language and the two choices are
/// independent.
///
/// Both selects are rendered *outside* the polled fragment. A `<select>` being
/// swapped from under the pointer mid-choice is worse than a picker that
/// updates a poll late.
#[derive(Debug, Clone)]
pub struct MarketPicker {
    /// Where the picker form submits, so it works the same on the live page
    /// and on an archived expansion.
    pub action: String,
    pub regions: Vec<PickerOption>,
    /// True when only one region is collected, so the picker can say why it
    /// is not offering more rather than looking broken.
    pub single_region: bool,
    /// Expansions still being collected.
    ///
    /// Split from the archived ones so the select can group them: which
    /// expansions are live is exactly what the visitor needs to know before
    /// choosing, and it is what the old tab strip showed with a badge.
    pub live_expansions: Vec<PickerOption>,
    /// Finished expansions: still readable, never updated again.
    pub archived_expansions: Vec<PickerOption>,
}

impl MarketPicker {
    pub fn new(action: String, collected: &[Region], region: Region) -> Self {
        Self {
            regions: collected
                .iter()
                .map(|r| PickerOption {
                    value: r.as_str().to_string(),
                    label: r.as_str().to_uppercase(),
                    selected: *r == region,
                })
                .collect(),
            single_region: collected.len() < 2,
            action,
            live_expansions: Vec::new(),
            archived_expansions: Vec::new(),
        }
    }

    /// Offer an expansion choice as well. Pages that only ever show one
    /// expansion leave this alone and the select is not rendered.
    pub fn with_expansions(mut self, catalogs: Vec<CatalogLink>) -> Self {
        for catalog in catalogs {
            let option = PickerOption {
                value: catalog.id,
                label: catalog.label,
                selected: catalog.selected,
            };
            if catalog.collecting {
                self.live_expansions.push(option);
            } else {
                self.archived_expansions.push(option);
            }
        }
        self
    }

    /// Whether to offer the expansion select at all.
    ///
    /// True for a single expansion as well as for many: with one option the
    /// control is not a choice, but it is still the clearest statement of
    /// which expansion's prices are on screen, and hiding it left the page
    /// looking like it had lost a control.
    pub fn has_expansions(&self) -> bool {
        !self.live_expansions.is_empty() || !self.archived_expansions.is_empty()
    }
}

/// One legend entry.
#[derive(Debug, Clone)]
pub struct SeriesKey {
    pub label: String,
    pub colour: &'static str,
}

#[derive(Debug, Clone)]
pub struct PickerOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

// --- the archive: expansion -> patch -> raid tier (Phase 9) ------------------
//
// Four views for four levels, and not one of them holds a statistic of its
// own. Everything with a price in it is a component this app already has: the
// patch table is `partials/patches.html`, the tier's gear is the same
// `gear_group` macro the Gear page calls, and an item's analysis is the item
// page. §7's rule that a new page is a new *use* of the existing design is
// the whole design here -- a tier that needed its own card would be a tier
// that had forked the product.

/// `/wow/archive` -- every expansion a visitor may browse.
#[derive(Debug, Clone, Default)]
pub struct ArchiveView {
    pub expansions: Vec<ArchiveExpansionCard>,
}

/// One expansion on the archive index.
#[derive(Debug, Clone)]
pub struct ArchiveExpansionCard {
    pub name: String,
    pub href: String,
    /// "2026-03-02 — present", already worded.
    pub span: String,
    pub patches: usize,
    pub tiers: usize,
    /// True while one of its catalogues is still being collected.
    pub collecting: bool,
    /// Straight to its prices, for a reader who came for those rather than for
    /// the history.
    pub markets_href: String,
}

/// `/wow/archive/{expansion}` -- one expansion's patches.
#[derive(Debug, Clone, Default)]
pub struct ExpansionView {
    pub name: String,
    pub span: String,
    pub collecting: bool,
    pub markets_href: String,
    pub patches: Vec<ArchivePatchCard>,
    pub tiers_total: usize,
}

/// One patch, with the tiers it opened listed inside it.
#[derive(Debug, Clone)]
pub struct ArchivePatchCard {
    /// The key -- "12.1" -- which is what a URL and a stored window are filed
    /// under.
    pub patch: String,
    pub name: String,
    pub href: String,
    pub started: String,
    /// "—" while it is the current patch.
    pub until: String,
    pub ran_days: u64,
    /// True for the newest patch of a collecting expansion.
    pub current: bool,
    pub tiers: Vec<ArchiveTierLink>,
}

/// One raid tier, wherever it is listed.
#[derive(Debug, Clone)]
pub struct ArchiveTierLink {
    pub name: String,
    pub href: String,
    pub opened: String,
    /// `0` where the game gave the tier no season number, which the template
    /// renders as nothing rather than as "Season 0".
    pub season: u8,
    pub current: bool,
}

/// `/wow/archive/{expansion}/{patch}` -- what happened in one patch.
#[derive(Debug, Clone, Default)]
pub struct ArchivePatchView {
    pub expansion: String,
    pub expansion_href: String,
    pub patch: String,
    pub name: String,
    pub started: String,
    pub until: String,
    pub ran_days: u64,
    pub current: bool,
    pub tiers: Vec<ArchiveTierLink>,
    pub timeline: Vec<TimelineRow>,
    /// The patch's own column of the price table, fetched when the reader
    /// scrolls to it. The same fragment the consumables page defers, narrowed
    /// to one patch -- not a second table.
    pub table_href: String,
    pub region: String,
}

/// `/wow/archive/{expansion}/{patch}/{tier}` -- one raid tier's market.
#[derive(Debug, Clone, Default)]
pub struct ArchiveTierView {
    pub expansion: String,
    pub expansion_href: String,
    pub patch: String,
    pub patch_href: String,
    pub name: String,
    pub opened: String,
    pub until: String,
    pub ran_days: u64,
    pub season: u8,
    pub current: bool,
    /// "EU", for a sentence. The lowercase code below is for a link.
    pub region: String,
    /// The code the card's own analysis link carries, so a reader who follows
    /// one lands in the region they were already reading.
    pub region_code: &'static str,
    /// Where its bind-on-equip pieces are priced with a realm picker.
    pub gear_href: String,
    pub pieces: usize,
    /// Age of the snapshot the cards below were priced from.
    pub observed: String,
    /// The tier's bind-on-equip gear, through the shared card macro.
    pub groups: Vec<GearGroup>,
    pub timeline: Vec<TimelineRow>,
}

/// One event on an archive page's timeline.
///
/// Deliberately *not* [`EventRow`]: that answers "what did this market's price
/// do either side of this event", which is a question about one market. This
/// answers "what happened during this patch", which has no market in it and
/// therefore no before and after to print. Sharing the type would have meant
/// rendering "this window does not reach across the event" on a page with no
/// window in it.
#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub kind: &'static str,
    pub title: String,
    pub when: String,
    /// Which regions, patch or category it claims to apply to. Printed for the
    /// same reason the item page prints it: an event scoped to one region is
    /// not evidence about another.
    pub scope: String,
    pub notes: Option<String>,
}

/// An item tooltip, in the order the game draws one.
///
/// Every field is already a string: the template decides layout, not content.
/// `quality` is a CSS slug rather than a colour, because the palette lives in
/// the stylesheet and has to differ between the light and dark themes.
#[derive(Debug, Clone)]
pub struct TooltipView {
    pub name: String,
    pub quality: &'static str,
    /// The same quality as a number, so a grid of cards can put the rarer
    /// item first. See [`app_core::item::ItemQuality::rarity`].
    pub rarity: u8,
    /// BCP-47 tag for the `lang` attribute: a German tooltip inside an English
    /// page is exactly what that attribute is for.
    pub lang: &'static str,
    /// "Consumable · Flask", from the item's class and subclass.
    pub type_line: Option<String>,
    /// The subclass alone -- "Herb", "Optional Reagents" -- already localised
    /// by the upstream. Reagent cards show it in place of a category label
    /// that would only ever read "Reagents".
    pub material: Option<String>,
    pub item_level: Option<String>,
    pub binding: Option<String>,
    pub unique: Option<String>,
    pub required_level: Option<String>,
    /// A gem's "Requires Item Level: 80".
    pub required_item_level: Option<String>,
    pub stats: Vec<String>,
    pub effects: Vec<String>,
    pub flavor: Option<String>,
    pub crafting_reagent: Option<String>,
    pub sell_price: Option<String>,
    /// "Rank 2 of 3" -- which market this icon leads to, when the item has
    /// several. Ours, not the game's.
    pub rank_line: Option<String>,
    /// Set when the upstream could not be reached, so the box explains itself
    /// instead of looking broken.
    pub note: Option<&'static str>,
}

impl TooltipView {
    /// The icon is deliberately absent: the tooltip hangs off the picture, so
    /// repeating it inside the box is noise.
    pub fn new(tooltip: &ItemTooltip, rank_line: Option<String>, available: bool) -> Self {
        // The tooltip mirrors the game, which draws only the class for items
        // whose subclass is hidden.
        let subclass = match tooltip.subclass_hidden {
            true => &None,
            false => &tooltip.item_subclass,
        };
        let type_line = match (&tooltip.item_class, subclass) {
            (Some(class), Some(sub)) if class != sub => Some(format!("{class} · {sub}")),
            (Some(class), None) => Some(class.clone()),
            (_, Some(sub)) => Some(sub.clone()),
            (None, None) => None,
        };
        Self {
            name: tooltip.name.clone(),
            quality: tooltip.quality.as_str(),
            rarity: tooltip.quality.rarity(),
            lang: tooltip.locale.bcp47(),
            material: tooltip.item_subclass.clone(),
            type_line,
            item_level: tooltip.item_level.clone(),
            binding: tooltip.binding.clone(),
            unique: tooltip.unique.clone(),
            required_level: tooltip.required_level.clone(),
            required_item_level: tooltip.required_item_level.clone(),
            stats: tooltip.stats.clone(),
            effects: tooltip.effects.clone(),
            flavor: tooltip.flavor.clone(),
            crafting_reagent: tooltip.crafting_reagent.clone(),
            // The upstream's own label where it gave us one, so the line is
            // not the only English in a translated tooltip.
            sell_price: tooltip.sell_price.map(|c| {
                let label = tooltip.sell_price_label.as_deref().unwrap_or("Sell price:");
                format!("{label} {c}")
            }),
            rank_line,
            note: (!available)
                .then_some("Item details are unavailable right now; prices below are unaffected."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_links_keep_page_state_and_replace_the_old_language() {
        let uri: Uri = "/wow/auctions/reagents?expansion=midnight&q=ore&lang=en_GB"
            .parse()
            .expect("valid URI");

        assert_eq!(
            language_href(&uri, Locale::EsEs),
            "/wow/auctions/reagents?expansion=midnight&q=ore&lang=es_ES"
        );
    }

    #[test]
    fn the_expansion_picker_shows_even_for_a_single_expansion() {
        let mut picker = MarketPicker::new("/wow/auctions".into(), &[Region::Eu], Region::Eu);
        assert!(
            !picker.has_expansions(),
            "nothing to show when no catalogue is loaded"
        );

        picker.live_expansions.push(PickerOption {
            value: "midnight".into(),
            label: "Midnight".into(),
            selected: true,
        });
        assert!(
            picker.has_expansions(),
            "one expansion still names which prices these are"
        );
    }
}
