//! The internal cluster event model.
//!
//! Everything interesting that happens is an event: the UI event log, the
//! structured logs and -- later -- replication and coordination all read from
//! this one stream.
use serde::{Deserialize, Serialize};

use crate::ids::{JobId, NodeId, TaskId};
use crate::job::FailureReason;
use crate::role::Role;
use crate::time::Millis;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClusterEvent {
    NodeJoined {
        node: NodeId,
    },
    NodeLeft {
        node: NodeId,
    },
    NodeUnhealthy {
        node: NodeId,
    },
    NodeRecovered {
        node: NodeId,
    },
    RoleAssigned {
        node: NodeId,
        role: Role,
    },
    RoleRemoved {
        node: NodeId,
        role: Role,
    },
    LeaderElected {
        node: NodeId,
    },
    LeaderLost {
        node: NodeId,
    },
    JobCreated {
        job: JobId,
    },
    JobCompleted {
        job: JobId,
    },
    JobFailed {
        job: JobId,
    },
    TaskAssigned {
        task: TaskId,
        node: NodeId,
    },
    TaskCompleted {
        task: TaskId,
        node: NodeId,
    },
    TaskFailed {
        task: TaskId,
        node: Option<NodeId>,
        reason: FailureReason,
    },
    TaskRequeued {
        task: TaskId,
    },
}

/// Coarse category, used for colouring in the UI and filtering in logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity {
    Info,
    Warn,
    Error,
}

impl EventSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            EventSeverity::Info => "info",
            EventSeverity::Warn => "warn",
            EventSeverity::Error => "error",
        }
    }
}

impl ClusterEvent {
    pub const fn severity(&self) -> EventSeverity {
        match self {
            ClusterEvent::NodeUnhealthy { .. }
            | ClusterEvent::NodeLeft { .. }
            | ClusterEvent::TaskRequeued { .. }
            | ClusterEvent::LeaderLost { .. } => EventSeverity::Warn,
            ClusterEvent::TaskFailed { .. } | ClusterEvent::JobFailed { .. } => {
                EventSeverity::Error
            }
            _ => EventSeverity::Info,
        }
    }

    /// Stable machine-readable discriminator, also used as the DB column value.
    pub const fn kind(&self) -> &'static str {
        match self {
            ClusterEvent::NodeJoined { .. } => "node_joined",
            ClusterEvent::NodeLeft { .. } => "node_left",
            ClusterEvent::NodeUnhealthy { .. } => "node_unhealthy",
            ClusterEvent::NodeRecovered { .. } => "node_recovered",
            ClusterEvent::RoleAssigned { .. } => "role_assigned",
            ClusterEvent::RoleRemoved { .. } => "role_removed",
            ClusterEvent::LeaderElected { .. } => "leader_elected",
            ClusterEvent::LeaderLost { .. } => "leader_lost",
            ClusterEvent::JobCreated { .. } => "job_created",
            ClusterEvent::JobCompleted { .. } => "job_completed",
            ClusterEvent::JobFailed { .. } => "job_failed",
            ClusterEvent::TaskAssigned { .. } => "task_assigned",
            ClusterEvent::TaskCompleted { .. } => "task_completed",
            ClusterEvent::TaskFailed { .. } => "task_failed",
            ClusterEvent::TaskRequeued { .. } => "task_requeued",
        }
    }

    /// Human-readable one-liner, as shown in the logs.
    pub fn message(&self) -> String {
        let (pattern, args) = self.message_parts();
        let mut out = pattern.to_string();
        for arg in args {
            if let Some(at) = out.find("{}") {
                out.replace_range(at..at + 2, &arg);
            }
        }
        out
    }

    /// The same message, split into a pattern and its substitutions.
    ///
    /// The UI needs the two apart: `"{} joined"` is a sentence that has to be
    /// translated, `"node-03"` is an identifier that must not be. Composing
    /// them here and translating afterwards would mean translating the node
    /// name too.
    pub fn message_parts(&self) -> (&'static str, Vec<String>) {
        match self {
            ClusterEvent::NodeJoined { node } => ("{} joined", vec![node.to_string()]),
            ClusterEvent::NodeLeft { node } => ("{} left", vec![node.to_string()]),
            ClusterEvent::NodeUnhealthy { node } => ("{} heartbeat lost", vec![node.to_string()]),
            ClusterEvent::NodeRecovered { node } => ("{} recovered", vec![node.to_string()]),
            ClusterEvent::RoleAssigned { node, role } => (
                "{} assigned to {}",
                vec![role.to_string(), node.to_string()],
            ),
            ClusterEvent::RoleRemoved { node, role } => (
                "{} removed from {}",
                vec![role.to_string(), node.to_string()],
            ),
            ClusterEvent::LeaderElected { node } => {
                ("{} elected coordinator", vec![node.to_string()])
            }
            ClusterEvent::LeaderLost { node } => {
                ("{} lost coordinator role", vec![node.to_string()])
            }
            ClusterEvent::JobCreated { job } => ("{} created", vec![job.to_string()]),
            ClusterEvent::JobCompleted { job } => ("{} completed", vec![job.to_string()]),
            ClusterEvent::JobFailed { job } => ("{} failed", vec![job.to_string()]),
            ClusterEvent::TaskAssigned { task, node } => (
                "{} assigned to {}",
                vec![task.to_string(), node.to_string()],
            ),
            ClusterEvent::TaskCompleted { task, node } => (
                "{} completed on {}",
                vec![task.to_string(), node.to_string()],
            ),
            ClusterEvent::TaskFailed { task, node, reason } => match node {
                Some(node) => (
                    "{} failed on {} ({})",
                    vec![task.to_string(), node.to_string(), reason.to_string()],
                ),
                None => ("{} failed ({})", vec![task.to_string(), reason.to_string()]),
            },
            ClusterEvent::TaskRequeued { task } => ("{} requeued", vec![task.to_string()]),
        }
    }

    /// The node this event is about, when there is one.
    pub const fn node(&self) -> Option<NodeId> {
        match *self {
            ClusterEvent::NodeJoined { node }
            | ClusterEvent::NodeLeft { node }
            | ClusterEvent::NodeUnhealthy { node }
            | ClusterEvent::NodeRecovered { node }
            | ClusterEvent::RoleAssigned { node, .. }
            | ClusterEvent::RoleRemoved { node, .. }
            | ClusterEvent::LeaderElected { node }
            | ClusterEvent::LeaderLost { node }
            | ClusterEvent::TaskAssigned { node, .. }
            | ClusterEvent::TaskCompleted { node, .. } => Some(node),
            ClusterEvent::TaskFailed { node, .. } => node,
            _ => None,
        }
    }
}

/// An event plus when it happened and a monotonic sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub seq: u64,
    pub at: Millis,
    pub event: ClusterEvent,
}

impl EventRecord {
    pub fn new(seq: u64, at: Millis, event: ClusterEvent) -> Self {
        Self { seq, at, event }
    }
}
