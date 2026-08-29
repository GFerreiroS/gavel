//! The node <-> supervisor wire protocol.
//!
//! This is the contract that lets a worker stop being a Tokio task in the web
//! process and start being a separate process -- on this machine, on another
//! one, or in another container. Both sides link *this* module.
//!
//! Two properties keep the worker transport predictable:
//!
//! 1. **Compact.** Frames are postcard, not JSON. A `Heartbeat` is ~21 bytes
//!    rather than ~140; at one per second per worker that is the difference
//!    between background noise and traffic.
//! 2. **Bounded.** Every frame is length-prefixed and capped at [`MAX_FRAME`],
//!    so a peer can *refuse* an absurd frame instead of allocating for it.
//!
//! The message pair mirrors the internal `NodeInbox`/`NodeReport` pair that
//! the local runtime already used between the supervisor and its simulated
//! nodes. That is not a coincidence -- it is why this port is small.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ids::{NodeId, TaskId};
use crate::job::{TaskOutcome, TaskSpec};
use crate::node::{Heartbeat, NodeCapabilities};

/// Bumped on any breaking change to the message types below. A worker whose
/// version does not match is rejected at `Hello` rather than being allowed to
/// half-work. Version 2 made `Hello.node` optional, so a worker can ask to be
/// given an identity instead of asserting one. Version 3 added the join token.
pub const PROTOCOL_VERSION: u16 = 3;

/// Longest join token either side will send or compare.
///
/// The frame cap already bounds it; this keeps a token from crowding out the
/// rest of the `Hello` and makes "too long" a clear refusal rather than a
/// malformed frame.
pub const MAX_TOKEN: usize = 256;

/// Largest frame either side will encode or accept, in bytes.
///
/// The biggest realistic message fits several times over, while an invalid
/// peer still cannot force an unbounded allocation.
pub const MAX_FRAME: usize = 2048;

/// Bytes of length prefix in front of every frame.
pub const LENGTH_PREFIX: usize = 2;

/// Worker -> supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeMessage {
    /// First frame on every connection.
    ///
    /// `node` is `None` for an ordinary worker, which is the normal case: a
    /// process that was just started has no identity of its own and asks the
    /// coordinator for one. Workers are interchangeable, and a worker that
    /// insisted on an id would collide with itself the moment two copies were
    /// started from the same config.
    ///
    /// `Some(id)` is for a worker with a *fixed* identity that must survive a
    /// restart -- one pinned to a named host or a volume. The
    /// coordinator then only accepts it if that id is declared.
    ///
    /// Capabilities travel from the worker rather than from coordinator
    /// config, because the worker is the thing that knows how many cores it
    /// was actually given.
    Hello {
        protocol: u16,
        node: Option<NodeId>,
        capabilities: NodeCapabilities,
        /// Shared secret proving this worker was invited.
        ///
        /// Before this existed, five bytes on a socket was the whole of
        /// joining a cluster: `Hello` with no identity, and the coordinator
        /// answered `Welcome`. Anyone who could reach the port could take
        /// work, report whatever outcome they liked for it, and crowd out the
        /// real workers -- on a port the deployment notes tell you to bind to
        /// `0.0.0.0`.
        ///
        /// `None` is for a coordinator configured without a token, which is
        /// only allowed when it is not listening on a socket at all.
        ///
        /// It crosses the wire as it is. That is the same trust boundary the
        /// rest of the deployment already has -- TLS terminates at the proxy,
        /// and the worker link belongs on a private network or through a
        /// tunnel. A token on an untrusted network is a token you have given
        /// away.
        token: Option<String>,
    },
    Heartbeat(Heartbeat),
    TaskStarted {
        task: TaskId,
    },
    TaskFinished {
        task: TaskId,
        outcome: TaskOutcome,
    },
}

/// Supervisor -> worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorMessage {
    /// Accepted. Carries the heartbeat interval so timing policy lives in one
    /// place rather than being compiled into each worker.
    ///
    /// Deliberately does *not* carry the supervisor's clock. An earlier
    /// version did, but nothing could use it: heartbeats are stamped on arrival by the
    /// supervisor precisely so a node's own clock never has to be correct in
    /// absolute terms. A field that no side reads is a contract waiting to be
    /// misread.
    Welcome {
        protocol: u16,
        /// The identity the coordinator assigned. A worker that asked for one
        /// learns it here; a worker that asserted one has it confirmed.
        node: NodeId,
        heartbeat_interval_ms: u64,
    },
    /// Rejected, with a reason the node can print to its console.
    Rejected {
        reason: RejectReason,
    },
    /// Run this task.
    ///
    /// Carries an id and a spec rather than the whole `Task`: state,
    /// assignment, attempt count and output all belong to the supervisor, and
    /// a node that cannot see the cluster has no use for them. The server
    /// keeps the full `Task` and matches reports back to it by id.
    Assign {
        task: TaskId,
        spec: WireTaskSpec,
    },
    /// Failure-simulation controls shared by remote and in-process workers.
    PauseHeartbeat(bool),
    InjectFailures(u32),
    SetDelay(u64),
    Shutdown,
}

