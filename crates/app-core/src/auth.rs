//! Registration, login, sessions.
//!
//! Password hashing sits behind [`PasswordHasher`] so the domain service does
//! not depend on Argon2 directly and remains straightforward to test.

use std::sync::Arc;

use cluster_core::Millis;

use crate::error::{AppError, AppResult, Message, text};
use crate::model::{Session, User};
use crate::repo::{SessionRepository, UserRepository};

pub const SESSION_COOKIE: &str = "wow_tracker_session";
pub const SESSION_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Argon2id over a password nobody holds, verified against when the username
/// does not exist.
///
/// The identical error message for "no such user" and "wrong password" only
/// hides which one happened if the two cost the same. They did not: a missing
/// user answered in under a millisecond, a real one after the ~420ms Argon2
/// spends, and a stopwatch told anyone which usernames were real. Verifying
/// this instead spends the same work on both paths.
///
/// The cost is the one encoded in this string -- `verify_password` reads the
/// parameters from the hash, not from the verifier -- so it is the reference
/// `Params::default()` of m=19456, t=2, p=1. Regenerate it if those change,
/// or the two paths drift apart again.
pub const ABSENT_USER_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$PBDGmYQeDFZi+ngthQy7oA$nWFqoA3BhV2HzsVjhYkCtBOCC1eJX9HiHrqd0jaL7aQ";

const MIN_USERNAME: usize = 3;
const MAX_USERNAME: usize = 32;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 256;

pub trait PasswordHasher: Send + Sync + 'static {
    fn hash(&self, password: &str) -> AppResult<String>;
    fn verify(&self, password: &str, hash: &str) -> AppResult<bool>;
}

impl<H: PasswordHasher> PasswordHasher for Arc<H> {
    fn hash(&self, password: &str) -> AppResult<String> {
        (**self).hash(password)
    }

    fn verify(&self, password: &str, hash: &str) -> AppResult<bool> {
        (**self).verify(password, hash)
    }
}

/// Source of unguessable session tokens.
pub trait TokenSource: Send + Sync + 'static {
    fn token(&self) -> String;
}

pub fn validate_username(username: &str) -> AppResult<()> {
    let len = username.chars().count();
    if !(MIN_USERNAME..=MAX_USERNAME).contains(&len) {
        return Err(AppError::validation_with(
            text::USERNAME_LENGTH,
            [MIN_USERNAME, MAX_USERNAME],
        ));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError::validation(text::USERNAME_CHARSET));
    }
    Ok(())
}

pub fn validate_password(password: &str) -> AppResult<()> {
    let len = password.chars().count();
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&len) {
        return Err(AppError::validation_with(
            text::PASSWORD_LENGTH,
            [MIN_PASSWORD, MAX_PASSWORD],
        ));
    }
    Ok(())
}

/// Stateless service over the auth ports.
pub struct AuthService<'a, U, S, H, T> {
    pub users: &'a U,
    pub sessions: &'a S,
    pub hasher: &'a H,
    pub tokens: &'a T,
}

