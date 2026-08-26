use app_core::Ports;
use app_core::service::JobService;
use axum::Extension;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Html;
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::render::page;
use crate::routes::partials;

#[derive(Debug, Deserialize)]
pub struct SubmitForm {
    pub kind: String,
    pub size: u64,
    pub tasks: u16,
    #[serde(default)]
    pub csrf_token: String,
}

/// `POST /jobs` -> the refreshed job list fragment (CLAUDE.md 32).
pub async fn submit<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<SubmitForm>,
) -> WebResult<Html<String>> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let jobs = JobService::new(env.cluster());
    let id = jobs.submit(&form.kind, form.size, form.tasks).await?;
    tracing::info!(job = %id, kind = %form.kind, tasks = form.tasks, "job submitted");

    page(&partials::jobs_fragment(&env).await)
}
