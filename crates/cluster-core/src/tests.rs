//! Unit tests for the portable core (CLAUDE.md 34).

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use crate::*;

fn node(id: u16, status: NodeStatus, roles: &[Role], running: u16, caps: NodeCapabilities) -> Node {
    let mut n = Node::new(NodeId(id), caps, Millis(0));
    n.status = status;
    n.roles = RoleSet::from_roles(roles.iter().copied());
    n.load.running_tasks = running;
    n
}

fn task() -> Task {
    Task::new(
        TaskId(1),
        JobId(1),
        0,
        TaskSpec::Sleep { millis: 10 },
        Millis(0),
    )
}

// --- roles ---------------------------------------------------------------

#[test]
fn role_set_is_a_set() {
    let mut roles = RoleSet::EMPTY;
    assert!(roles.is_empty());
    assert!(roles.insert(Role::Compute));
    assert!(
        !roles.insert(Role::Compute),
        "second insert reports no change"
    );
    assert!(roles.insert(Role::Gateway));
    assert_eq!(roles.len(), 2);
    assert!(roles.contains(Role::Gateway));
    assert!(roles.remove(Role::Gateway));
    assert!(!roles.remove(Role::Gateway));
    assert_eq!(roles.iter().collect::<Vec<_>>(), vec![Role::Compute]);
}

#[test]
fn a_node_may_hold_many_roles() {
    let n = node(
        1,
        NodeStatus::Healthy,
        &[Role::Gateway, Role::Frontend, Role::Compute],
        0,
        NodeCapabilities::ESP32_S3,
    );
    assert_eq!(n.roles.len(), 3);
    assert!(n.has_role(Role::Gateway) && n.has_role(Role::Compute));
}

#[test]
fn unmet_policies_are_listed_in_priority_order() {
    let policies = RolePolicies::default();
    // Nothing is running at all.
    let unmet = policies.unmet(|_| 0);
    let roles: Vec<Role> = unmet.iter().map(|(r, _)| *r).collect();
    assert_eq!(
        roles,
        vec![
            Role::Gateway,
            Role::Frontend,
            Role::Backend,
            Role::Storage,
            Role::Coordinator
        ]
    );
    assert_eq!(unmet[1].1, 2, "frontend wants two replicas");
}

#[test]
fn surplus_replicas_do_not_underflow() {
    // Regression: computing the deficit eagerly used to panic when a role was
    // over-provisioned.
    let policies = RolePolicies::default();
    assert!(policies.unmet(|_| 99).is_empty());
}

// --- health --------------------------------------------------------------

#[test]
fn health_degrades_by_heartbeat_age() {
    let policy = HealthPolicy {
        heartbeat_interval_ms: 1_000,
        suspect_after_ms: 3_000,
        offline_after_ms: 6_000,
    };
    let seen = Millis(10_000);
    assert_eq!(policy.classify(seen, Millis(11_000)), None, "still healthy");
    assert_eq!(
        policy.classify(seen, Millis(13_500)),
        Some(NodeStatus::Suspect)
    );
    assert_eq!(
        policy.classify(seen, Millis(20_000)),
        Some(NodeStatus::Offline)
    );
}

#[test]
fn only_healthy_compute_nodes_take_work() {
    let healthy_compute = node(
        1,
        NodeStatus::Healthy,
        &[Role::Compute],
        0,
        NodeCapabilities::HOST,
    );
    let healthy_frontend = node(
        2,
        NodeStatus::Healthy,
        &[Role::Frontend],
        0,
        NodeCapabilities::HOST,
    );
    let suspect_compute = node(
        3,
        NodeStatus::Suspect,
        &[Role::Compute],
        0,
        NodeCapabilities::HOST,
    );
    assert!(healthy_compute.is_schedulable());
    assert!(!healthy_frontend.is_schedulable());
    assert!(!suspect_compute.is_schedulable());
}

// --- job / task state machines -------------------------------------------

#[test]
fn job_transitions_are_checked() {
    let mut job = Job::new(
        JobId(1),
        JobSpec::Sleep {
            total_ms: 10,
            tasks: 1,
        },
        Millis(0),
    );
    assert_eq!(job.state, JobState::Queued);
    job.transition_to(JobState::Running, Millis(1)).unwrap();
    assert!(job.transition_to(JobState::Queued, Millis(2)).is_err());
    job.transition_to(JobState::Completed, Millis(5)).unwrap();
    assert_eq!(job.finished_at, Some(Millis(5)));
    assert!(job.state.is_terminal());
    assert!(job.transition_to(JobState::Running, Millis(6)).is_err());
}

