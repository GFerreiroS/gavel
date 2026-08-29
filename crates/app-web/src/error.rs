//! Turning an [`AppError`] into a response, in the reader's language.
//!
//! The awkward part is that `IntoResponse` takes nothing but `self`. There is
//! no request in scope, so there is no locale, so for a long time every error
//! in this app rendered in English on an otherwise fully translated page --
//! the one sentence a visitor reads at the worst possible moment.
//!
//! So the error travels a little further than the body does: `into_response`
//! renders the English and attaches the [`Message`] to the response, and
//! [`layer`] -- which runs inside `prefs` and therefore does know the
//! locale -- swaps the sentence for its translation on the way out.

use app_core::AppError;
use app_core::error::Message;
use app_core::locale::{DEFAULT_LOCALE, Locale};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};

use crate::prefs::MarketPrefs;

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

        // 5xx details go to the log. `AppError::message` already refuses to
        // put them on a page; this is where they are kept instead of lost.
        if !self.0.is_public() {
            tracing::error!(error = %self.0, "request failed");
        }

        let message = self.0.message();
        let mut response = (status, Html(alert(&message, DEFAULT_LOCALE))).into_response();
        response.extensions_mut().insert(message);
        response
    }
}

/// Re-render an error body in the language the page is being read in.
///
/// Placed inside `prefs`, which is what puts [`MarketPrefs`] on the request;
/// outside the handlers, which is where the error is produced. A response
/// carrying no [`Message`] is not an error and passes straight through.
pub async fn layer(request: Request, next: Next) -> Response {
    let locale = request
        .extensions()
        .get::<MarketPrefs>()
        .map_or(DEFAULT_LOCALE, |prefs| prefs.locale);

    let mut response = next.run(request).await;
    let Some(message) = response.extensions_mut().remove::<Message>() else {
        return response;
    };
    if locale == DEFAULT_LOCALE {
        return response;
    }

    let body = alert(&message, locale);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, body.len().into());
    *response.body_mut() = Body::from(body);
    response
}

/// The one shape an error takes on screen. Templates render everything else;
/// this is built by hand because it has to exist even when a template failed.
fn alert(message: &Message, locale: Locale) -> String {
    let sentence = message.render(crate::i18n::translate(locale, message.source));
    format!(
        "<div class=\"alert alert-error\" role=\"alert\">{}</div>",
        html_escape(&sentence)
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::error::text;

    #[test]
    fn an_error_renders_as_one_alert_in_english() {
        let body = alert(&Message::new(text::UNAUTHORIZED), Locale::EnGb);
        assert_eq!(
            body,
            "<div class=\"alert alert-error\" role=\"alert\">invalid username or password</div>"
        );
    }

    #[test]
    fn a_translated_error_keeps_its_values() {
        let body = alert(&Message::with(text::TOO_MANY_SIGN_INS, [5]), Locale::EsEs);
        assert!(body.contains('5'), "the value survives translation: {body}");
        assert!(
            !body.contains("too many sign-in attempts"),
            "Spanish must not fall through to the source string: {body}"
        );
    }

    /// User input reaches this path -- `unknown role '<script>'` -- and it is
    /// the one HTML in this app that no template escapes for us.
    #[test]
    fn values_substituted_into_a_message_are_escaped() {
        let body = alert(
            &Message::with(text::UNKNOWN_ROLE, ["<script>alert(1)</script>"]),
            Locale::EnGb,
        );
        assert!(!body.contains("<script>"), "{body}");
        assert!(body.contains("&lt;script&gt;"), "{body}");
    }

    #[test]
    fn a_five_hundred_says_nothing_about_itself() {
        let response = WebError(AppError::internal("password = hunter2")).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let message = response.extensions().get::<Message>().expect("attached");
        assert_eq!(message.source, text::INTERNAL);
    }
}
