//! An in-memory `JobStore` + `EventLog` so the runtime can be tested without
//! dragging SQLite (and therefore the whole application stack) into scope.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cluster_core::{
    ClusterStore, EventLog, EventRecord, Job, JobId, JobStore, Millis, NodeId, RoleSet,
    StoreResult, Task, TaskAttempt, TaskId,
};

#[derive(Default)]
struct State {
    jobs: BTreeMap<u64, Job>,
    tasks: BTreeMap<u64, Task>,
    failures: Vec<TaskAttempt>,
    events: Vec<EventRecord>,
    node_roles: BTreeMap<u16, RoleSet>,
    next_job: u64,
    next_task: u64,
}

#[derive(Clone, Default)]
pub struct MemoryStore {
    state: Arc<Mutex<State>>,
    event_write_gate: Arc<Mutex<Option<Arc<tokio::sync::Semaphore>>>>,
    event_write_started: Arc<tokio::sync::Notify>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failures(&self) -> Vec<TaskAttempt> {
        self.state.lock().unwrap().failures.clone()
    }

    /// Roles as they were last written, for asserting persistence.
    pub fn stored_roles(&self, node: NodeId) -> Option<RoleSet> {
        self.state
            .lock()
            .unwrap()
            .node_roles
            .get(&node.get())
            .copied()
    }

    pub fn event_kinds(&self) -> Vec<&'static str> {
        self.state
            .lock()
            .unwrap()
            .events
            .iter()
            .map(|e| e.event.kind())
            .collect()
    }

    /// Stop event persistence until the returned semaphore receives permits.
    /// Used to prove that cluster shutdown owns and drains the writer rather
    /// than merely hoping it finishes before the runtime disappears.
    pub fn block_event_writes(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.event_write_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    pub async fn wait_for_blocked_event_write(&self) {
        self.event_write_started.notified().await;
    }

    pub fn seed_jobs_beyond_history_window(&self) -> (JobId, JobId) {
        let mut state = self.state.lock().unwrap();
        for id in 1..=203u64 {
            let spec = cluster_core::JobSpec::Sleep {
                total_ms: 1,
                tasks: 1,
            };
            let mut job = Job::new(JobId(id), spec, Millis(id));
            let mut task = Task::new(TaskId(id), job.id, 0, spec.split()[0], Millis(id));
            match id {
                1 => {}
                2 => {
                    job.state = cluster_core::JobState::Running;
                    task.state = cluster_core::TaskState::Running;
                    task.assigned_to = Some(NodeId(99));
                }
                _ => {
                    job.state = cluster_core::JobState::Completed;
                    job.finished_at = Some(Millis(id));
                    task.state = cluster_core::TaskState::Completed;
                }
            }
            state.jobs.insert(id, job);
            state.tasks.insert(id, task);
        }
        state.next_job = 203;
        state.next_task = 203;
        (JobId(1), JobId(2))
    }
}

impl JobStore for MemoryStore {
    async fn next_job_id(&self) -> StoreResult<JobId> {
        let mut state = self.state.lock().unwrap();
        state.next_job += 1;
        Ok(JobId(state.next_job))
    }

    async fn reserve_task_ids(&self, count: u64) -> StoreResult<TaskId> {
        let mut state = self.state.lock().unwrap();
        let first = state.next_task + 1;
        state.next_task += count.max(1);
        Ok(TaskId(first))
    }

    async fn create_job(&self, job: &Job, tasks: &[Task]) -> StoreResult<()> {
        let mut state = self.state.lock().unwrap();
        state.jobs.insert(job.id.get(), job.clone());
        for task in tasks {
            state.tasks.insert(task.id.get(), task.clone());
        }
        Ok(())
    }

    async fn save_job(&self, job: &Job) -> StoreResult<()> {
        self.state
            .lock()
            .unwrap()
            .jobs
            .insert(job.id.get(), job.clone());
        Ok(())
    }

    async fn save_task(&self, task: &Task) -> StoreResult<()> {
        self.state
            .lock()
            .unwrap()
            .tasks
            .insert(task.id.get(), task.clone());
        Ok(())
    }

    async fn record_failure(&self, failure: &TaskAttempt) -> StoreResult<()> {
        self.state.lock().unwrap().failures.push(failure.clone());
        Ok(())
    }

    async fn job(&self, id: JobId) -> StoreResult<Option<Job>> {
        Ok(self.state.lock().unwrap().jobs.get(&id.get()).cloned())
    }

    async fn recent_jobs(&self, limit: usize) -> StoreResult<Vec<Job>> {
        let state = self.state.lock().unwrap();
        Ok(state.jobs.values().rev().take(limit).cloned().collect())
    }

    async fn tasks_for_job(&self, id: JobId) -> StoreResult<Vec<Task>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .tasks
            .values()
            .filter(|t| t.job_id == id)
            .cloned()
            .collect())
    }

    async fn failures_for_job(&self, id: JobId) -> StoreResult<Vec<TaskAttempt>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .failures
            .iter()
            .filter(|f| f.job_id == id)
            .cloned()
            .collect())
    }

    async fn unfinished_jobs(&self) -> StoreResult<Vec<(Job, Vec<Task>)>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .jobs
            .values()
            .filter(|job| !job.state.is_terminal())
            .map(|job| {
                let tasks = state
                    .tasks
                    .values()
                    .filter(|task| task.job_id == job.id)
                    .cloned()
                    .collect();
                (job.clone(), tasks)
            })
            .collect())
    }
}

impl EventLog for MemoryStore {
    async fn append(&self, record: &EventRecord) -> StoreResult<()> {
        let gate = self.event_write_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            self.event_write_started.notify_one();
            let permit = gate.acquire().await.expect("test semaphore stays open");
            permit.forget();
        }
        self.state.lock().unwrap().events.push(record.clone());
        Ok(())
    }

    async fn recent(&self, limit: usize) -> StoreResult<Vec<EventRecord>> {
        let state = self.state.lock().unwrap();
        Ok(state.events.iter().rev().take(limit).cloned().collect())
    }

    async fn last_seq(&self) -> StoreResult<u64> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .events
            .last()
            .map(|e| e.seq)
            .unwrap_or(0))
    }
}

impl ClusterStore for MemoryStore {
    async fn save_node_roles(&self, node: NodeId, roles: RoleSet, _now: Millis) -> StoreResult<()> {
        self.state
            .lock()
            .unwrap()
            .node_roles
            .insert(node.get(), roles);
        Ok(())
    }

    async fn load_node_roles(&self) -> StoreResult<Vec<(NodeId, RoleSet)>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .node_roles
            .iter()
            .map(|(id, roles)| (NodeId(*id), *roles))
            .collect())
    }
}
