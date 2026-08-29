//! Registration, login, sessions.
//!
//! Password hashing sits behind [`PasswordHasher`] so the domain service does
//! not depend on Argon2 directly and remains straightforward to test.

use cluster_core::Millis;

use crate::error::{AppError, AppResult};
use crate::model::{Session, User};
use crate::repo::{SessionRepository, UserRepository};

pub const SESSION_COOKIE: &str = "wow_tracker_session";
pub const SESSION_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

const MIN_USERNAME: usize = 3;
const MAX_USERNAME: usize = 32;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 256;

pub trait PasswordHasher: Send + Sync + 'static {
    fn hash(&self, password: &str) -> AppResult<String>;
    fn verify(&self, password: &str, hash: &str) -> AppResult<bool>;
}

/// Source of unguessable session tokens.
pub trait TokenSource: Send + Sync + 'static {
    fn token(&self) -> String;
}

pub fn validate_username(username: &str) -> AppResult<()> {
    let len = username.chars().count();
    if !(MIN_USERNAME..=MAX_USERNAME).contains(&len) {
        return Err(AppError::validation(format!(
            "username must be {MIN_USERNAME}-{MAX_USERNAME} characters"
        )));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError::validation(
            "username may contain letters, digits, '_' and '-' only",
        ));
    }
    Ok(())
}

pub fn validate_password(password: &str) -> AppResult<()> {
    let len = password.chars().count();
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&len) {
        return Err(AppError::validation(format!(
            "password must be {MIN_PASSWORD}-{MAX_PASSWORD} characters"
        )));
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
            return Err(AppError::Conflict("username already taken".into()));
        }
        let hash = self.hasher.hash(password)?;
        Ok(self.users.create(username, &hash, now).await?)
    }

    pub async fn login(&self, username: &str, password: &str, now: Millis) -> AppResult<Session> {
        // Same error for "no such user" and "wrong password", so the response
        // cannot be used to discover which usernames exist.
        let creds = self
            .users
            .by_username(username)
            .await?
            .ok_or(AppError::Unauthorized)?;
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
