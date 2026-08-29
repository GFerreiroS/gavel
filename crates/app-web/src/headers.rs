//! Response headers every page gets, whoever asked for it.
//!
//! A reverse proxy could add most of these, and on a real deployment it
//! probably also will. It is still the app's job: `cargo run` is the whole
//! story for a local machine (there is no proxy in front of it), and a header
//! that only exists in a Caddyfile is a header nobody notices going missing.

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// What the browser is allowed to load.
///
/// * `script-src 'self'` is the valuable half and it is achievable here: every
///   script is a file this binary serves. Keep it that way -- an inline
///   `<script>` anywhere in the templates silently needs a hash or a nonce,
///   and the usual repair is `'unsafe-inline'`, which gives the whole policy
///   away.
/// * `style-src` does allow inline, because a handful of cards carry a
///   computed `style="width: {{ n }}%"`. Those values come from the view
///   models, not from a request.
/// * Item icons come from Blizzard's renderer and character portraits from
///   Raider.IO, which serves them off the same regional hosts.
/// * `frame-ancestors 'none'` is the clickjacking control that matters; the
///   `X-Frame-Options` below repeats it for anything too old to read a CSP.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
     script-src 'self'; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data: https://render.worldofwarcraft.com https://*.worldofwarcraft.com; \
     connect-src 'self'; \
     form-action 'self'; \
     frame-ancestors 'none'; \
     base-uri 'none'; \
     object-src 'none'";

pub async fn layer(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    apply(response.headers_mut());
    response
}

/// Split out from the middleware so it can be asserted on without standing up
/// a service: what matters here is the exact set of headers, not the plumbing.
fn apply(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );

    // Every HTML response here depends on who is asking -- the nav alone says
    // whether you are signed in and whether you are the administrator -- so
    // none of it may be stored. The static assets set their own immutable
    // `max-age` and are left exactly as they are.
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn every_response_is_told_what_it_may_load_and_who_may_frame_it() {
        let mut headers = HeaderMap::new();
        apply(&mut headers);

        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(
            headers[header::REFERRER_POLICY],
            "strict-origin-when-cross-origin"
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    /// `script-src 'self'` with nothing loosening it is the half of the policy
    /// that is worth having, and the half an inline `<script>` would cost.
    #[test]
    fn the_policy_allows_no_inline_or_remote_script() {
        let mut headers = HeaderMap::new();
        apply(&mut headers);
        let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();

        assert!(policy.contains("script-src 'self';"), "{policy}");
        assert!(!policy.contains("unsafe-eval"), "{policy}");
        assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
        assert!(policy.contains("object-src 'none'"), "{policy}");
        assert!(policy.contains("base-uri 'none'"), "{policy}");

        // `style-src` is the one directive that allows inline, for the handful
        // of computed `style="width: N%"` attributes. Nothing else may.
        let inline: Vec<&str> = policy
            .split(';')
            .map(str::trim)
            .filter(|d| d.contains("'unsafe-inline'"))
            .collect();
        assert_eq!(inline.len(), 1, "only style-src may allow inline: {policy}");
        assert!(inline[0].starts_with("style-src"), "{policy}");
    }

    /// The static assets are content-hashed and set their own year-long
    /// `immutable`. Stamping `no-store` over it would throw that away on every
    /// asset on every page.
    #[test]
    fn an_asset_keeps_the_caching_it_asked_for() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        apply(&mut headers);
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
    }
}
