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

use core::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ids::{NodeId, TaskId};
use crate::job::{TaskOutcome, TaskSpec};
use crate::node::{Heartbeat, NodeCapabilities};

/// Bumped on any breaking change to the message types below. A worker whose
/// version does not match is rejected at `Hello` rather than being allowed to
/// half-work. Version 2 made `Hello.node` optional, so a worker can ask to be
/// given an identity instead of asserting one. Version 3 added the join token.
///
/// **Version 4 carries work that does not fit in a sentence.** A market
/// analysis partition is tens of kilobytes going out and over a hundred coming
/// back, measured on the real archive -- so the length prefix went from two
/// bytes to four (two could not address more than 64 KB whatever the cap said)
/// and the frame cap from 2 KB to [`MAX_FRAME`]. With it came an artifact
/// carrying its own length and integrity check, and a `TaskProduced` frame for
/// results that are bytes rather than a sentence.
pub const PROTOCOL_VERSION: u16 = 4;

/// Longest join token either side will send or compare.
///
/// The frame cap already bounds it; this keeps a token from crowding out the
/// rest of the `Hello` and makes "too long" a clear refusal rather than a
/// malformed frame.
pub const MAX_TOKEN: usize = 256;

/// Largest frame either side will encode or accept, in bytes.
///
/// **Sized from a measurement, and from the right half of it.** The partition
/// size was first chosen against the *input* -- 64 markets of price history,
/// ~81 KB -- and that was the smaller of the two things a partition puts on
/// the wire. A partition's *result* is 4.5 times its input: 64 markets came
/// back as 568,469 bytes, because Phase 6 gave every window a 96-slot chart
/// series and a histogram. Both numbers are on the real archive, with the
/// 515 real ladders from Phase 7 attached.
///
/// So the cap is 256 KiB and the partition is sized to fit inside it *both
/// ways*: at [`crate::MAX_ARTIFACT`] a worst-case result is 145,705 bytes and
/// a worst-case input 35,310. See `server::analysis_work` for the sweep.
///
/// The original argument for a small cap still holds and is the reason there
/// is a cap at all: an invalid peer must not be able to force an unbounded
/// allocation. A quarter of a megabyte is bounded. What it is *not* is small
/// enough to be free, so a `Heartbeat` still costs twenty-one bytes and only
/// an artifact frame ever approaches this.
pub const MAX_FRAME: usize = 256 * 1024;

/// Largest artifact either side will put in a frame, in bytes.
///
/// [`MAX_FRAME`] less an envelope, so that "will this fit" can be asked of the
/// bytes *before* they are encoded. Asking afterwards is the same question one
/// allocation too late, and it is asked on the path where the answer is no.
///
/// A kilobyte is far more envelope than a `TaskProduced` or an `Assign` needs
/// -- both are a task id, a digest and a length -- and being generous here
/// costs nothing that the partition size does not already have in hand.
pub const MAX_ARTIFACT: usize = MAX_FRAME - 1024;

/// Bytes of length prefix in front of every frame.
///
/// Four since version 4. Two could address 65,535 bytes and no more, so the
/// frame cap above was not a policy a two-byte prefix could have expressed --
/// raising the cap without widening the prefix would have been a cap that
/// silently truncated.
pub const LENGTH_PREFIX: usize = 4;

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
    /// Bytes this task produced, sent before its completion.
    ///
    /// Separate from `TaskFinished` because they are different things: a task
    /// row keeps an outcome and a sentence, and the analysis it produced
    /// belongs in the read model, staged and published by the coordinator
    /// (§15). Folding the artifact into the outcome would persist a partition
    /// of the read model inside the job history.
    TaskProduced {
        task: TaskId,
        artifact: Artifact,
    },
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireTaskSpec {
    Sleep {
        millis: u64,
    },
    Primes {
        start: u64,
        end: u64,
    },
    /// A materialisation partition, with the input it names.
    ///
    /// The *task* still references its input by `(version, algorithm,
    /// partition)` -- that triple is the idempotency key and it is what a
    /// result is filed under. The artifact rides with the assignment because a
    /// worker has no database to fetch it from, which is the whole point of
    /// §15's "workers receive immutable, bounded inputs".
    Analysis {
        version: u64,
        algorithm: u32,
        partition: u32,
        input: Artifact,
    },
}

