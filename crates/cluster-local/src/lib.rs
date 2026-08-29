//! Local (single-process) cluster runtime.
//!
//! Architecture: every in-process node is its own Tokio task with its own
//! mailbox, and all cluster state lives inside a single supervisor task that
//! is reached only by message passing. Nothing is shared behind a mutex.
#![forbid(unsafe_code)]

mod clock;
mod cluster;
mod config;
mod exec;
mod node;
mod persistence;
mod remote;
mod supervisor;

pub use clock::SystemClock;
pub use cluster::{EventStream, LocalCluster};
pub use config::{LocalClusterConfig, RemoteNode, default_profiles};
pub use exec::execute_task;
