//! Raider.IO character lookup.
//!
//! The character-lookup vertical slice. The response structs below are
//! private: everything that leaves this module is an `app_core::wow` type, so
//! adding a second provider later is a new file rather than a refactor.

use std::time::Duration;

use app_core::error::{AppError, AppResult};
use app_core::wow::{Character, CharacterProvider, CharacterQuery};
use cluster_core::{Clock, Millis};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://raider.io/api/v1";
const FIELDS: &str = "gear,mythic_plus_scores_by_season:current";

#[derive(Debug, Clone)]
pub struct RaiderIoConfig {
    pub base_url: String,
    pub timeout: Duration,
    pub user_agent: String,
}

impl Default for RaiderIoConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_secs(8),
            user_agent: concat!("wow-auction-tracker/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

pub struct RaiderIoClient<C> {
    http: reqwest::Client,
    config: RaiderIoConfig,
    clock: C,
}

impl<C: Clock + 'static> RaiderIoClient<C> {
    pub fn new(config: RaiderIoConfig, clock: C) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(config.user_agent.clone())
            .build()
            .map_err(|e| AppError::internal(format!("building HTTP client: {e}")))?;
        Ok(Self {
            http,
            config,
            clock,
        })
    }
}

impl<C: Clock + 'static> CharacterProvider for RaiderIoClient<C> {
    fn provider_name(&self) -> &'static str {
        "Raider.IO"
    }

    async fn character(&self, query: &CharacterQuery) -> AppResult<Character> {
        let url = format!("{}/characters/profile", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .query(&[
                ("region", query.region.as_str()),
                ("realm", query.realm.as_str()),
                ("name", query.name.as_str()),
                ("fields", FIELDS),
            ])
            .send()
            .await
            .map_err(|e| AppError::Integration(format!("Raider.IO request failed: {e}")))?;

        match response.status().as_u16() {
            200 => {}
            400 | 404 => return Err(AppError::NotFound),
            429 => {
                return Err(AppError::Integration(
                    "Raider.IO rate limit reached, try again shortly".into(),
                ));
            }
            status => {
                return Err(AppError::Integration(format!(
                    "Raider.IO returned HTTP {status}"
                )));
            }
        }

        let profile: Profile = response
            .json()
            .await
            .map_err(|e| AppError::Integration(format!("unexpected Raider.IO payload: {e}")))?;

        Ok(map_profile(profile, query, self.clock.now()))
    }
}

fn map_profile(profile: Profile, query: &CharacterQuery, now: Millis) -> Character {
    Character {
        name: profile.name,
        realm: profile.realm,
        region: profile.region.unwrap_or_else(|| query.region.clone()),
        class: profile.class,
        race: profile.race,
        spec: profile.active_spec_name,
        faction: profile.faction,
        level: None,
        item_level: profile.gear.and_then(|g| g.item_level_equipped),
        mythic_plus_score: profile
            .mythic_plus_scores_by_season
            .and_then(|seasons| seasons.into_iter().next())
            .and_then(|season| season.scores.all),
        thumbnail_url: profile.thumbnail_url,
        profile_url: profile.profile_url,
        fetched_at: now,
    }
}

// --- wire format --------------------------------------------------------
// Private on purpose. Unknown fields are ignored so an upstream addition
// cannot break the page.

#[derive(Debug, Deserialize)]
struct Profile {
    name: String,
    race: String,
    class: String,
    realm: String,
    region: Option<String>,
    faction: Option<String>,
    active_spec_name: Option<String>,
    thumbnail_url: Option<String>,
    profile_url: Option<String>,
    gear: Option<Gear>,
    mythic_plus_scores_by_season: Option<Vec<Season>>,
}

#[derive(Debug, Deserialize)]
struct Gear {
    item_level_equipped: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct Season {
    scores: Scores,
}

#[derive(Debug, Deserialize)]
struct Scores {
    all: Option<f32>,
}
