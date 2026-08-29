//! View models.
//!
//! The templates never see a `Node`, a `Job` or a Raider.IO response. Every
//! value here is already formatted, so the presentation layer cannot drift
//! into depending on internal cluster structures.

use app_core::WebConfig;
use app_core::item::ItemTooltip;
use app_core::locale::{ALL_LOCALES, Locale};
use app_core::market::Region;
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
}

/// The reagents page: every tracked crafting material, by profession.
///
/// Same cards as the consumables page -- an item is an item, and the figures
/// worth showing are the same -- grouped by profession instead of raid role,
/// and searchable because there are 223 of them rather than 26.
#[derive(Debug, Clone)]
pub struct ReagentsView {
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
    pub expansion: String,
    pub expansion_id: String,
    pub archived: bool,
    /// Age of the newest realm snapshot on the page.
    pub observed: String,
    /// Every collected realm, for the picker. Empty when none is configured.
    pub realms: Vec<RealmOption>,
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
}

/// One connected realm on the picker.
#[derive(Debug, Clone)]
pub struct RealmOption {
    /// `eu:1403` -- the form value, and what the cookie remembers.
    pub value: String,
    pub name: String,
    pub region: String,
    pub selected: bool,
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
    pub levels: Vec<GearLevelRow>,
    /// True when nothing is listed anywhere we look. The card still appears:
    /// "nobody is selling one" is an answer, and it is a different answer
    /// from "we do not track this".
    pub unlisted: bool,
}

/// One item level of an item, across every region in view.
#[derive(Debug, Clone)]
pub struct GearLevelRow {
    /// The item level, say 295. Zero when the catalog has no mapping for the
    /// bonus: a recipe, or gear the sync script has not resolved yet.
    pub item_level: u16,
    /// "Champion 2/6": the track and the rank within it.
    pub upgrade: String,
    /// False for recipes, which have exactly one version of themselves and
    /// would be lied about by a level heading.
    pub leveled: bool,
    /// Where its own statistics page lives. Empty when there is no level to
    /// have a page for.
    pub href: String,
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
    /// Sockets and tertiary stats, as counts rather than as separate markets:
    /// they change what a piece is worth without changing what it is.
    pub extras: Vec<GearExtra>,
}

#[derive(Debug, Clone, Default)]
pub struct GearWhere {
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
    pub item_id: u32,
    pub name: String,
    pub icon: Option<String>,
    pub tooltip: Option<TooltipView>,
    pub slot: &'static str,
    /// The item level, and "Champion 2/6". Both zero/empty for a recipe,
    /// which has one version of itself.
    pub item_level: u16,
    pub upgrade: String,
    /// The category this belongs to, for the breadcrumb.
    pub section: &'static str,
    pub section_href: &'static str,
    /// The other item levels of the same item, so the ladder is one click
    /// wide rather than a trip back to the grid.
    pub siblings: Vec<GearLevelLink>,
    /// Whose prices these are: a realm's name, or every realm.
    pub scope: Option<String>,
    /// The same realm as a slug, for links out of this page.
    pub realm_slug: String,
    pub realms: Vec<RealmOption>,
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
    pub item_level: u16,
    pub upgrade: String,
    pub href: String,
    pub current: bool,
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
    pub categories: Vec<AdminCategory>,
    pub regions: Vec<AdminRegion>,
    pub realms_enabled: usize,
    pub realms_total: usize,
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
        username: Option<String>,
        csrf: String,
    ) -> Self {
        const NAV: [(&str, &str); 7] = [
            ("Dashboard", "/"),
            ("Auction House", "/wow/auctions"),
            ("WoW", "/wow"),
            ("Cluster", "/cluster"),
            ("Nodes", "/nodes"),
            ("Jobs", "/jobs"),
            ("Account", "/account"),
        ];
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
                .map(|(label, href)| NavItem {
                    label,
                    href,
                    active: *href == current,
                })
                .collect(),
            signed_in: username.is_some(),
            username: username.unwrap_or_default(),
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
#[derive(Debug, Clone)]
pub struct RankColumn {
    pub item_id: u32,
    pub label: String,
    pub has_data: bool,
    pub current: String,
    pub mean: String,
    pub low: String,
    pub low_when: String,
    pub high: String,
    pub high_when: String,
    pub quantity: u64,
    pub delta_percent: i32,
    pub cheap: bool,
    pub dear: bool,
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
    pub archived: bool,

    pub has_data: bool,
    pub current: String,
    pub mean: String,
    pub median: String,
    pub low: String,
    pub low_when: String,
    pub high: String,
    pub high_when: String,
    pub quantity: u64,
    pub samples: usize,
    pub first_seen: String,
    pub volatility_percent: u32,
    pub trends: Vec<TrendView>,

    pub best_hour: Option<String>,
    pub best_weekday: Option<String>,

    /// Pre-rendered inline SVG.
    pub price_chart: String,
    pub stock_chart: String,
    pub hour_chart: String,
    pub weekday_chart: String,
    /// Rank labels for the price chart legend, in slot order.
    /// Legend entries: the label, and the colour the chart draws that slot in.
    ///
    /// The colour travels with the label rather than being looked up in CSS,
    /// so a legend swatch cannot lose its colour to a stylesheet edit the way
    /// it did when this was a `.swatch.s1` class.
    pub series_labels: Vec<SeriesKey>,

    pub patches: Vec<PatchStatRow>,
}

#[derive(Debug, Clone)]
pub struct AlertRow {
    pub name: String,
    pub region: String,
    pub severity: &'static str,
    pub current: String,
    pub baseline: String,
    pub discount_percent: u8,
    pub quantity: u64,
    pub when: String,
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
    pub patches: Vec<PatchColumn>,
    pub patch_rows: Vec<PatchRow>,
    pub alerts: Vec<AlertRow>,
    /// Age of the snapshot every card on the page was priced from.
    pub observed: String,
    pub baseline_days: u64,
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
