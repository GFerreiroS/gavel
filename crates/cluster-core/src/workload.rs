//! The actual computation behind a task.
//!
//! Pure: no async, no allocation beyond the result string, no platform calls.
//! This is what makes "the same code runs in every worker" a fact rather than
//! an aspiration: in-process and remote workers both call [`run_task`].

use crate::ids::NodeId;
use crate::job::TaskSpec;

/// What a caller must do to finish a task.
///
/// Waiting is the caller's job so the runtime can wait without blocking
/// heartbeats or other protocol work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskWork {
    /// Wait this long, then report `output`.
    Wait { millis: u64, output: String },
    /// Already computed; report `output` now.
    Done { output: String },
}

/// Execute the computable part of a task.
pub fn run_task(node: NodeId, spec: TaskSpec) -> TaskWork {
    match spec {
        TaskSpec::Sleep { millis } => TaskWork::Wait {
            millis,
            output: format!("slept {millis}ms on {node}"),
        },
        TaskSpec::Primes { start, end } => TaskWork::Done {
            output: primes_output(node, start, end, count_primes(start, end)),
        },
    }
}

/// How a prime count is reported.
///
/// Shared so that a node which computes the range in one call and a node which
/// takes it in slices produce byte-identical output. Without a single source
/// for this string the two would drift, and the difference would only show up
/// as a confusing diff between in-process and remote results.
pub fn primes_output(node: NodeId, start: u64, end: u64, count: u64) -> String {
    format!("{count} primes in {start}..{end} on {node}")
}

/// Count the primes in a half-open range.
///
/// Trial division on purpose: the point of this workload is to burn a
/// predictable amount of CPU that splits cleanly into independent ranges, not
/// to be the fastest possible sieve. A sieve would also need a bitmap
/// proportional to the range, which is the wrong shape for a node with a few
/// hundred KB of RAM.
pub fn count_primes(start: u64, end: u64) -> u64 {
    (start.max(2)..end).filter(|n| is_prime(*n)).count() as u64
}

pub fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n.is_multiple_of(2) {
        return n == 2;
    }
    if n.is_multiple_of(3) {
        return n == 3;
    }
    // 6k +/- 1: skips two thirds of the candidates for one extra add.
    let mut d: u64 = 5;
    while d * d <= n {
        if n.is_multiple_of(d) || n.is_multiple_of(d + 2) {
            return false;
        }
        d += 6;
    }
    true
}
