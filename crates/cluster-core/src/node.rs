//! Nodes, capabilities and health.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::NodeId;
use crate::role::{Role, RoleSet};
use crate::time::Millis;

/// What a worker can do.
///
/// Deliberately small and generic: how much work it can take, and how much
/// memory it has to do it in. Anything scheduling needs to know belongs here;
/// anything it does not need is noise the coordinator has to carry per worker
/// on every placement pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    pub cores: u8,
    pub memory_bytes: u64,
}

impl NodeCapabilities {
    /// What this machine actually offers.
    ///
    /// A worker reports its own numbers rather than trusting coordinator
    /// config, because a container may have been given a fraction of the host:
    /// `available_parallelism` honours cgroup CPU limits where the host's core
    /// count does not.
    pub fn local() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get().min(u8::MAX as usize) as u8)
            .unwrap_or(1);
        Self {
            cores,
            // Not measured: reading real memory limits needs a platform crate,
            // and nothing schedules on this yet. Workers that care can set it.
            memory_bytes: 0,
        }
    }

    pub const fn new(cores: u8, memory_bytes: u64) -> Self {
        Self {
            cores,
            memory_bytes,
        }
    }

    pub const fn usable_ram_bytes(&self) -> u64 {
        self.memory_bytes
    }

    /// Rough capacity weight used by the scheduler as a tie-breaker.
    pub const fn compute_weight(&self) -> u32 {
        self.cores as u32 * 100
    }
}

/// Health state machine: Healthy -> Suspect -> Offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Starting,
    Healthy,
    Suspect,
    Offline,
}

impl NodeStatus {
    /// Every variant, so exhaustive checks (tests, translation coverage) do
    /// not have to repeat the list and drift from it.
    pub const ALL: [NodeStatus; 4] = [
        NodeStatus::Starting,
        NodeStatus::Healthy,
        NodeStatus::Suspect,
        NodeStatus::Offline,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            NodeStatus::Starting => "starting",
            NodeStatus::Healthy => "healthy",
            NodeStatus::Suspect => "suspect",
            NodeStatus::Offline => "offline",
        }
    }

    /// Inverse of [`NodeStatus::as_str`].
    pub fn parse(s: &str) -> Option<NodeStatus> {
        [
            NodeStatus::Starting,
            NodeStatus::Healthy,
            NodeStatus::Suspect,
            NodeStatus::Offline,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }

    /// Only healthy nodes receive new work.
    pub const fn accepts_work(self) -> bool {
        matches!(self, NodeStatus::Healthy)
    }
}

impl fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sampled load. In-process workers currently report simulated figures;
/// remote workers can report measurements from their process or container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeLoad {
    pub load_percent: u8,
    pub running_tasks: u16,
    pub free_memory_bytes: u64,
    /// True while the values above are made up rather than measured.
    pub simulated: bool,
}

/// `Copy` on purpose: the supervisor clones the node list on every scheduling
/// pass. The display name was a `String` that only ever held `id.to_string()`,
/// so it is derived at render time instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub status: NodeStatus,
    pub roles: RoleSet,
    pub capabilities: NodeCapabilities,
    pub load: NodeLoad,
    pub last_seen: Millis,
    pub joined_at: Millis,
}

impl Node {
    pub fn new(id: NodeId, capabilities: NodeCapabilities, at: Millis) -> Self {
        Self {
            id,
            status: NodeStatus::Starting,
            roles: RoleSet::EMPTY,
            capabilities,
            load: NodeLoad::default(),
            last_seen: at,
            joined_at: at,
        }
    }

    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(role)
    }

    pub fn is_schedulable(&self) -> bool {
        self.status.accepts_work() && self.has_role(Role::Compute)
    }

    pub fn age_ms(&self, now: Millis) -> u64 {
        now.since(self.last_seen)
    }
}

/// One heartbeat from a node to whoever is tracking the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node: NodeId,
    pub load: NodeLoad,
    pub at: Millis,
}

/// Heartbeat timeouts live in this configuration instead of being scattered
/// through the coordinator as magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthPolicy {
    pub heartbeat_interval_ms: u64,
    pub suspect_after_ms: u64,
    pub offline_after_ms: u64,
}

impl HealthPolicy {
    /// Status implied purely by heartbeat age. Returns `None` while the node
    /// is still within its heartbeat budget, meaning "leave it alone".
    pub fn classify(&self, last_seen: Millis, now: Millis) -> Option<NodeStatus> {
        let age = now.since(last_seen);
        if age >= self.offline_after_ms {
            Some(NodeStatus::Offline)
        } else if age >= self.suspect_after_ms {
            Some(NodeStatus::Suspect)
        } else {
            None
        }
    }
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 1_000,
            suspect_after_ms: 3_000,
            offline_after_ms: 6_000,
        }
    }
}
