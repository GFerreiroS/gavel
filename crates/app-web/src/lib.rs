//! Server-rendered frontend and HTTP API.
#![forbid(unsafe_code)]

mod assets;
mod cards;
mod chart;
mod csrf;
mod error;
mod format;
mod headers;
mod i18n;
mod metrics;
mod prefs;
mod read_model;
mod render;
mod routes;
mod session;
mod shutdown;
mod throttle;
mod views;

pub use error::{WebError, WebResult};
pub use routes::router;
pub use shutdown::Shutdown;
