use app_core::{AppError, Ports};
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::{ClusterControl, NodeId, Role};
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::render::page;
use crate::routes::partials;

#[derive(Debug, Deserialize)]
pub struct RoleForm {
    pub role: String,
    /// Present and "true" to add the role, absent or "false" to remove it.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub csrf_token: String,
}

/// Change a node's roles at runtime without changing its identity
/// (CLAUDE.md 19).
pub async fn set_role<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    Path(id): Path<u16>,
    axum::Form(form): axum::Form<RoleForm>,
) -> WebResult<Html<String>> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let role = Role::parse(&form.role)
        .ok_or_else(|| AppError::validation(format!("unknown role '{}'", form.role)))?;
    env.cluster()
        .set_role(NodeId(id), role, form.enabled)
        .await?;

    page(&partials::nodes_fragment(&env).await)
}
