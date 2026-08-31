//! Running a task on the host.
//!
//! The computation itself lives in `cluster_core::workload` and is shared
//! verbatim with the remote worker. What is left here is the part that
//! genuinely differs: how you wait, and how you keep a CPU-bound loop from
//! blocking an async runtime.

use std::sync::Arc;

use cluster_core::{FailureReason, NodeId, TaskOutcome, TaskSpec, TaskWork, Workload, run_task};

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
    host: Option<&Arc<dyn Workload>>,
    store: Option<&Arc<dyn cluster_core::ArtifactStore>>,
) -> (TaskOutcome, Option<Vec<u8>>) {
    if extra_delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(extra_delay_ms)).await;
    }
    if force_failure {
        return (
            TaskOutcome::Failed {
                reason: FailureReason::Injected,
                detail: format!("failure injected on {node}"),
            },
            None,
        );
    }

    // The input the task references, from the store the host installed. In
    // this process that is a map lookup; across a socket the coordinator has
    // already put it in the assignment. Either way the worker is handed bytes
    // and nothing else (§15).
    let input = match store {
        Some(store) => store.input(spec).unwrap_or_default(),
        None => Vec::new(),
    };

    // CPU-bound work goes to the blocking pool so heartbeats from other nodes
    // are not delayed. A node still accepts one task at a time, matching the
    // remote agent's execution model.
    // Anything the built-in workload cannot compute goes to the handler the
    // host installed. It is CPU-bound and pure, so it goes to the blocking
    // pool for the same reason `Primes` does: a materialisation partition that
    // ran on the async runtime would stall every other node's heartbeat.
    let work = match spec {
        TaskSpec::Primes { .. } | TaskSpec::Analysis { .. } => {
            let host = host.cloned();
            match tokio::task::spawn_blocking(move || {
                run_task(node, spec).or_else(|| host.and_then(|h| h.run(node, spec, &input)))
            })
            .await
            {
                Ok(work) => work,
                Err(e) => {
                    return (
                        TaskOutcome::Failed {
                            reason: FailureReason::ExecutionError,
                            detail: format!("worker panicked: {e}"),
                        },
                        None,
                    );
                }
            }
        }
        _ => run_task(node, spec).or_else(|| host.and_then(|h| h.run(node, spec, &input))),
    };

    let Some(work) = work else {
        // No built-in and no handler. A task nobody can run is a failure that
        // says so rather than a success with an empty result.
        return (
            TaskOutcome::Failed {
                reason: FailureReason::ExecutionError,
                detail: format!("no workload on {node} can run {}", spec.describe()),
            },
            None,
        );
    };

    match work {
        TaskWork::Done { output } => (TaskOutcome::Completed { output }, None),
        TaskWork::Produced { output, artifact } => {
            (TaskOutcome::Completed { output }, Some(artifact))
        }
        TaskWork::Wait { millis, output } => {
            tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
            (TaskOutcome::Completed { output }, None)
        }
    }
}
