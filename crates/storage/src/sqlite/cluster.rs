//! Durable cluster state: the three runtime-facing ports on one handle.
//!
//! The runtime takes a single store type parameter, so this struct implements
//! `JobStore`, `EventLog` and `ClusterStore` together by delegating the first
//! two to the repositories that already exist.

use std::future::Future;

use cluster_core::{
    ClusterStore, EventLog, EventRecord, Job, JobId, JobStore, Millis, NodeId, RoleSet,
    StoreResult, Task, TaskAttempt, TaskId,
};
use sqlx::{Pool, Row, Sqlite};

use super::events::SqliteEvents;
use super::jobs::SqliteJobs;
use super::map_err;

#[derive(Clone)]
pub struct SqliteClusterStore {
    pool: Pool<Sqlite>,
    jobs: SqliteJobs,
    events: SqliteEvents,
}

impl SqliteClusterStore {
    pub(crate) fn new(pool: Pool<Sqlite>, jobs: SqliteJobs, events: SqliteEvents) -> Self {
        Self { pool, jobs, events }
    }
}

impl ClusterStore for SqliteClusterStore {
    async fn save_node_roles(&self, node: NodeId, roles: RoleSet, now: Millis) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO node_roles(node_id, roles, updated_at) VALUES(?, ?, ?)
             ON CONFLICT(node_id) DO UPDATE SET roles = excluded.roles, updated_at = excluded.updated_at",
        )
        .bind(node.get() as i64)
        .bind(roles.0 as i64)
        .bind(now.get() as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn load_node_roles(&self) -> StoreResult<Vec<(NodeId, RoleSet)>> {
        let rows = sqlx::query("SELECT node_id, roles FROM node_roles ORDER BY node_id")
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    NodeId(row.get::<i64, _>("node_id") as u16),
                    RoleSet(row.get::<i64, _>("roles") as u8),
                )
            })
            .collect())
    }
}

// --- delegation ----------------------------------------------------------

impl JobStore for SqliteClusterStore {
    fn next_job_id(&self) -> impl Future<Output = StoreResult<JobId>> + Send {
        self.jobs.next_job_id()
    }
    fn reserve_task_ids(&self, count: u64) -> impl Future<Output = StoreResult<TaskId>> + Send {
        self.jobs.reserve_task_ids(count)
    }
    fn create_job(
        &self,
        job: &Job,
        tasks: &[Task],
    ) -> impl Future<Output = StoreResult<()>> + Send {
        self.jobs.create_job(job, tasks)
    }
    fn save_job(&self, job: &Job) -> impl Future<Output = StoreResult<()>> + Send {
        self.jobs.save_job(job)
    }
    fn save_task(&self, task: &Task) -> impl Future<Output = StoreResult<()>> + Send {
        self.jobs.save_task(task)
    }
    fn record_failure(
        &self,
        failure: &TaskAttempt,
    ) -> impl Future<Output = StoreResult<()>> + Send {
        self.jobs.record_failure(failure)
    }
    fn job(&self, id: JobId) -> impl Future<Output = StoreResult<Option<Job>>> + Send {
        self.jobs.job(id)
    }
    fn recent_jobs(&self, limit: usize) -> impl Future<Output = StoreResult<Vec<Job>>> + Send {
        self.jobs.recent_jobs(limit)
    }
    fn tasks_for_job(&self, id: JobId) -> impl Future<Output = StoreResult<Vec<Task>>> + Send {
        self.jobs.tasks_for_job(id)
    }
    fn failures_for_job(
        &self,
        id: JobId,
    ) -> impl Future<Output = StoreResult<Vec<TaskAttempt>>> + Send {
        self.jobs.failures_for_job(id)
    }
    fn unfinished_jobs(&self) -> impl Future<Output = StoreResult<Vec<(Job, Vec<Task>)>>> + Send {
        self.jobs.unfinished_jobs()
    }
}

impl EventLog for SqliteClusterStore {
    fn append(&self, record: &EventRecord) -> impl Future<Output = StoreResult<()>> + Send {
        self.events.append(record)
    }
    fn recent(&self, limit: usize) -> impl Future<Output = StoreResult<Vec<EventRecord>>> + Send {
        self.events.recent(limit)
    }
    fn last_seq(&self) -> impl Future<Output = StoreResult<u64>> + Send {
        self.events.last_seq()
    }
}
