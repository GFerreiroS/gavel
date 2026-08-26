//! HTMX fragments. Same view models as the pages, rendered without the layout.

use app_core::{AppError, Ports};
use askama::Template;
use axum::extract::{Path, State};
use axum::response::Html;
use cluster_core::{ClusterControl, JobId};

use crate::error::WebResult;
use crate::render::page;
use crate::views::{EventView, JobDetailView, JobRow, MetricsView, NodeView, Stats};

#[derive(Template)]
#[template(path = "partials/stats.html")]
pub struct StatsFragment {
    pub stats: Stats,
}

#[derive(Template)]
#[template(path = "partials/nodes.html")]
pub struct NodesFragment {
    pub nodes: Vec<NodeView>,
    pub debug_controls: bool,
}

#[derive(Template)]
#[template(path = "partials/metrics.html")]
pub struct MetricsFragment {
    pub metrics: MetricsView,
}

#[derive(Template)]
#[template(path = "partials/events.html")]
pub struct EventsFragment {
    pub events: Vec<EventView>,
}

#[derive(Template)]
#[template(path = "partials/jobs.html")]
pub struct JobsFragment {
    pub jobs: Vec<JobRow>,
}

#[derive(Template)]
#[template(path = "partials/job_detail.html")]
pub struct JobDetailFragment {
    pub detail: JobDetailView,
}

pub async fn stats<E: Ports>(State(env): State<E>) -> WebResult<Html<String>> {
    let snapshot = env.cluster().snapshot().await;
    page(&StatsFragment {
        stats: Stats::from_snapshot(&snapshot),
    })
}

pub async fn nodes<E: Ports>(State(env): State<E>) -> WebResult<Html<String>> {
    page(&nodes_fragment(&env).await)
}

/// Shared with the role-toggle and debug handlers so a control click swaps in
/// a freshly rendered node list.
pub async fn nodes_fragment<E: Ports>(env: &E) -> NodesFragment {
    let snapshot = env.cluster().snapshot().await;
    let nodes = env.cluster().nodes().await;
    let now = env.now();
    NodesFragment {
        nodes: nodes
            .iter()
            .map(|n| NodeView::new(n, now, &snapshot))
            .collect(),
        debug_controls: env.config().debug_controls,
    }
}

pub async fn metrics<E: Ports>(State(env): State<E>) -> WebResult<Html<String>> {
    page(&MetricsFragment {
        metrics: MetricsView::new(&env.metrics().snapshot()),
    })
}

pub async fn events<E: Ports>(State(env): State<E>) -> WebResult<Html<String>> {
    let events = env
        .cluster()
        .recent_events(env.config().event_log_limit)
        .await;
    page(&EventsFragment {
        events: events.iter().map(EventView::new).collect(),
    })
}

pub async fn jobs<E: Ports>(State(env): State<E>) -> WebResult<Html<String>> {
    page(&jobs_fragment(&env).await)
}

pub async fn jobs_fragment<E: Ports>(env: &E) -> JobsFragment {
    let now = env.now();
    let jobs = env.cluster().jobs(50).await;
    JobsFragment {
        jobs: jobs.iter().map(|j| JobRow::new(j, now)).collect(),
    }
}

pub async fn job_detail<E: Ports>(
    State(env): State<E>,
    Path(id): Path<u64>,
) -> WebResult<Html<String>> {
    let detail = env
        .cluster()
        .job(JobId(id))
        .await
        .ok_or(AppError::NotFound)?;
    page(&JobDetailFragment {
        detail: JobDetailView::new(&detail, env.now()),
    })
}
