//! Domain records.
//!
//! These are *our* types. Nothing here mirrors a database row or an upstream
//! API response; adapters map into and out of these.

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

pub type UserId = i64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub created_at: Millis,
}

/// A user plus the secret material needed to verify a login. Kept separate
/// from [`User`] so a password hash cannot accidentally be rendered or logged.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub user: User,
    pub password_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Opaque random token; also the cookie value.
    pub id: String,
    pub user_id: UserId,
    pub created_at: Millis,
    pub expires_at: Millis,
}

impl Session {
    pub fn is_expired(&self, now: Millis) -> bool {
        now >= self.expires_at
    }
}

/// An external account linked to a local user (Battle.net, later others).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedAccount {
    pub user_id: UserId,
    pub provider: String,
    pub external_id: String,
    pub display_name: String,
    pub linked_at: Millis,
}