#[test]
fn task_lifecycle_counts_attempts() {
    let mut t = task();
    assert_eq!(t.attempt, 0);

    t.assign(NodeId(3), Millis(1)).unwrap();
    assert_eq!(t.attempt, 1);
    assert_eq!(t.assigned_to, Some(NodeId(3)));

    t.start(Millis(2)).unwrap();
    assert_eq!(t.state, TaskState::Running);

    // The worker dies: back to the queue, from the beginning.
    t.requeue(Millis(3)).unwrap();
    assert_eq!(t.state, TaskState::Queued);
    assert_eq!(t.assigned_to, None);
    assert_eq!(
        t.attempt, 1,
        "requeueing does not itself count as an attempt"
    );

    t.assign(NodeId(4), Millis(4)).unwrap();
    assert_eq!(t.attempt, 2, "the retry does");
    t.start(Millis(5)).unwrap();
    t.complete("done".to_string(), Millis(6)).unwrap();
    assert!(t.state.is_terminal());
    assert_eq!(t.output.as_deref(), Some("done"));
}

#[test]
fn a_completed_task_cannot_be_reopened() {
    let mut t = task();
    t.assign(NodeId(1), Millis(1)).unwrap();
    t.start(Millis(2)).unwrap();
    t.complete("done".to_string(), Millis(3)).unwrap();
    assert!(t.requeue(Millis(4)).is_err());
    assert!(t.fail(Millis(4)).is_err());
}

#[test]
fn a_task_lost_before_it_started_can_still_be_requeued() {
    // Assigned but never started: the node went offline in between.
    let mut t = task();
    t.assign(NodeId(1), Millis(1)).unwrap();
    t.requeue(Millis(2)).unwrap();
    assert_eq!(t.state, TaskState::Queued);
}

#[test]
fn a_permanently_failed_task_can_be_retried_by_policy() {
    let mut t = task();
    t.assign(NodeId(1), Millis(1)).unwrap();
    t.fail(Millis(2)).unwrap();
    assert!(t.requeue(Millis(3)).is_ok());
}

// --- job splitting -------------------------------------------------------

#[test]
fn sleep_splits_evenly_and_keeps_the_remainder() {
    let spec = JobSpec::Sleep {
        total_ms: 1_001,
        tasks: 4,
    };
    let parts = spec.split();
    assert_eq!(parts.len(), 4);
    let total: u64 = parts.iter().map(|p| p.weight()).sum();
    assert_eq!(total, 1_001, "no milliseconds are lost in the split");
}

#[test]
fn prime_ranges_tile_the_interval_without_gaps() {
    let spec = JobSpec::Primes {
        upper_bound: 1_000,
        tasks: 3,
    };
    let parts = spec.split();
    assert_eq!(parts.len(), 3);
    let mut expected_start = 0;
    for part in &parts {
        let TaskSpec::Primes { start, end } = *part else {
            panic!("wrong task kind");
        };
        assert_eq!(start, expected_start);
        expected_start = end;
    }
    assert_eq!(expected_start, 1_000, "the last range reaches the bound");
}

#[test]
fn a_job_always_has_at_least_one_task() {
    assert_eq!(
        JobSpec::Sleep {
            total_ms: 10,
            tasks: 0
        }
        .split()
        .len(),
        1
    );
}

#[test]
fn progress_is_reported_as_a_percentage() {
    let mut job = Job::new(
        JobId(1),
        JobSpec::Primes {
            upper_bound: 100,
            tasks: 4,
        },
        Millis(0),
    );
    assert_eq!(job.progress_percent(), 0);
    job.tasks_completed = 2;
    assert_eq!(job.progress_percent(), 50);
    job.tasks_completed = 4;
    assert_eq!(job.progress_percent(), 100);
}

// --- scheduling ----------------------------------------------------------

#[tokio::test]
async fn least_loaded_picks_the_idle_node() {
    let nodes = vec![
        node(
            1,
            NodeStatus::Healthy,
            &[Role::Compute],
            3,
            NodeCapabilities::HOST,
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::ESP32_C3,
        ),
        node(
            3,
            NodeStatus::Healthy,
            &[Role::Compute],
            1,
            NodeCapabilities::HOST,
        ),
    ];
    let chosen = LeastLoaded.select_node(&task(), &nodes).await.unwrap();
    assert_eq!(chosen, NodeId(2));
}

