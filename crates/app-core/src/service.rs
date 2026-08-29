//! Application services. Handlers call these; handlers contain no logic
//! themselves.

use cluster_core::{ClusterControl, JobId, JobSpec, Millis};

use crate::error::{AppError, AppResult, text};
use crate::item::{ItemDetailProvider, ItemTooltip};
use crate::locale::Locale;
use crate::market::{ItemId, Region};
use crate::repo::CacheStore;
use crate::wow::{Character, CharacterProvider, CharacterQuery};

/// Guard rails on what a browser may ask the cluster to run. Without these a
/// single form post can wedge every worker.
pub const MAX_TASKS_PER_JOB: u16 = 64;
pub const MAX_SLEEP_MS: u64 = 120_000;
pub const MAX_PRIME_BOUND: u64 = 50_000_000;

/// Validate raw user input and turn it into a [`JobSpec`].
///
/// Free-standing rather than a method, because it needs neither a cluster nor
/// a store -- which also makes it trivial to test.
pub fn build_job_spec(kind: &str, size: u64, tasks: u16) -> AppResult<JobSpec> {
    if tasks == 0 || tasks > MAX_TASKS_PER_JOB {
        return Err(AppError::validation_with(
            text::TASK_COUNT_RANGE,
            [MAX_TASKS_PER_JOB],
        ));
    }
    match kind {
        "sleep" => {
            if size == 0 || size > MAX_SLEEP_MS {
                return Err(AppError::validation_with(text::SLEEP_RANGE, [MAX_SLEEP_MS]));
            }
            Ok(JobSpec::Sleep {
                total_ms: size,
                tasks,
            })
        }
        "primes" => {
            if !(2..=MAX_PRIME_BOUND).contains(&size) {
                return Err(AppError::validation_with(
                    text::PRIME_RANGE,
                    [MAX_PRIME_BOUND],
                ));
            }
            Ok(JobSpec::Primes {
                upper_bound: size,
                tasks,
            })
        }
        other => Err(AppError::validation_with(text::UNKNOWN_JOB_KIND, [other])),
    }
}

/// Hands validated work to the cluster.
pub struct JobService<'a, C> {
    cluster: &'a C,
}

impl<'a, C: ClusterControl> JobService<'a, C> {
    pub fn new(cluster: &'a C) -> Self {
        Self { cluster }
    }

    pub async fn submit(&self, kind: &str, size: u64, tasks: u16) -> AppResult<JobId> {
        let spec = build_job_spec(kind, size, tasks)?;
        Ok(self.cluster.submit_job(spec).await?)
    }
}

/// Character lookup with a short-lived cache in front of the upstream.
pub struct CharacterService<'a, P, K> {
    provider: &'a P,
    cache: &'a K,
    ttl_ms: u64,
}

/// Where a rendered record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Cached,
    Fetched,
    /// The upstream does not know this item -- an id that has left the game
    /// data, which happens to old expansions. The caller got a name-only
    /// placeholder, and so will the next caller: the answer is cached, because
    /// "this item does not exist" is as static as the item data itself.
    Missing,
    /// Neither: the upstream could not answer and the caller got whatever
    /// could be assembled locally. Not cached, so the next call retries.
    Unavailable,
}

impl<'a, P: CharacterProvider, K: CacheStore> CharacterService<'a, P, K> {
    pub fn new(provider: &'a P, cache: &'a K, ttl_ms: u64) -> Self {
        Self {
            provider,
            cache,
            ttl_ms,
        }
    }

    pub fn validate(region: &str, realm: &str, name: &str) -> AppResult<CharacterQuery> {
        // Each field names its own two messages rather than sharing one with
        // the field name substituted in: the field name would be the one word
        // in the sentence that no catalogue could translate.
        let clean = |missing: &'static str, bad: &'static str, value: &str| -> AppResult<String> {
            let value = value.trim();
            if value.is_empty() || value.len() > 64 {
                return Err(AppError::validation(missing));
            }
            if !value
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '\'' | ' '))
            {
                return Err(AppError::validation(bad));
            }
            Ok(value.to_string())
        };
        Ok(CharacterQuery {
            region: clean(text::REGION_REQUIRED, text::REGION_CHARSET, region)?.to_lowercase(),
            realm: clean(text::REALM_REQUIRED, text::REALM_CHARSET, realm)?,
            name: clean(text::CHARACTER_REQUIRED, text::CHARACTER_CHARSET, name)?,
        })
    }

    pub async fn lookup(
        &self,
        query: &CharacterQuery,
        now: Millis,
    ) -> AppResult<(Character, Freshness)> {
        let key = query.cache_key();
        if let Some(bytes) = self.cache.get(&key, now).await?
            && let Ok(character) = serde_json::from_slice::<Character>(&bytes)
        {
            return Ok((character, Freshness::Cached));
        }

        let character = self.provider.character(query).await?;
        // A cache write failure must not fail the request.
        if let Ok(bytes) = serde_json::to_vec(&character) {
            let _ = self.cache.put(&key, &bytes, now.plus_ms(self.ttl_ms)).await;
        }
        Ok((character, Freshness::Fetched))
    }
}

