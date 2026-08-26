//! Portable cluster domain model.
//!
//! Everything in here is deliberately free of `std`, of any async runtime, and
//! of any transport. A node that happens to be a Tokio task on a laptop and a
//! node that happens to be an ESP32-C6 over Wi-Fi are described by the *same*
//! types; only the implementations of the traits differ.
// `no_std` except under `cargo test`, where the test harness needs std.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cluster;
pub mod coordinator;
pub mod error;
pub mod event;
pub mod ids;
pub mod job;
pub mod node;
pub mod persist;
pub mod role;
pub mod scheduler;
pub mod time;
pub mod workload;

#[cfg(test)]
mod tests;

pub use cluster::{ClusterControl, ClusterSnapshot, JobCounts, JobDetail, RoleCounts};
pub use coordinator::{Elector, LowestHealthyId};
pub use error::{ClusterError, SchedulerError, StateError};
pub use event::{ClusterEvent, EventRecord};
pub use ids::{JobId, NodeId, TaskId};
pub use job::{
    FailureReason, Job, JobSpec, JobState, Task, TaskAttempt, TaskOutcome, TaskSpec, TaskState,
};
pub use node::{CpuClass, HealthPolicy, Heartbeat, Node, NodeCapabilities, NodeLoad, NodeStatus};
pub use persist::{ClusterStore, EventLog, JobStore, StoreError, StoreResult};
pub use role::{ALL_ROLES, DEGRADATION_PRIORITY, Role, RolePolicies, RolePolicy, RoleSet};
pub use scheduler::{LeastLoaded, RoundRobin, Scheduler, schedulable};
pub use time::{Clock, Millis};
pub use workload::{TaskWork, count_primes, is_prime, run_task};
