//! The internal cluster event model (CLAUDE.md 17).
//!
//! Everything interesting that happens is an event: the UI event log, the
//! structured logs and -- later -- replication and coordination all read from
//! this one stream.

use alloc::string::String;
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

    /// Human-readable one-liner, as shown in the UI event log.
    pub fn message(&self) -> String {
        use alloc::format;
        match self {
            ClusterEvent::NodeJoined { node } => format!("{node} joined"),
            ClusterEvent::NodeLeft { node } => format!("{node} left"),
            ClusterEvent::NodeUnhealthy { node } => format!("{node} heartbeat lost"),
            ClusterEvent::NodeRecovered { node } => format!("{node} recovered"),
            ClusterEvent::RoleAssigned { node, role } => format!("{role} assigned to {node}"),
            ClusterEvent::RoleRemoved { node, role } => format!("{role} removed from {node}"),
            ClusterEvent::LeaderElected { node } => format!("{node} elected coordinator"),
            ClusterEvent::LeaderLost { node } => format!("{node} lost coordinator role"),
            ClusterEvent::JobCreated { job } => format!("{job} created"),
            ClusterEvent::JobCompleted { job } => format!("{job} completed"),
            ClusterEvent::JobFailed { job } => format!("{job} failed"),
            ClusterEvent::TaskAssigned { task, node } => format!("{task} assigned to {node}"),
            ClusterEvent::TaskCompleted { task, node } => format!("{task} completed on {node}"),
            ClusterEvent::TaskFailed { task, node, reason } => match node {
                Some(node) => format!("{task} failed on {node} ({reason})"),
                None => format!("{task} failed ({reason})"),
            },
            ClusterEvent::TaskRequeued { task } => format!("{task} requeued"),
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
