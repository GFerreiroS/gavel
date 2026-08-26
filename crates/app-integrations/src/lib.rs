//! External provider adapters.
#![forbid(unsafe_code)]

pub mod battlenet;
pub mod blizzard;
pub mod discord;
pub mod raiderio;

pub use blizzard::{BlizzardAuctions, BlizzardConfig, BlizzardCredentials};
pub use discord::DiscordWebhook;
pub use raiderio::{RaiderIoClient, RaiderIoConfig};
