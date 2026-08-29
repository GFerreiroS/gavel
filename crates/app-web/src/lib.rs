//! Server-rendered frontend and HTTP API.
#![forbid(unsafe_code)]

mod assets;
mod cards;
mod chart;
mod csrf;
mod error;
mod format;
mod i18n;
mod metrics;
mod prefs;
mod render;
mod routes;
mod session;
mod views;

pub use error::{WebError, WebResult};
pub use routes::router;
