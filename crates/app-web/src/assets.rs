//! Static assets, compiled into the binary.
//!
//! Embedding rather than serving from disk keeps deployment to a single file
//! and makes the asset budget visible at build time -- which matters, because
//! every byte here is on the critical path of a first page load.
//!
//! Those bytes are worth watching: CSS is render-blocking, and Pico is 70 KB
//! of it uncompressed. The compression layer in `routes` is what makes that
//! affordable -- the two stylesheets together are 17 KB gzipped, slightly less
//! than this app's own stylesheet cost uncompressed before Pico existed.

use std::sync::LazyLock;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Pico gives semantic HTML sane defaults -- typography, forms, buttons,
/// tables -- so this app's own stylesheet only has to describe the things Pico
/// has never heard of: item cards, cluster grids, price charts, item quality.
///
/// Loaded *before* `style.css`, so those components win where they disagree.
const PICO_CSS: &str = include_str!("../static/pico.min.css");
const STYLE_CSS: &str = include_str!("../static/style.css");
const HTMX_JS: &str = include_str!("../static/htmx.min.js");
const LIVE_JS: &str = include_str!("../static/live.js");

/// FNV-1a over the asset bytes, at compile time.
///
/// Assets are compiled into the binary, so their URLs were stable across
/// rebuilds while their contents were not -- and they are served with a long
/// `max-age`. The result was that a CSS change did not reach a browser that
/// had already cached the old file. Hashing the content into the URL makes
/// each version its own resource, which is what a long cache lifetime
/// actually requires.
const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

pub static PICO_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{:x}", fnv1a(PICO_CSS.as_bytes())));
pub static STYLE_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{:x}", fnv1a(STYLE_CSS.as_bytes())));
pub static HTMX_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{:x}", fnv1a(HTMX_JS.as_bytes())));
pub static LIVE_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{:x}", fnv1a(LIVE_JS.as_bytes())));

pub async fn pico() -> Response {
    asset(PICO_CSS, "text/css; charset=utf-8")
}

pub async fn style() -> Response {
    asset(STYLE_CSS, "text/css; charset=utf-8")
}

pub async fn htmx() -> Response {
    asset(HTMX_JS, "application/javascript; charset=utf-8")
}

pub async fn live() -> Response {
    asset(LIVE_JS, "application/javascript; charset=utf-8")
}

pub async fn favicon() -> Response {
    // An empty 204 beats a 404 in the log on every page load.
    StatusCode::NO_CONTENT.into_response()
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                // Safe to cache hard because the URL carries a content hash:
                // a changed asset is a different URL.
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        body,
    )
        .into_response()
}
