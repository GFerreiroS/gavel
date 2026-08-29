//! Integration tests for real nodes over the network transport.
//!
//! These do what the simulated tests cannot: they exercise the actual wire.
//! The "workers" here are in-test clients that speak the same protocol the
//! a real worker process speaks, over a real TCP socket to a real listener, using
//! the same `cluster_core::protocol` codec. Nothing is stubbed except the
//! work itself.
//!
//! The one that matters is `a_worker_that_vanishes_mid_task_loses_no_work`:
//! the failure test, but with the worker on the far side of a socket that
//! gets yanked -- which is how a killed process actually fails.

// The shared harness serves both test binaries; this one does not need every
// helper in it.
#[allow(dead_code)]
mod support;

use std::time::Duration;

use cluster_core::{
    ClusterControl, Heartbeat, JobSpec, JobState, Millis, NodeCapabilities, NodeId, NodeLoad,
    NodeMessage, PROTOCOL_VERSION, RejectReason, Role, SupervisorMessage, TaskOutcome, TaskState,
    decode_frame, encode_frame, frame_len,
};
use cluster_local::{LocalCluster, LocalClusterConfig, RemoteNode};
use support::MemoryStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Compressed timings, as in `failover.rs`: fast enough to run in a test,
/// slow enough to exercise the real health state machine.
fn remote_config(declared: Vec<RemoteNode>) -> LocalClusterConfig {
    let mut config = LocalClusterConfig {
        // No in-process workers: everything here arrives over the wire.
        node_count: 0,
        remote_nodes: declared,
        // Port 0 so the OS picks a free one and tests never collide.
        node_listen: Some("127.0.0.1:0".parse().unwrap()),
        tick_interval_ms: 20,
        ..LocalClusterConfig::default()
    };
    config.health.heartbeat_interval_ms = 50;
    config.health.suspect_after_ms = 150;
    config.health.offline_after_ms = 300;
    config
}