/// Item tooltips, cached hard.
///
/// Static item data changes only on a patch, so this cache is measured in
/// days rather than the minutes the character cache uses. That matters: the
/// tooltip is fetched on hover, and a page of forty cards must not turn into
/// forty upstream calls every time someone moves the mouse across it.
pub struct ItemTooltipService<'a, P, K> {
    provider: &'a P,
    cache: &'a K,
    ttl_ms: u64,
}

impl<'a, P: ItemDetailProvider, K: CacheStore> ItemTooltipService<'a, P, K> {
    pub fn new(provider: &'a P, cache: &'a K, ttl_ms: u64) -> Self {
        Self {
            provider,
            cache,
            ttl_ms,
        }
    }

    /// Cache-only read. Never touches the upstream, so it is safe to call
    /// while rendering a page: a page render must not be able to block on
    /// Battle.net.
    pub async fn cached(&self, locale: Locale, item: ItemId, now: Millis) -> Option<ItemTooltip> {
        let key = ItemTooltip::cache_key(item, locale);
        let bytes = self.cache.get(&key, now).await.ok().flatten()?;
        serde_json::from_slice::<ItemTooltip>(&bytes).ok()
    }

    /// Every cached tooltip among `items`, in one round trip.
    ///
    /// A page of item cards wants all of them at once. Asking one at a time is
    /// the same answer for an order of magnitude more work, and it was the
    /// largest single cost in rendering a category page.
    ///
    /// An item that is not cached is simply missing from the result, like
    /// [`Self::cached`] returning `None`. Nothing here fetches: a page renders
    /// what is already known and the collector is what fills the gaps.
    pub async fn cached_many(
        &self,
        locale: Locale,
        items: impl IntoIterator<Item = ItemId>,
        now: Millis,
    ) -> Vec<(ItemId, ItemTooltip)> {
        let mut wanted: Vec<ItemId> = items.into_iter().collect();
        wanted.sort_unstable();
        wanted.dedup();

        let keys: Vec<String> = wanted
            .iter()
            .map(|item| ItemTooltip::cache_key(*item, locale))
            .collect();
        let Ok(rows) = self.cache.get_many(&keys, now).await else {
            return Vec::new();
        };

        // Keyed back by the id inside the decoded tooltip rather than by
        // parsing the cache key: the key format is this module's business and
        // the tooltip already carries the id it is for.
        rows.into_iter()
            .filter_map(|(_, bytes)| serde_json::from_slice::<ItemTooltip>(&bytes).ok())
            .map(|tooltip| (tooltip.item, tooltip))
            .collect()
    }

    /// The tooltip for one item in one language, fetching if it is not cached.
    ///
    /// A miss fetches *every* language in one request and caches them all,
    /// because that is what the upstream returns anyway. Switching language is
    /// then free. `region` only decides which regional host is asked; the text
    /// that comes back is the same either way.
    pub async fn lookup(
        &self,
        region: Region,
        locale: Locale,
        item: ItemId,
        fallback_name: &str,
        now: Millis,
    ) -> (ItemTooltip, Freshness) {
        if let Some(tooltip) = self.cached(locale, item, now).await {
            return (tooltip, Freshness::Cached);
        }

        if !self.provider.is_configured() {
            return (
                ItemTooltip::placeholder(item, locale, fallback_name, now),
                Freshness::Unavailable,
            );
        }

        match self.provider.tooltips(region, item).await {
            Ok(localized) => {
                self.store_all(item, &localized, now).await;
                match localized.into_iter().find(|(l, _)| *l == locale) {
                    Some((_, tooltip)) => (tooltip, Freshness::Fetched),
                    // The upstream answered but not in this language. Treat it
                    // as missing rather than falling back silently to another
                    // language, which would look like a rendering bug.
                    None => (
                        ItemTooltip::placeholder(item, locale, fallback_name, now),
                        Freshness::Missing,
                    ),
                }
            }
            // A missing item is a permanent answer, so cache the placeholder.
            // Without this, an id the upstream has dropped would be re-fetched
            // on every hover, for ever.
            Err(AppError::NotFound) => {
                tracing::debug!(item = %item, region = %region, "item is not in the game data");
                let placeholder = ItemTooltip::placeholder(item, locale, fallback_name, now);
                self.store(locale, item, &placeholder, now).await;
                (placeholder, Freshness::Missing)
            }
            Err(e) => {
                tracing::warn!(item = %item, region = %region, error = %e, "item tooltip lookup failed");
                (
                    ItemTooltip::placeholder(item, locale, fallback_name, now),
                    Freshness::Unavailable,
                )
            }
        }
    }

    async fn store_all(&self, item: ItemId, localized: &[(Locale, ItemTooltip)], now: Millis) {
        for (locale, tooltip) in localized {
            self.store(*locale, item, tooltip, now).await;
        }
    }

    /// A cache write failure must not fail the request that triggered it.
    async fn store(&self, locale: Locale, item: ItemId, tooltip: &ItemTooltip, now: Millis) {
        if let Ok(bytes) = serde_json::to_vec(tooltip) {
            let key = ItemTooltip::cache_key(item, locale);
            let _ = self.cache.put(&key, &bytes, now.plus_ms(self.ttl_ms)).await;
        }
    }
}
