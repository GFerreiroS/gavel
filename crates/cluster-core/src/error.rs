//! Core error types. Deliberately small: they carry ids and enums, not
//! backtraces or `std::io::Error`.
use thiserror::Error;

use crate::ids::{JobId, NodeId, TaskId};
use crate::job::{JobState, TaskState};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateError {
    #[error("illegal job transition {from} -> {to}")]
    IllegalJobTransition { from: JobState, to: JobState },
    #[error("illegal task transition {from} -> {to}")]
    IllegalTaskTransition { from: TaskState, to: TaskState },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchedulerError {
    /// No node currently satisfies the task's requirements. The caller should
    /// leave the task queued and retry, not fail the job.
    #[error("no eligible node available")]
    NoEligibleNode,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClusterError {
    #[error("unknown node {0}")]
    UnknownNode(NodeId),
    #[error("unknown job {0}")]
    UnknownJob(JobId),
    #[error("unknown task {0}")]
    UnknownTask(TaskId),
    #[error("node {0} is already running")]
    NodeAlreadyRunning(NodeId),
    #[error("node {0} is not running")]
    NodeNotRunning(NodeId),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    /// The transport/runtime underneath failed, such as a closed channel or
    /// disconnected worker socket.
    #[error("cluster runtime unavailable: {0}")]
    Unavailable(String),
}
