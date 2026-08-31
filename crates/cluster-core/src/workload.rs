//! The actual computation behind a task.
//!
//! Pure: no async, no allocation beyond the result string, no platform calls.
//! This is what makes "the same code runs in every worker" a fact rather than
//! an aspiration: in-process and remote workers both call [`run_task`].

use core::fmt;

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
    /// Computed, and it produced bytes for the coordinator.
    ///
    /// The `output` is still a sentence, because that is what a task row keeps
    /// and what an operator reads on `/jobs`. The artifact is not persisted
    /// with it: it is the *analysis*, and where analysis lives is the read
    /// model, staged and published by the coordinator (§15).
    Produced { output: String, artifact: Vec<u8> },
}

/// What runs a task this module cannot.
///
/// `cluster-core` depends on serde, thiserror, postcard and futures-core and
/// nothing else (§3), so it cannot compute a market statistic -- the
/// definitions live in `app-core`, on the other side of that wall. The host
/// that *does* know both installs one of these, and because the worker binary
/// is the same binary as the server (§4) it is installed in every worker too.
///
/// Sync and pure, exactly like [`run_task`], for the same reason: "the same
/// code runs in every worker" has to stay a fact. Anything that needs to wait,
/// fetch or allocate a database connection belongs on the other side of the
/// port, not here.
pub trait Workload: fmt::Debug + Send + Sync + 'static {
    /// `input` is the artifact the task referenced, empty for a spec that
    /// references nothing.
    ///
    /// `None` for a spec this handler does not recognise, which is how an
    /// unknown task becomes a reported failure rather than a silent success.
    fn run(&self, node: NodeId, spec: TaskSpec, input: &[u8]) -> Option<TaskWork>;
}

/// Where a task's input comes from and where its result goes.
///
/// The coordinator's side of the same seam [`Workload`] is the worker's side.
/// One is installed on the process that *has* the data, the other on every
/// process that computes; in a single-machine deployment that is the same
/// process and the artifact never leaves memory, which is what keeps
/// `cargo run` the whole story (§2).
///
/// Deliberately bytes. `cluster-core` does not know what a market is (§3), and
/// an opaque artifact is what lets that stay true while the thing inside it is
/// versioned by whoever made it.
pub trait ArtifactStore: fmt::Debug + Send + Sync + 'static {
    /// The input this task references, if it is still wanted.
    ///
    /// `None` for a partition of a candidate that has been abandoned, which is
    /// how a stale assignment stops before it is sent rather than after it is
    /// computed.
    fn input(&self, spec: TaskSpec) -> Option<Vec<u8>>;

    /// What a worker produced for it.
    ///
    /// Idempotent by the task's own key: a retry after a worker died and a
    /// duplicated report both arrive here, and the second must say what the
    /// first did (§15).
    fn produced(&self, spec: TaskSpec, bytes: &[u8]);
}

/// Execute the computable part of a task.
///
/// `None` for a spec only a host-installed [`Workload`] can run.
pub fn run_task(node: NodeId, spec: TaskSpec) -> Option<TaskWork> {
    // Nothing here references an artifact; a spec that does goes to the host.

    match spec {
        TaskSpec::Sleep { millis } => Some(TaskWork::Wait {
            millis,
            output: format!("slept {millis}ms on {node}"),
        }),
        TaskSpec::Primes { start, end } => Some(TaskWork::Done {
            output: primes_output(node, start, end, count_primes(start, end)),
        }),
        // Referenced work: the input lives with whoever registered it.
        TaskSpec::Analysis { .. } => None,
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
