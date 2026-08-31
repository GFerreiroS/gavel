//! Remote workers over TCP.
//!
//! This transport contains no cluster logic: its entire job is to make a worker on
//! the far end of a socket indistinguishable, to the supervisor, from a Tokio
//! task in this process. Both are reached by pushing a `NodeInbox` into a
//! channel; this module is the piece that turns those pushes into frames and
//! turns frames back into `NodeReport`s.
//!
//! Workers dial in rather than being dialled. That is not a convenience -- a
//! worker container has whatever address the network gave it, may sit behind a
//! NAT, and is restarted whenever the orchestrator feels like it. The one
//! stable address in the system is the coordinator's, so it listens and the
//! workers connect. It is also what makes `--scale worker=8` work without
//! telling the coordinator anything.
//!
//! TCP, and deliberately so: ordered, reliable delivery keeps the failure
//! modes worth thinking about down to one, which is the connection dropping.
//! The protocol still carries explicit task ids in both directions and never
//! relies on a message's position in the stream, so a lossier transport can
//! replace this one without touching the coordinator.

use std::net::SocketAddr;
use std::time::Duration;

use cluster_core::{
    FailureReason, Heartbeat, NodeId, NodeMessage, PROTOCOL_VERSION, RejectReason,
    SupervisorMessage, Task, TaskId, TaskOutcome, WireTaskSpec, decode_frame, encode_frame,
    frame_len, token_accepted,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use crate::node::{NodeInbox, NodeReport};
use crate::supervisor::{Command, RemoteAttachment};

/// How long a freshly opened connection has to send its `Hello` before it is
/// dropped. Without this a half-open socket from a stalled worker or a port
/// scanner would hold a slot indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to spend trying to tell a node it is being disconnected before
/// giving up and closing anyway.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(1);

/// Accept worker connections until the supervisor goes away.
pub(crate) async fn serve(
    listener: TcpListener,
    commands: mpsc::Sender<Command>,
    token: Option<String>,
    artifacts: Option<std::sync::Arc<dyn cluster_core::ArtifactStore>>,
) {
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    tracing::info!(address = %local, "listening for cluster nodes");

    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::error!(error = %e, "accepting a node connection failed");
                // Usually transient: fd exhaustion, or a peer that vanished
                // between the SYN and the accept. Pause rather than spinning
                // the CPU retrying thousands of times a second.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        if commands.is_closed() {
            break;
        }
        let commands = commands.clone();
        let token = token.clone();
        let artifacts = artifacts.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(socket, peer, commands, token.as_deref(), artifacts).await {
                tracing::debug!(peer = %peer, error = %e, "node connection ended");
            }
        });
    }
}

