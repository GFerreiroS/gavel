//! Server-rendered frontend and HTTP API.
#![forbid(unsafe_code)]

mod assets;
mod chart;
mod csrf;
mod error;
mod format;
mod metrics;
mod render;
mod routes;
mod session;
mod views;

pub use error::{WebError, WebResult};
pub use routes::router;
