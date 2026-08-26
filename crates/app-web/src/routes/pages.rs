//! Full-page handlers.

use app_core::repo::{Store, UserRepository};
use app_core::{AppError, Ports};
use askama::Template;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use axum::{Extension, Json};
use cluster_core::{ClusterControl, JobId};

use crate::csrf::Csrf;
use crate::error::WebResult;
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
    title: &str,
    current: &'static str,
) -> WebResult<Layout> {
    let user = current_user(env, headers).await?;
    Ok(Layout::new(
        env.config(),
        title,
        current,
        user.map(|u| u.username),
        csrf.0.clone(),
    ))
}

pub async fn dashboard<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    let events = env
        .cluster()
        .recent_events(env.config().event_log_limit)
        .await;
    page(&DashboardPage {
        layout: layout(&env, &headers, &csrf, "Dashboard", "/").await?,
        stats: Stats::from_snapshot(&snapshot),
        metrics: MetricsView::new(&env.metrics().snapshot()),
        events: events.iter().map(EventView::new).collect(),
    })
}

pub async fn cluster<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    let nodes = env.cluster().nodes().await;
    let events = env
        .cluster()
        .recent_events(env.config().event_log_limit)
        .await;
    let now = env.now();
    page(&ClusterPage {
        layout: layout(&env, &headers, &csrf, "Cluster", "/cluster").await?,
        stats: Stats::from_snapshot(&snapshot),
        nodes: nodes
            .iter()
            .map(|n| NodeView::new(n, now, &snapshot))
            .collect(),
        events: events.iter().map(EventView::new).collect(),
        debug_controls: env.config().debug_controls,
    })
}

pub async fn nodes<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    let nodes = env.cluster().nodes().await;
    let now = env.now();
    page(&NodesPage {
        layout: layout(&env, &headers, &csrf, "Nodes", "/nodes").await?,
        nodes: nodes
            .iter()
            .map(|n| NodeView::new(n, now, &snapshot))
            .collect(),
        debug_controls: env.config().debug_controls,
    })
}

pub async fn jobs<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let now = env.now();
    let jobs = env.cluster().jobs(JOB_LIST_LIMIT).await;
    page(&JobsPage {
        layout: layout(&env, &headers, &csrf, "Jobs", "/jobs").await?,
        jobs: jobs.iter().map(|j| JobRow::new(j, now)).collect(),
    })
}

pub async fn job_detail<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> WebResult<Html<String>> {
    let detail = env
        .cluster()
        .job(JobId(id))
        .await
        .ok_or(AppError::NotFound)?;
    let now = env.now();
    page(&JobDetailPage {
        layout: layout(&env, &headers, &csrf, &format!("Job {id}"), "/jobs").await?,
        detail: JobDetailView::new(&detail, now),
    })
}

pub async fn account<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
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
    page(&AccountPage {
        layout: Layout::new(
            env.config(),
            "Account",
            "/account",
            user.as_ref().map(|u| u.username.clone()),
            csrf.0.clone(),
        ),
        signed_in: user.is_some(),
        username: user.map(|u| u.username).unwrap_or_default(),
        linked,
    })
}

pub async fn wow<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    page(&WowPage {
        layout: layout(&env, &headers, &csrf, "WoW", "/wow").await?,
    })
}

/// Machine-readable snapshot, for scripts and future non-browser clients.
/// The browser uses the HTML fragments instead (CLAUDE.md 32).
pub async fn snapshot_json<E: Ports>(State(env): State<E>) -> Json<cluster_core::ClusterSnapshot> {
    Json(env.cluster().snapshot().await)
}

/// Request-side counters, for the same audience.
pub async fn metrics_json<E: Ports>(State(env): State<E>) -> Json<app_core::MetricsSnapshot> {
    Json(env.metrics().snapshot())
}
