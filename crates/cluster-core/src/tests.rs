//! Unit tests for the cluster core.

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
        NodeCapabilities::new(2, 0),
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
        NodeCapabilities::new(4, 0),
    );
    let healthy_frontend = node(
        2,
        NodeStatus::Healthy,
        &[Role::Frontend],
        0,
        NodeCapabilities::new(4, 0),
    );
    let suspect_compute = node(
        3,
        NodeStatus::Suspect,
        &[Role::Compute],
        0,
        NodeCapabilities::new(4, 0),
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
            NodeCapabilities::new(4, 0),
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::new(1, 0),
        ),
        node(
            3,
            NodeStatus::Healthy,
            &[Role::Compute],
            1,
            NodeCapabilities::new(4, 0),
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
            NodeCapabilities::new(1, 0),
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::new(2, 0),
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
            NodeCapabilities::new(4, 0),
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Backend],
            0,
            NodeCapabilities::new(4, 0),
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
            NodeCapabilities::new(4, 0),
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::new(4, 0),
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
            NodeCapabilities::new(4, 0),
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::new(4, 0),
        ),
        node(
            3,
            NodeStatus::Healthy,
            &[Role::Compute],
            0,
            NodeCapabilities::new(4, 0),
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
            NodeCapabilities::new(4, 0),
        ),
        node(
            1,
            NodeStatus::Offline,
            &[Role::Coordinator],
            0,
            NodeCapabilities::new(4, 0),
        ),
        node(
            2,
            NodeStatus::Healthy,
            &[Role::Coordinator],
            0,
            NodeCapabilities::new(4, 0),
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
            format!("{y:04}-{m:02}-{d:02}"),
            "round trip failed for {y}-{m}-{d}"
        );
    }
    assert_eq!(Millis::from_utc_date(1970, 1, 1), Millis(0));
}

// --- the node agent -------------------------------------------------------
//
// These are the behaviours that would otherwise only be observable by watching
// process logs from a worker that is misbehaving.

use crate::agent::{Action, Agent};
use crate::protocol::{
    NodeMessage, PROTOCOL_VERSION, RejectReason, SupervisorMessage, WireTaskSpec, decode_frame,
    encode_frame, frame_len,
};

fn agent() -> Agent {
    Agent::with_id(NodeId(3), NodeCapabilities::new(2, 0), 1_000)
}

fn welcome() -> SupervisorMessage {
    SupervisorMessage::Welcome {
        protocol: PROTOCOL_VERSION,
        node: NodeId(3),
        heartbeat_interval_ms: 1_000,
    }
}

/// Everything the agent wants sent, in order.
fn sent(actions: &[Action]) -> Vec<NodeMessage> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_node_introduces_itself_with_its_own_capabilities() {
    let agent = agent();
    let NodeMessage::Hello {
        protocol,
        node,
        capabilities,
        ..
    } = agent.hello()
    else {
        panic!("hello is a Hello");
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert_eq!(node, Some(NodeId(3)));
    assert_eq!(
        capabilities,
        NodeCapabilities::new(2, 0),
        "the worker reports what it is, rather than trusting server config"
    );
}

#[test]
fn a_welcomed_node_heartbeats_immediately_and_then_on_interval() {
    let mut agent = agent();
    let actions = agent.handle(welcome(), Millis(10_000));
    assert!(agent.joined());
    assert!(
        matches!(sent(&actions).first(), Some(NodeMessage::Heartbeat(_))),
        "a node proves it is alive at once rather than after a full interval"
    );

    // Nothing due yet.
    assert!(sent(&agent.poll(Millis(10_500))).is_empty());
    // Interval elapsed.
    assert!(matches!(
        sent(&agent.poll(Millis(11_000))).first(),
        Some(NodeMessage::Heartbeat(_))
    ));
}

#[test]
fn a_node_runs_a_task_and_reports_the_result() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(10_000));

    let actions = agent.handle(
        SupervisorMessage::Assign {
            task: TaskId(7),
            spec: WireTaskSpec::Primes {
                start: 0,
                end: 1_000,
            },
        },
        Millis(10_000),
    );
    let messages = sent(&actions);
    assert!(
        matches!(messages[0], NodeMessage::TaskStarted { task: TaskId(7) }),
        "the supervisor is told work has begun before it is finished"
    );
    let Some(NodeMessage::TaskFinished {
        task: TaskId(7),
        outcome: TaskOutcome::Completed { output },
    }) = messages.get(1)
    else {
        panic!("compute finishes in the same pass: {messages:?}");
    };
    assert!(
        output.contains("168"),
        "the remote agent computes the same answer as the local runtime: {output}"
    );
}

