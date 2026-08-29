use std::net::SocketAddr;

use cluster_core::{HealthPolicy, NodeCapabilities, NodeId, RolePolicies};

/// What an in-process worker claims to be.
///
/// One profile, because in-process workers all share this machine. Workers on
/// other machines report their own capabilities when they connect, so a mixed
/// cluster is still described accurately -- the scheduler must never assume
/// every worker is identical.
pub fn default_profiles() -> Vec<NodeCapabilities> {
    vec![NodeCapabilities::local()]
}

/// A worker with a *fixed* identity, declared up front.
///
/// The unusual case. An ordinary worker is anonymous: it dials in, is given an
/// id, and is forgotten when it leaves. Declare one only when its identity has
/// to survive a restart -- for example, when it is pinned to a volume or a
/// named host -- because a declared worker keeps its registry entry and its
/// roles, while it is offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteNode {
    pub id: NodeId,
    /// What to assume about the worker until it connects and says otherwise.
    pub capabilities: NodeCapabilities,
}

#[derive(Debug, Clone)]
pub struct LocalClusterConfig {
    pub node_count: u16,
    /// Heartbeat interval and the suspect/offline thresholds.
    pub health: HealthPolicy,
    /// Desired replicas per role.
    pub policies: RolePolicies,
    /// How often the supervisor sweeps health, elects and dispatches.
    pub tick_interval_ms: u64,
    /// Events retained in memory for the UI.
    pub event_buffer: usize,
    /// Jobs retained in memory; older ones stay only in the store.
    pub job_buffer: usize,
    /// Total execution attempts allowed per task before it fails for good.
    pub max_task_attempts: u16,
    /// Capability profiles, cycled across nodes.
    pub profiles: Vec<NodeCapabilities>,
    /// Workers with fixed identities, declared up front. Their ids must not
    /// collide with the in-process workers, which occupy `1..=node_count`.
    ///
    /// Usually empty: workers normally arrive anonymously and are given an id.
    pub remote_nodes: Vec<RemoteNode>,
    /// Where to accept worker connections. `None` means this process runs
    /// alone with its in-process pool, which is the default and is all a
    /// single-server deployment needs until it outgrows one machine.
    pub node_listen: Option<SocketAddr>,
    /// Shared secret a connecting worker must present.
    ///
    /// `None` is only sound with `node_listen` at `None` too: an open socket
    /// with no token admits anyone who can reach it. The server refuses that
    /// combination at startup rather than leaving it to be noticed later.
    pub join_token: Option<String>,
    /// Fill in plausible load/memory numbers for in-process workers. Remote
    /// workers report their own values.
    pub simulate_load: bool,
}

impl Default for LocalClusterConfig {
    fn default() -> Self {
        Self {
            node_count: 8,
            health: HealthPolicy::default(),
            policies: RolePolicies::default(),
            tick_interval_ms: 500,
            event_buffer: 200,
            job_buffer: 200,
            max_task_attempts: 3,
            profiles: default_profiles(),
            remote_nodes: Vec::new(),
            node_listen: None,
            join_token: None,
            simulate_load: true,
        }
    }
}
