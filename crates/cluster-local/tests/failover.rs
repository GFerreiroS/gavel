//! Integration tests for the runtime (CLAUDE.md 34).
//!
//! The important one is `a_dead_worker_does_not_lose_its_task`: submit, kill
//! the worker mid-flight, and assert the task is re-run elsewhere and the job
//! still completes.

mod support;

use std::time::Duration;

use cluster_core::{ClusterControl, JobSpec, JobState, NodeId, Role, TaskState};
use cluster_local::{LocalCluster, LocalClusterConfig};
use support::MemoryStore;

/// Timings are compressed so the tests finish quickly but still exercise the
/// real health state machine.
fn fast_config(nodes: u16) -> LocalClusterConfig {
    let mut config = LocalClusterConfig {
        node_count: nodes,
        tick_interval_ms: 20,
        ..LocalClusterConfig::default()
    };
    config.health.heartbeat_interval_ms = 50;
    config.health.suspect_after_ms = 150;
    config.health.offline_after_ms = 300;
    config
}

async fn wait_for(label: &str, mut predicate: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        if predicate().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cluster_starts_with_healthy_roled_nodes() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(8), store.clone());

    wait_for("all nodes online", async || {
        cluster.snapshot().await.nodes_online == 8
    })
    .await;

    let snapshot = cluster.snapshot().await;
    assert_eq!(snapshot.nodes_total, 8);
    assert!(snapshot.leader.is_some(), "a coordinator is elected");
    assert!(snapshot.gateway.is_some(), "a gateway is assigned");
    assert_eq!(
        snapshot.roles.get(Role::Compute),
        8,
        "every node can compute"
    );
    for role in [Role::Gateway, Role::Frontend, Role::Backend] {
        assert!(
            snapshot.roles.get(role) >= snapshot.policies.get(role).min_replicas,
            "{role} minimum is met at startup"
        );
    }
    assert!(!snapshot.is_degraded());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_is_split_across_workers_and_completes() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(4), store.clone());
    wait_for("nodes online", async || {
        cluster.snapshot().await.nodes_online == 4
    })
    .await;

    let job_id = cluster
        .submit_job(JobSpec::Sleep {
            total_ms: 400,
            tasks: 4,
        })
        .await
        .expect("submit");

    wait_for("job completes", async || {
        cluster
            .job(job_id)
            .await
            .is_some_and(|d| d.job.state == JobState::Completed)
    })
    .await;

    let detail = cluster.job(job_id).await.unwrap();
    assert_eq!(detail.job.tasks_completed, 4);
    assert_eq!(detail.job.progress_percent(), 100);
    assert!(detail.failures.is_empty());

    let mut nodes: Vec<NodeId> = detail.tasks.iter().filter_map(|t| t.assigned_to).collect();
    nodes.sort();
    nodes.dedup();
    assert!(
        nodes.len() > 1,
        "work should land on more than one node, got {nodes:?}"
    );
    for task in &detail.tasks {
        assert_eq!(task.state, TaskState::Completed);
        assert!(task.output.as_ref().is_some_and(|o| o.contains("node-")));
    }
}

