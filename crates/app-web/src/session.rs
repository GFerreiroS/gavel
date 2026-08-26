//! Cookie parsing and session lookup.
//!
//! No cookie crate: two helpers are cheaper than a dependency, and the format
//! is fixed.

use app_core::auth::{AuthService, SESSION_COOKIE, SESSION_TTL_MS};
use app_core::model::User;
use app_core::repo::Store;
use app_core::{Ports, WebConfig};
use axum::http::{HeaderMap, HeaderValue, header};

use crate::error::WebResult;

/// Value of one cookie from the request headers.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().to_string())
}

/// The signed-in user, or `None`. An unknown or expired session is simply
/// treated as signed out.
pub async fn current_user<E: Ports>(env: &E, headers: &HeaderMap) -> WebResult<Option<User>> {
    let Some(token) = cookie_value(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    let store = env.store();
    let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
    Ok(auth.authenticate(&token, env.now()).await?)
}

pub fn session_cookie(token: &str, config: &WebConfig) -> HeaderValue {
    let secure = if config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let max_age = SESSION_TTL_MS / 1000;
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .expect("session cookie is ASCII")
}

pub fn cleared_session_cookie(config: &WebConfig) -> HeaderValue {
    let secure = if config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"
    ))
    .expect("session cookie is ASCII")
}
