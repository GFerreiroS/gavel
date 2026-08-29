//! Application domain: types, ports and services.
//!
//! Dependency direction is inwards. This crate defines the traits (ports) that
//! adapters implement -- `storage` implements the repositories, `cluster-local`
//! implements [`cluster_core::ClusterControl`], `app-integrations` implements
//! [`wow::CharacterProvider`]. Nothing here knows those crates exist.
#![forbid(unsafe_code)]

pub mod auth;
pub mod error;
pub mod item;
pub mod locale;
pub mod market;
pub mod metrics;
pub mod model;
pub mod ports;
pub mod repo;
pub mod service;
pub mod wow;

pub use error::{AppError, AppResult, RepoError, RepoResult};
pub use item::{ItemDetailProvider, ItemQuality, ItemTooltip, LocalizedTooltips};
pub use locale::{ALL_LOCALES, DEFAULT_LOCALE, Locale};
pub use market::{Catalog, CatalogSet, Copper, ItemId, PriceSample, Region};
pub use metrics::{Metrics, MetricsSnapshot};
pub use ports::{Ports, WebConfig};
pub use repo::{EventRepository, JobRepository, Store};
