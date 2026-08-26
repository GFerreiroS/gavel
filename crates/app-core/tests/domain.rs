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
