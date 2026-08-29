//! Running a task on the host.
//!
//! The computation itself lives in `cluster_core::workload` and is shared
//! verbatim with the remote worker. What is left here is the part that
//! genuinely differs: how you wait, and how you keep a CPU-bound loop from
//! blocking an async runtime.

use cluster_core::{FailureReason, NodeId, TaskOutcome, TaskSpec, TaskWork, run_task};

/// Run one task on this node.
///
/// `extra_delay_ms` and `force_failure` come from the failure-simulation
/// controls; they are inputs to the worker, never special cases in the
/// scheduler.
pub async fn execute_task(
    node: NodeId,
    spec: TaskSpec,
    extra_delay_ms: u64,
    force_failure: bool,
) -> TaskOutcome {
    if extra_delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(extra_delay_ms)).await;
    }
    if force_failure {
        return TaskOutcome::Failed {
            reason: FailureReason::Injected,
            detail: format!("failure injected on {node}"),
        };
    }

    // CPU-bound work goes to the blocking pool so heartbeats from other nodes
    // are not delayed. A node still accepts one task at a time, matching the
    // remote agent's execution model.
    let work = match spec {
        TaskSpec::Primes { .. } => {
            match tokio::task::spawn_blocking(move || run_task(node, spec)).await {
                Ok(work) => work,
                Err(e) => {
                    return TaskOutcome::Failed {
                        reason: FailureReason::ExecutionError,
                        detail: format!("worker panicked: {e}"),
                    };
                }
            }
        }
        _ => run_task(node, spec),
    };

    match work {
        TaskWork::Done { output } => TaskOutcome::Completed { output },
        TaskWork::Wait { millis, output } => {
            tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
            TaskOutcome::Completed { output }
        }
    }
}