fn declared(n: u16) -> Vec<RemoteNode> {
    (1..=n)
        .map(|id| RemoteNode {
            id: NodeId(id),
            capabilities: NodeCapabilities::new(2, 0),
        })
        .collect()
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

/// A stand-in for a worker process: it speaks the wire protocol and nothing else.
struct FakeWorker {
    id: NodeId,
    socket: TcpStream,
}

impl FakeWorker {
    /// Connect and complete the handshake.
    async fn join(address: &str, id: NodeId, capabilities: NodeCapabilities) -> Self {
        let mut socket = TcpStream::connect(address).await.expect("connect");
        let mut worker = {
            // Send Hello before there is a `FakeWorker` to send it from.
            let mut buf = Vec::new();
            encode_frame(
                &NodeMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    node: Some(id),
                    capabilities,
                },
                &mut buf,
            )
            .expect("encode hello");
            socket.write_all(&buf).await.expect("send hello");
            FakeWorker { id, socket }
        };

        match worker.recv().await {
            SupervisorMessage::Welcome { protocol, .. } => {
                assert_eq!(protocol, PROTOCOL_VERSION);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
        worker
    }

    /// Connect and expect to be turned away.
    async fn join_expecting_rejection(address: &str, id: NodeId, protocol: u16) -> RejectReason {
        let mut socket = TcpStream::connect(address).await.expect("connect");
        let mut buf = Vec::new();
        encode_frame(
            &NodeMessage::Hello {
                protocol,
                node: Some(id),
                capabilities: NodeCapabilities::new(2, 0),
            },
            &mut buf,
        )
        .expect("encode hello");
        socket.write_all(&buf).await.expect("send hello");

        let mut worker = FakeWorker { id, socket };
        match worker.recv().await {
            SupervisorMessage::Rejected { reason } => reason,
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    async fn send(&mut self, message: &NodeMessage) {
        let mut buf = Vec::new();
        encode_frame(message, &mut buf).expect("encode");
        self.socket.write_all(&buf).await.expect("send");
    }

    /// Receive, with a deadline. An unbounded wait here turns any scheduling
    /// regression into a test run that hangs forever instead of failing.
    async fn recv(&mut self) -> SupervisorMessage {
        match tokio::time::timeout(Duration::from_secs(10), self.recv_unbounded()).await {
            Ok(message) => message,
            Err(_) => panic!("timed out waiting for a message from the coordinator"),
        }
    }

    async fn recv_unbounded(&mut self) -> SupervisorMessage {
        let mut prefix = [0u8; 2];
        self.socket.read_exact(&mut prefix).await.expect("prefix");
        let len = frame_len(prefix).expect("length");
        let mut body = vec![0u8; len];
        self.socket.read_exact(&mut body).await.expect("body");
        decode_frame(&body).expect("decode")
    }

    async fn heartbeat(&mut self) {
        let id = self.id;
        self.send(&NodeMessage::Heartbeat(Heartbeat {
            node: id,
            load: NodeLoad {
                load_percent: 5,
                running_tasks: 0,
                free_memory_bytes: 8 * 1024 * 1024,
                // A real worker measures rather than simulates.
                simulated: false,
            },
            // Deliberately nonsense: a worker with no RTC boots its clock at
            // zero. The supervisor must stamp arrival itself rather than
            // believing this, or every real node is instantly "years stale".
            at: Millis(0),
        }))
        .await;
    }

    /// Heartbeat forever, so the node stays healthy while a test does other
    /// things. Returns a handle that stops the worker when dropped/aborted.
    fn heartbeat_forever(mut self, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.heartbeat().await;
                tokio::time::sleep(interval).await;
            }
        })
    }
}

/// Start a cluster and return its handle plus the address workers dial.
///
/// The listener binds on port 0, so the port has to be discovered rather than
/// assumed. Binding happens in a spawned task, hence the short retry.
async fn start(config: LocalClusterConfig) -> (LocalCluster, String, MemoryStore) {
    let listen = config.node_listen.expect("a listen address");
    let store = MemoryStore::new();
    // Port 0 means the OS assigns; bind here first to learn the port, then
    // hand the concrete address to the cluster.
    let probe = tokio::net::TcpListener::bind(listen).await.expect("bind");
    let address = probe.local_addr().expect("addr").to_string();
    drop(probe);

    let mut config = config;
    config.node_listen = Some(address.parse().unwrap());
    let (cluster, _task) = LocalCluster::start(config, store.clone());

    // Wait for the listener to come up.
    wait_for("listener accepting", async || {
        TcpStream::connect(&address).await.is_ok()
    })
    .await;

    (cluster, address, store)
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_workers_exist_as_offline_nodes_before_they_connect() {
    let (cluster, _address, _store) = start(remote_config(declared(5))).await;

    let nodes = cluster.nodes().await;
    assert_eq!(nodes.len(), 5, "all five workers are in the registry");
    assert!(
        nodes.iter().all(|n| !n.status.accepts_work()),
        "a declared worker that has never connected is offline, not healthy"
    );

    let snapshot = cluster.snapshot().await;
    assert_eq!(snapshot.nodes_online, 0);
    assert_eq!(
        snapshot.nodes_total, 5,
        "an unplugged worker is still a member of the cluster"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_worker_keeps_its_roles_when_it_connects() {
    let (cluster, address, _store) = start(remote_config(declared(5))).await;

    // Roles are assigned at bootstrap, before any remote worker connects.
    let before = cluster.node(NodeId(1)).await.expect("node 1 declared");
    assert!(
        before.roles.contains(Role::Compute),
        "roles are assigned to workers that have not connected yet"
    );

    let worker = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let _pulse = worker.heartbeat_forever(Duration::from_millis(50));

    wait_for("worker online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    let after = cluster.node(NodeId(1)).await.expect("node 1 online");
    assert_eq!(
        after.roles, before.roles,
        "connecting changes a node's status, never its identity or roles"
    );
    assert!(!after.load.simulated, "a real worker reports measured load");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_is_turned_away_when_it_does_not_belong() {
    let (_cluster, address, _store) = start(remote_config(declared(2))).await;

    assert_eq!(
        FakeWorker::join_expecting_rejection(&address, NodeId(99), PROTOCOL_VERSION).await,
        RejectReason::UnknownNode,
        "a worker with an id nobody declared is refused"
    );
    assert_eq!(
        FakeWorker::join_expecting_rejection(&address, NodeId(1), PROTOCOL_VERSION + 1).await,
        RejectReason::ProtocolMismatch,
        "a worker built against a different protocol is refused"
    );

    let first = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let _pulse = first.heartbeat_forever(Duration::from_millis(50));
    assert_eq!(
        FakeWorker::join_expecting_rejection(&address, NodeId(1), PROTOCOL_VERSION).await,
        RejectReason::AlreadyConnected,
        "two workers cannot claim the same identity"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn work_is_scheduled_onto_remote_workers_over_the_wire() {
    let (cluster, address, _store) = start(remote_config(declared(2))).await;

    let mut one = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let mut two = FakeWorker::join(&address, NodeId(2), NodeCapabilities::new(2, 0)).await;
    one.heartbeat().await;
    two.heartbeat().await;

    wait_for("both workers online", async || {
        cluster.snapshot().await.nodes_online == 2
    })
    .await;

    let job = cluster
        .submit_job(JobSpec::Sleep {
            total_ms: 100,
            tasks: 2,
        })
        .await
        .expect("submit");

    // Each worker takes one task, runs it, and reports back -- exactly what the
    // worker's task loop does.
    for worker in [&mut one, &mut two] {
        let SupervisorMessage::Assign { task, .. } = worker.recv().await else {
            panic!("expected an assignment");
        };
        let id = worker.id;
        worker.send(&NodeMessage::TaskStarted { task }).await;
        worker
            .send(&NodeMessage::TaskFinished {
                task,
                outcome: TaskOutcome::Completed {
                    output: format!("done on {id}"),
                },
            })
            .await;
    }

    wait_for("job completes", async || {
        cluster
            .job(job)
            .await
            .is_some_and(|d| d.job.state == JobState::Completed)
    })
    .await;

    let detail = cluster.job(job).await.expect("job");
    assert!(
        detail
            .tasks
            .iter()
            .all(|t| t.state == TaskState::Completed && t.assigned_to.is_some()),
        "every task ran on a real worker"
    );
}

/// The required failure test over the network: a worker dies holding a task
/// and the work must survive it.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_that_vanishes_mid_task_loses_no_work() {
    // Generous health timings on purpose. The dead worker is detected from its
    // closed socket, which is immediate, so nothing here needs a short
    // timeout -- and with one, the *surviving* worker ages out mid-test. It
    // heartbeats only when this test tells it to, because it is also the
    // socket being read from, so under a loaded parallel run it was going
    // Offline before the requeue landed and leaving nothing schedulable.
    let mut config = remote_config(declared(2));
    config.health.suspect_after_ms = 30_000;
    config.health.offline_after_ms = 60_000;
    let (cluster, address, _store) = start(config).await;

    let mut victim = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let mut survivor = FakeWorker::join(&address, NodeId(2), NodeCapabilities::new(2, 0)).await;
    victim.heartbeat().await;
    survivor.heartbeat().await;

    wait_for("both workers online", async || {
        cluster.snapshot().await.nodes_online == 2
    })
    .await;

    // One task, so exactly one worker gets it and the other is the fallback.
    let job = cluster
        .submit_job(JobSpec::Sleep {
            total_ms: 200,
            tasks: 1,
        })
        .await
        .expect("submit");

    // Whichever worker is chosen becomes the victim; the other must finish the
    // work. Placement is the scheduler's business, not this test's.
    let (victim, mut survivor) = {
        let assignment = tokio::time::timeout(Duration::from_secs(5), victim.recv()).await;
        match assignment {
            Ok(SupervisorMessage::Assign { task, .. }) => {
                victim.send(&NodeMessage::TaskStarted { task }).await;
                (victim, survivor)
            }
            // Node 1 was not chosen, so node 2 holds the task instead.
            _ => {
                let SupervisorMessage::Assign { task, .. } = survivor.recv().await else {
                    panic!("neither worker was assigned the task");
                };
                survivor.send(&NodeMessage::TaskStarted { task }).await;
                (survivor, victim)
            }
        }
    };

    let dead = victim.id;
    // Yank the power: drop the socket without any goodbye. This is what a
    // hard-killed worker process looks like from the coordinator's side.
    drop(victim);

    wait_for("the dead worker is seen to be gone", async || {
        cluster
            .node(dead)
            .await
            .is_some_and(|n| !n.status.accepts_work())
    })
    .await;

    // The survivor is handed the same task, on a fresh attempt.
    let SupervisorMessage::Assign { task: requeued, .. } = survivor.recv().await else {
        panic!("the task was not re-assigned to the surviving worker");
    };
    survivor
        .send(&NodeMessage::TaskStarted { task: requeued })
        .await;
    survivor
        .send(&NodeMessage::TaskFinished {
            task: requeued,
            outcome: TaskOutcome::Completed {
                output: "finished after a worker died".into(),
            },
        })
        .await;

    wait_for("job completes despite the dead worker", async || {
        cluster
            .job(job)
            .await
            .is_some_and(|d| d.job.state == JobState::Completed)
    })
    .await;

    let detail = cluster.job(job).await.expect("job");
    assert_eq!(
        detail.tasks[0].assigned_to,
        Some(survivor.id),
        "the task finished on the worker that survived"
    );
    assert!(
        detail.tasks[0].attempt >= 2,
        "the re-run is recorded as a second attempt, not silently retried"
    );
    assert!(
        !detail.failures.is_empty(),
        "the failure is recorded and visible"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_worker_may_reconnect_under_the_same_identity() {
    let (cluster, address, _store) = start(remote_config(declared(1))).await;

    let worker = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let pulse = worker.heartbeat_forever(Duration::from_millis(50));
    wait_for("worker online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;
    let joined_at = cluster.node(NodeId(1)).await.expect("node").joined_at;

    pulse.abort();
    wait_for("worker offline", async || {
        cluster.snapshot().await.nodes_online == 0
    })
    .await;

    // Same worker, powered back on.
    let worker = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let _pulse = worker.heartbeat_forever(Duration::from_millis(50));
    wait_for("worker back online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    assert_eq!(
        cluster.node(NodeId(1)).await.expect("node").joined_at,
        joined_at,
        "a restart does not give the worker a new identity"
    );
}

/// A worker that loses power leaves a *half-open* socket behind: no FIN ever
/// arrives, so the server's TCP stack keeps the connection for hours. The node
/// correctly goes Offline on heartbeat timeout, but its connection slot is
/// still occupied -- and a worker that reboots two seconds later then finds its
/// own identity taken.
///
/// Simulated the way the server actually experiences it: a connection that
/// stays open and simply goes quiet. `zombie` is deliberately held in scope to
/// the end of the test, because dropping it would send a FIN and turn this
/// into the easy case that already worked.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_worker_reclaims_its_identity_from_a_dead_connection() {
    let (cluster, address, _store) = start(remote_config(declared(1))).await;

    let mut zombie = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    zombie.heartbeat().await;
    wait_for("worker online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    // Power cut: the worker stops talking. The socket stays open.
    wait_for("the silent worker is declared offline", async || {
        cluster.snapshot().await.nodes_online == 0
    })
    .await;

    // The worker reboots and dials back in. Its identity must be available.
    let reborn = FakeWorker::join(&address, NodeId(1), NodeCapabilities::new(2, 0)).await;
    let _pulse = reborn.heartbeat_forever(Duration::from_millis(50));

    wait_for("the rebooted worker rejoins", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    // Hold the dead connection open for the whole test.
    drop(zombie);
}

// --- anonymous workers ----------------------------------------------------
//
// The ordinary case on a server: a worker process starts, dials in, and is
// given an identity. Nothing about it is configured in advance, which is what
// makes `docker compose up --scale worker=8` mean anything.

impl FakeWorker {
    /// Connect without asserting an identity, and take the one assigned.
    async fn join_anonymous(address: &str) -> Self {
        let mut socket = TcpStream::connect(address).await.expect("connect");
        let mut buf = Vec::new();
        encode_frame(
            &NodeMessage::Hello {
                protocol: PROTOCOL_VERSION,
                node: None,
                capabilities: NodeCapabilities::new(2, 0),
            },
            &mut buf,
        )
        .expect("encode hello");
        socket.write_all(&buf).await.expect("send hello");

        // The id is not known until the coordinator answers.
        let mut worker = FakeWorker {
            id: NodeId(0),
            socket,
        };
        match worker.recv().await {
            SupervisorMessage::Welcome { node, .. } => worker.id = node,
            other => panic!("expected Welcome, got {other:?}"),
        }
        worker
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_worker_is_given_an_identity() {
    // Nothing declared: the whole point is that no configuration mentions
    // these processes.
    let (cluster, address, _store) = start(remote_config(Vec::new())).await;
    assert!(cluster.nodes().await.is_empty(), "nothing is declared");

    let first = FakeWorker::join_anonymous(&address).await;
    let second = FakeWorker::join_anonymous(&address).await;
    assert_ne!(
        first.id, second.id,
        "two workers started from the same config must not share an identity"
    );

    let pulses = [
        first.heartbeat_forever(Duration::from_millis(50)),
        second.heartbeat_forever(Duration::from_millis(50)),
    ];
    wait_for("both workers online", async || {
        cluster.snapshot().await.nodes_online == 2
    })
    .await;

    for node in cluster.nodes().await {
        assert!(
            node.has_role(Role::Compute),
            "{} joined to do work, so it can be scheduled",
            node.id
        );
    }

    for pulse in pulses {
        pulse.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_departed_anonymous_worker_leaves_no_trace() {
    let (cluster, address, _store) = start(remote_config(Vec::new())).await;

    let worker = FakeWorker::join_anonymous(&address).await;
    let id = worker.id;
    let pulse = worker.heartbeat_forever(Duration::from_millis(50));
    wait_for("worker online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;

    // The process stops. Dropping the socket sends a FIN, which is what an
    // orderly `docker compose scale` looks like.
    pulse.abort();

    wait_for("the worker is forgotten", async || {
        cluster.nodes().await.is_empty()
    })
    .await;
    assert_eq!(
        cluster.node(id).await,
        None,
        "a replica that scaled away must not linger in the registry: one \
         tombstone per departed process would grow without bound"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_freed_identity_is_reused() {
    let (cluster, address, _store) = start(remote_config(Vec::new())).await;

    let first = FakeWorker::join_anonymous(&address).await;
    let id = first.id;
    let pulse = first.heartbeat_forever(Duration::from_millis(50));
    wait_for("worker online", async || {
        cluster.snapshot().await.nodes_online == 1
    })
    .await;
    pulse.abort();
    wait_for("worker forgotten", async || {
        cluster.nodes().await.is_empty()
    })
    .await;

    // A coordinator that counted upwards forever would be showing node-4291
    // after a few thousand restarts.
    let replacement = FakeWorker::join_anonymous(&address).await;
    assert_eq!(replacement.id, id, "the freed id is handed out again");
}
