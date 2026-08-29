//! Leader election, behind a trait.
//!
//! V0 deliberately does NOT implement Raft. What matters now is
//! that the *state transitions* around gaining and losing leadership are
//! exercised, so the policy is a one-method trait that a real protocol can
//! replace later without touching callers.
//!
//! Gateway and Coordinator stay separate concepts even though V0 will often
//! put both on the same node.

use crate::ids::NodeId;
use crate::node::Node;

pub trait Elector: Send + Sync {
    /// Pick the coordinator from the current view, or `None` if the cluster
    /// has no eligible node.
    fn elect(&self, nodes: &[Node]) -> Option<NodeId>;
}

/// Deterministic policy: the healthy node with the lowest id wins.
#[derive(Debug, Default, Clone, Copy)]
pub struct LowestHealthyId;

impl Elector for LowestHealthyId {
    fn elect(&self, nodes: &[Node]) -> Option<NodeId> {
        nodes
            .iter()
            .filter(|n| n.status.accepts_work())
            .min_by_key(|n| n.id.get())
            .map(|n| n.id)
    }
}