/// One worker, from handshake to disconnect.
async fn connection(
    socket: TcpStream,
    peer: SocketAddr,
    commands: mpsc::Sender<Command>,
    token: Option<&str>,
    artifacts: Option<std::sync::Arc<dyn cluster_core::ArtifactStore>>,
) -> std::io::Result<()> {
    // Frames are small and latency matters: a heartbeat held back by Nagle's
    // algorithm is a node that looks slower than it is.
    socket.set_nodelay(true)?;

    let (mut rd, mut wr) = socket.into_split();

    let hello = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut rd)).await {
        Ok(Ok(Some(message))) => message,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            tracing::debug!(peer = %peer, "node did not complete its handshake in time");
            return Ok(());
        }
    };

    let NodeMessage::Hello {
        protocol,
        node: requested,
        capabilities,
        token: presented,
    } = hello
    else {
        tracing::warn!(peer = %peer, "first frame from a node was not Hello");
        return Ok(());
    };

    if protocol != PROTOCOL_VERSION {
        tracing::warn!(
            peer = %peer, node = ?requested, theirs = protocol, ours = PROTOCOL_VERSION,
            "rejecting worker: protocol mismatch"
        );
        return reject(&mut wr, RejectReason::ProtocolMismatch).await;
    }

    // Checked here, before the supervisor is even asked. An unauthenticated
    // peer must not reach the registry: `AttachRemote` allocates an id and
    // evicts whatever held it, which is work an unknown caller should not be
    // able to make the coordinator do.
    if !token_accepted(token, presented.as_deref()) {
        tracing::warn!(peer = %peer, "rejecting worker: missing or incorrect join token");
        return reject(&mut wr, RejectReason::Unauthorized).await;
    }

    // Ask the supervisor whether this worker may join, and which identity it
    // gets. It owns the registry, so it -- not this task -- decides.
    let (inbox_tx, mut inbox) = mpsc::channel(8);
    let (shutdown_tx, mut shutdown) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    if commands
        .send(Command::AttachRemote {
            id: requested,
            capabilities,
            inbox: inbox_tx,
            shutdown: shutdown_tx,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return Ok(());
    }
    let Ok(verdict) = reply_rx.await else {
        return Ok(());
    };
    let RemoteAttachment {
        id,
        reports,
        heartbeat_interval_ms,
        generation,
    } = match verdict {
        Ok(attachment) => attachment,
        Err(reason) => {
            tracing::warn!(peer = %peer, node = ?requested, %reason, "rejecting worker");
            return reject(&mut wr, reason).await;
        }
    };

    tracing::info!(peer = %peer, node = %id, "worker joined");
    write_frame(
        &mut wr,
        &SupervisorMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            node: id,
            heartbeat_interval_ms,
        },
    )
    .await?;

    // Reading runs in its own task rather than as a `select!` branch.
    //
    // `read_frame` awaits twice -- once for the length prefix, once for the
    // body -- so it is not cancel-safe: a `select!` that dropped it between
    // those two awaits would swallow the prefix and leave the stream
    // permanently misaligned. Forwarding through a channel means the select
    // below only ever awaits a `recv`, which is cancel-safe.
    let (frames_tx, mut frames) = mpsc::channel::<NodeMessage>(16);
    let reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut rd).await {
                Ok(Some(message)) => {
                    if frames_tx.send(message).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(node = %id, error = %e, "reading from node failed");
                    break;
                }
            }
        }
    });

    // The task this worker is currently holding. Kept here so the wire can
    // carry a bare `TaskId` while the supervisor still receives the whole
    // `Task` it expects -- the worker has no reason to echo back a structure
    // the server already has.
    let mut assigned: Option<Task> = None;
    let mut out = Vec::with_capacity(256);

    let result = loop {
        tokio::select! {
            biased;

            // The supervisor is shutting this node down.
            _ = &mut shutdown => {
                // Bounded, because this is often reached precisely when the
                // peer is dead: an evicted stale connection has a worker on the
                // far end that stopped acknowledging, so once the kernel send
                // buffer fills, an unbounded `write_all` never returns and
                // this task -- and its socket -- leaks. The goodbye is a
                // courtesy; the disconnect is not conditional on it landing.
                let _ = tokio::time::timeout(
                    GOODBYE_TIMEOUT,
                    write_frame(&mut wr, &SupervisorMessage::Shutdown),
                )
                .await;
                break Ok(());
            }

            message = inbox.recv() => {
                let Some(message) = message else { break Ok(()) };
                out.clear();
                // Each arm fills `out` itself, because the assignment is the
                // one that can decline to. Encoding after the match, as this
                // used to, meant a task the wire could not carry was already
                // recorded as this node's -- assigned to a worker that never
                // received it, and held until a health timeout noticed.
                let refused = match message {
                    NodeInbox::Assign(task) => {
                        if let Some(previous) = &assigned {
                            tracing::warn!(
                                node = %id, task = %previous.id,
                                "assigning over a task this node had not finished"
                            );
                        }
                        // The coordinator fetches the input and puts it in the
                        // assignment: a worker has no database to fetch it
                        // from, which is the property that lets it be on
                        // another machine at all.
                        let input = artifacts.as_ref().and_then(|a| a.input(task.spec));
                        match ship(task.id, task.spec, input, &mut out) {
                            Ok(()) => {
                                assigned = Some(*task);
                                None
                            }
                            Err(why) => Some((task, why)),
                        }
                    }
                    // Control frames: a task id and a number, with nothing to
                    // requeue if the encoder somehow refuses one.
                    other => {
                        let control = match other {
                            NodeInbox::PauseHeartbeat(paused) => {
                                SupervisorMessage::PauseHeartbeat(paused)
                            }
                            NodeInbox::InjectFailures(count) => {
                                SupervisorMessage::InjectFailures(count)
                            }
                            NodeInbox::SetDelay(ms) => SupervisorMessage::SetDelay(ms),
                            NodeInbox::Assign(_) => unreachable!("matched above"),
                        };
                        if encode_frame(&control, &mut out).is_err() {
                            tracing::error!(node = %id, "could not encode a message for this node");
                            out.clear();
                        }
                        None
                    }
                };
                // A task the wire cannot carry is reported as a failed
                // attempt, so the scheduler requeues it and it lands on a
                // worker that can run it -- which today means an in-process
                // one, where an artifact never leaves memory.
                if let Some((task, why)) = refused {
                    tracing::warn!(node = %id, task = %task.id, %why,
                        "refusing to assign a task this worker cannot be sent");
                    let _ = reports
                        .send(NodeReport::TaskFinished {
                            node: id,
                            task,
                            outcome: TaskOutcome::Failed {
                                reason: FailureReason::ExecutionError,
                                detail: why,
                            },
                        })
                        .await;
                    continue;
                }
                if out.is_empty() {
                    continue;
                }
                if let Err(e) = wr.write_all(&out).await {
                    break Err(e);
                }
            }

            frame = frames.recv() => {
                let Some(message) = frame else { break Ok(()) };
                let report = match message {
                    // A worker may only speak for itself. Overriding the id
                    // rather than trusting it means a buggy or hostile node
                    // cannot forge another node's health.
                    NodeMessage::Heartbeat(hb) => {
                        Some(NodeReport::Heartbeat(Heartbeat { node: id, ..hb }))
                    }
                    NodeMessage::TaskStarted { task } => match_task(&assigned, task, id)
                        .map(|task| NodeReport::TaskStarted { node: id, task }),
                    // The result, before the completion. Delivered to the same
                    // store the in-process path writes to, so only the
                    // transport differs and the coordinator cannot tell which
                    // way a partition arrived.
                    //
                    // A corrupt artifact is dropped rather than staged: the
                    // task then completes with nothing recorded for it, the
                    // candidate stays incomplete, and the coordinator redoes
                    // it. A market computed from damaged bytes would render
                    // perfectly and be wrong, which is the failure this digest
                    // exists to prevent.
                    NodeMessage::TaskProduced { task, artifact } => {
                        if let Some(task) = match_task(&assigned, task, id) {
                            match (artifact.verify(), &artifacts) {
                                (Some(bytes), Some(store)) => store.produced(task.spec, bytes),
                                (None, _) => tracing::warn!(
                                    node = %id, task = %task.id,
                                    "discarding a result whose integrity check failed"
                                ),
                                (_, None) => tracing::warn!(
                                    node = %id, task = %task.id,
                                    "a worker produced a result and nothing is here to take it"
                                ),
                            }
                        }
                        None
                    }
                    NodeMessage::TaskFinished { task, outcome } => {
                        let report = match_task(&assigned, task, id)
                            .map(|task| NodeReport::TaskFinished { node: id, task, outcome });
                        if report.is_some() {
                            assigned = None;
                        }
                        report
                    }
                    NodeMessage::Hello { .. } => {
                        tracing::warn!(node = %id, "worker said Hello twice");
                        None
                    }
                };
                if let Some(report) = report
                    && reports.send(report).await.is_err()
                {
                    break Ok(());
                }
            }
        }
    };

    reader.abort();

    // Tell the supervisor at once. Its heartbeat timeout would notice
    // eventually, but a closed socket is proof where silence is only evidence,
    // and the difference is several seconds of a task sitting on a dead node.
    let _ = commands
        .send(Command::DetachRemote { id, generation })
        .await;
    tracing::info!(node = %id, "worker left the cluster");
    result
}

