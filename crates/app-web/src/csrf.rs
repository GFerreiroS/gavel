//! Double-submit CSRF protection.
//!
//! A random token is issued in a cookie and echoed back in every state-changing
//! request, either as a hidden form field or as the `X-CSRF-Token` header that
//! HTMX sends from `hx-headers` on `<body>`. An attacker's page can cause the
//! cookie to be sent but cannot read it, so it cannot produce a matching token.
//!
//! Two things make that argument hold up:
//!
//! * **The cookie takes the `__Host-` prefix** wherever `Secure` is on. Plain
//!   double-submit assumes nobody else can *write* the cookie, and a sibling
//!   subdomain can: `evil.example.com` may set a cookie for `example.com`, and
//!   then it knows the token it just planted. `__Host-` is the browser
//!   refusing exactly that.
//! * **What the page renders is masked**, freshly per response. The token
//!   itself is a fixed secret sitting in the same compressed response as a
//!   search box that echoes back whatever was typed into it, which is the
//!   shape BREACH needs. Masking makes the bytes different every time, so
//!   there is nothing for a compression-size oracle to converge on.

use app_core::auth::SESSION_TTL_MS;
use app_core::{AppError, Ports, WebConfig};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::WebResult;
use crate::session::cookie_value;

/// The cookie's name on a plain-HTTP deployment. See [`cookie_name`].
pub const CSRF_COOKIE: &str = "wow_tracker_csrf";
/// With `Secure` available, the browser-enforced version of the same name.
pub const CSRF_COOKIE_HOST: &str = "__Host-wow_tracker_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Raw token length in bytes, and therefore the length of everything derived
/// from it: 64 hex characters in the cookie, 128 in a masked page value.
const TOKEN_BYTES: usize = 32;

/// `__Host-` is only legal on a cookie that is `Secure`, `Path=/` and carries
/// no `Domain` -- the browser rejects the cookie outright otherwise, which on
/// a plain-HTTP dev machine would mean no CSRF cookie at all and a 403 on
/// every form. So the prefix follows `--secure-cookies`, like the flag does.
pub fn cookie_name(config: &WebConfig) -> &'static str {
    if config.secure_cookies {
        CSRF_COOKIE_HOST
    } else {
        CSRF_COOKIE
    }
}

/// The token for the current request, injected as a request extension.
///
/// Holds the raw token -- what the cookie carries. What a page shows is
/// [`Csrf::masked`], and what arrives back is unmasked before comparison.
#[derive(Debug, Clone)]
pub struct Csrf(pub String);

impl Csrf {
    /// A fresh masking of this token, for rendering into a page.
    ///
    /// `pad || (pad XOR token)`, hex, with a new pad every time it is called.
    /// Any number of these decode back to the one token, so a page may carry
    /// as many as it has forms.
    pub fn masked(&self) -> String {
        let Some(token) = from_hex(&self.0) else {
            // An unreadable cookie cannot be masked into anything that will
            // verify. Returning the raw value keeps the page rendering; the
            // submission fails the check, and the next request is issued a
            // fresh cookie by `layer`.
            return self.0.clone();
        };
        let mut pad = [0u8; TOKEN_BYTES];
        if getrandom::fill(&mut pad).is_err() {
            return self.0.clone();
        }
        let masked: Vec<u8> = pad.iter().zip(&token).map(|(p, t)| p ^ t).collect();
        let mut out = to_hex(&pad);
        out.push_str(&to_hex(&masked));
        out
    }