/// The test CLAUDE.md 34 calls out explicitly.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_worker_does_not_lose_its_task() {
    let store = MemoryStore::new();
    // One node only, so the task is certain to be on the node we kill.
    let (cluster, _task) = LocalCluster::start(fast_config(2), store.clone());
    wait_for("nodes online", async || {
        cluster.snapshot().await.nodes_online == 2
    })
    .await;

    let job_id = cluster
        .submit_job(JobSpec::Sleep {
            total_ms: 4_000,
            tasks: 1,
        })
        .await
        .expect("submit");

    // Wait until a node actually picks the task up.
    wait_for("task assigned", async || {
        cluster
            .job(job_id)
            .await
            .is_some_and(|d| d.tasks[0].assigned_to.is_some())
    })
    .await;

    let victim = cluster.job(job_id).await.unwrap().tasks[0]
        .assigned_to
        .expect("assigned");
    cluster.stop_node(victim).await.expect("stop");

    wait_for("job completes anyway", async || {
        cluster
            .job(job_id)
            .await
            .is_some_and(|d| d.job.state == JobState::Completed)
    })
    .await;

    let detail = cluster.job(job_id).await.unwrap();
    let task = &detail.tasks[0];
    assert_eq!(task.state, TaskState::Completed);
    assert_ne!(task.assigned_to, Some(victim), "it ran somewhere else");
    assert_eq!(task.attempt, 2, "the second attempt is the one that worked");

    // The failure is recorded, not swallowed.
    let failures = store.failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].node_id, Some(victim));
    assert_eq!(failures[0].reason, cluster_core::FailureReason::NodeOffline);

    let kinds = store.event_kinds();
    for expected in [
        "task_assigned",
        "task_failed",
        "task_requeued",
        "job_completed",
    ] {
        assert!(kinds.contains(&expected), "missing event {expected}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_keeps_failing_eventually_fails_the_job() {
    let store = MemoryStore::new();
    let mut config = fast_config(1);
    config.max_task_attempts = 2;
    let (cluster, _task) = LocalCluster::start(config, store.clone());
    wait_for("node online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    // Arm both attempts up front. Re-arming between them would be a race:
    // a requeued task is now re-dispatched the instant the failure is
    // reported, so the retry can start before a second arm arrives.
    cluster.inject_failures(NodeId(1), 2).await.unwrap();
    let job_id = cluster
        .submit_job(JobSpec::Sleep {
            total_ms: 20,
            tasks: 1,
        })
        .await
        .unwrap();

    wait_for("job fails", async || {
        cluster
            .job(job_id)
            .await
            .is_some_and(|d| d.job.state == JobState::Failed)
    })
    .await;

    let detail = cluster.job(job_id).await.unwrap();
    assert_eq!(detail.job.tasks_failed, 1);
    assert_eq!(detail.tasks[0].state, TaskState::Failed);
    assert_eq!(detail.tasks[0].attempt, 2, "capped by max_task_attempts");
    assert_eq!(detail.failures.len(), 2, "every attempt is recorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missed_heartbeat_moves_a_node_through_suspect_to_offline() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(3), store.clone());
    wait_for("nodes online", async || {
        cluster.snapshot().await.nodes_online == 3
    })
    .await;

    // The node keeps running but stops reporting -- the case a plain "stop"
    // would not exercise.
    cluster.pause_heartbeat(NodeId(2), true).await.unwrap();

    wait_for("node goes offline", async || {
        cluster
            .node(NodeId(2))
            .await
            .is_some_and(|n| n.status == cluster_core::NodeStatus::Offline)
    })
    .await;
    assert_eq!(cluster.snapshot().await.nodes_online, 2);

    cluster.pause_heartbeat(NodeId(2), false).await.unwrap();
    wait_for("node recovers", async || {
        cluster.snapshot().await.nodes_online == 3
    })
    .await;

    let kinds = store.event_kinds();
    assert!(kinds.contains(&"node_unhealthy"));
    assert!(kinds.contains(&"node_recovered"));
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_can_be_changed_at_runtime_without_changing_identity() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(4), store.clone());
    wait_for("nodes online", async || {
        cluster.snapshot().await.nodes_online == 4
    })
    .await;

    let before = cluster.node(NodeId(4)).await.unwrap();
    // Whichever roles startup happened to hand out, pick one it does not have,
    // so the test does not depend on the initial assignment order.
    let new_role = cluster_core::ALL_ROLES
        .into_iter()
        .find(|r| !before.has_role(*r))
        .expect("a node does not start with every role");
    cluster.set_role(NodeId(4), new_role, true).await.unwrap();

    let after = cluster.node(NodeId(4)).await.unwrap();
    assert_eq!(after.id, before.id, "identity is unchanged");
    assert_eq!(after.joined_at, before.joined_at);
    assert!(after.has_role(new_role));
    assert!(after.has_role(Role::Compute), "existing roles are kept");
    assert_eq!(after.roles.len(), before.roles.len() + 1);

    cluster
        .set_role(NodeId(4), Role::Compute, false)
        .await
        .unwrap();
    assert!(
        !cluster
            .node(NodeId(4))
            .await
            .unwrap()
            .has_role(Role::Compute)
    );

    assert!(store.event_kinds().contains(&"role_assigned"));
    assert!(store.event_kinds().contains(&"role_removed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn losing_the_coordinator_elects_a_new_one() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(4), store.clone());
    wait_for("leader elected", async || {
        cluster.snapshot().await.leader.is_some()
    })
    .await;

    let leader = cluster.snapshot().await.leader.unwrap();
    cluster.stop_node(leader).await.unwrap();

    wait_for("new leader", async || {
        cluster.snapshot().await.leader.is_some_and(|l| l != leader)
    })
    .await;
    assert!(store.event_kinds().contains(&"leader_elected"));
}

#[tokio::test(flavor = "multi_thread")]
async fn work_queues_when_no_node_can_take_it_and_runs_when_one_can() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(1), store.clone());
    wait_for("node online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    // Remove the only Compute role: nothing is schedulable.
    cluster
        .set_role(NodeId(1), Role::Compute, false)
        .await
        .unwrap();
    let job_id = cluster
        .submit_job(JobSpec::Primes {
            upper_bound: 5_000,
            tasks: 2,
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let snapshot = cluster.snapshot().await;
    assert_eq!(snapshot.tasks_queued, 2, "tasks wait rather than failing");
    assert_eq!(snapshot.jobs.queued, 1);

    // Give the role back; the queue drains.
    cluster
        .set_role(NodeId(1), Role::Compute, true)
        .await
        .unwrap();
    wait_for("job completes", async || {
        cluster
            .job(job_id)
            .await
            .is_some_and(|d| d.job.state == JobState::Completed)
    })
    .await;
    assert!(store.failures().is_empty(), "queueing is not a failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn role_changes_survive_a_restart() {
    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(4), store.clone());
    wait_for("nodes online", async || {
        cluster.snapshot().await.nodes_online == 4
    })
    .await;

    // Startup assignments are written out, not just held in memory.
    let assigned = cluster.node(NodeId(2)).await.unwrap().roles;
    assert_eq!(store.stored_roles(NodeId(2)), Some(assigned));

    cluster
        .set_role(NodeId(2), Role::Storage, true)
        .await
        .unwrap();
    let after = cluster.node(NodeId(2)).await.unwrap().roles;
    assert!(after.contains(Role::Storage));
    assert_eq!(
        store.stored_roles(NodeId(2)),
        Some(after),
        "the change is persisted immediately, not at shutdown"
    );

    // A second cluster over the same store is what a restart looks like.
    drop(cluster);
    let (restarted, _task2) = LocalCluster::start(fast_config(4), store.clone());
    wait_for("restarted nodes online", async || {
        restarted.snapshot().await.nodes_online == 4
    })
    .await;

    let recovered = restarted.node(NodeId(2)).await.unwrap();
    assert_eq!(recovered.id, NodeId(2), "identity is unchanged");
    assert_eq!(
        recovered.roles, after,
        "the runtime role change survived the restart"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_pushes_live_events() {
    use tokio_stream::StreamExt;

    let store = MemoryStore::new();
    let (cluster, _task) = LocalCluster::start(fast_config(2), store.clone());
    wait_for("nodes online", async || {
        cluster.snapshot().await.nodes_online == 2
    })
    .await;

    // Subscribe first, then cause something: the stream is live, not a replay.
    let mut stream = cluster.subscribe();
    cluster
        .submit_job(JobSpec::Sleep {
            total_ms: 40,
            tasks: 1,
        })
        .await
        .unwrap();

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            Ok(Some(record)) => {
                seen.push(record.event.kind());
                if seen.contains(&"job_completed") {
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(seen.contains(&"job_created"), "got {seen:?}");
    assert!(seen.contains(&"job_completed"), "got {seen:?}");
}
