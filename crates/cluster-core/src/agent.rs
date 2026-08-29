//! The node side of the protocol, as a state machine.
//!
//! It owns the whole of a worker's behaviour -- handshake, heartbeats,
//! accepting one task at a time,
//! running it, reporting the result, honouring the debug controls -- and it
//! does all of it without touching a socket, a timer or an allocator beyond
//! the result string.
//!
//! Keeping it in the protocol core buys two things:
//!
//! * **It is testable.** The failure modes that matter (a task arriving while
//!   one is running, a stale control message, a heartbeat due mid-task) are
//!   ordinary unit tests instead of process-level integration failures.
//! * **It cannot drift.** There is one implementation of node behaviour. The
//!   worker binary supplies a socket and a timer, and drives *this*.
//!
//! The shape is deliberate: the agent never blocks and never waits. It is fed
//! events and returns actions, so the caller owns every I/O decision. That is
//! the same reason `TaskWork` makes waiting the caller's job -- `tokio::time`
//! in the worker runtime.

use crate::ids::{NodeId, TaskId};
use crate::job::{FailureReason, TaskOutcome, TaskSpec};
use crate::node::{Heartbeat, NodeCapabilities, NodeLoad};
use crate::protocol::{NodeMessage, PROTOCOL_VERSION, SupervisorMessage};
use crate::time::Millis;
use crate::workload::{TaskWork, count_primes, primes_output, run_task};

/// What the agent wants the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Put this on the wire.
    Send(NodeMessage),
    /// Run [`Agent::poll`] again no later than this many milliseconds from
    /// now -- a task is waiting out its sleep, or a heartbeat is due.
    WakeIn(u64),
    /// The supervisor said goodbye, or refused us. Close the connection.
    Disconnect,
}

/// How many candidates a node checks per [`Agent::poll`].
///
/// An agent takes one task at a time, so compute must yield to protocol work.
/// Running a whole range in one call is what makes a worker miss every
/// heartbeat while it computes, get declared Offline, and have its work
/// requeued out from under it -- so the range is taken in slices instead, with
/// a chance to heartbeat between each.
///
/// Sized to stay comfortably inside a one-second heartbeat interval on an
/// ordinary worker while keeping per-slice overhead negligible.
const COMPUTE_SLICE: u64 = 8_000;

/// The state of the task this node is holding.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Work {
    /// The answer is known; it is only waiting for `ready_at`.
    Ready(String),
    /// Still counting, `COMPUTE_SLICE` candidates at a time.
    ///
    /// Carries the worker's identity so that finishing the count never has to
    /// ask whether this worker has one: a task cannot be accepted before the
    /// coordinator has welcomed it and said who it is.
    Primes {
        node: NodeId,
        start: u64,
        end: u64,
        cursor: u64,
        found: u64,
    },
}

/// A task this node has accepted and not yet finished.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Running {
    id: TaskId,
    /// Earliest time the result may be reported: the far end of a `Sleep`, and
    /// of any injected delay. Compute finishing is tracked separately, in
    /// `work` -- a task is done when both are.
    ready_at: Millis,
    work: Work,
}

/// One node's behaviour, independent of how its bytes travel.
pub struct Agent {
    /// `None` until the coordinator hands one over in `Welcome`. An ordinary
    /// worker starts anonymous: it is one of many interchangeable processes
    /// and has no business choosing its own identity.
    id: Option<NodeId>,
    capabilities: NodeCapabilities,
    heartbeat_interval_ms: u64,
    next_heartbeat: Millis,
    running: Option<Running>,
    /// Set by `PauseHeartbeat`: stop sending without stopping the node
    /// The supervisor should then watch this node go Suspect
    /// and Offline while it is demonstrably still alive.
    heartbeat_paused: bool,
    /// Counter rather than a flag, matching the simulated node: with immediate
    /// re-dispatch a retry can begin before a second one-shot arm arrives,
    /// which made "fail every attempt" impossible to express.
    pending_failures: u32,
    /// Artificial per-task delay in milliseconds.
    extra_delay_ms: u64,
    /// Last free-heap figure the platform reported. Only the platform can
    /// measure this, so the agent stores what it is told rather than guessing.
    free_memory_bytes: u64,
    joined: bool,
    /// Shared secret presented in `Hello`. `None` for an in-process worker,
    /// which never crosses a socket and has nothing to prove.
    token: Option<String>,
}

