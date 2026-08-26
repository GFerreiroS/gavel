//! Battle.net account linking -- configuration only, deliberately unimplemented.
//!
//! CLAUDE.md 38 is explicit: do not implement fake OAuth. What exists here is
//! the configuration shape and the place the adapter will go, so that nothing
//! else in the application has to move when the real flow is written against
//! the current official Battle.net OAuth documentation.
//!
//! Intended flow:
//!
//! ```text
//! user -> /account/link/battlenet
//!      -> redirect to Battle.net authorize endpoint (state + PKCE)
//!      -> callback with authorization code
//!      -> token exchange (server side, client secret from the environment)
//!      -> fetch account id
//!      -> store as app_core::model::LinkedAccount
//! ```

/// Credentials come from the environment; never from a file in the repository
/// and never from a literal (CLAUDE.md 9/30).
#[derive(Clone)]
pub struct BattleNetConfig {
    pub client_id: String,
    /// Never logged, never rendered, never persisted in plaintext.
    pub client_secret: String,
    pub redirect_uri: String,
    pub region: String,
}

impl BattleNetConfig {
    /// `None` when the variables are absent, which is the normal state in V0 --
    /// the account page then shows linking as unavailable rather than failing.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            client_id: std::env::var("BATTLENET_CLIENT_ID").ok()?,
            client_secret: std::env::var("BATTLENET_CLIENT_SECRET").ok()?,
            redirect_uri: std::env::var("BATTLENET_REDIRECT_URI")
                .unwrap_or_else(|_| "http://127.0.0.1:3000/account/link/battlenet".into()),
            region: std::env::var("BATTLENET_REGION").unwrap_or_else(|_| "eu".into()),
        })
    }
}

/// Hand-written so the secret cannot leak through a stray `{:?}`.
impl std::fmt::Debug for BattleNetConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BattleNetConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("region", &self.region)
            .finish()
    }
}
