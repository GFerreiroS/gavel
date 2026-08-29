//! Registration, login and logout.

use std::sync::Arc;

use app_core::Ports;
use app_core::auth::AuthService;
use app_core::error::{Message, text};
use app_core::repo::Store;
use axum::Extension;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::session::{cleared_session_cookie, cookie_name, cookie_value, session_cookie};
use crate::throttle::{LoginThrottle, SignUpThrottle, WINDOW_MS};

#[derive(Debug, Deserialize)]
pub struct CredentialsForm {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub csrf_token: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct LogoutForm {
    #[serde(default)]
    pub csrf_token: String,
}

/// Header HTMX puts on every request it makes.
fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Send the browser to `path` after a successful sign-in, registration or
/// sign-out.
///
/// **One mechanism per client, never both.** Answering an HTMX request with
/// `303 + Location + HX-Redirect` looks belt-and-braces and is in fact broken:
/// HTMX rides on `XMLHttpRequest`, which follows a redirect transparently, so
/// HTMX never sees the 303 at all -- it sees the 200 that came back from
/// `GET /account`, whose headers carry no `HX-Redirect`, and swaps a whole
/// `<!doctype html>` document into whatever `hx-target` named. The session
/// cookie was set, the navigation never happened, and the page still said
/// "Sign in" until it was reloaded by hand.
///
/// So: `204` plus the header for HTMX, which navigates on the header before
/// any swap can happen; a plain `303` for a browser posting the form without
/// JavaScript, which follows `Location` itself.
fn redirect_to(path: &'static str, htmx: bool, cookie: Option<HeaderValue>) -> Response {
    let mut headers = HeaderMap::new();
    let status = if htmx {
        headers.insert("HX-Redirect", HeaderValue::from_static(path));
        StatusCode::NO_CONTENT
    } else {
        headers.insert(header::LOCATION, HeaderValue::from_static(path));
        StatusCode::SEE_OTHER
    };
    if let Some(cookie) = cookie {
        headers.insert(header::SET_COOKIE, cookie);
    }
    (status, headers).into_response()
}

pub async fn register<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(sign_ups): Extension<Arc<SignUpThrottle>>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CredentialsForm>,
) -> WebResult<Response> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let now = env.now();
    // Counted before the username is looked up, because this endpoint answers
    // "does this account exist" -- a signup form has to -- and the thing that
    // must not happen is it answering ten thousand times. It is also the only
    // unauthenticated way to make this server hash a password.
    if !sign_ups.take(now) {
        tracing::warn!("registration throttled");
        return Err(app_core::AppError::TooManyRequests(Message::with(
            text::TOO_MANY_SIGN_UPS,
            [WINDOW_MS / 60_000],
        ))
        .into());
    }

    let store = env.store();
    let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
    let user = auth.register(&form.username, &form.password, now).await?;
    tracing::info!(user = %user.username, "user registered");

    // Registering signs you in -- on the strength of having just created the
    // account, not by verifying the password against the hash this call made
    // two lines ago. That second Argon2 pass proved nothing and doubled what
    // one unauthenticated request cost.
    let session = auth.start_session(&user, now).await?;
    Ok(redirect_to(
        "/account",
        is_htmx(&headers),
        Some(session_cookie(&session.id, env.config())),
    ))
}

pub async fn login<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(throttle): Extension<Arc<LoginThrottle>>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CredentialsForm>,
) -> WebResult<Response> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let now = env.now();
    // Before the hash, not after: a refused attempt has to be cheap, or the
    // limit is a slower way of spending the same CPU.
    if !throttle.allows(&form.username, now) {
        tracing::warn!(user = %form.username, "sign-in throttled");
        return Err(app_core::AppError::TooManyRequests(Message::with(
            text::TOO_MANY_SIGN_INS,
            [WINDOW_MS / 60_000],
        ))
        .into());
    }

    let store = env.store();
    let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
    let session = match auth.login(&form.username, &form.password, now).await {
        Ok(session) => {
            throttle.succeeded(&form.username);
            session
        }
        Err(e) => {
            // Only a rejected password counts. A store that is down is not
            // the caller's doing and must not lock them out of their account.
            if matches!(e, app_core::AppError::Unauthorized) {
                throttle.failed(&form.username, now);
            }
            return Err(e.into());
        }
    };
    tracing::info!(user = %form.username, "user signed in");

    Ok(redirect_to(
        "/account",
        is_htmx(&headers),
        Some(session_cookie(&session.id, env.config())),
    ))
}

/// CSRF from whichever channel the client has: the `X-CSRF-Token` header when
/// HTMX made the request, the hidden field when the browser posted the form on
/// its own. Reading the field as well as the header is what lets the sign-out
/// button work with JavaScript switched off; before this it was a hidden input
/// nothing ever looked at.
pub async fn logout<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<LogoutForm>,
) -> WebResult<Response> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    if let Some(token) = cookie_value(&headers, cookie_name(env.config())) {
        let store = env.store();
        let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
        auth.logout(&token).await?;
    }
    Ok(redirect_to(
        "/account",
        is_htmx(&headers),
        Some(cleared_session_cookie(env.config())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(response: &Response, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// The bug this function exists for: an XHR follows a 3xx before HTMX can
    /// read anything off it, so an HTMX response must carry the header and no
    /// redirect at all. A `Location` here means the browser fetches the target
    /// and HTMX swaps a whole page into the error slot.
    #[test]
    fn an_htmx_redirect_is_a_header_and_never_a_location() {
        let response = redirect_to("/account", true, None);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            header(&response, "HX-Redirect").as_deref(),
            Some("/account")
        );
        assert_eq!(header(&response, "location"), None);
    }

    /// And the other way round for a form posted without JavaScript: the
    /// browser follows `Location` itself and has never heard of `HX-Redirect`.
    #[test]
    fn a_plain_form_post_gets_a_see_other() {
        let response = redirect_to("/account", false, None);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(header(&response, "location").as_deref(), Some("/account"));
        assert_eq!(header(&response, "HX-Redirect"), None);
    }

    #[test]
    fn the_session_cookie_rides_along_either_way() {
        let cookie = HeaderValue::from_static("s=1; Path=/");
        for htmx in [true, false] {
            let response = redirect_to("/account", htmx, Some(cookie.clone()));
            assert_eq!(
                header(&response, "set-cookie").as_deref(),
                Some("s=1; Path=/")
            );
        }
    }

    #[test]
    fn only_the_header_htmx_actually_sends_counts_as_htmx() {
        let mut headers = HeaderMap::new();
        assert!(!is_htmx(&headers));

        headers.insert("HX-Request", HeaderValue::from_static("true"));
        assert!(is_htmx(&headers));

        headers.insert("HX-Request", HeaderValue::from_static("TRUE"));
        assert!(is_htmx(&headers));

        // htmx sets this to "true" for a boosted request too; anything else is
        // not htmx asking.
        headers.insert("HX-Request", HeaderValue::from_static("false"));
        assert!(!is_htmx(&headers));
    }
}
