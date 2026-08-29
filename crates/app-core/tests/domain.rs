//! Domain-layer tests: validation, hashing, and the guard rails on job
//! submission. No database and no cluster involved.

use app_core::auth::{
    Argon2Hasher, OsTokens, PasswordHasher, TokenSource, validate_password, validate_username,
};
use app_core::model::Session;
use app_core::service::{MAX_PRIME_BOUND, MAX_SLEEP_MS, MAX_TASKS_PER_JOB, build_job_spec};
use app_core::wow::CharacterQuery;
use cluster_core::{JobSpec, Millis};

#[test]
fn usernames_are_constrained() {
    assert!(validate_username("valid_name-1").is_ok());
    assert!(validate_username("ab").is_err(), "too short");
    assert!(validate_username(&"a".repeat(33)).is_err(), "too long");
    assert!(validate_username("has space").is_err());
    assert!(validate_username("drop;table").is_err());
    assert!(validate_username("").is_err());
}

#[test]
fn passwords_have_a_minimum_length() {
    assert!(validate_password("correct-horse").is_ok());
    assert!(validate_password("short").is_err());
    assert!(validate_password(&"a".repeat(257)).is_err());
}

#[test]
fn hashing_is_salted_and_verifiable() {
    let hasher = Argon2Hasher::new();
    let first = hasher.hash("correct-horse").unwrap();
    let second = hasher.hash("correct-horse").unwrap();

    assert_ne!(first, second, "each hash gets its own salt");
    assert!(!first.contains("correct-horse"));
    assert!(first.starts_with("$argon2id$"));

    assert!(hasher.verify("correct-horse", &first).unwrap());
    assert!(hasher.verify("correct-horse", &second).unwrap());
    assert!(!hasher.verify("wrong-password", &first).unwrap());
    assert!(hasher.verify("x", "not-a-hash").is_err());
}