impl<'a, U, S, H, T> AuthService<'a, U, S, H, T>
where
    U: UserRepository,
    S: SessionRepository,
    H: PasswordHasher,
    T: TokenSource,
{
    pub fn new(users: &'a U, sessions: &'a S, hasher: &'a H, tokens: &'a T) -> Self {
        Self {
            users,
            sessions,
            hasher,
            tokens,
        }
    }

    pub async fn register(&self, username: &str, password: &str, now: Millis) -> AppResult<User> {
        validate_username(username)?;
        validate_password(password)?;
        if self.users.by_username(username).await?.is_some() {
            return Err(AppError::Conflict(Message::new(text::USERNAME_TAKEN)));
        }
        let hash = self.hasher.hash(password)?;
        Ok(self.users.create(username, &hash, now).await?)
    }

    pub async fn login(&self, username: &str, password: &str, now: Millis) -> AppResult<Session> {
        // Same error for "no such user" and "wrong password", so the response
        // cannot be used to discover which usernames exist -- and the same
        // work on both paths, so the clock cannot either.
        let Some(creds) = self.users.by_username(username).await? else {
            let _ = self.hasher.verify(password, ABSENT_USER_HASH);
            return Err(AppError::Unauthorized);
        };
        if !self.hasher.verify(password, &creds.password_hash)? {
            return Err(AppError::Unauthorized);
        }
        let session = Session {
            id: self.tokens.token(),
            user_id: creds.user.id,
            created_at: now,
            expires_at: now.plus_ms(SESSION_TTL_MS),
        };
        self.sessions.create(&session).await?;
        Ok(session)
    }

    /// Sign in a user who has just been created.
    ///
    /// Not `login` with the password again: that is a second Argon2 pass over
    /// a hash this call produced moments ago, which proves nothing and doubles
    /// what one unauthenticated request costs the server.
    pub async fn start_session(&self, user: &User, now: Millis) -> AppResult<Session> {
        let session = Session {
            id: self.tokens.token(),
            user_id: user.id,
            created_at: now,
            expires_at: now.plus_ms(SESSION_TTL_MS),
        };
        self.sessions.create(&session).await?;
        Ok(session)
    }

    pub async fn logout(&self, session_id: &str) -> AppResult<()> {
        self.sessions.delete(session_id).await?;
        Ok(())
    }

    /// Resolve a cookie value to a user, dropping the session if it expired.
    pub async fn authenticate(&self, session_id: &str, now: Millis) -> AppResult<Option<User>> {
        let Some(session) = self.sessions.get(session_id).await? else {
            return Ok(None);
        };
        if session.is_expired(now) {
            self.sessions.delete(session_id).await?;
            return Ok(None);
        }
        Ok(self.users.by_id(session.user_id).await?)
    }
}

#[cfg(feature = "argon2")]
mod argon2_impl {
    // NOTE: randomness comes straight from `getrandom` rather than from a
    // `rand` facade, so the workspace does not end up carrying two
    // incompatible `rand_core` versions just to make a 16-byte salt.
    use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier, SaltString};
    use argon2::{Algorithm, Argon2, Params, Version};

    use super::{AppError, AppResult, PasswordHasher, TokenSource};

    /// Argon2id with the reference parameters.
    pub struct Argon2Hasher {
        argon2: Argon2<'static>,
    }

    impl Argon2Hasher {
        pub fn new() -> Self {
            Self {
                argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default()),
            }
        }
    }

    impl Default for Argon2Hasher {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PasswordHasher for Argon2Hasher {
        fn hash(&self, password: &str) -> AppResult<String> {
            let mut salt_bytes = [0u8; 16];
            getrandom::fill(&mut salt_bytes)
                .map_err(|e| AppError::internal(format!("OS randomness unavailable: {e}")))?;
            let salt = SaltString::encode_b64(&salt_bytes)
                .map_err(|e| AppError::internal(format!("salt encoding failed: {e}")))?;
            self.argon2
                .hash_password(password.as_bytes(), &salt)
                .map(|h| h.to_string())
                .map_err(|e| AppError::internal(format!("password hashing failed: {e}")))
        }

        fn verify(&self, password: &str, hash: &str) -> AppResult<bool> {
            let parsed = PasswordHash::new(hash)
                .map_err(|e| AppError::internal(format!("stored hash unreadable: {e}")))?;
            Ok(self
                .argon2
                .verify_password(password.as_bytes(), &parsed)
                .is_ok())
        }
    }

    /// 256 bits of OS randomness, hex-encoded.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct OsTokens;

    impl TokenSource for OsTokens {
        fn token(&self) -> String {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("OS randomness unavailable");
            // A `format!` per byte allocated 32 times per login; this allocates
            // once. Same for the CSRF token, which runs on every first request.
            let mut out = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
            out
        }
    }
}

#[cfg(feature = "argon2")]
pub use argon2_impl::{Argon2Hasher, OsTokens};

#[cfg(all(test, feature = "argon2"))]
mod tests {
    use super::{ABSENT_USER_HASH, Argon2Hasher, PasswordHasher};

    /// The stand-in only costs what a real hash costs if Argon2 actually runs
    /// over it, and it only runs if the string parses. A typo would fail
    /// open -- `verify` returning `Err` in a microsecond, the timing gap back,
    /// and nothing anywhere saying so.
    #[test]
    fn the_absent_user_hash_parses_and_matches_nothing() {
        let hasher = Argon2Hasher::new();
        assert_eq!(
            hasher.verify("any password at all", ABSENT_USER_HASH).ok(),
            Some(false),
            "must be a parseable argon2id hash, not an error"
        );
    }
}
