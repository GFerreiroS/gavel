//! The single owner of cluster state.
//!
//! Everything -- the registry, the job/task tables, the queue, the event log,
//! leadership -- lives inside this one task and is reached only by message.
//! There is no shared mutable state and no lock in the runtime.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cluster_core::{
    Clock, ClusterError, ClusterEvent, ClusterSnapshot, ClusterStore, Elector, EventRecord,
    FailureReason, HealthPolicy, Job, JobCounts, JobDetail, JobId, JobSpec, JobState, Millis, Node,
    NodeCapabilities, NodeId, NodeStatus, RejectReason, Role, RoleCounts, RoleSet, Scheduler, Task,
    TaskAttempt, TaskId, TaskOutcome, TaskState,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::LocalClusterConfig;
use crate::node::{NodeConfig, NodeHandle, NodeInbox, NodeReport, spawn_node};
use crate::persistence::Writer;

type Reply<T> = oneshot::Sender<T>;

/// Requests from [`crate::LocalCluster`] to the supervisor.
pub(crate) enum Command {
    Snapshot(Reply<ClusterSnapshot>),
    Nodes(Reply<Vec<Node>>),
    Node(NodeId, Reply<Option<Node>>),
    Events(usize, Reply<Vec<EventRecord>>),
    Jobs(usize, Reply<Vec<Job>>),
    Job(JobId, Reply<Option<JobDetail>>),
    SubmitJob(JobSpec, Reply<Result<JobId, ClusterError>>),
    SetRole {
        node: NodeId,
        role: Role,
        enabled: bool,
        reply: Reply<Result<(), ClusterError>>,
    },
    StopNode(NodeId, Reply<Result<(), ClusterError>>),
    StartNode(NodeId, Reply<Result<(), ClusterError>>),
    PauseHeartbeat(NodeId, bool, Reply<Result<(), ClusterError>>),
    InjectFailures(NodeId, u32, Reply<Result<(), ClusterError>>),
    SetTaskDelay(NodeId, u64, Reply<Result<(), ClusterError>>),
    /// A worker has connected and completed its handshake.
    AttachRemote {
        /// `None` when the worker is anonymous and wants an id allocated.
        id: Option<NodeId>,
        capabilities: NodeCapabilities,
        inbox: mpsc::Sender<NodeInbox>,
        shutdown: oneshot::Sender<()>,
        reply: Reply<Result<RemoteAttachment, RejectReason>>,
    },
    /// Its connection has dropped. Arrives before the heartbeat timeout would
    /// notice, which is why a worker that closes cleanly is marked offline
    /// immediately instead of lingering as Suspect for six seconds.
    DetachRemote {
        id: NodeId,
        /// Only detach if this is still the live connection. A reconnect that
        /// races the old connection's cleanup must not evict the new one.
        generation: u64,
    },
}

/// What a freshly attached worker is told, and what its connection task needs
/// in order to keep talking.
pub(crate) struct RemoteAttachment {
    /// The identity the worker was given, so the connection can tell it.
    pub id: NodeId,
    pub reports: mpsc::Sender<NodeReport>,
    pub heartbeat_interval_ms: u64,
    pub generation: u64,
}

/// Where a node actually runs. The supervisor needs this only at the edges --
/// launching, stopping, bootstrapping -- never when scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    /// A Tokio task in this process.
    Simulated,
    /// A worker process that connects over the network.
    ///
    /// `declared` separates the two kinds of remote worker, and it decides
    /// what happens when one disconnects. A declared worker has a fixed
    /// identity from configuration and stays in the registry as Offline, so
    /// its roles survive a restart. An undeclared one was allocated an id when
    /// it dialled in and is removed on disconnect: replicas are cattle, and a
    /// registry that accumulated an entry per departed process would grow
    /// without bound.
    Remote { declared: bool },
}

struct NodeEntry {
    node: Node,
    kind: NodeKind,
    /// `None` while the node is stopped, or -- for a remote node -- while it
    /// is simply not connected. The registry entry survives either way so the
    /// node keeps its identity and roles.
    handle: Option<NodeHandle>,
    heartbeat_paused: bool,
    /// Incremented on every attach, so a late `DetachRemote` from a dead
    /// connection cannot evict the connection that replaced it.
    generation: u64,
}

pub(crate) struct Supervisor<P, S, L, C> {
    config: LocalClusterConfig,
    store: P,
    /// Durable writes go here rather than being awaited inline.
    writer: Writer,
    scheduler: S,
    elector: L,
    clock: C,

    nodes: BTreeMap<NodeId, NodeEntry>,
    jobs: BTreeMap<JobId, Job>,
    tasks: BTreeMap<TaskId, Task>,
    queue: VecDeque<TaskId>,
    events: VecDeque<EventRecord>,
    event_seq: u64,
    leader: Option<NodeId>,
    gateway: Option<NodeId>,

