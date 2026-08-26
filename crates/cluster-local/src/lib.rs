//! Local (single-process) cluster runtime.
//!
//! Architecture: every simulated node is its own Tokio task with its own
//! mailbox, and all cluster state lives inside a single supervisor task that
//! is reached only by message passing. Nothing is shared behind a mutex.
//!
//! That is a deliberate choice rather than the easiest one. The eventual
//! cluster is a set of microcontrollers exchanging messages over an unreliable
//! link; modelling it as shared memory now would produce code that cannot be
//! ported later.
#![forbid(unsafe_code)]

mod clock;
mod cluster;
mod config;
mod exec;
mod node;
mod persistence;
mod supervisor;

pub use clock::SystemClock;
pub use cluster::{EventStream, LocalCluster};
pub use config::{LocalClusterConfig, default_profiles};
pub use exec::execute_task;
