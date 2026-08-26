//! View models.
//!
//! The templates never see a `Node`, a `Job` or a Raider.IO response. Every
//! value here is already formatted, so the presentation layer cannot drift
//! into depending on internal cluster structures (CLAUDE.md 31).

use app_core::WebConfig;
use app_core::wow::Character;
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
    pub css_version: &'static str,
    pub htmx_version: &'static str,
    pub live_version: &'static str,
}

impl Layout {
    pub fn new(
        config: &WebConfig,
        title: impl Into<String>,
        current: &'static str,
        username: Option<String>,
        csrf: String,
    ) -> Self {
        const NAV: [(&str, &str); 7] = [
            ("Dashboard", "/"),
            ("Cluster", "/cluster"),
            ("Nodes", "/nodes"),
            ("Jobs", "/jobs"),
            ("WoW", "/wow"),
            ("Consumables", "/wow/consumables"),
            ("Account", "/account"),
        ];
        Self {
            title: title.into(),
            app_name: config.app_name.clone(),
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
            css_version: &crate::assets::STYLE_VERSION,
            htmx_version: &crate::assets::HTMX_VERSION,
            live_version: &crate::assets::LIVE_VERSION,
        }
    }
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
    pub cpu_class: &'static str,
    pub cores: u8,
    pub memory: String,
    pub flash: String,
    pub psram: String,
    pub load_percent: u8,
    pub running_tasks: u16,
    pub free_memory: String,
    pub simulated: bool,
    pub last_seen: String,
    pub is_leader: bool,
    pub is_gateway: bool,
}

impl NodeView {
    pub fn new(node: &Node, now: Millis, snapshot: &ClusterSnapshot) -> Self {
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
            cpu_class: node.capabilities.cpu_class.as_str(),
            cores: node.capabilities.cores,
            memory: format::bytes(node.capabilities.memory_bytes),
            flash: format::bytes(node.capabilities.flash_bytes),
            psram: node
                .capabilities
                .psram_bytes
                .map(format::bytes)
                .unwrap_or_else(|| "none".into()),
            load_percent: node.load.load_percent,
            running_tasks: node.load.running_tasks,
            free_memory: format::bytes(node.load.free_memory_bytes),
            simulated: node.load.simulated,
            last_seen: format::ago(node.age_ms(now)),
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
    pub fn new(record: &EventRecord) -> Self {
        Self {
            time: record.at.to_clock_string(),
            message: record.event.message(),
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
    pub category: &'static str,
    pub stat: &'static str,
    /// All ranks share a snapshot, so the timestamp belongs to the card.
    pub observed: String,
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
    pub category: &'static str,
    pub audience: &'static str,
    pub stat: &'static str,
    pub rank: u8,
    pub ranks_total: usize,
    pub expansion: String,
    pub catalog_id: String,
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
    pub series_labels: Vec<String>,

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
    pub region: String,
    /// True when viewing a finished expansion: read-only, never updated again.
    pub archived: bool,
    /// False when Battle.net credentials are missing -- the page says so
    /// rather than silently showing an empty table.
    pub configured: bool,
    pub tracked_items: usize,
    pub samples_held: usize,
    pub last_observed: String,
    pub catalogs: Vec<CatalogLink>,
    pub groups: Vec<CardGroup>,
    pub patches: Vec<PatchColumn>,
    pub patch_rows: Vec<PatchRow>,
    pub alerts: Vec<AlertRow>,
    pub baseline_days: u64,
}
