//! Persistence ports for cluster state.
//!
//! These live in `cluster-core` rather than in the application layer on
//! purpose: jobs, tasks, failures and events are cluster concepts, and the
//! runtime must be able to durably record them without depending on anything
//! application-shaped. `storage` implements them over SQLite today; another
//! adapter could implement them over a shared database.

use std::future::Future;
use thiserror::Error;

use crate::event::EventRecord;
use crate::ids::{JobId, NodeId, TaskId};
use crate::job::{Job, JobSpec, Task, TaskAttempt};
use crate::role::RoleSet;
use crate::time::Millis;

pub type StoreResult<T> = Result<T, StoreError>;

/// What a store can go wrong with, stated without naming a database.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
    /// Anything the backing store itself reported.
    #[error("storage backend: {0}")]
    Backend(String),
}

/// Durable record of what the cluster was asked to do and what happened.
///
/// The runtime keeps hot state in memory; this is the copy that survives a
/// restart and backs the `/jobs` pages.
pub trait JobStore: Send + Sync + 'static {
    fn next_job_id(&self) -> impl Future<Output = StoreResult<JobId>> + Send;

    /// Reserve `count` contiguous task ids and return the first.
    ///
    /// One round-trip per job rather than one per task: submitting a 64-task
    /// job used to cost 65 sequential database writes before any work started.
    fn reserve_task_ids(&self, count: u64) -> impl Future<Output = StoreResult<TaskId>> + Send;

    /// A job and its tasks must appear together or not at all.
    fn create_job(&self, job: &Job, tasks: &[Task])
    -> impl Future<Output = StoreResult<()>> + Send;

    fn save_job(&self, job: &Job) -> impl Future<Output = StoreResult<()>> + Send;

    fn save_task(&self, task: &Task) -> impl Future<Output = StoreResult<()>> + Send;

    fn record_failure(&self, failure: &TaskAttempt)
    -> impl Future<Output = StoreResult<()>> + Send;

    fn job(&self, id: JobId) -> impl Future<Output = StoreResult<Option<Job>>> + Send;

    fn recent_jobs(&self, limit: usize) -> impl Future<Output = StoreResult<Vec<Job>>> + Send;

    fn tasks_for_job(&self, id: JobId) -> impl Future<Output = StoreResult<Vec<Task>>> + Send;

    fn failures_for_job(
        &self,
        id: JobId,
    ) -> impl Future<Output = StoreResult<Vec<TaskAttempt>>> + Send;

    /// Jobs that were mid-flight when the process died, so the runtime can
    /// decide what to do with them on boot.
    fn unfinished_jobs(&self) -> impl Future<Output = StoreResult<Vec<(Job, Vec<Task>)>>> + Send;

    /// Page unfinished jobs by monotonically increasing id. The default keeps
    /// compatibility for simple stores; durable adapters should query it
    /// directly so restart memory is bounded.
    fn unfinished_jobs_page(
        &self,
        after: Option<JobId>,
        limit: usize,
    ) -> impl Future<Output = StoreResult<Vec<(Job, Vec<Task>)>>> + Send {
        async move {
            let mut jobs = self.unfinished_jobs().await?;
            jobs.retain(|(job, _)| after.is_none_or(|id| job.id > id));
            jobs.truncate(limit);
            Ok(jobs)
        }
    }

    fn prune_terminal_before(
        &self,
        _before: Millis,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        async { Ok(0) }
    }

    /// Allocate ids and build a job plus its split tasks.
    fn allocate(
        &self,
        spec: JobSpec,
        now: Millis,
    ) -> impl Future<Output = StoreResult<(Job, Vec<Task>)>> + Send
    where
        Self: Sized,
    {
        async move {
            let job_id = self.next_job_id().await?;
            let job = Job::new(job_id, spec, now);
            let specs = spec.split();
            let first_task = self.reserve_task_ids(specs.len() as u64).await?;
            let tasks = specs
                .into_iter()
                .enumerate()
                .map(|(index, task_spec)| {
                    Task::new(
                        TaskId(first_task.get() + index as u64),
                        job_id,
                        index as u16,
                        task_spec,
                        now,
                    )
                })
                .collect();
            Ok((job, tasks))
        }
    }
}

/// Append-only cluster event log.
pub trait EventLog: Send + Sync + 'static {
    fn append(&self, record: &EventRecord) -> impl Future<Output = StoreResult<()>> + Send;

    fn recent(&self, limit: usize) -> impl Future<Output = StoreResult<Vec<EventRecord>>> + Send;

    /// Highest sequence number persisted, so the runtime can resume numbering.
    fn last_seq(&self) -> impl Future<Output = StoreResult<u64>> + Send;

    fn prune_before(&self, _before: Millis) -> impl Future<Output = StoreResult<u64>> + Send {
        async { Ok(0) }
    }
}

/// Durable cluster state that is not a job and not an event.
///
/// Today that means role assignments, which are mutable at runtime and must
/// survive a restart: a node keeps its identity *and* the roles it was given
/// Bundling the three stores behind one supertrait keeps the
/// runtime to a single store type parameter.
pub trait ClusterStore: JobStore + EventLog {
    /// Record the roles a node currently holds.
    fn save_node_roles(
        &self,
        node: NodeId,
        roles: RoleSet,
        now: Millis,
    ) -> impl Future<Output = StoreResult<()>> + Send;

    /// Every stored assignment, for replay at startup.
    fn load_node_roles(&self) -> impl Future<Output = StoreResult<Vec<(NodeId, RoleSet)>>> + Send;
}
