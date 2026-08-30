//! A bounded cache of rendered fragments, and the ETag that goes with them.
//!
//! `docs/market-analysis.md` §15: "A server-side cache key includes at least
//! market version, catalogue/release, region, category, comparison window,
//! realm where applicable, and locale. Cache misses for the same key are
//! coalesced so one publication cannot cause every visitor to rebuild the same
//! fragment simultaneously."
//!
//! Both halves matter and they are different problems. The cache stops the
//! same fragment being built twice; the *coalescing* stops it being built
//! fifty times at once, which is exactly what happens the moment a new version
//! is published and every open tab refreshes on the same event.
//!
//! Invalidation is not a mechanism here. The published version is part of the
//! key, so a new version simply misses -- there is nothing to remember to
//! clear, and no way for a stale entry to be served under a new version's
//! name. The bound is what keeps the old keys from accumulating.
//!
//! **This is a fourth thing that stops two web replicas running**, after
//! SQLite, the in-process SSE bus and the sign-in throttle (CLAUDE.md §6).
//! Two processes would keep two caches, which is wasteful rather than wrong --
//! each entry is a pure function of its key -- but the list in §6 is a list
//! and this belongs on it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

use crate::error::WebResult;

/// Entries kept before the oldest is dropped.
///
/// The key space is bounded in principle -- categories times regions times
/// windows times locales -- but a realm choice multiplies it by 184, so it is
/// bounded in practice by this. Eviction is oldest-first rather than
/// least-recently-used: an LRU needs a touch on every read, and the thing
/// being protected is memory rather than a hit rate somebody is tuning.
const CAPACITY: usize = 512;

/// Everything that decides what a fragment says.
///
/// Built as one string because it is used as one: the map's key, and the
/// ETag. A field that mattered and was left out here would be a fragment
/// served to somebody it was not built for -- so the constructor takes all of
/// them and there is no way to build a partial one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FragmentKey(String);

impl FragmentKey {
    /// `version` is the published analysis version. `None` means there is
    /// none, which is a state worth its own key rather than one that shares
    /// with version zero.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        route: &str,
        version: Option<u64>,
        catalog: &str,
        region: &str,
        baseline_days: u64,
        realm: &str,
        locale: &str,
        group: Option<&str>,
    ) -> FragmentKey {
        FragmentKey(format!(
            "{route}|v{}|{catalog}|{region}|{baseline_days}d|{realm}|{locale}|{}",
            version.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            group.unwrap_or("*"),
        ))
    }

    /// The ETag this key produces.
    ///
    /// The key itself, quoted. Not a hash of the body: two bodies built from
    /// the same key are the same body, and a key that changed would change the
    /// ETag whether or not the bytes happened to match. It is also readable in
    /// a log, which a digest is not.
    pub(crate) fn etag(&self) -> String {
        format!("\"{}\"", self.0)
    }
}

/// Rendered fragments, bounded and coalesced.
#[derive(Debug, Default)]
pub struct FragmentCache {
    inner: Mutex<Inner>,
    /// Bumped whenever an administrator changes the market events.
    ///
    /// **The one thing in a fragment that is not a function of the published
    /// analysis version.** Everything else a cached fragment says comes from a
    /// version, a catalogue and the reader's preferences, so a change to any
    /// of them is a different key and a miss. Events are reviewed out of band:
    /// publishing one is an administrator pressing a button, and the analysis
    /// version does not move. Without this, a newly published event was
    /// invisible on every page a reader had already warmed -- found by
    /// publishing one and watching it not appear.
    ///
    /// Still not an invalidation *mechanism*: it goes into the key, so a
    /// change simply misses. Nothing is cleared and no entry can be served
    /// under a newer epoch's name.
    events_epoch: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<FragmentKey, Arc<OnceCell<Arc<str>>>>,
    /// Insertion order, for eviction.
    order: Vec<FragmentKey>,
}

impl FragmentCache {
    pub fn new() -> FragmentCache {
        FragmentCache::default()
    }

    /// The current events epoch, for a key that shows events.
    ///
    /// Only the fragments that render events include this. A category card
    /// shows none, and putting the epoch in its key would throw away every
    /// card on the site because somebody wrote down a hotfix.
    pub(crate) fn events_epoch(&self) -> u64 {
        self.events_epoch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Move the epoch on, because the events changed.
    pub fn bump_events(&self) {
        self.events_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl FragmentCache {
    /// The cached fragment for this key, building it if nobody has.
    ///
    /// Concurrent callers with the same key share one build: the first to
    /// arrive runs `build`, the rest wait on it. A build that fails is not
    /// remembered -- the cell stays empty, so the next request tries again
    /// rather than serving an error for as long as the entry lives.
    pub(crate) async fn get_or_build<F>(&self, key: &FragmentKey, build: F) -> WebResult<Arc<str>>
    where
        F: std::future::Future<Output = WebResult<String>>,
    {
        let cell = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cell) = inner.entries.get(key) {
                Arc::clone(cell)
            } else {
                let cell = Arc::new(OnceCell::new());
                inner.entries.insert(key.clone(), Arc::clone(&cell));
                inner.order.push(key.clone());
                while inner.order.len() > CAPACITY {
                    let oldest = inner.order.remove(0);
                    inner.entries.remove(&oldest);
                }
                cell
            }
        };

        let html = cell
            .get_or_try_init(|| async { build.await.map(|html| Arc::from(html.as_str())) })
            .await?;
        Ok(Arc::clone(html))
    }

    /// How many entries are held. For the operations page and for tests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Serve a fragment from the cache, with revalidation.
///
/// The one narrowly scoped exception §15 allows to the `no-store` baseline,
/// and it is narrow on purpose:
///
/// * only the **card fragments** -- public market data, no session, no CSRF
///   token, nothing personal. The shells that carry those keep `no-store`
///   without exception, which `headers::layer` still applies to everything
///   that does not set its own;
/// * `private`, so a shared cache never holds one. Locale and market
///   preferences come from a cookie, so a proxy caching this for one reader
///   would serve it to another in the wrong language;
/// * `no-cache`, so the browser revalidates every time rather than deciding
///   for itself how long a price stays true. The saving is the body, not the
///   request.
///
/// The ETag is the cache key, which is what makes "changing locale, region,
/// baseline, realm or version can never reuse the wrong cache" true of the
/// browser's copy as well as of ours.
pub(crate) async fn respond<F>(
    cache: &FragmentCache,
    headers: &axum::http::HeaderMap,
    key: FragmentKey,
    build: F,
) -> WebResult<axum::response::Response>
where
    F: std::future::Future<Output = WebResult<String>>,
{
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let etag = key.etag();
    let known = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));