#[test]
fn a_sleep_task_is_reported_only_once_its_time_has_passed() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(10_000));

    let actions = agent.handle(
        SupervisorMessage::Assign {
            task: TaskId(1),
            spec: WireTaskSpec::Sleep { millis: 500 },
        },
        Millis(10_000),
    );
    assert_eq!(sent(&actions).len(), 1, "started, but not yet finished");
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::WakeIn(ms) if *ms <= 500)),
        "the caller is told when to come back: {actions:?}"
    );

    assert!(sent(&agent.poll(Millis(10_400))).is_empty());
    assert!(
        matches!(
            sent(&agent.poll(Millis(10_500))).first(),
            Some(NodeMessage::TaskFinished { .. })
        ),
        "the result lands when the sleep is actually over"
    );
}

#[test]
fn a_busy_node_bounces_a_second_task_instead_of_queueing_it() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(10_000));
    agent.handle(
        SupervisorMessage::Assign {
            task: TaskId(1),
            spec: WireTaskSpec::Sleep { millis: 500 },
        },
        Millis(10_000),
    );

    let messages = sent(&agent.handle(
        SupervisorMessage::Assign {
            task: TaskId(2),
            spec: WireTaskSpec::Sleep { millis: 10 },
        },
        Millis(10_100),
    ));
    assert!(
        matches!(
            messages.first(),
            Some(NodeMessage::TaskFinished {
                task: TaskId(2),
                outcome: TaskOutcome::Failed { .. }
            })
        ),
        "the extra task fails fast so the supervisor can place it elsewhere: {messages:?}"
    );
}

#[test]
fn injected_failures_are_consumed_one_task_at_a_time() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(10_000));
    agent.handle(SupervisorMessage::InjectFailures(2), Millis(10_000));

    for attempt in 1..=2 {
        let messages = sent(&agent.handle(
            SupervisorMessage::Assign {
                task: TaskId(attempt),
                spec: WireTaskSpec::Sleep { millis: 0 },
            },
            Millis(10_000),
        ));
        assert!(
            matches!(
                messages.get(1),
                Some(NodeMessage::TaskFinished {
                    outcome: TaskOutcome::Failed {
                        reason: FailureReason::Injected,
                        ..
                    },
                    ..
                })
            ),
            "attempt {attempt} was armed to fail: {messages:?}"
        );
    }

    // The third is not armed and succeeds.
    let messages = sent(&agent.handle(
        SupervisorMessage::Assign {
            task: TaskId(3),
            spec: WireTaskSpec::Sleep { millis: 0 },
        },
        Millis(10_000),
    ));
    assert!(matches!(
        messages.get(1),
        Some(NodeMessage::TaskFinished {
            outcome: TaskOutcome::Completed { .. },
            ..
        })
    ));
}

#[test]
fn a_paused_node_stays_alive_but_stops_proving_it() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(10_000));
    agent.handle(SupervisorMessage::PauseHeartbeat(true), Millis(10_000));

    assert!(
        sent(&agent.poll(Millis(20_000))).is_empty(),
        "a paused node emits nothing, so the supervisor must time it out"
    );

    // Resuming proves liveness at once rather than waiting out an interval:
    // the node has a backlog of silence to make up for.
    let resumed = agent.handle(SupervisorMessage::PauseHeartbeat(false), Millis(20_000));
    assert!(matches!(
        sent(&resumed).first(),
        Some(NodeMessage::Heartbeat(_))
    ));
}

#[test]
fn a_rejected_or_dismissed_node_disconnects() {
    for message in [
        SupervisorMessage::Rejected {
            reason: RejectReason::UnknownNode,
        },
        SupervisorMessage::Shutdown,
    ] {
        let mut agent = agent();
        assert_eq!(agent.handle(message, Millis(0)), vec![Action::Disconnect]);
    }
}

