//! The WoW vertical slice, expressed as a port plus our own domain types.
//!
//! `app-integrations` provides the Raider.IO adapter. Nothing outside that
//! adapter ever sees a provider-shaped JSON field.

use std::future::Future;

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharacterQuery {
    pub region: String,
    pub realm: String,
    pub name: String,
}

impl CharacterQuery {
    pub fn cache_key(&self) -> String {
        format!(
            "character:{}:{}:{}",
            self.region.to_lowercase(),
            self.realm.to_lowercase(),
            self.name.to_lowercase()
        )
    }
}

/// Our normalised character. Provider-agnostic on purpose: a second provider
/// must be able to fill this in without changing the template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Character {
    pub name: String,
    pub realm: String,
    pub region: String,
    pub class: String,
    pub race: String,
    pub spec: Option<String>,
    pub faction: Option<String>,
    pub level: Option<u16>,
    pub item_level: Option<f32>,
    pub mythic_plus_score: Option<f32>,
    pub thumbnail_url: Option<String>,
    pub profile_url: Option<String>,
    /// When this record was produced, for cache display.
    pub fetched_at: Millis,
}

pub trait CharacterProvider: Send + Sync + 'static {
    /// Stable name of the upstream, shown in the UI as the data source.
    fn provider_name(&self) -> &'static str;

    fn character(
        &self,
        query: &CharacterQuery,
    ) -> impl Future<Output = AppResult<Character>> + Send;
}
