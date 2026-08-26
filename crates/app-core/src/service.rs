//! Application services. Handlers call these; handlers contain no logic
//! themselves (CLAUDE.md 7).

use cluster_core::{ClusterControl, JobId, JobSpec, Millis};

use crate::error::{AppError, AppResult};
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
        return Err(AppError::validation(format!(
            "task count must be between 1 and {MAX_TASKS_PER_JOB}"
        )));
    }
    match kind {
        "sleep" => {
            if size == 0 || size > MAX_SLEEP_MS {
                return Err(AppError::validation(format!(
                    "sleep duration must be between 1 and {MAX_SLEEP_MS} ms"
                )));
            }
            Ok(JobSpec::Sleep {
                total_ms: size,
                tasks,
            })
        }
        "primes" => {
            if !(2..=MAX_PRIME_BOUND).contains(&size) {
                return Err(AppError::validation(format!(
                    "prime bound must be between 2 and {MAX_PRIME_BOUND}"
                )));
            }
            Ok(JobSpec::Primes {
                upper_bound: size,
                tasks,
            })
        }
        other => Err(AppError::validation(format!("unknown job kind '{other}'"))),
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

/// Whether a rendered character came from the cache or from the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Cached,
    Fetched,
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
        let clean = |label: &str, value: &str| -> AppResult<String> {
            let value = value.trim();
            if value.is_empty() || value.len() > 64 {
                return Err(AppError::validation(format!("{label} is required")));
            }
            if !value
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '\'' | ' '))
            {
                return Err(AppError::validation(format!(
                    "{label} contains invalid characters"
                )));
            }
            Ok(value.to_string())
        };
        Ok(CharacterQuery {
            region: clean("region", region)?.to_lowercase(),
            realm: clean("realm", realm)?,
            name: clean("character name", name)?,
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