impl Agent {
    /// An ordinary worker: anonymous until welcomed.
    pub fn anonymous(capabilities: NodeCapabilities, heartbeat_interval_ms: u64) -> Self {
        Self::new(None, capabilities, heartbeat_interval_ms)
    }

    /// A worker with a fixed identity that must survive a restart.
    pub fn with_id(id: NodeId, capabilities: NodeCapabilities, heartbeat_interval_ms: u64) -> Self {
        Self::new(Some(id), capabilities, heartbeat_interval_ms)
    }

    pub fn new(
        id: Option<NodeId>,
        capabilities: NodeCapabilities,
        heartbeat_interval_ms: u64,
    ) -> Self {
        Self {
            id,
            capabilities,
            heartbeat_interval_ms,
            next_heartbeat: Millis::ZERO,
            running: None,
            heartbeat_paused: false,
            pending_failures: 0,
            extra_delay_ms: 0,
            free_memory_bytes: 0,
            joined: false,
            token: None,
        }
    }

    /// The join token this worker will present.
    ///
    /// A builder step rather than a constructor argument: every worker in the
    /// tests is a worker on a channel, with no socket and nothing to
    /// authenticate to, and threading `None` through all of them would be
    /// noise around the one place it matters.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    pub fn id(&self) -> Option<NodeId> {
        self.id
    }

    /// True once the supervisor has said `Welcome`.
    pub fn joined(&self) -> bool {
        self.joined
    }

    /// The first thing to put on the wire after connecting.
    pub fn hello(&self) -> NodeMessage {
        NodeMessage::Hello {
            protocol: PROTOCOL_VERSION,
            node: self.id,
            capabilities: self.capabilities,
            token: self.token.clone(),
        }
    }

    /// Handle one message from the supervisor.
    pub fn handle(&mut self, message: SupervisorMessage, now: Millis) -> Vec<Action> {
        match message {
            SupervisorMessage::Welcome {
                heartbeat_interval_ms,
                node,
                ..
            } => {
                // The coordinator's answer is authoritative, including for a
                // worker that asked for a specific id: it is the thing holding
                // the registry.
                self.id = Some(node);
                self.joined = true;
                // Timing policy lives on the coordinator; a worker
                // that hard-coded its own interval would drift from the
                // supervisor's timeouts the moment they were tuned.
                if heartbeat_interval_ms > 0 {
                    self.heartbeat_interval_ms = heartbeat_interval_ms;
                }
                // Heartbeat immediately so the node is visibly healthy rather
                // than waiting out a first full interval.
                self.next_heartbeat = now;
                self.poll(now)
            }
            SupervisorMessage::Rejected { .. } => vec![Action::Disconnect],
            SupervisorMessage::Shutdown => vec![Action::Disconnect],
            SupervisorMessage::PauseHeartbeat(paused) => {
                self.heartbeat_paused = paused;
                self.poll(now)
            }
            SupervisorMessage::InjectFailures(count) => {
                self.pending_failures = self.pending_failures.saturating_add(count);
                self.poll(now)
            }
            SupervisorMessage::SetDelay(ms) => {
                self.extra_delay_ms = ms;
                self.poll(now)
            }
            SupervisorMessage::Assign { task, spec } => self.accept(task, spec.into(), now),
        }
    }

    /// Begin a task.
    fn accept(&mut self, id: TaskId, spec: TaskSpec, now: Millis) -> Vec<Action> {
        // Work cannot arrive before `Welcome`, because the coordinator has
        // nowhere to send it until it has a registry entry. If it somehow
        // does, drop it rather than inventing an identity for the result.
        let Some(node) = self.id else {
            return Vec::new();
        };
        if let Some(running) = &self.running {
            if running.id == id {
                // The same task, handed back. The supervisor requeued it --
                // usually after this node looked briefly unhealthy -- and
                // placed it here again. The work is already in flight, so
                // failing it would burn a retry to no purpose. Say nothing and
                // let the running copy report when it finishes.
                return Vec::new();
            }

            // A *different* task while busy: the two sides disagree. Bounce it
            // rather than queueing it silently behind the running task -- the
            // same choice the simulated node makes, for the same reason.
            return vec![Action::Send(NodeMessage::TaskFinished {
                task: id,
                outcome: TaskOutcome::Failed {
                    reason: FailureReason::ExecutionError,
                    detail: String::from("node was already busy"),
                },
            })];
        }

        let mut actions = vec![Action::Send(NodeMessage::TaskStarted { task: id })];

        if self.pending_failures > 0 {
            self.pending_failures -= 1;
            actions.push(Action::Send(NodeMessage::TaskFinished {
                task: id,
                outcome: TaskOutcome::Failed {
                    reason: FailureReason::Injected,
                    detail: String::from("failure injected on this node"),
                },
            }));
            return actions;
        }

        // Compute is started here but deliberately not finished here: see
        // `COMPUTE_SLICE`. Everything else resolves immediately and only has
        // to wait out a clock.
        let (wait_ms, work) = match spec {
            TaskSpec::Primes { start, end } => (
                0,
                Work::Primes {
                    node,
                    start,
                    end,
                    cursor: start,
                    found: 0,
                },
            ),
            other => match run_task(node, other) {
                TaskWork::Done { output } => (0, Work::Ready(output)),
                TaskWork::Wait { millis, output } => (millis, Work::Ready(output)),
            },
        };

        self.running = Some(Running {
            id,
            ready_at: now.plus_ms(wait_ms.saturating_add(self.extra_delay_ms)),
            work,
        });
        actions.extend(self.poll(now));
        actions
    }

    /// Drive time forward. Call on every wake-up, and whenever the caller has
    /// nothing better to do.
    pub fn poll(&mut self, now: Millis) -> Vec<Action> {
        let mut actions = Vec::new();

        // Advance any computation by one slice before anything else, so the
        // node makes progress on every pass.
        if let Some(running) = &mut self.running
            && let Work::Primes {
                node,
                start,
                end,
                cursor,
                found,
            } = &mut running.work
        {
            let stop = (*cursor).saturating_add(COMPUTE_SLICE).min(*end);
            *found += count_primes(*cursor, stop);
            *cursor = stop;
            if cursor >= end {
                running.work = Work::Ready(primes_output(*node, *start, *end, *found));
            }
        }

        // A finished task is reported before a heartbeat: the result is what
        // the cluster is waiting on.
        if let Some(running) = &self.running
            && matches!(running.work, Work::Ready(_))
            && now >= running.ready_at
        {
            let running = self.running.take().expect("checked just above");
            let Work::Ready(output) = running.work else {
                unreachable!("checked just above");
            };
            actions.push(Action::Send(NodeMessage::TaskFinished {
                task: running.id,
                outcome: TaskOutcome::Completed { output },
            }));
        }

        // Nothing is sent before the supervisor has welcomed this node. Until
        // then the only frame it has agreed to receive is `Hello`.
        if let Some(node) = self.id
            && self.joined
            && !self.heartbeat_paused
            && now >= self.next_heartbeat
        {
            self.next_heartbeat = now.plus_ms(self.heartbeat_interval_ms.max(1));
            actions.push(Action::Send(NodeMessage::Heartbeat(Heartbeat {
                node,
                load: self.load(),
                at: now,
            })));
        }

        if let Some(wake) = self.next_wake_ms(now) {
            actions.push(Action::WakeIn(wake));
        }
        actions
    }

    /// Milliseconds until the agent next has something to do.
    ///
    /// Read-only on purpose: a caller deciding how long to block must not
    /// have to run `poll` to find out, because `poll` produces actions and
    /// asking the question would then silently discard a heartbeat.
    pub fn next_wake_ms(&self, now: Millis) -> Option<u64> {
        let heartbeat =
            (self.joined && !self.heartbeat_paused).then(|| self.next_heartbeat.since(now));
        let task = self.running.as_ref().map(|r| match r.work {
            // Still computing: come straight back for the next slice.
            Work::Primes { .. } => 0,
            Work::Ready(_) => r.ready_at.since(now),
        });
        match (heartbeat, task) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// What this node reports about itself.
    ///
    /// Measured, not simulated: the runtime supplies `free_memory_bytes`, and
    /// `simulated: false` tells the UI these are worker-reported numbers.
    fn load(&self) -> NodeLoad {
        let busy = self.running.is_some();
        NodeLoad {
            // A single-task node is either working or it is not. Reporting a
            // fabricated percentage would be worse than reporting the truth
            // coarsely.
            load_percent: if busy { 100 } else { 0 },
            running_tasks: u16::from(busy),
            free_memory_bytes: self.free_memory_bytes,
            simulated: false,
        }
    }

    /// Tell the agent how much memory the worker currently has free, to be
    /// included in the next heartbeat. The agent cannot measure the hosting
    /// process or container on its own.
    pub fn set_free_memory(&mut self, bytes: u64) {
        self.free_memory_bytes = bytes;
    }
}
