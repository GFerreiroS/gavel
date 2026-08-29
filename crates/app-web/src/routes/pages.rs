//! Full-page handlers.

use app_core::locale::Locale;
use app_core::repo::{Store, UserRepository};
use app_core::{AppError, Ports};
use askama::Template;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Html;
use axum::{Extension, Json};
use cluster_core::{ClusterControl, JobId};

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{EventView, JobDetailView, JobRow, Layout, MetricsView, NodeView, Stats};

const JOB_LIST_LIMIT: usize = 50;

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    layout: Layout,
    stats: Stats,
    metrics: MetricsView,
    events: Vec<EventView>,
}

#[derive(Template)]
#[template(path = "cluster.html")]
struct ClusterPage {
    layout: Layout,
    stats: Stats,
    nodes: Vec<NodeView>,
    events: Vec<EventView>,
    debug_controls: bool,
}

#[derive(Template)]
#[template(path = "nodes.html")]
struct NodesPage {
    layout: Layout,
    nodes: Vec<NodeView>,
    debug_controls: bool,
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsPage {
    layout: Layout,
    jobs: Vec<JobRow>,
}

#[derive(Template)]
#[template(path = "job_detail.html")]
struct JobDetailPage {
    layout: Layout,
    detail: JobDetailView,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    layout: Layout,
    signed_in: bool,
    username: String,
    linked: Vec<String>,
    /// The collection settings live behind this, and only an admin sees the
    /// way in. A nav item for everyone would be a link most people cannot
    /// use, on every page.
    is_admin: bool,
}

#[derive(Template)]
#[template(path = "wow.html")]
struct WowPage {
    layout: Layout,
}

/// Assemble the layout for a page, resolving the signed-in user once.
async fn layout<E: Ports>(
    env: &E,
    headers: &HeaderMap,
    csrf: &Csrf,
    locale: Locale,
    title: &str,
    current: &'static str,
    request_uri: &Uri,
) -> WebResult<Layout> {
    let user = current_user(env, headers).await?;
    Ok(Layout::new(
        env.config(),
        locale,
        title,
        current,
        request_uri,
        user.map(|u| u.username),
        csrf.0.clone(),
    ))
}

pub async fn dashboard<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    let events = env
        .cluster()
        .recent_events(env.config().event_log_limit)
        .await;
    page(
        &DashboardPage {
            layout: layout(&env, &headers, &csrf, prefs.locale, "Dashboard", "/", &uri).await?,
            stats: Stats::from_snapshot(&snapshot),
            metrics: MetricsView::new(&env.metrics().snapshot()),
            events: events
                .iter()
                .map(|e| EventView::new(e, prefs.locale))
                .collect(),
        },
        prefs.locale,
    )
}

pub async fn cluster<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    let nodes = env.cluster().nodes().await;
    let events = env
        .cluster()
        .recent_events(env.config().event_log_limit)
        .await;
    let now = env.now();
    page(
        &ClusterPage {
            layout: layout(
                &env,
                &headers,
                &csrf,
                prefs.locale,
                "Cluster",
                "/cluster",
                &uri,
            )
            .await?,
            stats: Stats::from_snapshot(&snapshot),
            nodes: nodes
                .iter()
                .map(|n| NodeView::new(n, now, &snapshot, prefs.locale))
                .collect(),
            events: events
                .iter()
                .map(|e| EventView::new(e, prefs.locale))
                .collect(),
            debug_controls: env.config().debug_controls,
        },
        prefs.locale,
    )
}

pub async fn nodes<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    let nodes = env.cluster().nodes().await;
    let now = env.now();
    page(
        &NodesPage {
            layout: layout(&env, &headers, &csrf, prefs.locale, "Nodes", "/nodes", &uri).await?,
            nodes: nodes
                .iter()
                .map(|n| NodeView::new(n, now, &snapshot, prefs.locale))
                .collect(),
            debug_controls: env.config().debug_controls,
        },
        prefs.locale,
    )
}

pub async fn jobs<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let now = env.now();
    let jobs = env.cluster().jobs(JOB_LIST_LIMIT).await;
    page(
        &JobsPage {
            layout: layout(&env, &headers, &csrf, prefs.locale, "Jobs", "/jobs", &uri).await?,
            jobs: jobs.iter().map(|j| JobRow::new(j, now)).collect(),
        },
        prefs.locale,
    )
}

pub async fn job_detail<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> WebResult<Html<String>> {
    let detail = env
        .cluster()
        .job(JobId(id))
        .await
        .ok_or(AppError::NotFound)?;
    let now = env.now();
    page(
        &JobDetailPage {
            layout: layout(
                &env,
                &headers,
                &csrf,
                prefs.locale,
                &format!("Job {id}"),
                "/jobs",
                &uri,
            )
            .await?,
            detail: JobDetailView::new(&detail, now),
        },
        prefs.locale,
    )
}

pub async fn account<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let linked = match &user {
        Some(user) => env
            .store()
            .users()
            .linked_accounts(user.id)
            .await?
            .into_iter()
            .map(|a| format!("{} ({})", a.provider, a.display_name))
            .collect(),
        None => Vec::new(),
    };
    page(
        &AccountPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Account",
                "/account",
                &uri,
                user.as_ref().map(|u| u.username.clone()),
                csrf.0.clone(),
            ),
            signed_in: user.is_some(),
            is_admin: user.as_ref().is_some_and(|u| u.is_admin),
            username: user.map(|u| u.username).unwrap_or_default(),
            linked,
        },
        prefs.locale,
    )
}

pub async fn wow<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    page(
        &WowPage {
            layout: layout(&env, &headers, &csrf, prefs.locale, "WoW", "/wow", &uri).await?,
        },
        prefs.locale,
    )
}

/// Machine-readable snapshot, for scripts and future non-browser clients.
/// The browser uses the HTML fragments instead.
pub async fn snapshot_json<E: Ports>(State(env): State<E>) -> Json<cluster_core::ClusterSnapshot> {
    Json(env.cluster().snapshot().await)
}

/// Request-side counters, for the same audience.
pub async fn metrics_json<E: Ports>(State(env): State<E>) -> Json<app_core::MetricsSnapshot> {
    Json(env.metrics().snapshot())
}
