//! External provider adapters.
#![forbid(unsafe_code)]

pub mod battlenet;
pub mod blizzard;
pub mod discord;
pub mod raiderio;
pub mod tsm;

pub use blizzard::{
    BlizzardAuctions, BlizzardConfig, BlizzardCredentials, BlizzardItems, BlizzardRealms,
};
pub use discord::{DiscordWebhook, PerUserDiscord};
pub use raiderio::{RaiderIoClient, RaiderIoConfig};
pub use tsm::TsmClient;
