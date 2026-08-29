//! Running this binary as a worker.
//!
//! A worker is the same executable as the web server, started with
//! `--connect <coordinator>`. It serves no HTTP, opens no database and holds
//! no application state: it dials the coordinator, says hello, and then does
//! what it is told.
//!
//! Using one binary for both roles is deliberate. A worker built separately is
//! a second thing to version, ship and get wrong; this way the code that runs
//! a task on a worker is byte-identical to the code that was tested.
//!
//! All the behaviour -- when to heartbeat, whether to accept a task, what to
//! report -- lives in `cluster_core::Agent`, which has no socket and no timer.
//! This module is only the part that genuinely needs the outside world: a TCP
//! connection, a clock, and a loop.

use std::time::Duration;

use cluster_core::{
    Action, Agent, Millis, NodeCapabilities, SupervisorMessage, decode_frame, encode_frame,
    frame_len,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long to wait before redialling. A worker that reconnected instantly
/// would hammer a coordinator that is restarting.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Connect to `address` and work until the process is stopped.
///
/// Never returns on its own: losing the coordinator is an expected condition,
/// not a reason to exit. A worker that quit on a dropped connection would need
/// something else to restart it, which is exactly the babysitting a process
/// manager should not have to do.
pub async fn run(address: &str, token: Option<String>) -> anyhow::Result<()> {
    let capabilities = NodeCapabilities::local();
    if token.is_none() {
        tracing::warn!("APP_CLUSTER_TOKEN is not set; the coordinator will refuse this worker");
    }
    tracing::info!(
        coordinator = address,
        cores = capabilities.cores,
        "worker starting"
    );

    loop {
        match session(address, capabilities, token.as_deref()).await {
            Ok(()) => tracing::info!("coordinator closed the connection"),
            Err(e) => tracing::warn!(error = %e, "connection lost"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// One connection, from `Hello` until the socket dies.
async fn session(
    address: &str,
    capabilities: NodeCapabilities,
    token: Option<&str>,
) -> anyhow::Result<()> {
    let mut socket = TcpStream::connect(address).await?;
    // Frames are small and latency matters: a heartbeat held back by Nagle's
    // algorithm is a worker that looks slower than it is.
    socket.set_nodelay(true)?;

    // Anonymous: the coordinator hands out identity. A worker that chose its
    // own would collide with itself the moment two replicas started from the
    // same configuration.
    let mut agent = Agent::anonymous(capabilities, 1_000).with_token(token.map(str::to_string));

    let mut out = Vec::with_capacity(256);
    encode_frame(&agent.hello(), &mut out)?;
    socket.write_all(&out).await?;

    let (mut rd, mut wr) = socket.into_split();

    // Reading runs in its own task rather than as a `select!` branch, because
    // reading a frame awaits twice -- once for the length prefix, once for the
    // body -- and is therefore not cancel-safe. A `select!` that dropped it
    // between those awaits would swallow the prefix and leave the stream
    // permanently misaligned.
    let (frames_tx, mut frames) = tokio::sync::mpsc::channel::<SupervisorMessage>(16);
    let reader = tokio::spawn(async move {
        loop {
            let mut prefix = [0u8; 2];
            if rd.read_exact(&mut prefix).await.is_err() {
                break;
            }
            let Ok(len) = frame_len(prefix) else { break };
            let mut body = vec![0u8; len];
            if rd.read_exact(&mut body).await.is_err() {
                break;
            }
            match decode_frame::<SupervisorMessage>(&body) {
                Ok(message) => {
                    if frames_tx.send(message).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "undecodable frame from the coordinator");
                    break;
                }
            }
        }
    });

    let result = drive(&mut agent, &mut frames, &mut wr).await;
    reader.abort();
    result
}

/// Feed the agent, and do what it asks.
async fn drive(
    agent: &mut Agent,
    frames: &mut tokio::sync::mpsc::Receiver<SupervisorMessage>,
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
) -> anyhow::Result<()> {
    let started = tokio::time::Instant::now();
    // Milliseconds since this worker started. Deliberately not wall time: the
    // coordinator stamps heartbeat arrival with its own clock precisely so a
    // worker's clock never has to agree with anyone else's.
    let now = move || Millis(started.elapsed().as_millis() as u64);

    let mut out = Vec::with_capacity(256);
    loop {
        // How long the agent is willing to wait before it next needs to act.
        // Asking rather than polling: `poll` emits actions, so using it to
        // answer a timing question would discard whatever it produced.
        let wake = agent.next_wake_ms(now()).unwrap_or(1_000).max(1);

        let mut actions = Vec::new();
        tokio::select! {
            message = frames.recv() => match message {
                Some(message) => {
                    if let SupervisorMessage::Rejected { reason } = &message {
                        // A refused worker is a configuration mistake. Naming
                        // it beats "connection closed" on a log line.
                        tracing::error!(%reason, "the coordinator refused this worker");
                    }
                    actions.extend(agent.handle(message, now()));
                }
                None => return Ok(()),
            },
            _ = tokio::time::sleep(Duration::from_millis(wake)) => {}
        }

        actions.extend(agent.poll(now()));

        for action in actions {
            match action {
                Action::Send(message) => {
                    out.clear();
                    encode_frame(&message, &mut out)?;
                    wr.write_all(&out).await?;
                }
                // Honoured by the deadline at the top of the next pass.
                Action::WakeIn(_) => {}
                Action::Disconnect => return Ok(()),
            }
        }
    }
}
