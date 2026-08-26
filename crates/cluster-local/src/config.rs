use cluster_core::{HealthPolicy, NodeCapabilities, RolePolicies};

/// Capability profiles cycled across the simulated nodes.
///
/// V0 mixes them on purpose: scheduling must never quietly assume every node
/// is identical (CLAUDE.md 13).
pub fn default_profiles() -> Vec<NodeCapabilities> {
    vec![
        NodeCapabilities::ESP32_S3,
        NodeCapabilities::ESP32_C6,
        NodeCapabilities::ESP32_C3,
        NodeCapabilities::ESP32_S3,
    ]
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
    /// Fill in plausible load/memory numbers on heartbeats. Clearly SIMULATED;
    /// real nodes will report measured values.
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
            simulate_load: true,
        }
    }
}
