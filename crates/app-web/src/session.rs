//! Cookie parsing and session lookup.
//!
//! No cookie crate: two helpers are cheaper than a dependency, and the format
//! is fixed.

use app_core::auth::{AuthService, SESSION_COOKIE, SESSION_TTL_MS};
use app_core::error::AppError;
use app_core::model::User;
use app_core::repo::Store;
use app_core::{Ports, WebConfig};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::error::WebResult;

/// With `Secure` available, the browser-enforced version of the session
/// cookie's name.
pub const SESSION_COOKIE_HOST: &str = "__Host-wow_tracker_session";

/// What this deployment calls the session cookie.
///
/// `__Host-` is only legal on a `Secure`, `Path=/`, no-`Domain` cookie, so the
/// prefix follows `--secure-cookies`. Where it applies it is the browser
/// refusing to let any other origin -- a sibling subdomain, a plain-HTTP
/// impostor -- write this name, which is a guarantee no amount of care on
/// this side of the wire can produce.
pub fn cookie_name(config: &WebConfig) -> &'static str {
    if config.secure_cookies {
        SESSION_COOKIE_HOST
    } else {
        SESSION_COOKIE
    }
}

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
    let Some(token) = cookie_value(headers, cookie_name(env.config())) else {
        return Ok(None);
    };
    let store = env.store();
    let auth = AuthService::new(store.users(), store.sessions(), env.hasher(), env.tokens());
    Ok(auth.authenticate(&token, env.now()).await?)
}

/// Refuse anything but an administrator.
///
/// **Not found, not forbidden.** A 403 confirms the page exists to whoever
/// asked, and these pages describe how the deployment is doing; a visitor who
/// was guessing learns nothing from a 404.
///
/// The one exception is `/`, which everybody visits: rather than a 404 on the
/// front door, a non-admin is sent to the auction house, which is what they
/// came for.
pub async fn admin_only<E: Ports>(State(env): State<E>, request: Request, next: Next) -> Response {
    let admin = match current_user(&env, request.headers()).await {
        Ok(user) => has_admin_access(user.as_ref()),
        // A store that cannot answer is not an authorisation to proceed.
        Err(e) => {
            tracing::warn!(error = ?e, "could not resolve the session; refusing admin access");
            false
        }
    };
    if admin {
        return next.run(request).await;
    }
    refuse(request.uri().path())
}

fn has_admin_access(user: Option<&User>) -> bool {
    user.is_some_and(|user| user.is_admin)
}

/// What somebody who is not an administrator gets.
///
/// Split out so the rule can be asserted rather than described: 404 for every
/// operations page, and the one deliberate exception on the front door.
fn refuse(path: &str) -> Response {
    if path == "/" {
        return Redirect::to("/wow/auctions").into_response();
    }
    crate::error::WebError::from(AppError::NotFound).into_response()
}

pub fn session_cookie(token: &str, config: &WebConfig) -> HeaderValue {
    let secure = if config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let max_age = SESSION_TTL_MS / 1000;
    let name = cookie_name(config);
    HeaderValue::from_str(&format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .expect("session cookie is ASCII")
}

pub fn cleared_session_cookie(config: &WebConfig) -> HeaderValue {
    let secure = if config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let name = cookie_name(config);
    HeaderValue::from_str(&format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"
    ))
    .expect("session cookie is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn only_an_administrator_passes_the_operations_gate() {
        let ordinary = User {
            id: 1,
            username: "ordinary".into(),
            created_at: cluster_core::Millis(1),
            is_admin: false,
        };
        let admin = User {
            is_admin: true,
            ..ordinary.clone()
        };
        assert!(!has_admin_access(None), "a visitor is refused");
        assert!(
            !has_admin_access(Some(&ordinary)),
            "a normal user is refused"
        );
        assert!(
            has_admin_access(Some(&admin)),
            "an administrator is admitted"
        );
    }

    /// A 403 confirms the page is there. These pages describe how the
    /// deployment is doing, and somebody guessing must learn nothing.
    #[test]
    fn operations_pages_are_missing_rather_than_forbidden() {
        for path in [
            "/cluster",
            "/nodes",
            "/jobs",
            "/jobs/7",
            "/admin",
            "/api/cluster",
            "/api/metrics",
            "/partials/stats",
            "/partials/nodes",
            "/debug/nodes/1/stop",
        ] {
            assert_eq!(refuse(path).status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// The front door is the exception: everybody visits it, and it should
    /// open on the thing the app is for rather than deny it exists.
    #[test]
    fn the_front_door_sends_a_visitor_to_the_auction_house() {
        let response = refuse("/");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/wow/auctions");
    }

    #[test]
    fn the_host_prefix_follows_secure_cookies() {
        let mut config = WebConfig {
            secure_cookies: false,
            ..WebConfig::default()
        };
        assert_eq!(cookie_name(&config), SESSION_COOKIE);
        assert!(
            !session_cookie("t", &config)
                .to_str()
                .unwrap()
                .contains("Secure")
        );

        config.secure_cookies = true;
        assert_eq!(cookie_name(&config), SESSION_COOKIE_HOST);
        let cookie = session_cookie("t", &config);
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.starts_with("__Host-"), "{cookie}");
        assert!(cookie.contains("; Secure"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(
            !cookie.contains("Domain="),
            "__Host- forbids Domain: {cookie}"
        );
    }

    /// Signing out has to clear the same name it set, or the browser keeps
    /// sending a cookie nothing overwrites.
    #[test]
    fn clearing_a_session_targets_the_name_that_was_set() {
        for secure_cookies in [false, true] {
            let config = WebConfig {
                secure_cookies,
                ..WebConfig::default()
            };
            let name = cookie_name(&config);
            let cleared = cleared_session_cookie(&config);
            let cleared = cleared.to_str().unwrap();
            assert!(cleared.starts_with(&format!("{name}=;")), "{cleared}");
            assert!(cleared.contains("Max-Age=0"), "{cleared}");
        }
    }

    #[test]
    fn a_cookie_is_read_out_of_a_crowded_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; wow_tracker_session=abc; b=2"),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE),
            Some("abc".to_string())
        );
        assert_eq!(cookie_value(&headers, SESSION_COOKIE_HOST), None);
    }
}