    let cached = cache.get_or_build(&key, build).await?;
    let mut response = if known {
        // Nothing to send: the reader already has this exact representation.
        // Built anyway, because the cache is what the *next* reader wants and
        // a 304 that had skipped the build would leave the entry cold.
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        axum::response::Html(cached.to_string()).into_response()
    };

    let mut set = |name: header::HeaderName, value: &str| {
        if let Ok(value) = header::HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    };
    set(header::ETAG, &etag);
    set(header::CACHE_CONTROL, "private, no-cache");
    // Two readers of the same fragment differ by cookie -- locale, region,
    // comparison window -- so any cache keying on the URL alone would be
    // wrong. `private` already forbids a shared one; this says why.
    set(header::VARY, "Cookie, Accept-Encoding");
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(region: &str) -> FragmentKey {
        FragmentKey::new(
            "reagents",
            Some(7),
            "midnight",
            region,
            7,
            "",
            "en_GB",
            None,
        )
    }

    #[tokio::test]
    async fn a_second_request_for_the_same_key_is_not_built_again() {
        let cache = FragmentCache::new();
        let builds = AtomicUsize::new(0);
        let build = || async {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok("<p>hello</p>".to_string())
        };

        let first = cache.get_or_build(&key("eu"), build()).await.unwrap();
        let second = cache.get_or_build(&key("eu"), build()).await.unwrap();
        assert_eq!(&*first, "<p>hello</p>");
        assert_eq!(first, second);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    /// Every part of the key has to separate. A fragment served under the
    /// wrong region, window, realm or language is the failure this whole file
    /// exists to make impossible, so each one is asserted rather than assumed.
    #[tokio::test]
    async fn changing_any_part_of_the_key_is_a_different_fragment() {
        let base = FragmentKey::new("reagents", Some(7), "midnight", "eu", 7, "", "en_GB", None);
        let others = [
            FragmentKey::new("enchants", Some(7), "midnight", "eu", 7, "", "en_GB", None),
            FragmentKey::new("reagents", Some(8), "midnight", "eu", 7, "", "en_GB", None),
            FragmentKey::new("reagents", None, "midnight", "eu", 7, "", "en_GB", None),
            FragmentKey::new("reagents", Some(7), "warwithin", "eu", 7, "", "en_GB", None),
            FragmentKey::new("reagents", Some(7), "midnight", "us", 7, "", "en_GB", None),
            FragmentKey::new("reagents", Some(7), "midnight", "eu", 30, "", "en_GB", None),
            FragmentKey::new(
                "reagents",
                Some(7),
                "midnight",
                "eu",
                7,
                "sargeras",
                "en_GB",
                None,
            ),
            FragmentKey::new("reagents", Some(7), "midnight", "eu", 7, "", "es_ES", None),
            FragmentKey::new(
                "reagents",
                Some(7),
                "midnight",
                "eu",
                7,
                "",
                "en_GB",
                Some("alchemy"),
            ),
        ];
        for other in &others {
            assert_ne!(&base, other, "{other:?} collided with the base key");
            assert_ne!(base.etag(), other.etag());
        }
    }

    /// A build that failed must not be remembered, or one upstream hiccup
    /// becomes an error served for as long as the entry lives.
    #[tokio::test]
    async fn a_failed_build_is_not_cached() {
        let cache = FragmentCache::new();
        let failed = cache
            .get_or_build(&key("eu"), async {
                Err(app_core::AppError::NotFound.into())
            })
            .await;
        assert!(failed.is_err());

        let ok = cache
            .get_or_build(&key("eu"), async { Ok("<p>later</p>".to_string()) })
            .await
            .unwrap();
        assert_eq!(&*ok, "<p>later</p>");
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let cache = FragmentCache::new();
        for n in 0..CAPACITY + 50 {
            let region = format!("r{n}");
            cache
                .get_or_build(&key(&region), async { Ok(String::new()) })
                .await
                .unwrap();
        }
        assert_eq!(cache.len(), CAPACITY);
    }
}