#[test]
fn every_protocol_message_survives_a_round_trip() {
    let node_messages = vec![
        NodeMessage::Hello {
            protocol: PROTOCOL_VERSION,
            node: Some(NodeId(1)),
            capabilities: NodeCapabilities::new(2, 0),
            token: Some("s3cret".into()),
        },
        NodeMessage::Heartbeat(Heartbeat {
            node: NodeId(1),
            load: NodeLoad::default(),
            at: Millis(1),
        }),
        NodeMessage::TaskStarted { task: TaskId(2) },
        NodeMessage::TaskFinished {
            task: TaskId(2),
            outcome: TaskOutcome::Failed {
                reason: FailureReason::NodeOffline,
                detail: String::from("gone"),
            },
        },
    ];
    for message in node_messages {
        let mut buf = Vec::new();
        encode_frame(&message, &mut buf).expect("encode");
        let len = frame_len([buf[0], buf[1]]).expect("length");
        assert_eq!(len, buf.len() - 2, "the prefix describes the body");
        assert_eq!(
            decode_frame::<NodeMessage>(&buf[2..]).expect("decode"),
            message
        );
    }

    let supervisor_messages = vec![
        SupervisorMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            node: NodeId(1),
            heartbeat_interval_ms: 1_000,
        },
        SupervisorMessage::Rejected {
            reason: RejectReason::ProtocolMismatch,
        },
        SupervisorMessage::Assign {
            task: TaskId(3),
            spec: WireTaskSpec::Primes { start: 0, end: 10 },
        },
        SupervisorMessage::PauseHeartbeat(true),
        SupervisorMessage::InjectFailures(2),
        SupervisorMessage::SetDelay(50),
        SupervisorMessage::Shutdown,
    ];
    for message in supervisor_messages {
        let mut buf = Vec::new();
        encode_frame(&message, &mut buf).expect("encode");
        assert_eq!(
            decode_frame::<SupervisorMessage>(&buf[2..]).expect("decode"),
            message
        );
    }
}

#[test]
fn a_heartbeat_frame_stays_within_the_protocol_budget() {
    let mut buf = Vec::new();
    encode_frame(
        &NodeMessage::Heartbeat(Heartbeat {
            node: NodeId(5),
            load: NodeLoad {
                load_percent: 100,
                running_tasks: 1,
                free_memory_bytes: 8 * 1024 * 1024,
                simulated: false,
            },
            at: Millis(1_700_000_000_000),
        }),
        &mut buf,
    )
    .expect("encode");
    // Sent once per second per node, forever. If this ever balloons, the
    // cause is a String on the hot path and this test is the alarm.
    assert!(
        buf.len() <= 32,
        "a heartbeat frame grew to {} bytes",
        buf.len()
    );
}

#[test]
fn a_node_says_nothing_until_it_has_been_welcomed() {
    let mut agent = agent();
    // No Welcome yet: the supervisor has not agreed this node exists.
    let actions = agent.poll(Millis(10_000));
    assert!(
        sent(&actions).is_empty(),
        "heartbeating before the handshake puts frames on the wire the \
         supervisor has not agreed to parse: {actions:?}"
    );
    assert_eq!(
        agent.next_wake_ms(Millis(10_000)),
        None,
        "with nothing to do, the node must not ask to be woken immediately -- \
         a zero deadline spins the worker while it waits for Welcome"
    );
}

