//! Durable writes, off the supervisor's critical path.
//!
//! The supervisor is a single task. When it awaited every `save_task` inline it
//! could not process heartbeats or task reports while SQLite was writing, so a
//! burst of task completions serialised behind a burst of disk writes and the
//! whole cluster appeared to stall.
//!
//! Writes now go to a bounded queue drained by a dedicated task. The queue is
//! bounded on purpose: if the store cannot keep up, the supervisor slows down
//! rather than growing memory without limit. Ordering is preserved because
//! there is exactly one sender and one drainer.
//!
//! The trade-off is a small durability lag -- a crash can lose whatever is
//! still queued. Job *creation* is still awaited inline, so a job never exists
//! in memory without existing in the store; only subsequent updates are
//! deferred.

use cluster_core::{ClusterStore, EventRecord, Job, Millis, NodeId, RoleSet, Task, TaskAttempt};
use tokio::sync::mpsc;

/// Deep enough that normal bursts never touch it, shallow enough to be a real
/// backstop.
const QUEUE_DEPTH: usize = 1024;

pub(crate) enum Write {
    Job(Box<Job>),
    Task(Box<Task>),
    Failure(Box<TaskAttempt>),
    Event(Box<EventRecord>),
    Roles {
        node: NodeId,
        roles: RoleSet,
        at: Millis,
    },
}

#[derive(Clone)]
pub(crate) struct Writer {
    queue: mpsc::Sender<Write>,
    telemetry: std::sync::Arc<crate::telemetry::Telemetry>,
}

impl Writer {
    pub(crate) fn spawn<P: ClusterStore>(
        store: P,
        telemetry: std::sync::Arc<crate::telemetry::Telemetry>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let handle = tokio::spawn(drain(store, rx, telemetry.clone()));
        (
            Self {
                queue: tx,
                telemetry,
            },
            handle,
        )
    }

    /// Applies backpressure when the store falls behind; that is the point.
    async fn push(&self, write: Write) {
        self.telemetry.queued();
        if self.queue.send(write).await.is_err() {
            self.telemetry.dequeued();
            self.telemetry.persistence_error();
            tracing::error!("persistence worker stopped; cluster state is no longer durable");
        }
    }

    pub(crate) async fn job(&self, job: Job) {
        self.push(Write::Job(Box::new(job))).await;
    }

    pub(crate) async fn task(&self, task: Task) {
        self.push(Write::Task(Box::new(task))).await;
    }

    pub(crate) async fn failure(&self, failure: TaskAttempt) {
        self.push(Write::Failure(Box::new(failure))).await;
    }

    pub(crate) async fn event(&self, record: EventRecord) {
        self.push(Write::Event(Box::new(record))).await;
    }

    pub(crate) async fn roles(&self, node: NodeId, roles: RoleSet, at: Millis) {
        self.push(Write::Roles { node, roles, at }).await;
    }
}

async fn drain<P: ClusterStore>(
    store: P,
    mut queue: mpsc::Receiver<Write>,
    telemetry: std::sync::Arc<crate::telemetry::Telemetry>,
) {
    while let Some(write) = queue.recv().await {
        let result = match &write {
            Write::Job(job) => store.save_job(job).await,
            Write::Task(task) => store.save_task(task).await,
            Write::Failure(failure) => store.record_failure(failure).await,
            Write::Event(record) => store.append(record).await,
            Write::Roles { node, roles, at } => store.save_node_roles(*node, *roles, *at).await,
        };
        telemetry.dequeued();
        if let Err(e) = result {
            telemetry.persistence_error();
            // A failed write must not take the cluster down, but it must be
            // loud: the UI is now showing state the store does not have.
            tracing::error!(error = %e, kind = write.kind(), "durable write failed");
        }
    }
}

impl Write {
    fn kind(&self) -> &'static str {
        match self {
            Write::Job(_) => "job",
            Write::Task(_) => "task",
            Write::Failure(_) => "failure",
            Write::Event(_) => "event",
            Write::Roles { .. } => "roles",
        }
    }
}
