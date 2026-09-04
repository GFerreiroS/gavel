use app_core::error::RepoResult;
use app_core::repo::JobRepository;
use cluster_core::{
    FailureReason, Job, JobId, JobSpec, JobState, Millis, NodeId, Task, TaskAttempt, TaskId,
    TaskSpec, TaskState,
};
use sqlx::sqlite::SqliteRow;
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err, write_guard};

#[derive(Clone)]
pub struct SqliteJobs {
    pool: Pool<Sqlite>,
}

impl SqliteJobs {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }

    /// Atomically reserve `count` ids and return the last one allocated.
    async fn reserve(&self, name: &str, count: u64) -> RepoResult<u64> {
        let _write = write_guard("cluster id reservation").await;
        let row =
            sqlx::query("UPDATE sequences SET value = value + ? WHERE name = ? RETURNING value")
                .bind(count as i64)
                .bind(name)
                .fetch_one(&self.pool)
                .await
                .map_err(map_err)?;
        Ok(row.get::<i64, _>("value") as u64)
    }
}

fn job_from_row(row: &SqliteRow) -> RepoResult<Job> {
    let spec_json: String = row.get("spec_json");
    let state_str: String = row.get("state");
    Ok(Job {
        id: JobId(row.get::<i64, _>("id") as u64),
        spec: serde_json::from_str::<JobSpec>(&spec_json).map_err(|e| corrupt("job spec", e))?,
        state: JobState::parse(&state_str).ok_or_else(|| corrupt("job state", state_str))?,
        task_count: row.get::<i64, _>("task_count") as u16,
        tasks_completed: row.get::<i64, _>("tasks_completed") as u16,
        tasks_failed: row.get::<i64, _>("tasks_failed") as u16,
        created_at: Millis(row.get::<i64, _>("created_at") as u64),
        finished_at: row
            .get::<Option<i64>, _>("finished_at")
            .map(|v| Millis(v as u64)),
    })
}

fn task_from_row(row: &SqliteRow) -> RepoResult<Task> {
    let spec_json: String = row.get("spec_json");
    let state_str: String = row.get("state");
    Ok(Task {
        id: TaskId(row.get::<i64, _>("id") as u64),
        job_id: JobId(row.get::<i64, _>("job_id") as u64),
        index: row.get::<i64, _>("idx") as u16,
        spec: serde_json::from_str::<TaskSpec>(&spec_json).map_err(|e| corrupt("task spec", e))?,
        state: TaskState::parse(&state_str).ok_or_else(|| corrupt("task state", state_str))?,
        assigned_to: row
            .get::<Option<i64>, _>("assigned_to")
            .map(|v| NodeId(v as u16)),
        attempt: row.get::<i64, _>("attempt") as u16,
        output: row.get::<Option<String>, _>("output"),
        updated_at: Millis(row.get::<i64, _>("updated_at") as u64),
    })
}

impl JobRepository for SqliteJobs {
    async fn next_job_id(&self) -> RepoResult<JobId> {
        Ok(JobId(self.reserve("job", 1).await?))
    }

    async fn reserve_task_ids(&self, count: u64) -> RepoResult<TaskId> {
        let count = count.max(1);
        let last = self.reserve("task", count).await?;
        Ok(TaskId(last - count + 1))
    }

