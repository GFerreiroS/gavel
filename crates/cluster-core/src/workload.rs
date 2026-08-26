//! The actual computation behind a task.
//!
//! Pure and `no_std`: no async, no allocation beyond the result string, no
//! platform calls. This is what makes "the same code runs on the PC and on the
//! device" a fact rather than an aspiration -- `cluster-local` and the ESP32-S3
//! firmware both call [`run_task`] and neither has its own copy.

use alloc::format;
use alloc::string::String;

use crate::ids::NodeId;
use crate::job::TaskSpec;

/// What a caller must do to finish a task.
///
/// Waiting is the caller's job because that is the one part that genuinely
/// differs: `tokio::time::sleep` on the host, a timer peripheral on the device.
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
        TaskSpec::Primes { start, end } => {
            let count = count_primes(start, end);
            TaskWork::Done {
                output: format!("{count} primes in {start}..{end} on {node}"),
            }
        }
    }
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
    if n % 2 == 0 {
        return n == 2;
    }
    if n % 3 == 0 {
        return n == 3;
    }
    // 6k +/- 1: skips two thirds of the candidates for one extra add.
    let mut d: u64 = 5;
    while d * d <= n {
        if n % d == 0 || n % (d + 2) == 0 {
            return false;
        }
        d += 6;
    }
    true
}
