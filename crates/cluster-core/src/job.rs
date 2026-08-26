//! Jobs, tasks and their state machines.
//!
//! A Job is what a user submits; it splits into independent Tasks that the
//! scheduler places on nodes. Tasks are re-executed from the beginning on
//! failure -- V0 has no checkpoint/resume (CLAUDE.md 16).

use core::fmt;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::error::StateError;
use crate::ids::{JobId, NodeId, TaskId};
use crate::time::Millis;

/// What a user asked the cluster to do.
///
/// Kept as a small closed enum on purpose: a task description has to be
/// serialisable into a few dozen bytes to be shipped to an ESP32 later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum JobSpec {
    /// Scheduling/failure demo: sleep, then report which node ran it.
    Sleep { total_ms: u64, tasks: u16 },
    /// CPU demo: count primes below `upper_bound`, split into ranges.
    Primes { upper_bound: u64, tasks: u16 },
}

impl JobSpec {
    pub const fn kind(&self) -> &'static str {
        match self {
            JobSpec::Sleep { .. } => "sleep",
            JobSpec::Primes { .. } => "primes",
        }
    }

    pub fn task_count(&self) -> u16 {
        match *self {
            JobSpec::Sleep { tasks, .. } | JobSpec::Primes { tasks, .. } => tasks.max(1),
        }
    }

    pub fn describe(&self) -> String {
        use alloc::format;
        match *self {
            JobSpec::Sleep { total_ms, tasks } => format!("sleep {total_ms}ms over {tasks} tasks"),
            JobSpec::Primes { upper_bound, tasks } => {
                format!("primes below {upper_bound} over {tasks} tasks")
            }
        }
    }

    /// Split into independent units of work.
    pub fn split(&self) -> Vec<TaskSpec> {
        let n = u64::from(self.task_count());
        match *self {
            JobSpec::Sleep { total_ms, .. } => {
                let each = total_ms / n;
                let rem = total_ms % n;
                (0..n)
                    .map(|i| TaskSpec::Sleep {
                        millis: each + u64::from(i < rem),
                    })
                    .collect()
            }
            JobSpec::Primes { upper_bound, .. } => {
                let chunk = upper_bound / n;
                (0..n)
                    .map(|i| {
                        let start = i * chunk;
                        let end = if i + 1 == n {
                            upper_bound
                        } else {
                            start + chunk
                        };
                        TaskSpec::Primes { start, end }
                    })
                    .collect()
            }
        }
    }
}

/// One independently schedulable unit of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TaskSpec {
    Sleep { millis: u64 },
    Primes { start: u64, end: u64 },
}

impl TaskSpec {
    pub fn describe(&self) -> String {
        use alloc::format;
        match *self {
            TaskSpec::Sleep { millis } => format!("sleep {millis}ms"),
            TaskSpec::Primes { start, end } => format!("primes in {start}..{end}"),
        }
    }

    /// Rough cost hint; a capability-aware scheduler can use this later.
    pub const fn weight(&self) -> u64 {
        match *self {
            TaskSpec::Sleep { millis } => millis,
            TaskSpec::Primes { start, end } => end.saturating_sub(start),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        )
    }

    /// Inverse of [`JobState::as_str`]; used when reading persisted state.
    pub fn parse(s: &str) -> Option<JobState> {
        [
            JobState::Queued,
            JobState::Running,
            JobState::Completed,
            JobState::Failed,
            JobState::Cancelled,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }

    pub const fn can_transition_to(self, next: JobState) -> bool {
        use JobState::*;
        matches!(
            (self, next),
            (Queued, Running)
                | (Queued, Cancelled)
                | (Queued, Failed)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Cancelled)
        )
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Assigned,
    Running,
    Completed,
    Failed,
}

impl TaskState {
    pub const fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Assigned => "assigned",
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed)
    }

    /// Inverse of [`TaskState::as_str`]; used when reading persisted state.
    pub fn parse(s: &str) -> Option<TaskState> {
        [
            TaskState::Queued,
            TaskState::Assigned,
            TaskState::Running,
            TaskState::Completed,
            TaskState::Failed,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }

    pub const fn can_transition_to(self, next: TaskState) -> bool {
        use TaskState::*;
        matches!(
            (self, next),
            (Queued, Assigned)
                | (Assigned, Running)
                | (Assigned, Failed)
                // requeue after a worker died holding the task
                | (Assigned, Queued)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Queued)
                | (Failed, Queued)
        )
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    /// The node holding the task stopped heartbeating.
    NodeOffline,
    /// The workload itself returned an error.
    ExecutionError,
    /// Deliberately injected by the failure-simulation controls.
    Injected,
    Timeout,
    Cancelled,
}

