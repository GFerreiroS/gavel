use app_core::AppError;
use askama::Template;
use axum::response::Html;

use crate::error::WebResult;

/// Render a template, turning a template failure into a 500 rather than a
/// panic.
pub(crate) fn page<T: Template>(template: &T) -> WebResult<Html<String>> {
    let body = template
        .render()
        .map_err(|e| AppError::internal(format!("template render failed: {e}")))?;
    Ok(Html(body))
}