/// Put one assignment in `out`, or say why it cannot go.
///
/// Two refusals and one ending. `WireTaskSpec::of` declines a task whose input
/// has gone or is larger than a frame carries; the encode declines what is
/// somehow larger still. Neither costs the connection: the caller reports a
/// failed attempt and the scheduler places the task somewhere it fits.
fn ship(
    id: TaskId,
    spec: cluster_core::TaskSpec,
    input: Option<Vec<u8>>,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let spec = WireTaskSpec::of(spec, input).map_err(|why| why.to_string())?;
    encode_frame(&SupervisorMessage::Assign { task: id, spec }, out).map_err(|why| {
        out.clear();
        why.to_string()
    })
}

/// Resolve a task id reported by a worker against the task it was actually
/// given. A mismatch means the report is stale -- the task was requeued
/// elsewhere while the worker was still working on it -- and is dropped here
/// rather than being allowed to disturb the task table.
fn match_task(assigned: &Option<Task>, reported: TaskId, node: NodeId) -> Option<Box<Task>> {
    match assigned {
        Some(task) if task.id == reported => Some(Box::new(task.clone())),
        _ => {
            tracing::debug!(node = %node, task = %reported, "ignoring a stale task report");
            None
        }
    }
}

async fn reject(wr: &mut OwnedWriteHalf, reason: RejectReason) -> std::io::Result<()> {
    write_frame(wr, &SupervisorMessage::Rejected { reason }).await
}

/// Read one length-prefixed frame. `None` means the peer closed cleanly.
async fn read_frame(rd: &mut OwnedReadHalf) -> std::io::Result<Option<NodeMessage>> {
    let mut prefix = [0u8; cluster_core::LENGTH_PREFIX];
    match rd.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    // Checked before a single byte of body is buffered, so an oversized
    // declaration costs nothing to refuse.
    let len = frame_len(prefix).map_err(invalid)?;
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body).await?;
    decode_frame(&body).map(Some).map_err(invalid)
}

async fn write_frame(wr: &mut OwnedWriteHalf, message: &SupervisorMessage) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(64);
    encode_frame(message, &mut buf).map_err(invalid)?;
    wr.write_all(&buf).await
}

fn invalid(e: cluster_core::ProtocolError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}