    /// Check a value a client sent back, masked or not.
    ///
    /// Constant time over the comparison itself: the tokens are fixed-length
    /// random, so a length check plus a byte fold is the whole of it.
    pub fn verify(&self, presented: &str) -> WebResult<()> {
        let Some(expected) = from_hex(&self.0) else {
            return Err(AppError::Forbidden.into());
        };
        let Some(actual) = unmask(presented) else {
            return Err(AppError::Forbidden.into());
        };
        let equal = expected.len() == actual.len()
            && expected
                .iter()
                .zip(&actual)
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0;
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

/// Recover the raw token from a masked page value.
///
/// A bare token is accepted too. Not for the browser's sake -- every page this
/// app renders is masked -- but so that `curl` against a POST endpoint stays
/// usable with the value straight out of the cookie.
fn unmask(presented: &str) -> Option<Vec<u8>> {
    match presented.len() {
        n if n == TOKEN_BYTES * 2 => from_hex(presented),
        n if n == TOKEN_BYTES * 4 => {
            let (pad, masked) = presented.split_at(TOKEN_BYTES * 2);
            let pad = from_hex(pad)?;
            let masked = from_hex(masked)?;
            Some(pad.iter().zip(&masked).map(|(p, m)| p ^ m).collect())
        }
        _ => None,
    }
}

/// Issue the CSRF cookie when the browser does not already have one.
///
/// `HttpOnly`, because nothing reads this cookie from JavaScript: the value a
/// page submits is masked into the form and into `hx-headers` server-side.
/// `Secure` and the `__Host-` prefix follow the session cookie, so a
/// deployment that has TLS gets the browser's own guarantee that no other
/// origin planted this.
///
/// It lives exactly as long as a session. It used to expire after a day, which
/// meant a tab left open overnight submitted a token the middleware had
/// already replaced: a 403 on a sign-out that looked, in the browser, like the
/// button doing nothing.
pub async fn layer<E: Ports>(State(env): State<E>, mut request: Request, next: Next) -> Response {
    let name = cookie_name(env.config());
    // A cookie that is not a well-formed token is treated as absent rather
    // than trusted: it is either a leftover from another format or something
    // somebody else wrote.
    let existing = cookie_value(request.headers(), name).filter(|v| from_hex(v).is_some());
    let (token, issue) = match existing {
        Some(token) => (token, false),
        None => (new_token(), true),
    };

    request.extensions_mut().insert(Csrf(token.clone()));
    let mut response = next.run(request).await;

    let secure = if env.config().secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let max_age = SESSION_TTL_MS / 1000;
    if issue
        && let Ok(value) = HeaderValue::from_str(&format!(
            "{name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
        ))
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

fn new_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).expect("OS randomness unavailable");
    to_hex(&bytes)
}

/// Same encoding as the session tokens, and allocating once rather than per
/// byte: this runs on the first request of every visit.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    let digits = text.as_bytes();
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> Csrf {
        Csrf(new_token())
    }

    #[test]
    fn a_token_verifies_against_itself() {
        let csrf = token();
        assert!(csrf.verify(&csrf.0).is_ok());
    }

    #[test]
    fn a_masked_value_verifies_and_a_foreign_one_does_not() {
        let csrf = token();
        assert!(csrf.verify(&csrf.masked()).is_ok());
        assert!(csrf.verify(&token().masked()).is_err());
        assert!(csrf.verify(&token().0).is_err());
    }

    /// The point of masking: two renderings of one page put different bytes on
    /// the wire, so response size tells a compression oracle nothing.
    #[test]
    fn masking_a_token_twice_gives_two_different_values() {
        let csrf = token();
        let first = csrf.masked();
        let second = csrf.masked();
        assert_ne!(first, second);
        assert_eq!(first.len(), TOKEN_BYTES * 4);
        assert!(csrf.verify(&first).is_ok());
        assert!(csrf.verify(&second).is_ok());
    }

    #[test]
    fn rubbish_is_refused_rather_than_parsed() {
        let csrf = token();
        let short = csrf.0[..62].to_string();
        let zs = "z".repeat(64);
        let long = "a".repeat(127);
        for presented in ["", "x", &zs, &short, &long] {
            assert!(csrf.verify(presented).is_err(), "accepted {presented:?}");
        }
    }

    /// Flipping any single character of a masked value must break it -- a mask
    /// that verified regardless of its second half would be no mask at all.
    #[test]
    fn a_tampered_mask_does_not_verify() {
        let csrf = token();
        let masked = csrf.masked();
        for index in [0, 63, 64, 127] {
            let mut broken: Vec<char> = masked.chars().collect();
            broken[index] = if broken[index] == '0' { '1' } else { '0' };
            let broken: String = broken.into_iter().collect();
            assert!(csrf.verify(&broken).is_err(), "accepted a flip at {index}");
        }
    }

    #[test]
    fn the_header_is_preferred_and_the_form_field_is_the_fallback() {
        let csrf = token();
        let mut headers = HeaderMap::new();
        assert!(csrf.verify_request(&headers, Some(&csrf.masked())).is_ok());
        assert!(csrf.verify_request(&headers, None).is_err());

        headers.insert(CSRF_HEADER, HeaderValue::from_str(&csrf.masked()).unwrap());
        assert!(csrf.verify_request(&headers, None).is_ok());

        // A wrong header is a refusal, not an invitation to try the body.
        headers.insert(CSRF_HEADER, HeaderValue::from_static("nope"));
        assert!(csrf.verify_request(&headers, Some(&csrf.0)).is_err());
    }

    #[test]
    fn the_host_prefix_follows_secure_cookies() {
        let mut config = WebConfig {
            secure_cookies: false,
            ..WebConfig::default()
        };
        assert_eq!(cookie_name(&config), CSRF_COOKIE);
        config.secure_cookies = true;
        assert_eq!(cookie_name(&config), CSRF_COOKIE_HOST);
    }

    #[test]
    fn hex_round_trips_and_rejects_what_is_not_hex() {
        assert_eq!(from_hex("00ff10").unwrap(), vec![0, 255, 16]);
        assert_eq!(to_hex(&[0, 255, 16]), "00ff10");
        assert!(from_hex("0").is_none(), "odd length");
        assert!(from_hex("gg").is_none(), "not a hex digit");
        assert!(from_hex("").is_none());
    }
}
