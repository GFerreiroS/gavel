//! Placement policy.
//!
//! Note the trait shape: `fn ... -> impl Future<Output = _> + Send` rather than
//! `async fn` or `#[async_trait]`. That keeps the call allocation-free (no
//! `Box<dyn Future>` per task placement) and keeps the trait usable from a
//! `no_std` executor such as embassy. Implementations may still be written as
//! plain `async fn`.

use core::future::Future;

use alloc::vec::Vec;

use crate::error::SchedulerError;
use crate::ids::NodeId;
use crate::job::Task;
use crate::node::Node;

pub trait Scheduler: Send + Sync {
    fn select_node(
        &self,
        task: &Task,
        nodes: &[Node],
    ) -> impl Future<Output = Result<NodeId, SchedulerError>> + Send;
}

/// Nodes that may currently receive work: healthy, and carrying the Compute
/// role. This is the one place that decision is made.
pub fn schedulable<'a>(nodes: &'a [Node], _task: &Task) -> Vec<&'a Node> {
    nodes.iter().filter(|n| n.is_schedulable()).collect()
}

/// Fewest running tasks wins; ties broken by more capable hardware, then by id
/// so the result is deterministic and therefore testable.
#[derive(Debug, Default, Clone, Copy)]
pub struct LeastLoaded;

impl Scheduler for LeastLoaded {
    async fn select_node(&self, task: &Task, nodes: &[Node]) -> Result<NodeId, SchedulerError> {
        schedulable(nodes, task)
            .into_iter()
            .min_by_key(|n| {
                (
                    n.load.running_tasks,
                    n.load.load_percent,
                    core::cmp::Reverse(n.capabilities.compute_weight()),
                    n.id.get(),
                )
            })
            .map(|n| n.id)
            .ok_or(SchedulerError::NoEligibleNode)
    }
}

/// Rotation over the eligible set, keyed by task id. Useful to make placement
/// spread obvious in the UI while developing.
///
/// Deliberately stateless. An earlier version held an `AtomicUsize` cursor,
/// which does not compile for `riscv32imc-unknown-none-elf` -- the ESP32-C3
/// has no atomic instructions at all. Deriving the slot from the task id needs
/// no shared mutable state, works on every target, and has the bonus property
/// that two schedulers looking at the same cluster agree on the answer.
#[derive(Debug, Default, Clone, Copy)]
pub struct RoundRobin;

impl Scheduler for RoundRobin {
    async fn select_node(&self, task: &Task, nodes: &[Node]) -> Result<NodeId, SchedulerError> {
        let eligible = schedulable(nodes, task);
        if eligible.is_empty() {
            return Err(SchedulerError::NoEligibleNode);
        }
        let slot = (task.id.get() as usize) % eligible.len();
        Ok(eligible[slot].id)
    }
}