impl FailureReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureReason::NodeOffline => "node_offline",
            FailureReason::ExecutionError => "execution_error",
            FailureReason::Injected => "injected",
            FailureReason::Timeout => "timeout",
            FailureReason::Cancelled => "cancelled",
        }
    }
}

impl FailureReason {
    /// Inverse of [`FailureReason::as_str`].
    pub fn parse(s: &str) -> Option<FailureReason> {
        [
            FailureReason::NodeOffline,
            FailureReason::ExecutionError,
            FailureReason::Injected,
            FailureReason::Timeout,
            FailureReason::Cancelled,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub spec: JobSpec,
    pub state: JobState,
    pub task_count: u16,
    pub tasks_completed: u16,
    pub tasks_failed: u16,
    pub created_at: Millis,
    pub finished_at: Option<Millis>,
}

impl Job {
    pub fn new(id: JobId, spec: JobSpec, at: Millis) -> Self {
        Self {
            id,
            spec,
            state: JobState::Queued,
            task_count: spec.task_count(),
            tasks_completed: 0,
            tasks_failed: 0,
            created_at: at,
            finished_at: None,
        }
    }

    pub fn transition_to(&mut self, next: JobState, at: Millis) -> Result<(), StateError> {
        if self.state == next {
            return Ok(());
        }
        if !self.state.can_transition_to(next) {
            return Err(StateError::IllegalJobTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        if next.is_terminal() {
            self.finished_at = Some(at);
        }
        Ok(())
    }

    /// 0..=100, for the progress bar.
    pub fn progress_percent(&self) -> u8 {
        if self.task_count == 0 {
            return 100;
        }
        ((u32::from(self.tasks_completed) * 100) / u32::from(self.task_count)) as u8
    }

    pub fn duration_ms(&self, now: Millis) -> u64 {
        self.finished_at.unwrap_or(now).since(self.created_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub job_id: JobId,
    /// Position within the job, for stable display ordering.
    pub index: u16,
    pub spec: TaskSpec,
    pub state: TaskState,
    pub assigned_to: Option<NodeId>,
    /// How many times execution has been *started*, including the current try.
    pub attempt: u16,
    pub output: Option<String>,
    pub updated_at: Millis,
}

impl Task {
    pub fn new(id: TaskId, job_id: JobId, index: u16, spec: TaskSpec, at: Millis) -> Self {
        Self {
            id,
            job_id,
            index,
            spec,
            state: TaskState::Queued,
            assigned_to: None,
            attempt: 0,
            output: None,
            updated_at: at,
        }
    }

    fn transition_to(&mut self, next: TaskState, at: Millis) -> Result<(), StateError> {
        if !self.state.can_transition_to(next) {
            return Err(StateError::IllegalTaskTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.updated_at = at;
        Ok(())
    }

    pub fn assign(&mut self, node: NodeId, at: Millis) -> Result<(), StateError> {
        self.transition_to(TaskState::Assigned, at)?;
        self.assigned_to = Some(node);
        self.attempt = self.attempt.saturating_add(1);
        Ok(())
    }

    pub fn start(&mut self, at: Millis) -> Result<(), StateError> {
        self.transition_to(TaskState::Running, at)
    }

    pub fn complete(&mut self, output: String, at: Millis) -> Result<(), StateError> {
        self.transition_to(TaskState::Completed, at)?;
        self.output = Some(output);
        Ok(())
    }

    pub fn fail(&mut self, at: Millis) -> Result<(), StateError> {
        self.transition_to(TaskState::Failed, at)
    }

    /// Put the task back on the queue so another worker runs it from scratch.
    pub fn requeue(&mut self, at: Millis) -> Result<(), StateError> {
        self.transition_to(TaskState::Queued, at)?;
        self.assigned_to = None;
        self.output = None;
        Ok(())
    }
}

/// A recorded failure. One row per failed attempt, never overwritten -- the UI
/// has to be able to show the whole history (CLAUDE.md 16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAttempt {
    pub task_id: TaskId,
    pub job_id: JobId,
    pub node_id: Option<NodeId>,
    pub attempt: u16,
    pub at: Millis,
    pub reason: FailureReason,
    pub detail: String,
}

impl TaskAttempt {
    pub fn new(task: &Task, reason: FailureReason, detail: impl ToString, at: Millis) -> Self {
        Self {
            task_id: task.id,
            job_id: task.job_id,
            node_id: task.assigned_to,
            attempt: task.attempt,
            at,
            reason,
            detail: detail.to_string(),
        }
    }
}

/// What a worker reports back after running a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskOutcome {
    Completed {
        output: String,
    },
    Failed {
        reason: FailureReason,
        detail: String,
    },
}