#[test]
fn session_tokens_are_long_and_unique() {
    let tokens = OsTokens;
    let a = tokens.token();
    let b = tokens.token();
    assert_eq!(a.len(), 64, "256 bits, hex encoded");
    assert_ne!(a, b);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn sessions_know_when_they_have_expired() {
    let session = Session {
        id: "t".into(),
        user_id: 1,
        created_at: Millis(0),
        expires_at: Millis(1_000),
    };
    assert!(!session.is_expired(Millis(999)));
    assert!(session.is_expired(Millis(1_000)));
}

#[test]
fn job_submission_is_bounded() {
    assert!(matches!(
        build_job_spec("sleep", 1_000, 4),
        Ok(JobSpec::Sleep {
            total_ms: 1_000,
            tasks: 4
        })
    ));
    assert!(matches!(
        build_job_spec("primes", 10_000, 8),
        Ok(JobSpec::Primes {
            upper_bound: 10_000,
            tasks: 8
        })
    ));

    // A single form post must not be able to wedge the cluster.
    assert!(build_job_spec("sleep", 1_000, 0).is_err());
    assert!(build_job_spec("sleep", 1_000, MAX_TASKS_PER_JOB + 1).is_err());
    assert!(build_job_spec("sleep", MAX_SLEEP_MS + 1, 4).is_err());
    assert!(build_job_spec("sleep", 0, 4).is_err());
    assert!(build_job_spec("primes", MAX_PRIME_BOUND + 1, 4).is_err());
    assert!(build_job_spec("primes", 1, 4).is_err());
    assert!(build_job_spec("mine-bitcoin", 10, 1).is_err());
}

#[test]
fn character_cache_keys_are_case_insensitive() {
    let a = CharacterQuery {
        region: "EU".into(),
        realm: "Silvermoon".into(),
        name: "Someone".into(),
    };
    let b = CharacterQuery {
        region: "eu".into(),
        realm: "silvermoon".into(),
        name: "someone".into(),
    };
    assert_eq!(a.cache_key(), b.cache_key());
    assert_eq!(a.cache_key(), "character:eu:silvermoon:someone");
}

// ---------------------------------------------------------------------------
// Sign-in must cost the same whether or not the username exists.
//
// The identical error message was doing no work on its own: a missing user
// answered without hashing anything, so a stopwatch told a caller which
// usernames were real. These fakes count the hash verifications rather than
// timing them -- a timing assertion in CI is a flaky test, an equal number of
// Argon2 passes is the property that makes the timings equal.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};

use app_core::auth::AuthService;
use app_core::error::{AppError, RepoResult};
use app_core::model::{Credentials, LinkedAccount, User, UserId};
use app_core::repo::{SessionRepository, UserRepository};

#[derive(Default)]
struct CountingHasher {
    verifications: AtomicUsize,
}

impl PasswordHasher for CountingHasher {
    fn hash(&self, password: &str) -> Result<String, AppError> {
        Ok(format!("hashed:{password}"))
    }

    fn verify(&self, password: &str, hash: &str) -> Result<bool, AppError> {
        self.verifications.fetch_add(1, Ordering::SeqCst);
        Ok(hash == format!("hashed:{password}"))
    }
}

struct OneUser(Option<Credentials>);

impl UserRepository for OneUser {
    async fn create(&self, _: &str, _: &str, _: Millis) -> RepoResult<User> {
        unreachable!("registration is not under test here")
    }

    async fn by_username(&self, username: &str) -> RepoResult<Option<Credentials>> {
        Ok(self
            .0
            .as_ref()
            .filter(|c| c.user.username == username)
            .map(|c| Credentials {
                user: c.user.clone(),
                password_hash: c.password_hash.clone(),
            }))
    }

    async fn by_id(&self, _: UserId) -> RepoResult<Option<User>> {
        Ok(self.0.as_ref().map(|c| c.user.clone()))
    }

    async fn linked_accounts(&self, _: UserId) -> RepoResult<Vec<LinkedAccount>> {
        Ok(Vec::new())
    }
}

struct NoSessions;

impl SessionRepository for NoSessions {
    async fn create(&self, _: &Session) -> RepoResult<()> {
        Ok(())
    }
    async fn get(&self, _: &str) -> RepoResult<Option<Session>> {
        Ok(None)
    }
    async fn delete(&self, _: &str) -> RepoResult<()> {
        Ok(())
    }
    async fn purge_expired(&self, _: Millis) -> RepoResult<u64> {
        Ok(0)
    }
}

struct FixedToken;

impl TokenSource for FixedToken {
    fn token(&self) -> String {
        "token".into()
    }
}

fn alice() -> Credentials {
    Credentials {
        user: User {
            id: 1,
            username: "alice".into(),
            created_at: Millis(0),
            is_admin: true,
        },
        password_hash: "hashed:correct-horse".into(),
    }
}

async fn failed_login_verifications(username: &str) -> usize {
    let hasher = CountingHasher::default();
    let users = OneUser(Some(alice()));
    let sessions = NoSessions;
    let auth = AuthService::new(&users, &sessions, &hasher, &FixedToken);

    let outcome = auth.login(username, "not-the-password", Millis(0)).await;
    assert!(
        matches!(outcome, Err(AppError::Unauthorized)),
        "both failures must be the same error"
    );
    hasher.verifications.load(Ordering::SeqCst)
}

#[tokio::test]
async fn an_unknown_username_costs_the_same_as_a_wrong_password() {
    let real = failed_login_verifications("alice").await;
    let absent = failed_login_verifications("nobody-by-that-name").await;

    assert_eq!(
        real, 1,
        "a wrong password is checked against the stored hash"
    );
    assert_eq!(
        absent, real,
        "an unknown username must spend the same hash, or the clock says which is which"
    );
}

/// The parseability of the stand-in hash is checked as a unit test next to the
/// constant; this is the end of the path, with the real hasher in place.
#[tokio::test]
async fn an_unknown_username_is_refused_by_the_real_hasher() {
    let hasher = Argon2Hasher::new();
    let users = OneUser(None);
    let sessions = NoSessions;
    let auth = AuthService::new(&users, &sessions, &hasher, &FixedToken);

    let outcome = auth.login("nobody", "any password at all", Millis(0)).await;
    assert!(matches!(outcome, Err(AppError::Unauthorized)));
}