/// [`TaskSpec`] in a form this codec can decode.
///
/// The domain type is internally tagged (`#[serde(tag = "kind")]`) because it
/// is persisted as JSON in SQLite and served over the JSON API, where a flat
/// `{"kind":"sleep","millis":100}` is the shape callers expect. Postcard
/// cannot decode internally tagged enums at all -- they require a
/// self-describing format it deliberately is not.
///
/// Rather than change a persisted representation (which would strand every
/// job row already in the database) or give up on a compact wire format, the
/// protocol carries its own externally tagged mirror. The conversions below
/// match exhaustively with no wildcard arm, so adding a `TaskSpec` variant
/// without teaching the wire about it is a compile error rather than a
/// runtime protocol mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireTaskSpec {
    Sleep { millis: u64 },
    Primes { start: u64, end: u64 },
}

impl From<TaskSpec> for WireTaskSpec {
    fn from(spec: TaskSpec) -> Self {
        match spec {
            TaskSpec::Sleep { millis } => WireTaskSpec::Sleep { millis },
            TaskSpec::Primes { start, end } => WireTaskSpec::Primes { start, end },
        }
    }
}

impl From<WireTaskSpec> for TaskSpec {
    fn from(spec: WireTaskSpec) -> Self {
        match spec {
            WireTaskSpec::Sleep { millis } => TaskSpec::Sleep { millis },
            WireTaskSpec::Primes { start, end } => TaskSpec::Primes { start, end },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RejectReason {
    /// `Hello.protocol` did not match [`PROTOCOL_VERSION`].
    ProtocolMismatch,
    /// A fixed id was asserted that the coordinator does not have declared.
    UnknownNode,
    /// A worker with that id is already connected.
    AlreadyConnected,
    /// The coordinator is not accepting any more workers.
    Full,
    /// No join token, or the wrong one.
    Unauthorized,
}

impl RejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            RejectReason::ProtocolMismatch => "protocol version mismatch",
            RejectReason::UnknownNode => "node id not in cluster topology",
            RejectReason::AlreadyConnected => "a node with that id is already connected",
            RejectReason::Full => "the coordinator is not accepting more workers",
            RejectReason::Unauthorized => "missing or incorrect join token",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame of {size} bytes exceeds the {MAX_FRAME} byte limit")]
    FrameTooLarge { size: usize },
    #[error("malformed frame")]
    Malformed,
}

/// Whether a presented join token matches the one this coordinator expects.
///
/// Constant time over the comparison, so the reply cannot be used to recover
/// the token one byte at a time. A coordinator with no token configured
/// accepts anything -- which is why the server refuses to open the worker
/// socket at all in that state.
pub fn token_accepted(expected: Option<&str>, presented: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(presented) = presented else {
        return false;
    };
    if presented.len() > MAX_TOKEN {
        return false;
    }
    let a = expected.as_bytes();
    let b = presented.as_bytes();
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Encode one message as `[u16 length][postcard body]`, appended to `out`.
///
/// Appends into a caller-owned buffer so the send path reuses one allocation
/// across messages instead of returning a fresh `Vec` each time. The body is
/// built in a temporary first because the length prefix cannot be written
/// until the encoded size is known, and because postcard's streaming writer
/// consumes the buffer it is given -- which would strand the caller's pending
/// bytes if encoding ever failed partway.
pub fn encode_frame<T: Serialize>(message: &T, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
    let body = postcard::to_allocvec(message).map_err(|_| ProtocolError::Malformed)?;
    if body.len() > MAX_FRAME {
        return Err(ProtocolError::FrameTooLarge { size: body.len() });
    }
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

/// Decode one message from a frame body (the bytes *after* the length prefix).
pub fn decode_frame<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtocolError> {
    postcard::from_bytes(body).map_err(|_| ProtocolError::Malformed)
}

/// Read a length prefix, rejecting anything oversized before a single byte of
/// body is buffered. Both transports call this before allocating.
pub fn frame_len(prefix: [u8; LENGTH_PREFIX]) -> Result<usize, ProtocolError> {
    let size = u16::from_be_bytes(prefix) as usize;
    if size > MAX_FRAME {
        return Err(ProtocolError::FrameTooLarge { size });
    }
    Ok(size)
}