    commands: mpsc::Receiver<Command>,
    reports_tx: mpsc::Sender<NodeReport>,
    reports: mpsc::Receiver<NodeReport>,
    broadcast: broadcast::Sender<EventRecord>,
}

impl<P, S, L, C> Supervisor<P, S, L, C>
where
    P: ClusterStore,
    S: Scheduler,
    L: Elector,
    C: Clock,
{
    #[allow(clippy::too_many_arguments)] // a composition root, not an API
    pub(crate) fn new(
        config: LocalClusterConfig,
        store: P,
        writer: Writer,
        scheduler: S,
        elector: L,
        clock: C,
        commands: mpsc::Receiver<Command>,
        broadcast: broadcast::Sender<EventRecord>,
    ) -> Self {
        let (reports_tx, reports) = mpsc::channel(256);
        Self {
            config,
            store,
            writer,
            scheduler,
            elector,
            clock,
            nodes: BTreeMap::new(),
            jobs: BTreeMap::new(),
            tasks: BTreeMap::new(),
            queue: VecDeque::new(),
            events: VecDeque::new(),
            event_seq: 0,
            leader: None,
            gateway: None,
            commands,
            reports_tx,
            reports,
            broadcast,
        }
    }

    pub(crate) async fn run(mut self) {
        self.bootstrap().await;

        let mut tick = tokio::time::interval(std::time::Duration::from_millis(
            self.config.tick_interval_ms.max(50),
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.handle_command(command).await,
                    // Every handle dropped: shut the cluster down.
                    None => break,
                },
                Some(report) = self.reports.recv() => self.handle_report(report).await,
                _ = tick.tick() => self.tick().await,
            }
        }

        self.shutdown().await;
    }

    // --- startup ------------------------------------------------------------

    async fn bootstrap(&mut self) {
        self.event_seq = self.store.last_seq().await.unwrap_or(0);

        let now = self.clock.now();
        for index in 0..self.config.node_count {
            let id = NodeId(index + 1);
            let profile = self.config.profiles[index as usize % self.config.profiles.len().max(1)];
            let mut node = Node::new(id, profile, now);
            node.status = NodeStatus::Healthy;
            self.nodes.insert(
                id,
                NodeEntry {
                    node,
                    kind: NodeKind::Simulated,
                    handle: None,
                    heartbeat_paused: false,
                    generation: 0,
                },
            );
        }

        // A declared remote worker joins the registry immediately but Offline.
        // That is what makes a role assignment survive the process restarting.
        for declared in &self.config.remote_nodes {
            if self.nodes.contains_key(&declared.id) {
                tracing::error!(
                    node = %declared.id,
                    "remote node id collides with a simulated node; ignoring the declaration"
                );
                continue;
            }
            let mut node = Node::new(declared.id, declared.capabilities, now);
            node.status = NodeStatus::Offline;
            self.nodes.insert(
                declared.id,
                NodeEntry {
                    node,
                    kind: NodeKind::Remote { declared: true },
                    handle: None,
                    heartbeat_paused: false,
                    generation: 0,
                },
            );
        }

        self.assign_initial_roles();
        self.restore_roles().await;

        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for id in ids {
            let roles = self.nodes[&id].node.roles;
            self.persist_roles(id, roles).await;
            // A remote node is not launched -- it launches itself, by
            // connecting. It joins the event log when it does, not now.
            if self.nodes[&id].kind == NodeKind::Simulated {
                self.launch_node(id);
                self.emit(ClusterEvent::NodeJoined { node: id }).await;
            }
        }

        self.restore_jobs().await;
        self.refresh_leadership().await;
    }

    /// Deterministic first assignment: every node can compute, and the
    /// role minimums are spread round-robin in degradation-priority order so
    /// no single node accumulates everything.
    fn assign_initial_roles(&mut self) {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            if let Some(entry) = self.nodes.get_mut(id) {
                entry.node.roles.insert(Role::Compute);
            }
        }
        let mut cursor = 0usize;
        for role in cluster_core::DEGRADATION_PRIORITY {
            if role == Role::Compute {
                continue;
            }
            for _ in 0..self.config.policies.get(role).min_replicas {
                // Scan forward for a node that does not already hold this
                // role, rather than taking whichever one the cursor happens to
                // land on: a blind cursor hands the same node a role twice and
                // leaves the minimum unmet with nothing logged.
                let placed = (0..ids.len()).find_map(|offset| {
                    let id = ids[(cursor + offset) % ids.len()];
                    let entry = self.nodes.get_mut(&id)?;
                    let eligible = !entry.node.roles.contains(role);
                    eligible.then(|| {
                        entry.node.roles.insert(role);
                        offset
                    })
                });

                match placed {
                    Some(offset) => cursor += offset + 1,
                    None => {
                        tracing::warn!(
                            role = %role,
                            "no remaining node can hold this role; minimum left unmet"
                        );
                        break;
                    }
                }
            }
        }
    }

    /// Replay stored role assignments over the startup defaults so runtime
    /// changes survive a restart without changing node identity.
    async fn restore_roles(&mut self) {
        let stored = match self.store.load_node_roles().await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::error!(error = %e, "could not load role assignments");
                return;
            }
        };
        let mut restored = 0usize;
        for (id, roles) in stored {
            if let Some(entry) = self.nodes.get_mut(&id) {
                entry.node.roles = roles;
                restored += 1;
            }
        }
        if restored > 0 {
            tracing::info!(nodes = restored, "restored role assignments from store");
        }
    }

    /// Persist one node's current roles. Called on every change, and once per
    /// node at startup so the stored set always reflects reality.
    async fn persist_roles(&self, node: NodeId, roles: RoleSet) {
        self.writer.roles(node, roles, self.clock.now()).await;
    }

    /// Reload recent jobs so the UI survives a restart, and requeue anything
    /// that was still in flight when the process died.
    async fn restore_jobs(&mut self) {
        let recent = match self.store.recent_jobs(self.config.job_buffer).await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!(error = %e, "could not load jobs from store");
                return;
            }
        };
        let now = self.clock.now();
        for job in recent {
            let tasks = self.store.tasks_for_job(job.id).await.unwrap_or_default();
            let unfinished = !job.state.is_terminal();
            self.jobs.insert(job.id, job);
            for mut task in tasks {
                if unfinished && !task.state.is_terminal() {
                    // The node that held it no longer exists in this process.
                    task.state = TaskState::Queued;
                    task.assigned_to = None;
                    task.updated_at = now;
                    let _ = self.store.save_task(&task).await;
                    self.queue.push_back(task.id);
                }
                self.tasks.insert(task.id, task);
            }
        }
        if !self.queue.is_empty() {
            tracing::info!(count = self.queue.len(), "requeued tasks from previous run");
        }
    }

    fn launch_node(&mut self, id: NodeId) {
        let Some(entry) = self.nodes.get_mut(&id) else {
            return;
        };
        let handle = spawn_node(
            NodeConfig {
                id,
                capabilities: entry.node.capabilities,
                heartbeat_interval_ms: self.config.health.heartbeat_interval_ms,
                simulate_load: self.config.simulate_load,
                workload: self.config.workload.clone(),
                artifacts: self.config.artifacts.clone(),
            },
            self.reports_tx.clone(),
        );
        entry.node.status = NodeStatus::Healthy;
        entry.node.last_seen = self.clock.now();
        entry.node.load = Default::default();
        entry.heartbeat_paused = false;
        entry.handle = Some(handle);
    }

    async fn shutdown(&mut self) {
        for (_, entry) in std::mem::take(&mut self.nodes) {
            if let Some(handle) = entry.handle {
                // Ask first, then wait: a clean shutdown lets a node finish
                // what it is doing. A remote node has no join handle to wait
                // on -- closing its socket is the whole of the goodbye.
                let _ = handle.shutdown.send(());
                if let Some(join) = handle.join {
                    let _ = join.await;
                }
            }
        }
    }

    // --- periodic work ------------------------------------------------------

    async fn tick(&mut self) {
        self.sweep_health().await;
        self.refresh_leadership().await;
        self.dispatch().await;
        self.prune_memory();
    }

    /// Healthy -> Suspect -> Offline, purely from heartbeat age.
    async fn sweep_health(&mut self) {
        let now = self.clock.now();
        let policy: HealthPolicy = self.config.health;
        let mut transitions = Vec::new();

        for entry in self.nodes.values_mut() {
            // A node that was deliberately stopped stays offline until it is
            // started again; only running nodes are judged by heartbeat age.
            if entry.handle.is_none() {
                continue;
            }
            let implied = policy.classify(entry.node.last_seen, now);
            let next = implied.unwrap_or(NodeStatus::Healthy);
            if next != entry.node.status {
                entry.node.status = next;
                transitions.push((entry.node.id, next));
            }
        }

        for (id, status) in transitions {
            match status {
                NodeStatus::Suspect => self.emit(ClusterEvent::NodeUnhealthy { node: id }).await,
                NodeStatus::Offline => {
                    self.emit(ClusterEvent::NodeLeft { node: id }).await;
                    self.requeue_tasks_of(id).await;
                }
                NodeStatus::Healthy => self.emit(ClusterEvent::NodeRecovered { node: id }).await,
                NodeStatus::Starting => {}
            }
        }
    }

    /// Gateway and coordinator are chosen separately, because they are
    /// separate concepts even when they land on the same node.
    async fn refresh_leadership(&mut self) {
        let eligible: Vec<Node> = self
            .nodes
            .values()
            .filter(|e| e.node.status.accepts_work() && e.node.has_role(Role::Coordinator))
            .map(|e| e.node)
            .collect();
        let fallback: Vec<Node> = self
            .nodes
            .values()
            .filter(|e| e.node.status.accepts_work())
            .map(|e| e.node)
            .collect();

        let elected = self
            .elector
            .elect(&eligible)
            .or_else(|| self.elector.elect(&fallback));

        if elected != self.leader {
            if let Some(previous) = self.leader {
                self.emit(ClusterEvent::LeaderLost { node: previous }).await;
            }
            if let Some(node) = elected {
                self.emit(ClusterEvent::LeaderElected { node }).await;
            }
            self.leader = elected;
        }

        self.gateway = self
            .nodes
            .values()
            .find(|e| e.node.status.accepts_work() && e.node.has_role(Role::Gateway))
            .map(|e| e.node.id);
    }

    /// Place queued tasks on idle, healthy compute nodes.
    async fn dispatch(&mut self) {
        // Built once per dispatch pass and maintained as we place work. It used
        // to be rebuilt from the whole task table on every iteration, which made
        // draining a queue of N tasks quadratic.
        let mut busy: BTreeSet<NodeId> = self
            .tasks
            .values()
            .filter(|t| matches!(t.state, TaskState::Assigned | TaskState::Running))
            .filter_map(|t| t.assigned_to)
            .collect();

        loop {
            let Some(&task_id) = self.queue.front() else {
                return;
            };
            let Some(task) = self.tasks.get(&task_id).cloned() else {
                self.queue.pop_front();
                continue;
            };

            let candidates: Vec<Node> = self
                .nodes
                .values()
                .filter(|e| e.handle.is_some() && e.node.is_schedulable())
                .filter(|e| !busy.contains(&e.node.id))
                .map(|e| e.node)
                .collect();

            let Ok(chosen) = self.scheduler.select_node(&task, &candidates).await else {
                // Nothing free right now: leave it queued and try next tick.
                return;
            };

            self.queue.pop_front();
            let now = self.clock.now();
            let mut task = task;
            if let Err(e) = task.assign(chosen, now) {
                tracing::error!(task = %task.id, error = %e, "illegal assignment");
                continue;
            }

            // `try_send`, never `send().await`.
            //
            // Awaiting here would let one unresponsive node freeze the entire
            // supervisor: a mailbox fills only when the thing draining it has
            // stopped, and for a remote worker that means a wedged socket.
            // Blocking on it would stall health sweeps,
            // commands and every other worker's work behind one wedged one. A
            // full mailbox means "this node is not keeping up", which is
            // exactly the condition the requeue path below already handles.
            let sent = match self.nodes.get(&chosen).and_then(|e| e.handle.as_ref()) {
                Some(handle) => handle
                    .inbox
                    .try_send(NodeInbox::Assign(Box::new(task.clone())))
                    .is_ok(),
                None => false,
            };

            if !sent {
                // The node vanished, or stopped draining its mailbox, between
                // selection and send. Put the task back.
                let _ = task.requeue(now);
                self.tasks.insert(task.id, task.clone());
                self.queue.push_front(task.id);
                let _ = self.store.save_task(&task).await;
                return;
            }

            busy.insert(chosen);
            self.tasks.insert(task.id, task.clone());
            self.persist_task(&task).await;
            self.start_job_if_queued(task.job_id).await;
            self.emit(ClusterEvent::TaskAssigned {
                task: task.id,
                node: chosen,
            })
            .await;
        }
    }

    /// Keep memory bounded: the store is the durable record, this is a cache.
    fn prune_memory(&mut self) {
        while self.events.len() > self.config.event_buffer {
            self.events.pop_front();
        }
        if self.jobs.len() <= self.config.job_buffer {
            return;
        }
        let removable: Vec<JobId> = self
            .jobs
            .values()
            .filter(|j| j.state.is_terminal())
            .map(|j| j.id)
            .take(self.jobs.len() - self.config.job_buffer)
            .collect();
        let removable: BTreeSet<JobId> = removable.into_iter().collect();
        for id in &removable {
            self.jobs.remove(id);
        }
        // Single sweep: retaining once per removed job re-scanned the whole
        // task table each time.
        self.tasks.retain(|_, t| !removable.contains(&t.job_id));
    }

    // --- reports from nodes -------------------------------------------------

    async fn handle_report(&mut self, report: NodeReport) {
        match report {
            NodeReport::Heartbeat(hb) => {
                // Stamped on arrival by the supervisor's clock, not by the
                // sender's. Worker clocks can have a different epoch, so
                // trusting `hb.at` would classify a healthy node as stale.
                // The sender's own timestamp stays on the wire because clock
                // skew is worth being able to see, but it must not drive
                // health.
                let now = self.clock.now();
                if let Some(entry) = self.nodes.get_mut(&hb.node) {
                    entry.node.last_seen = now;
                    entry.node.load = hb.load;
                }
            }
            NodeReport::TaskStarted { node, task } => {
                let now = self.clock.now();
                if let Some(current) = self.tasks.get_mut(&task.id)
                    && current.assigned_to == Some(node)
                    && current.start(now).is_ok()
                {
                    let snapshot = current.clone();
                    self.persist_task(&snapshot).await;
                }
            }
            NodeReport::TaskFinished {
                node,
                task,
                outcome,
            } => {
                self.finish_task(node, task.id, outcome).await;
                // Place the next task immediately. Waiting for the periodic
                // sweep left a node idle for up to a full tick after every
                // task, which dominated the runtime of any job with more
                // tasks than nodes: 64 tasks over 8 nodes paid that latency
                // eight times over.
                self.dispatch().await;
            }
        }
    }

    async fn finish_task(&mut self, node: NodeId, task_id: TaskId, outcome: TaskOutcome) {
        let now = self.clock.now();
        let Some(mut task) = self.tasks.get(&task_id).cloned() else {
            return;
        };
        // A late report from a node whose task was already requeued elsewhere.
        if task.assigned_to != Some(node) || task.state.is_terminal() {
            return;
        }

        match outcome {
            TaskOutcome::Completed { output } => {
                if task.complete(output, now).is_err() {
                    return;
                }
                self.tasks.insert(task.id, task.clone());
                self.persist_task(&task).await;
                self.emit(ClusterEvent::TaskCompleted {
                    task: task.id,
                    node,
                })
                .await;
                if let Some(job) = self.jobs.get_mut(&task.job_id) {
                    job.tasks_completed = job.tasks_completed.saturating_add(1);
                    let job = job.clone();
                    self.persist_job(&job).await;
                }
                self.settle_job(task.job_id).await;
            }
            TaskOutcome::Failed { reason, detail } => {
                self.record_failure(&task, reason, &detail).await;
                self.emit(ClusterEvent::TaskFailed {
                    task: task.id,
                    node: Some(node),
                    reason,
                })
                .await;
                self.retry_or_give_up(task_id, now).await;
            }
        }
    }

    /// A node went offline holding work: fail the attempt and re-run the task
    /// from the beginning somewhere else.
    async fn requeue_tasks_of(&mut self, node: NodeId) {
        let now = self.clock.now();
        let affected: Vec<TaskId> = self
            .tasks
            .values()
            .filter(|t| t.assigned_to == Some(node) && !t.state.is_terminal())
            .map(|t| t.id)
            .collect();

        for task_id in affected {
            let Some(task) = self.tasks.get(&task_id).cloned() else {
                continue;
            };
            self.record_failure(&task, FailureReason::NodeOffline, "node went offline")
                .await;
            self.emit(ClusterEvent::TaskFailed {
                task: task_id,
                node: Some(node),
                reason: FailureReason::NodeOffline,
            })
            .await;
            self.retry_or_give_up(task_id, now).await;
        }
    }

    async fn retry_or_give_up(&mut self, task_id: TaskId, now: Millis) {
        let Some(mut task) = self.tasks.get(&task_id).cloned() else {
            return;
        };

        if task.attempt < self.config.max_task_attempts {
            if task.requeue(now).is_err() {
                return;
            }
            self.tasks.insert(task.id, task.clone());
            self.persist_task(&task).await;
            self.queue.push_back(task.id);
            self.emit(ClusterEvent::TaskRequeued { task: task.id })
                .await;
            return;
        }

        if task.fail(now).is_err() {
            return;
        }
        self.tasks.insert(task.id, task.clone());
        self.persist_task(&task).await;
        if let Some(job) = self.jobs.get_mut(&task.job_id) {
            job.tasks_failed = job.tasks_failed.saturating_add(1);
            let job = job.clone();
            self.persist_job(&job).await;
        }
        self.settle_job(task.job_id).await;
    }

    async fn record_failure(&mut self, task: &Task, reason: FailureReason, detail: &str) {
        let record = TaskAttempt::new(task, reason, detail, self.clock.now());
        self.writer.failure(record).await;
        tracing::warn!(
            task = %task.id,
            job = %task.job_id,
            node = ?task.assigned_to.map(|n| n.to_string()),
            attempt = task.attempt,
            reason = %reason,
            "task attempt failed: {detail}"
        );
    }

    async fn start_job_if_queued(&mut self, job_id: JobId) {
        let now = self.clock.now();
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        if job.state == JobState::Queued && job.transition_to(JobState::Running, now).is_ok() {
            let job = job.clone();
            self.persist_job(&job).await;
        }
    }

    /// Move a job to a terminal state once every task has settled.
    ///
    /// The outstanding count is derived from the job's own counters rather than
    /// scanning the task table: this runs on every task completion, so a scan
    /// made finishing a job quadratic in its task count.
    async fn settle_job(&mut self, job_id: JobId) {
        let now = self.clock.now();
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        let settled = u32::from(job.tasks_completed) + u32::from(job.tasks_failed);
        if settled < u32::from(job.task_count) {
            return;
        }
        if job.state.is_terminal() {
            return;
        }
        let next = if job.tasks_failed > 0 {
            JobState::Failed
        } else {
            JobState::Completed
        };
        if job.transition_to(next, now).is_err() {
            return;
        }
        let job = job.clone();
        self.persist_job(&job).await;
        let event = if next == JobState::Failed {
            ClusterEvent::JobFailed { job: job_id }
        } else {
            ClusterEvent::JobCompleted { job: job_id }
        };
        self.emit(event).await;
    }

    // --- commands -----------------------------------------------------------

    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
            }
            Command::Nodes(reply) => {
                let _ = reply.send(self.nodes.values().map(|e| e.node).collect());
            }
            Command::Node(id, reply) => {
                let _ = reply.send(self.nodes.get(&id).map(|e| e.node));
            }
            Command::Events(limit, reply) => {
                let _ = reply.send(self.events.iter().rev().take(limit).cloned().collect());
            }
            Command::Jobs(limit, reply) => {
                let mut jobs: Vec<Job> = self.jobs.values().cloned().collect();
                jobs.sort_by_key(|job| core::cmp::Reverse(job.id));
                jobs.truncate(limit);
                let _ = reply.send(jobs);
            }
            Command::Job(id, reply) => {
                let _ = reply.send(self.job_detail(id).await);
            }
            Command::SubmitJob(spec, reply) => {
                let _ = reply.send(self.submit_job(spec).await);
            }
            Command::SetRole {
                node,
                role,
                enabled,
                reply,
            } => {
                let _ = reply.send(self.set_role(node, role, enabled).await);
            }
            Command::StopNode(id, reply) => {
                let _ = reply.send(self.stop_node(id).await);
            }
            Command::StartNode(id, reply) => {
                let _ = reply.send(self.start_node(id).await);
            }
            Command::PauseHeartbeat(id, paused, reply) => {
                let result = self
                    .send_to_node(id, NodeInbox::PauseHeartbeat(paused))
                    .await;
                if result.is_ok()
                    && let Some(entry) = self.nodes.get_mut(&id)
                {
                    entry.heartbeat_paused = paused;
                }
                let _ = reply.send(result);
            }
            Command::InjectFailures(id, count, reply) => {
                let _ = reply.send(
                    self.send_to_node(id, NodeInbox::InjectFailures(count))
                        .await,
                );
            }
            Command::AttachRemote {
                id,
                capabilities,
                inbox,
                shutdown,
                reply,
            } => {
                let _ = reply.send(self.attach_remote(id, capabilities, inbox, shutdown).await);
            }
            Command::DetachRemote { id, generation } => self.detach_remote(id, generation).await,
            Command::SetTaskDelay(id, ms, reply) => {
                let _ = reply.send(self.send_to_node(id, NodeInbox::SetDelay(ms)).await);
            }
        }
    }

    async fn submit_job(&mut self, spec: JobSpec) -> Result<JobId, ClusterError> {
        let now = self.clock.now();
        let (job, tasks) = self
            .store
            .allocate(spec, now)
            .await
            .map_err(|e| ClusterError::Unavailable(e.to_string()))?;
        self.store
            .create_job(&job, &tasks)
            .await
            .map_err(|e| ClusterError::Unavailable(e.to_string()))?;

        let job_id = job.id;
        self.jobs.insert(job_id, job);
        for task in tasks {
            self.queue.push_back(task.id);
            self.tasks.insert(task.id, task);
        }
        self.emit(ClusterEvent::JobCreated { job: job_id }).await;
        self.dispatch().await;
        Ok(job_id)
    }

    async fn job_detail(&self, id: JobId) -> Option<JobDetail> {
        let job = match self.jobs.get(&id) {
            Some(job) => job.clone(),
            None => self.store.job(id).await.ok().flatten()?,
        };
        let mut tasks: Vec<Task> = self
            .tasks
            .values()
            .filter(|t| t.job_id == id)
            .cloned()
            .collect();
        if tasks.is_empty() {
            tasks = self.store.tasks_for_job(id).await.unwrap_or_default();
        }
        tasks.sort_by_key(|t| t.index);
        let failures = self.store.failures_for_job(id).await.unwrap_or_default();
        Some(JobDetail {
            job,
            tasks,
            failures,
        })
    }

    async fn set_role(
        &mut self,
        id: NodeId,
        role: Role,
        enabled: bool,
    ) -> Result<(), ClusterError> {
        let entry = self
            .nodes
            .get_mut(&id)
            .ok_or(ClusterError::UnknownNode(id))?;
        // No capability veto: every worker in this deployment can host any
        // role. Placement constraints belong in the scheduler.
        let changed = if enabled {
            entry.node.roles.insert(role)
        } else {
            entry.node.roles.remove(role)
        };
        if changed {
            let roles = entry.node.roles;
            self.persist_roles(id, roles).await;
            let event = if enabled {
                ClusterEvent::RoleAssigned { node: id, role }
            } else {
                ClusterEvent::RoleRemoved { node: id, role }
            };
            self.emit(event).await;
            self.refresh_leadership().await;
        }
        Ok(())
    }

    // --- worker processes ----------------------------------------------------

    /// The lowest id no node currently holds.
    ///
    /// Reuses ids freed by departed workers rather than counting upwards
    /// forever, so a long-lived coordinator that has seen thousands of
    /// replicas come and go still shows `node-03` instead of `node-4291`.
    fn next_free_id(&self) -> Option<NodeId> {
        (1..=u16::MAX)
            .map(NodeId)
            .find(|id| !self.nodes.contains_key(id))
    }

    /// A worker finished its handshake. From here on it is an ordinary node:
    /// nothing downstream of this function knows it is not a Tokio task.
    async fn attach_remote(
        &mut self,
        id: Option<NodeId>,
        capabilities: NodeCapabilities,
        inbox: mpsc::Sender<NodeInbox>,
        shutdown: oneshot::Sender<()>,
    ) -> Result<RemoteAttachment, RejectReason> {
        let now = self.clock.now();

        // An anonymous worker -- the ordinary case -- is given an identity and
        // a registry entry here. It is not declared anywhere, so it is also
        // removed again when it disconnects.
        let id = match id {
            Some(id) => id,
            None => {
                let id = self.next_free_id().ok_or(RejectReason::Full)?;
                let mut node = Node::new(id, capabilities, now);
                node.status = NodeStatus::Starting;
                // Every worker computes. The other roles describe where the
                // web tier runs, which on a normal deployment is a matter for
                // the process manager rather than for this registry.
                node.roles.insert(Role::Compute);
                self.nodes.insert(
                    id,
                    NodeEntry {
                        node,
                        kind: NodeKind::Remote { declared: false },
                        handle: None,
                        heartbeat_paused: false,
                        generation: 0,
                    },
                );
                id
            }
        };

        let Some(entry) = self.nodes.get_mut(&id) else {
            return Err(RejectReason::UnknownNode);
        };
        if !matches!(entry.kind, NodeKind::Remote { .. }) {
            // That id belongs to a simulated node. Letting a worker claim it
            // would give two different things the same identity.
            return Err(RejectReason::UnknownNode);
        }
        if entry.handle.is_some() {
            if entry.node.status.accepts_work() {
                // Something healthy is already using this identity. For a
                // declared worker that means two processes configured with the
                // same fixed id.
                return Err(RejectReason::AlreadyConnected);
            }

            // The node is not healthy, yet its connection slot is still held.
            //
            // This is what a killed worker looks like from the coordinator:
            // it stops heartbeating, so it is correctly declared Offline, but
            // no FIN ever arrives and TCP will hold that half-open socket for
            // hours. Refusing the worker on that basis would mean one that is
            // hard-killed can never rejoin under its fixed id -- it would be
            // locked out by its own corpse.
            //
            // A worker dialling in is proof the old connection is dead, so
            // evict it. The stale connection's own `DetachRemote` arrives
            // later carrying the previous generation and is ignored.
            tracing::info!(node = %id, "replacing the stale connection of an offline node");
            if let Some(stale) = entry.handle.take() {
                stale.terminate();
            }
        }

        // The worker is the authority on its own resources. If it disagrees
        // with what was declared, believe the worker -- it is the process that
        // knows how many cores it was actually given.
        if entry.node.capabilities != capabilities {
            tracing::warn!(
                node = %id,
                "worker reports different capabilities than the topology declares; using the worker's"
            );
            entry.node.capabilities = capabilities;
        }

        entry.generation = entry.generation.wrapping_add(1);
        entry.node.status = NodeStatus::Healthy;
        entry.node.last_seen = now;
        entry.node.load = Default::default();
        entry.heartbeat_paused = false;
        entry.handle = Some(NodeHandle {
            inbox,
            shutdown,
            join: None,
        });
        let attachment = RemoteAttachment {
            id,
            reports: self.reports_tx.clone(),
            heartbeat_interval_ms: self.config.health.heartbeat_interval_ms,
            generation: entry.generation,
        };

        // Roles were assigned at bootstrap and possibly changed at runtime;
        // re-persist so a worker's first connection cannot lose them.
        let roles = entry.node.roles;
        self.persist_roles(id, roles).await;
        self.emit(ClusterEvent::NodeJoined { node: id }).await;
        self.refresh_leadership().await;
        self.dispatch().await;
        Ok(attachment)
    }

    /// A worker's connection dropped. Its in-flight work is requeued at once
    /// rather than waiting out the heartbeat timeout -- a closed socket is
    /// proof, where silence is only evidence.
    async fn detach_remote(&mut self, id: NodeId, generation: u64) {
        let Some(entry) = self.nodes.get_mut(&id) else {
            return;
        };
        if entry.generation != generation || entry.handle.is_none() {
            // A stale detach from a connection that has already been replaced.
            return;
        }
        entry.handle = None;
        entry.node.status = NodeStatus::Offline;
        entry.node.load = Default::default();
        let declared = matches!(entry.kind, NodeKind::Remote { declared: true });

        self.emit(ClusterEvent::NodeLeft { node: id }).await;
        // Requeue *before* forgetting the node: the requeue path looks work up
        // by the node it was assigned to.
        self.requeue_tasks_of(id).await;

        if !declared {
            // An anonymous worker has no identity to come back to. Keeping a
            // tombstone per departed replica would grow the registry without
            // bound and fill the nodes page with things that no longer exist.
            self.nodes.remove(&id);
            self.persist_roles(id, RoleSet::EMPTY).await;
        }

        self.refresh_leadership().await;
        self.dispatch().await;
    }

    async fn stop_node(&mut self, id: NodeId) -> Result<(), ClusterError> {
        let entry = self
            .nodes
            .get_mut(&id)
            .ok_or(ClusterError::UnknownNode(id))?;
        let handle = entry
            .handle
            .take()
            .ok_or(ClusterError::NodeNotRunning(id))?;
        handle.terminate();
        entry.node.status = NodeStatus::Offline;
        entry.node.load = Default::default();

        self.emit(ClusterEvent::NodeLeft { node: id }).await;
        self.requeue_tasks_of(id).await;
        self.refresh_leadership().await;
        self.dispatch().await;
        Ok(())
    }

    async fn start_node(&mut self, id: NodeId) -> Result<(), ClusterError> {
        let entry = self.nodes.get(&id).ok_or(ClusterError::UnknownNode(id))?;
        if entry.handle.is_some() {
            return Err(ClusterError::NodeAlreadyRunning(id));
        }
        if matches!(entry.kind, NodeKind::Remote { .. }) {
            // Nothing here can make a worker reconnect: it dials in, not out.
            // Saying so is more useful than pretending the button worked.
            return Err(ClusterError::Unavailable(format!(
                "{id} is a remote worker and rejoins by reconnecting on its own"
            )));
        }
        self.launch_node(id);
        self.emit(ClusterEvent::NodeJoined { node: id }).await;
        self.refresh_leadership().await;
        self.dispatch().await;
        Ok(())
    }

    async fn send_to_node(&self, id: NodeId, message: NodeInbox) -> Result<(), ClusterError> {
        let entry = self.nodes.get(&id).ok_or(ClusterError::UnknownNode(id))?;
        let handle = entry
            .handle
            .as_ref()
            .ok_or(ClusterError::NodeNotRunning(id))?;
        // Non-blocking for the same reason as dispatch: a control message must
        // never be able to wedge the supervisor on a node that has stopped
        // reading.
        handle
            .inbox
            .try_send(message)
            .map_err(|_| ClusterError::Unavailable(format!("{id} is not responding")))
    }

    // --- derived views ------------------------------------------------------

    fn snapshot(&self) -> ClusterSnapshot {
        let mut roles = RoleCounts::default();
        let mut online = 0usize;
        for entry in self.nodes.values() {
            if !entry.node.status.accepts_work() {
                continue;
            }
            online += 1;
            for role in entry.node.roles.iter() {
                roles.increment(role);
            }
        }

        let mut jobs = JobCounts::default();
        for job in self.jobs.values() {
            match job.state {
                JobState::Queued => jobs.queued += 1,
                JobState::Running => jobs.running += 1,
                JobState::Completed => jobs.completed += 1,
                JobState::Failed => jobs.failed += 1,
                JobState::Cancelled => jobs.cancelled += 1,
            }
        }

        ClusterSnapshot {
            nodes_total: self.nodes.len(),
            nodes_online: online,
            roles,
            policies: self.config.policies,
            jobs,
            tasks_running: self
                .tasks
                .values()
                .filter(|t| matches!(t.state, TaskState::Running | TaskState::Assigned))
                .count(),
            tasks_queued: self.queue.len(),
            leader: self.leader,
            gateway: self.gateway,
        }
    }

    // --- plumbing -----------------------------------------------------------

    async fn emit(&mut self, event: ClusterEvent) {
        self.event_seq += 1;
        let record = EventRecord::new(self.event_seq, self.clock.now(), event);

        match record.event.severity() {
            cluster_core::event::EventSeverity::Error => {
                tracing::error!(
                    seq = record.seq,
                    kind = record.event.kind(),
                    "{}",
                    record.event.message()
                )
            }
            cluster_core::event::EventSeverity::Warn => {
                tracing::warn!(
                    seq = record.seq,
                    kind = record.event.kind(),
                    "{}",
                    record.event.message()
                )
            }
            cluster_core::event::EventSeverity::Info => {
                tracing::info!(
                    seq = record.seq,
                    kind = record.event.kind(),
                    "{}",
                    record.event.message()
                )
            }
        }

        self.writer.event(record.clone()).await;
        // No subscribers is normal: nobody has the page open.
        let _ = self.broadcast.send(record.clone());
        self.events.push_back(record);
    }

    async fn persist_task(&self, task: &Task) {
        self.writer.task(task.clone()).await;
    }

    async fn persist_job(&self, job: &Job) {
        self.writer.job(job.clone()).await;
    }
}