/// A compute task must not stop a node from proving it is alive.
///
/// This is the failure that would look like "the cluster breaks whenever I
/// submit a real workload": the node runs the range to completion inside one
/// call, misses every heartbeat while it does, is declared Offline, has its
/// task requeued elsewhere -- and then reports a result nobody is waiting for.
#[test]
fn a_long_computation_does_not_starve_heartbeats() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(0));

    // Big enough that running it in one go would outlast any sane heartbeat
    // timeout on a 240 MHz core.
    let range = 0..200_000u64;
    let assigned = agent.handle(
        SupervisorMessage::Assign {
            task: TaskId(9),
            spec: WireTaskSpec::Primes {
                start: range.start,
                end: range.end,
            },
        },
        Millis(0),
    );
    let messages = sent(&assigned);
    assert!(matches!(
        messages.first(),
        Some(NodeMessage::TaskStarted { .. })
    ));
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, NodeMessage::TaskFinished { .. })),
        "the work is taken in slices, not run to completion inside one call"
    );

    // Drive the clock forward the way the worker loop does.
    let mut last_heard = 0u64;
    let mut longest_silence = 0u64;
    let mut output = None;
    let mut at = 0u64;
    while output.is_none() && at < 600_000 {
        at += 100;
        for message in sent(&agent.poll(Millis(at))) {
            match message {
                NodeMessage::Heartbeat(_) => {
                    longest_silence = longest_silence.max(at - last_heard);
                    last_heard = at;
                }
                NodeMessage::TaskFinished {
                    outcome: TaskOutcome::Completed { output: done },
                    ..
                } => output = Some(done),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    longest_silence = longest_silence.max(at - last_heard);

    let output = output.expect("the task finishes");
    // The real bound: the supervisor starts doubting a node after
    // `suspect_after_ms` of silence and requeues its work after
    // `offline_after_ms`. Computing must never take a node near either.
    let suspect_after = HealthPolicy::default().suspect_after_ms;
    assert!(
        longest_silence < suspect_after,
        "went {longest_silence}ms without a heartbeat while computing, and the \
         supervisor grows suspicious at {suspect_after}ms"
    );
    assert_eq!(
        output,
        crate::workload::primes_output(
            NodeId(3),
            range.start,
            range.end,
            crate::workload::count_primes(range.start, range.end)
        ),
        "slicing the range must not change the answer or how it is reported"
    );
}

#[test]
fn re_assigning_the_task_a_node_is_already_running_is_not_a_failure() {
    let mut agent = agent();
    agent.handle(welcome(), Millis(0));

    let assign = || SupervisorMessage::Assign {
        task: TaskId(4),
        spec: WireTaskSpec::Sleep { millis: 500 },
    };
    agent.handle(assign(), Millis(0));

    // The supervisor requeued this task after a network hiccup and handed it
    // back to the same node. The node is already doing exactly that work.
    let messages = sent(&agent.handle(assign(), Millis(100)));
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, NodeMessage::TaskFinished { .. })),
        "failing the duplicate would burn a retry on work already in flight: \
         {messages:?}"
    );

    // And the original still completes on its own schedule.
    assert!(matches!(
        sent(&agent.poll(Millis(500))).first(),
        Some(NodeMessage::TaskFinished {
            task: TaskId(4),
            outcome: TaskOutcome::Completed { .. }
        })
    ));
}

// --- the join token ---------------------------------------------------------

#[test]
fn a_coordinator_with_a_token_refuses_a_worker_without_one() {
    use crate::protocol::token_accepted;

    assert!(token_accepted(Some("s3cret"), Some("s3cret")));
    assert!(!token_accepted(Some("s3cret"), Some("s3cre")), "prefix");
    assert!(!token_accepted(Some("s3cret"), Some("s3cretx")), "suffix");
    assert!(!token_accepted(Some("s3cret"), Some("S3CRET")), "case");
    assert!(!token_accepted(Some("s3cret"), None), "no token at all");
    assert!(!token_accepted(Some("s3cret"), Some("")), "empty");
}

/// A token longer than the cap is refused rather than compared, so nothing
/// downstream has to reason about how long a "token" a peer may send.
#[test]
fn an_oversized_token_is_refused() {
    use crate::protocol::{MAX_TOKEN, token_accepted};

    let huge = "a".repeat(MAX_TOKEN + 1);
    assert!(!token_accepted(Some(&huge), Some(&huge)));
}

/// In-process workers never cross a socket. A coordinator with no token
/// configured must keep working for them -- the server is what refuses to open
/// the port in that state.
#[test]
fn no_token_configured_accepts_the_in_process_case() {
    use crate::protocol::token_accepted;

    assert!(token_accepted(None, None));
    assert!(token_accepted(None, Some("anything")));
}

/// The token has to actually reach the wire, or the check above is checking a
/// field nobody fills in.
#[test]
fn the_agent_puts_its_token_in_hello() {
    let agent =
        Agent::anonymous(NodeCapabilities::new(4, 0), 1_000).with_token(Some("s3cret".into()));

    let NodeMessage::Hello {
        token, protocol, ..
    } = agent.hello()
    else {
        panic!("hello is a Hello");
    };
    assert_eq!(token.as_deref(), Some("s3cret"));
    assert_eq!(protocol, PROTOCOL_VERSION);
}