/// Bytes a task needs, or produced, with a length and an integrity check.
///
/// The integrity check is [`digest`] -- FNV-1a, sixty-four bits. **It is not a
/// security control and must not be read as one**: anyone who can rewrite the
/// bytes can rewrite the digest beside them, and what keeps a stranger off
/// this socket is the join token and a private network (§10). What it catches
/// is the thing that actually happens: a frame reassembled wrongly, a
/// truncated read, an encoder and a decoder that disagree about a type. Those
/// fail loudly here instead of becoming a market with plausible numbers in it.
///
/// A hand-rolled hash rather than a crate because `cluster-core` depends on
/// serde, thiserror, postcard and futures-core and nothing else (§3), and an
/// integrity check is not worth breaking that for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub bytes: Vec<u8>,
    pub digest: u64,
}

impl Artifact {
    pub fn new(bytes: Vec<u8>) -> Artifact {
        let digest = digest(&bytes);
        Artifact { bytes, digest }
    }

    /// The bytes, if they are the bytes that were sent.
    pub fn verify(&self) -> Option<&[u8]> {
        (digest(&self.bytes) == self.digest).then_some(&self.bytes[..])
    }
}

/// FNV-1a, 64-bit. Small, dependency-free, and adequate for detecting
/// corruption; see [`Artifact`] for what it is not for.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// A task that cannot be sent, and which of the two reasons it is.
///
/// Both end the same way and that is the point: the transport reports a failed
/// attempt, the scheduler requeues, and the task lands on a worker that can
/// take it -- which today means an in-process one. A refusal here costs the
/// attempt; sending a frame nobody can decode costs the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotShippable {
    pub kind: &'static str,
    pub why: Unshippable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unshippable {
    /// The input it references has gone: the candidate was abandoned while the
    /// task sat in a queue. Refusing here is cheaper than sending 35 KB for a
    /// result that would be dropped on arrival.
    InputGone,
    /// The input is larger than a frame can carry.
    ///
    /// Unreachable while the partition size and [`MAX_ARTIFACT`] are the pair
    /// they are measured to be -- and it is here because that pair is a
    /// measurement rather than a law. A window added to the analysis moves the
    /// number; this is what makes that a requeue onto a local worker instead
    /// of a dropped socket.
    TooLarge { bytes: usize },
}

impl fmt::Display for NotShippable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            Unshippable::InputGone => write!(
                f,
                "a {} task cannot be sent: the input it references is no longer registered, \
                 so the candidate it belongs to has been abandoned",
                self.kind
            ),
            Unshippable::TooLarge { bytes } => write!(
                f,
                "a {} task cannot be sent: its input is {bytes} bytes and a frame carries \
                 at most {MAX_ARTIFACT}",
                self.kind
            ),
        }
    }
}

impl WireTaskSpec {
    /// Put a task on the wire, fetching whatever input it references.
    ///
    /// Fallible for one reason: a task whose input has gone -- the candidate
    /// was abandoned while this sat in a queue -- must not be sent. Stopping
    /// here is cheaper than sending 81 KB for a result that would be dropped
    /// on arrival.
    pub fn of(spec: TaskSpec, input: Option<Vec<u8>>) -> Result<WireTaskSpec, NotShippable> {
        match spec {
            TaskSpec::Sleep { millis } => Ok(WireTaskSpec::Sleep { millis }),
            TaskSpec::Primes { start, end } => Ok(WireTaskSpec::Primes { start, end }),
            TaskSpec::Analysis {
                version,
                algorithm,
                partition,
            } => match input {
                Some(bytes) if bytes.len() > MAX_ARTIFACT => Err(NotShippable {
                    kind: "analysis",
                    why: Unshippable::TooLarge { bytes: bytes.len() },
                }),
                Some(bytes) => Ok(WireTaskSpec::Analysis {
                    version,
                    algorithm,
                    partition,
                    input: Artifact::new(bytes),
                }),
                None => Err(NotShippable {
                    kind: "analysis",
                    why: Unshippable::InputGone,
                }),
            },
        }
    }

    /// The input this spec carries, if it carries one and it survived the
    /// journey. `None` from a corrupt artifact, which the worker then reports
    /// as a failure rather than computing from damaged bytes.
    pub fn input(&self) -> Option<&[u8]> {
        match self {
            WireTaskSpec::Analysis { input, .. } => input.verify(),
            _ => Some(&[]),
        }
    }
}

impl From<&WireTaskSpec> for TaskSpec {
    fn from(spec: &WireTaskSpec) -> Self {
        match *spec {
            WireTaskSpec::Sleep { millis } => TaskSpec::Sleep { millis },
            WireTaskSpec::Primes { start, end } => TaskSpec::Primes { start, end },
            WireTaskSpec::Analysis {
                version,
                algorithm,
                partition,
                ..
            } => TaskSpec::Analysis {
                version,
                algorithm,
                partition,
            },
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
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
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
    let size = u32::from_be_bytes(prefix) as usize;
    if size > MAX_FRAME {
        return Err(ProtocolError::FrameTooLarge { size });
    }
    Ok(size)
}
