//! The port the application talks to.
//!
//! `app-web` and `app-core` depend on this trait, never on `cluster-local`.
//! Swapping the in-process worker pool for networked workers is then a matter of
//! providing another implementor.

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::future::Future;

use crate::error::ClusterError;
use crate::event::EventRecord;
use crate::ids::{JobId, NodeId};
use crate::job::{Job, JobSpec, Task, TaskAttempt};
use crate::node::Node;
use crate::role::{Role, RolePolicies};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCounts {
    pub queued: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl JobCounts {
    pub fn total(&self) -> usize {
        self.queued + self.running + self.completed + self.failed + self.cancelled
    }
}

/// How many healthy nodes currently hold each role, indexed by `Role::index`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCounts(pub [usize; 6]);

impl RoleCounts {
    pub fn get(&self, role: Role) -> usize {
        self.0[role.index()]
    }

    pub fn increment(&mut self, role: Role) {
        self.0[role.index()] += 1;
    }
}

/// `Default` is the "supervisor is not answering" view: an empty cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSnapshot {
    pub nodes_total: usize,
    pub nodes_online: usize,
    pub roles: RoleCounts,
    pub policies: RolePolicies,
    pub jobs: JobCounts,
    pub tasks_running: usize,
    pub tasks_queued: usize,
    /// Coordinates cluster state.
    pub leader: Option<NodeId>,
    /// Accepts external HTTP traffic. Separate concept from the leader.
    pub gateway: Option<NodeId>,
    /// Low-cardinality operational saturation signals.
    pub worker_connections: usize,
    pub worker_preauth: usize,
    pub worker_rejected: u64,
    pub persistence_queue: usize,
    pub persistence_oldest_ms: u64,
    pub persistence_errors: u64,
    pub jobs_recovered: u64,
    pub task_retries: u64,
}

impl ClusterSnapshot {
    pub fn is_degraded(&self) -> bool {
        self.nodes_online < self.nodes_total
            || !self.policies.unmet(|r| self.roles.get(r)).is_empty()
    }

    pub fn status_label(&self) -> &'static str {
        if self.nodes_online == 0 {
            "down"
        } else if self.is_degraded() {
            "degraded"
        } else {
            "healthy"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobDetail {
    pub job: Job,
    pub tasks: Vec<Task>,
    pub failures: Vec<TaskAttempt>,
}

/// Everything the web layer is allowed to ask the cluster to do.
///
/// The `-> impl Future + Send` shape (rather than `#[async_trait]`) keeps this
/// dyn-free and allocation-free per call; see `scheduler.rs` for the rationale.
pub trait ClusterControl: Send + Sync + 'static {
    /// Live event stream, for pushing updates to connected clients.
    ///
    /// A named associated type rather than `impl Stream`, so the stream is
    /// plainly independent of the borrow it was created from -- and a `Stream`
    /// rather than a channel type, so the runtime stays swappable (for
    /// example, from an in-process broadcast to an external event bus).
    type Events: Stream<Item = EventRecord> + Send + Unpin + 'static;

    fn snapshot(&self) -> impl Future<Output = ClusterSnapshot> + Send;

    fn subscribe(&self) -> Self::Events;

    fn nodes(&self) -> impl Future<Output = Vec<Node>> + Send;

    fn node(&self, id: NodeId) -> impl Future<Output = Option<Node>> + Send;

    fn recent_events(&self, limit: usize) -> impl Future<Output = Vec<EventRecord>> + Send;

    fn jobs(&self, limit: usize) -> impl Future<Output = Vec<Job>> + Send;

    fn job(&self, id: JobId) -> impl Future<Output = Option<JobDetail>> + Send;

    fn submit_job(&self, spec: JobSpec)
    -> impl Future<Output = Result<JobId, ClusterError>> + Send;

    fn set_role(
        &self,
        node: NodeId,
        role: Role,
        enabled: bool,
    ) -> impl Future<Output = Result<(), ClusterError>> + Send;

    // --- failure simulation controls -----------------------------------------
    // Debug-only surface; the server mounts these behind a flag.

    fn stop_node(&self, node: NodeId) -> impl Future<Output = Result<(), ClusterError>> + Send;

    fn start_node(&self, node: NodeId) -> impl Future<Output = Result<(), ClusterError>> + Send;

    fn pause_heartbeat(
        &self,
        node: NodeId,
        paused: bool,
    ) -> impl Future<Output = Result<(), ClusterError>> + Send;

    /// Make the node's next `count` tasks fail.
    fn inject_failures(
        &self,
        node: NodeId,
        count: u32,
    ) -> impl Future<Output = Result<(), ClusterError>> + Send;

    /// Add an artificial per-task delay on the node.
    fn set_task_delay(
        &self,
        node: NodeId,
        millis: u64,
    ) -> impl Future<Output = Result<(), ClusterError>> + Send;
}