#[tokio::test]
async fn least_loaded_breaks_ties_by_capability_then_id() {
    let nodes = vec![
        node(
            1,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::ESP32_C3,
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::ESP32_S3,
        ),
    ];
    // Both idle; the dual-core S3 wins over the single-core C3.
    assert_eq!(
        LeastLoaded.select_node(&task(), &nodes).await.unwrap(),
        NodeId(2)
    );
}

#[tokio::test]
async fn scheduling_skips_unhealthy_and_non_compute_nodes() {
    let nodes = vec![
        node(
            1,
            NodeStatus::Offline,
            &[Role::Compute],
            0,
            NodeCapabilities::HOST,
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Backend],
            0,
            NodeCapabilities::HOST,
        ),
    ];
    assert_eq!(
        LeastLoaded.select_node(&task(), &nodes).await,
        Err(SchedulerError::NoEligibleNode)
    );
}

#[tokio::test]
async fn round_robin_rotates_across_tasks() {
    let nodes = vec![
        node(
            1,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::HOST,
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::HOST,
        ),
    ];
    let task_n = |n: u64| {
        let mut t = task();
        t.id = TaskId(n);
        t
    };
    let first = RoundRobin.select_node(&task_n(1), &nodes).await.unwrap();
    let second = RoundRobin.select_node(&task_n(2), &nodes).await.unwrap();
    let third = RoundRobin.select_node(&task_n(3), &nodes).await.unwrap();
    assert_ne!(first, second, "consecutive tasks land on different nodes");
    assert_eq!(first, third, "and then it wraps");
}

#[tokio::test]
async fn round_robin_is_deterministic() {
    // Two schedulers with the same view must agree: there is no cursor to
    // diverge, which matters once more than one node can schedule.
    let nodes = vec![
        node(
            1,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::HOST,
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::HOST,
        ),
        node(
            3,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::HOST,
        ),
    ];
    let mut t = task();
    t.id = TaskId(7);
    assert_eq!(
        RoundRobin.select_node(&t, &nodes).await,
        RoundRobin.select_node(&t, &nodes).await
    );
}

// --- election ------------------------------------------------------------

#[test]
fn the_lowest_healthy_id_leads() {
    let nodes = vec![
        node(
            3,
            NodeStatus::Healthy,
            &[Role::Coordinator],
            0,
            NodeCapabilities::HOST,
        ),
        node(
            1,
            NodeStatus::Offline,
            &[Role::Coordinator],
            0,
            NodeCapabilities::HOST,
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Coordinator],
            0,
            NodeCapabilities::HOST,
        ),
    ];
    assert_eq!(LowestHealthyId.elect(&nodes), Some(NodeId(2)));
    assert_eq!(LowestHealthyId.elect(&[]), None);
}

// --- time ----------------------------------------------------------------

#[test]
fn timestamps_format_without_a_date_library() {
    // 2024-02-29T12:34:56Z -- a leap day, to exercise the civil-from-days math.
    assert_eq!(
        Millis(1_709_210_096_000).to_utc_string(),
        "2024-02-29 12:34:56"
    );
    assert_eq!(Millis(0).to_utc_string(), "1970-01-01 00:00:00");
    assert_eq!(Millis(1_709_210_096_000).to_clock_string(), "12:34:56");
}

#[test]
fn elapsed_time_saturates_rather_than_underflowing() {
    assert_eq!(Millis(5).since(Millis(10)), 0);
}

// --- events --------------------------------------------------------------

#[test]
fn events_describe_themselves() {
    let event = ClusterEvent::TaskFailed {
        task: TaskId(7),
        node: Some(NodeId(3)),
        reason: FailureReason::NodeOffline,
    };
    assert_eq!(event.kind(), "task_failed");
    assert_eq!(event.node(), Some(NodeId(3)));
    assert_eq!(event.message(), "task-07 failed on node-03 (node_offline)");
    assert_eq!(event.severity(), event::EventSeverity::Error);
}

#[test]
fn calendar_dates_round_trip_through_millis() {
    for (y, m, d) in [
        (1970, 1, 1),
        (2000, 2, 29),
        (2024, 2, 29),
        (2026, 8, 18),
        (2026, 12, 31),
    ] {
        let at = Millis::from_utc_date(y, m, d);
        assert_eq!(
            at.to_date_string(),
            alloc::format!("{y:04}-{m:02}-{d:02}"),
            "round trip failed for {y}-{m}-{d}"
        );
    }
    assert_eq!(Millis::from_utc_date(1970, 1, 1), Millis(0));
}
