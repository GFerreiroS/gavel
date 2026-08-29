use app_core::AppError;
use app_core::locale::Locale;
use askama::Template;
use axum::response::Html;

use crate::error::WebResult;
use crate::i18n;

/// Render a template in one language, turning a template failure into a 500
/// rather than a panic.
///
/// The locale travels as an Askama *value* rather than as a field on every
/// template struct, so a partial three levels down can translate a string
/// without anyone having threaded it there.
pub(crate) fn page<T: Template>(template: &T, locale: Locale) -> WebResult<Html<String>> {
    let body = template
        .render_with_values(&i18n::Ctx { locale })
        .map_err(|e| AppError::internal(format!("template render failed: {e}")))?;
    Ok(Html(body))
}
