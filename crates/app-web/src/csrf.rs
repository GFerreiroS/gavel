//! Double-submit CSRF protection.
//!
//! A random token is issued in a cookie and echoed back in every state-changing
//! request, either as a hidden form field or as the `X-CSRF-Token` header that
//! HTMX sends from `hx-headers` on `<body>`. An attacker's page can cause the
//! cookie to be sent but cannot read it, so it cannot produce a matching token.

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use app_core::AppError;

use crate::error::WebResult;
use crate::session::cookie_value;

pub const CSRF_COOKIE: &str = "wow_tracker_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// The token for the current request, injected as a request extension.
#[derive(Debug, Clone)]
pub struct Csrf(pub String);

impl Csrf {
    /// Constant-time-ish comparison. Tokens are random 256-bit hex, so a
    /// length check plus a byte fold is sufficient here.
    pub fn verify(&self, presented: &str) -> WebResult<()> {
        let a = self.0.as_bytes();
        let b = presented.as_bytes();
        let equal =
            a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
        if equal {
            Ok(())
        } else {
            Err(AppError::Forbidden.into())
        }
    }

    /// Verify against whichever channel the client used.
    pub fn verify_request(&self, headers: &HeaderMap, form_token: Option<&str>) -> WebResult<()> {
        if let Some(token) = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) {
            return self.verify(token);
        }
        match form_token {
            Some(token) => self.verify(token),
            None => Err(AppError::Forbidden.into()),
        }
    }
}

/// Issue the CSRF cookie when the browser does not already have one.
pub async fn layer(mut request: Request, next: Next) -> Response {
    let existing = cookie_value(request.headers(), CSRF_COOKIE);
    let (token, issue) = match existing {
        Some(token) => (token, false),
        None => (new_token(), true),
    };

    request.extensions_mut().insert(Csrf(token.clone()));
    let mut response = next.run(request).await;

    if issue
        && let Ok(value) = HeaderValue::from_str(&format!(
            "{CSRF_COOKIE}={token}; Path=/; SameSite=Lax; Max-Age=86400"
        ))
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

fn new_token() -> String {
    // Same source and encoding as the session tokens, and allocating once
    // rather than per byte: this runs on the first request of every visit.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness unavailable");
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