    /// A job and its tasks appear together or not at all.
    async fn create_job(&self, job: &Job, tasks: &[Task]) -> RepoResult<()> {
        let _write = write_guard("cluster job creation").await;
        let mut tx = self.pool.begin().await.map_err(map_err)?;

        let spec_json = serde_json::to_string(&job.spec).map_err(|e| corrupt("job spec", e))?;
        sqlx::query(
            "INSERT INTO jobs(id, kind, spec_json, state, task_count, tasks_completed, tasks_failed, created_at, finished_at)
             VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.get() as i64)
        .bind(job.spec.kind())
        .bind(spec_json)
        .bind(job.state.as_str())
        .bind(job.task_count as i64)
        .bind(job.tasks_completed as i64)
        .bind(job.tasks_failed as i64)
        .bind(job.created_at.get() as i64)
        .bind(job.finished_at.map(|v| v.get() as i64))
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        for task in tasks {
            let spec_json =
                serde_json::to_string(&task.spec).map_err(|e| corrupt("task spec", e))?;
            sqlx::query(
                "INSERT INTO tasks(id, job_id, idx, spec_json, state, assigned_to, attempt, output, updated_at)
                 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(task.id.get() as i64)
            .bind(task.job_id.get() as i64)
            .bind(task.index as i64)
            .bind(spec_json)
            .bind(task.state.as_str())
            .bind(task.assigned_to.map(|n| n.get() as i64))
            .bind(task.attempt as i64)
            .bind(task.output.as_deref())
            .bind(task.updated_at.get() as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        }

        tx.commit().await.map_err(map_err)
    }

    async fn save_job(&self, job: &Job) -> RepoResult<()> {
        let _write = write_guard("cluster job update").await;
        sqlx::query(
            "UPDATE jobs SET state = ?, tasks_completed = ?, tasks_failed = ?, finished_at = ?
             WHERE id = ?",
        )
        .bind(job.state.as_str())
        .bind(job.tasks_completed as i64)
        .bind(job.tasks_failed as i64)
        .bind(job.finished_at.map(|v| v.get() as i64))
        .bind(job.id.get() as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn save_task(&self, task: &Task) -> RepoResult<()> {
        let _write = write_guard("cluster task update").await;
        sqlx::query(
            "UPDATE tasks SET state = ?, assigned_to = ?, attempt = ?, output = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(task.state.as_str())
        .bind(task.assigned_to.map(|n| n.get() as i64))
        .bind(task.attempt as i64)
        .bind(task.output.as_deref())
        .bind(task.updated_at.get() as i64)
        .bind(task.id.get() as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn record_failure(&self, failure: &TaskAttempt) -> RepoResult<()> {
        let _write = write_guard("cluster task failure").await;
        sqlx::query(
            "INSERT INTO task_failures(task_id, job_id, node_id, attempt, at, reason, detail)
             VALUES(?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(failure.task_id.get() as i64)
        .bind(failure.job_id.get() as i64)
        .bind(failure.node_id.map(|n| n.get() as i64))
        .bind(failure.attempt as i64)
        .bind(failure.at.get() as i64)
        .bind(failure.reason.as_str())
        .bind(&failure.detail)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn job(&self, id: JobId) -> RepoResult<Option<Job>> {
        let row = sqlx::query("SELECT * FROM jobs WHERE id = ?")
            .bind(id.get() as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
        row.as_ref().map(job_from_row).transpose()
    }

    async fn recent_jobs(&self, limit: usize) -> RepoResult<Vec<Job>> {
        let rows = sqlx::query("SELECT * FROM jobs ORDER BY id DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(job_from_row).collect()
    }

    async fn tasks_for_job(&self, id: JobId) -> RepoResult<Vec<Task>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE job_id = ? ORDER BY idx")
            .bind(id.get() as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(task_from_row).collect()
    }

    async fn failures_for_job(&self, id: JobId) -> RepoResult<Vec<TaskAttempt>> {
        let rows =
            sqlx::query("SELECT * FROM task_failures WHERE job_id = ? ORDER BY at DESC, id DESC")
                .bind(id.get() as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(map_err)?;
        rows.into_iter()
            .map(|row| {
                let reason_str: String = row.get("reason");
                Ok(TaskAttempt {
                    task_id: TaskId(row.get::<i64, _>("task_id") as u64),
                    job_id: JobId(row.get::<i64, _>("job_id") as u64),
                    node_id: row
                        .get::<Option<i64>, _>("node_id")
                        .map(|v| NodeId(v as u16)),
                    attempt: row.get::<i64, _>("attempt") as u16,
                    at: Millis(row.get::<i64, _>("at") as u64),
                    reason: FailureReason::parse(&reason_str)
                        .ok_or_else(|| corrupt("failure reason", reason_str))?,
                    detail: row.get::<String, _>("detail"),
                })
            })
            .collect()
    }

    async fn unfinished_jobs(&self) -> RepoResult<Vec<(Job, Vec<Task>)>> {
        let rows =
            sqlx::query("SELECT * FROM jobs WHERE state IN ('queued', 'running') ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .map_err(map_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let job = job_from_row(row)?;
            let tasks = self.tasks_for_job(job.id).await?;
            out.push((job, tasks));
        }
        Ok(out)
    }

    async fn unfinished_jobs_page(
        &self,
        after: Option<JobId>,
        limit: usize,
    ) -> RepoResult<Vec<(Job, Vec<Task>)>> {
        let rows = sqlx::query(
            "SELECT * FROM jobs
              WHERE state IN ('queued', 'running') AND id > ?
              ORDER BY id LIMIT ?",
        )
        .bind(after.map_or(0, JobId::get) as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let job = job_from_row(row)?;
            let tasks = self.tasks_for_job(job.id).await?;
            out.push((job, tasks));
        }
        Ok(out)
    }

    async fn prune_terminal_before(&self, before: Millis) -> RepoResult<u64> {
        let _write = write_guard("cluster job pruning").await;
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        sqlx::query(
            "DELETE FROM task_failures WHERE job_id IN (
                 SELECT id FROM jobs
                  WHERE state IN ('completed','failed','cancelled')
                    AND COALESCE(finished_at, created_at) < ?)",
        )
        .bind(before.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;
        let deleted = sqlx::query(
            "DELETE FROM jobs
              WHERE state IN ('completed','failed','cancelled')
                AND COALESCE(finished_at, created_at) < ?",
        )
        .bind(before.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?
        .rows_affected();
        tx.commit().await.map_err(map_err)?;
        Ok(deleted)
    }
}
