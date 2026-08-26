use app_core::AppError;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub type WebResult<T> = Result<T, WebError>;

/// Newtype so `AppError` (defined in `app-core`) can be turned into a
/// response here.
#[derive(Debug)]
pub struct WebError(pub AppError);

impl<E: Into<AppError>> From<E> for WebError {
    fn from(err: E) -> Self {
        WebError(err.into())
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // 5xx details go to the log, never to the browser.
        let message = if self.0.is_public() {
            self.0.to_string()
        } else {
            tracing::error!(error = %self.0, "request failed");
            "Something went wrong on our side.".to_string()
        };

        let body = format!(
            "<div class=\"alert alert-error\" role=\"alert\">{}</div>",
            html_escape(&message)
        );
        (status, Html(body)).into_response()
    }
}

/// Minimal escaping for the few places that build HTML outside a template.
/// Everything else goes through Askama, which escapes by default.
pub(crate) fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}
