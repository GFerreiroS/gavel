//! Nodes, capabilities and health.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::NodeId;
use crate::role::{Role, RoleSet};
use crate::time::Millis;

/// Broad hardware family. Scheduling must never branch on a *specific* board
/// (CLAUDE.md 12) -- only on capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CpuClass {
    /// The development host. Effectively unlimited by ESP standards.
    Host,
    /// Xtensa LX6/LX7 (ESP32, ESP32-S3, ...).
    Xtensa,
    /// RISC-V (ESP32-C3, C6, H2, ...).
    RiscV32,
}

impl CpuClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            CpuClass::Host => "host",
            CpuClass::Xtensa => "xtensa",
            CpuClass::RiscV32 => "riscv32",
        }
    }
}

impl fmt::Display for CpuClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a node can physically do. Schedulers and role assignment read this;
/// they never read a board name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    pub cpu_class: CpuClass,
    pub cores: u8,
    pub memory_bytes: u64,
    pub flash_bytes: u64,
    pub psram_bytes: Option<u64>,
    pub has_sd: bool,
    pub has_display: bool,
    pub has_wifi: bool,
    pub has_ethernet: bool,
}

impl NodeCapabilities {
    /// A worker process on the dev PC.
    pub const HOST: NodeCapabilities = NodeCapabilities {
        cpu_class: CpuClass::Host,
        cores: 4,
        memory_bytes: 512 * 1024 * 1024,
        flash_bytes: 8 * 1024 * 1024 * 1024,
        psram_bytes: None,
        has_sd: true,
        has_display: false,
        has_wifi: true,
        has_ethernet: true,
    };

    /// Roughly an ESP32-S3 with octal PSRAM.
    pub const ESP32_S3: NodeCapabilities = NodeCapabilities {
        cpu_class: CpuClass::Xtensa,
        cores: 2,
        memory_bytes: 512 * 1024,
        flash_bytes: 8 * 1024 * 1024,
        psram_bytes: Some(8 * 1024 * 1024),
        has_sd: true,
        has_display: true,
        has_wifi: true,
        has_ethernet: false,
    };

    /// Roughly an ESP32-C3: single RISC-V core, no PSRAM.
    pub const ESP32_C3: NodeCapabilities = NodeCapabilities {
        cpu_class: CpuClass::RiscV32,
        cores: 1,
        memory_bytes: 400 * 1024,
        flash_bytes: 4 * 1024 * 1024,
        psram_bytes: None,
        has_sd: false,
        has_display: false,
        has_wifi: true,
        has_ethernet: false,
    };

    /// Roughly an ESP32-C6.
    pub const ESP32_C6: NodeCapabilities = NodeCapabilities {
        cpu_class: CpuClass::RiscV32,
        cores: 1,
        memory_bytes: 512 * 1024,
        flash_bytes: 4 * 1024 * 1024,
        psram_bytes: None,
        has_sd: false,
        has_display: false,
        has_wifi: true,
        has_ethernet: false,
    };

    pub const fn usable_ram_bytes(&self) -> u64 {
        match self.psram_bytes {
            Some(psram) => self.memory_bytes + psram,
            None => self.memory_bytes,
        }
    }

    /// Can this node serve HTTP to the outside world at all?
    pub const fn can_gateway(&self) -> bool {
        self.has_wifi || self.has_ethernet
    }

    /// Rough capacity weight used by the scheduler as a tie-breaker.
    pub const fn compute_weight(&self) -> u32 {
        self.cores as u32 * 100
    }
}

/// Health state machine: Healthy -> Suspect -> Offline (CLAUDE.md 18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Starting,
    Healthy,
    Suspect,
    Offline,
}

impl NodeStatus {
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

/// Sampled load. On the PC these numbers are SIMULATED; on real hardware they
/// will come from the FreeRTOS idle-task counters and heap stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeLoad {
    pub load_percent: u8,
    pub running_tasks: u16,
    pub free_memory_bytes: u64,
    /// True while the values above are made up rather than measured.
    pub simulated: bool,
}

/// `Copy` on purpose: the supervisor clones the node list on every scheduling
/// pass, and a heap allocation per node per pass is exactly the kind of cost a
/// microcontroller cannot absorb. The display name was a `String` that only
/// ever held `id.to_string()`, so it is derived at render time instead.
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

/// Timeouts live here, in configuration, not scattered as magic numbers
/// (CLAUDE.md 18).
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
