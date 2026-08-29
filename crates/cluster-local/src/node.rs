//! One in-process node.
//!
//! A node knows only: its own id and capabilities, its mailbox, and where to
//! send reports. It has no view of the cluster -- exactly the amount of
//! knowledge a worker process has.

use std::time::Duration;

use cluster_core::{Clock, Heartbeat, NodeCapabilities, NodeId, NodeLoad, Task, TaskOutcome};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::clock::SystemClock;
use crate::exec::execute_task;

/// Messages the supervisor sends to a node.
#[derive(Debug)]
pub(crate) enum NodeInbox {
    Assign(Box<Task>),
    /// Stop sending heartbeats without stopping the node.
    PauseHeartbeat(bool),
    /// Make the next `count` tasks fail.
    ///
    /// A counter rather than a flag: with immediate re-dispatch a retry can
    /// start before a second one-shot arm arrives, which made "fail every
    /// attempt" impossible to express from outside.
    InjectFailures(u32),
    /// Artificial per-task delay, in milliseconds.
    SetDelay(u64),
}

/// Messages a node sends back.
#[derive(Debug)]
pub(crate) enum NodeReport {
    Heartbeat(Heartbeat),
    TaskStarted {
        node: NodeId,
        task: Box<Task>,
    },
    TaskFinished {
        node: NodeId,
        task: Box<Task>,
        outcome: TaskOutcome,
    },
}

/// The supervisor's handle on a running node.
///
/// Deliberately says nothing about *where* the node is. An in-process node is a
/// Tokio task in this process; a remote worker is a process at the other end of
/// a TCP connection. Both are reached the same way -- push a [`NodeInbox`]
/// into a channel -- which is why the supervisor needs no notion of remoteness
/// to schedule work.
pub(crate) struct NodeHandle {
    pub inbox: mpsc::Sender<NodeInbox>,
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    /// `None` for a remote node: the connection task owns itself and cannot
    /// hand out a handle to its own join. Stopping one closes its socket
    /// instead of aborting a task.
    pub join: Option<tokio::task::JoinHandle<()>>,
}

impl NodeHandle {
    /// Stop the node behind this handle, however it happens to be running.
    pub fn terminate(self) {
        let _ = self.shutdown.send(());
        if let Some(join) = self.join {
            join.abort();
        }
    }
}

pub(crate) struct NodeConfig {
    pub id: NodeId,
    pub capabilities: NodeCapabilities,
    pub heartbeat_interval_ms: u64,
    pub simulate_load: bool,
}

/// Spawn a node task and return the handle used to talk to it.
pub(crate) fn spawn_node(config: NodeConfig, reports: mpsc::Sender<NodeReport>) -> NodeHandle {
    // Small mailbox: back-pressure is a real property of remote workers and
    // should not be papered over with an unbounded queue.
    let (inbox_tx, inbox_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(run_node(config, inbox_rx, reports, shutdown_rx));
    NodeHandle {
        inbox: inbox_tx,
        shutdown: shutdown_tx,
        join: Some(join),
    }
}

async fn run_node(
    config: NodeConfig,
    mut inbox: mpsc::Receiver<NodeInbox>,
    reports: mpsc::Sender<NodeReport>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let clock = SystemClock;
    let id = config.id;
    let mut heartbeat =
        tokio::time::interval(Duration::from_millis(config.heartbeat_interval_ms.max(50)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut paused = false;
    let mut pending_failures: u32 = 0;
    let mut extra_delay_ms = 0u64;
    let mut wobble: u32 = u32::from(id.get()).wrapping_mul(2_654_435_761);

    // At most one task at a time, matching what a single-core node can do,
    // but run off the select loop so heartbeats keep flowing while it works.
    let mut running: JoinSet<(Box<Task>, TaskOutcome)> = JoinSet::new();

    tracing::debug!(node = %id, cores = config.capabilities.cores, "worker started");

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => break,

            _ = heartbeat.tick() => {
                if paused {
                    continue;
                }
                wobble = wobble.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let busy = !running.is_empty();
                let load = NodeLoad {
                    load_percent: if config.simulate_load {
                        let jitter = (wobble >> 24) as u8 % 12;
                        if busy { 65 + jitter } else { 3 + jitter }
                    } else {
                        0
                    },
                    running_tasks: running.len() as u16,
                    free_memory_bytes: config
                        .capabilities
                        .usable_ram_bytes()
                        .saturating_sub(if busy { 96 * 1024 } else { 32 * 1024 }),
                    simulated: config.simulate_load,
                };
                if reports
                    .send(NodeReport::Heartbeat(Heartbeat { node: id, load, at: clock.now() }))
                    .await
                    .is_err()
                {
                    break;
                }
            }

            message = inbox.recv() => {
                match message {
                    Some(NodeInbox::Assign(task)) => {
                        if running.is_empty() {
                            let spec = task.spec;
                            let fail = pending_failures > 0;
                            pending_failures = pending_failures.saturating_sub(1);
                            let delay = extra_delay_ms;
                            if reports
                                .send(NodeReport::TaskStarted { node: id, task: task.clone() })
                                .await
                                .is_err()
                            {
                                break;
                            }
                            running.spawn(async move {
                                (task, execute_task(id, spec, delay, fail).await)
                            });
                        } else {
                            // The supervisor only assigns to idle nodes; if one
                            // slips through, bounce it rather than queueing it
                            // silently behind the running task.
                            let _ = reports
                                .send(NodeReport::TaskFinished {
                                    node: id,
                                    task,
                                    outcome: TaskOutcome::Failed {
                                        reason: cluster_core::FailureReason::ExecutionError,
                                        detail: format!("{id} was already busy"),
                                    },
                                })
                                .await;
                        }
                    }
                    Some(NodeInbox::PauseHeartbeat(value)) => paused = value,
                    Some(NodeInbox::InjectFailures(count)) => {
                        pending_failures = pending_failures.saturating_add(count)
                    }
                    Some(NodeInbox::SetDelay(ms)) => extra_delay_ms = ms,
                    None => break,
                }
            }

            Some(finished) = running.join_next(), if !running.is_empty() => {
                match finished {
                    Ok((task, outcome)) => {
                        if reports
                            .send(NodeReport::TaskFinished { node: id, task, outcome })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // The worker itself died. The supervisor will notice the
                    // task never finished when the node goes offline, or on
                    // the next sweep.
                    Err(e) => tracing::warn!(node = %id, error = %e, "task worker aborted"),
                }
            }
        }
    }

    tracing::debug!(node = %id, "node stopped");
}
