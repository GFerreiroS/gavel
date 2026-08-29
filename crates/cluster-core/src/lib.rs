//! The work cluster's domain model.
//!
//! Everything in here is free of any async runtime and of any transport. A
//! worker that happens to be a Tokio task in this process and a worker that
//! happens to be a separate process on another machine are described by the
//! *same* types; only the implementations of the traits differ.
//!
//! That separation is what makes the scheduler, the job state machine and the
//! retry rules testable without standing up a runtime or opening a socket.
#![forbid(unsafe_code)]

pub mod agent;
pub mod cluster;
pub mod coordinator;
pub mod error;
pub mod event;
pub mod ids;
pub mod job;
pub mod node;
pub mod persist;
pub mod protocol;
pub mod role;
pub mod scheduler;
pub mod time;
pub mod workload;

#[cfg(test)]
mod tests;

pub use agent::{Action, Agent};
pub use cluster::{ClusterControl, ClusterSnapshot, JobCounts, JobDetail, RoleCounts};
pub use coordinator::{Elector, LowestHealthyId};
pub use error::{ClusterError, SchedulerError, StateError};
pub use event::{ClusterEvent, EventRecord};
pub use ids::{JobId, NodeId, TaskId};
pub use job::{
    FailureReason, Job, JobSpec, JobState, Task, TaskAttempt, TaskOutcome, TaskSpec, TaskState,
};
pub use node::{HealthPolicy, Heartbeat, Node, NodeCapabilities, NodeLoad, NodeStatus};
pub use persist::{ClusterStore, EventLog, JobStore, StoreError, StoreResult};
pub use protocol::{
    MAX_FRAME, NodeMessage, PROTOCOL_VERSION, ProtocolError, RejectReason, SupervisorMessage,
    WireTaskSpec, decode_frame, encode_frame, frame_len, token_accepted,
};
pub use role::{ALL_ROLES, DEGRADATION_PRIORITY, Role, RolePolicies, RolePolicy, RoleSet};
pub use scheduler::{LeastLoaded, RoundRobin, Scheduler, schedulable};
pub use time::{Clock, Millis};
pub use workload::{TaskWork, count_primes, is_prime, primes_output, run_task};
