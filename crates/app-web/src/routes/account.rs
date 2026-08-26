//! Registration, login and logout.

use app_core::Ports;
use app_core::auth::AuthService;
use app_core::repo::Store;
use axum::Extension;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::session::{cleared_session_cookie, cookie_value, session_cookie};

#[derive(Debug, Deserialize)]
pub struct CredentialsForm {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// `HX-Redirect` makes HTMX perform a full navigation, which is what should
/// happen after a successful sign-in.
fn redirect_to(path: &'static str, cookie: Option<HeaderValue>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("HX-Redirect", HeaderValue::from_static(path));
    headers.insert(header::LOCATION, HeaderValue::from_static(path));
    if let Some(cookie) = cookie {
        headers.insert(header::SET_COOKIE, cookie);
    }
    (axum::http::StatusCode::SEE_OTHER, headers).into_response()
}

pub async fn register<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CredentialsForm>,
) -> WebResult<Response> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let store = env.store();
    let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
    let user = auth
        .register(&form.username, &form.password, env.now())
        .await?;
    tracing::info!(user = %user.username, "user registered");

    // Registering signs you in.
    let session = auth
        .login(&form.username, &form.password, env.now())
        .await?;
    Ok(redirect_to(
        "/account",
        Some(session_cookie(&session.id, env.config())),
    ))
}

pub async fn login<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CredentialsForm>,
) -> WebResult<Response> {
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let store = env.store();
    let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
    let session = auth
        .login(&form.username, &form.password, env.now())
        .await?;
    tracing::info!(user = %form.username, "user signed in");

    Ok(redirect_to(
        "/account",
        Some(session_cookie(&session.id, env.config())),
    ))
}

/// No body to parse, so CSRF comes from the header HTMX always sends.
pub async fn logout<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
) -> WebResult<Response> {
    csrf.verify_request(&headers, None)?;

    if let Some(token) = cookie_value(&headers, app_core::auth::SESSION_COOKIE) {
        let store = env.store();
        let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
        auth.logout(&token).await?;
    }
    Ok(redirect_to(
        "/account",
        Some(cleared_session_cookie(env.config())),
    ))
}
