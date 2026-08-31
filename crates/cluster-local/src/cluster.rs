//! The handle the rest of the application holds.
//!
//! Every method is "send a command, await the reply". If the supervisor has
//! stopped, reads degrade to empty results and writes report
//! [`ClusterError::Unavailable`] -- the web layer must never panic because the
//! cluster is gone.

use cluster_core::{
    Clock, ClusterControl, ClusterError, ClusterSnapshot, ClusterStore, Elector, EventRecord, Job,
    JobDetail, JobId, JobSpec, LeastLoaded, LowestHealthyId, Node, NodeId, Role, Scheduler,
};
use core::pin::Pin;
use core::task::{Context, Poll};

use futures_core::Stream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;

use crate::clock::SystemClock;
use crate::config::LocalClusterConfig;
use crate::supervisor::{Command, Supervisor};

/// Cluster coordinator with optional in-process and remote workers.
///
/// Cheap to clone; every clone talks to the same supervisor task.
pub struct LocalCluster {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<EventRecord>,
}

impl Clone for LocalCluster {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            events: self.events.clone(),
        }
    }
}

impl LocalCluster {
    /// Start a cluster with the default scheduler and election policy.
    pub fn start<P: ClusterStore + Clone>(
        config: LocalClusterConfig,
        store: P,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        Self::start_with(config, store, LeastLoaded, LowestHealthyId, SystemClock)
    }

    /// Start a cluster with an explicit scheduler, elector and clock. Tests use
    /// this to pin behaviour; it is also the seam for trying a different
    /// placement policy without touching the runtime.
    pub fn start_with<P, S, L, C>(
        config: LocalClusterConfig,
        store: P,
        scheduler: S,
        elector: L,
        clock: C,
    ) -> (Self, tokio::task::JoinHandle<()>)
    where
        P: ClusterStore + Clone,
        S: Scheduler + 'static,
        L: Elector + 'static,
        C: Clock + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(config.event_buffer.max(16));
        let listen = config.node_listen;
        let join_token = config.join_token.clone();
        let artifacts_for_listener = config.artifacts.clone();
        // Durable writes are drained by their own task so the supervisor never
        // blocks on the store while there are messages to process.
        let (writer, _writer_task) = crate::persistence::Writer::spawn(store.clone());
        let supervisor = Supervisor::new(
            config,
            store,
            writer,
            scheduler,
            elector,
            clock,
            command_rx,
            event_tx.clone(),
        );
        let handle = tokio::spawn(supervisor.run());

        // Remote workers, if this deployment expects any. Bound before the
        // workers can connect and independent of the supervisor task, so a
        // worker that dials in during startup simply waits in the accept queue
        // rather than being refused.
        if let Some(address) = listen {
            let commands = command_tx.clone();
            let artifacts = artifacts_for_listener.clone();
            tokio::spawn(async move {
                match tokio::net::TcpListener::bind(address).await {
                    Ok(listener) => {
                        crate::remote::serve(listener, commands, join_token, artifacts).await
                    }
                    // Not fatal: the in-process part of the cluster still runs,
                    // and saying so beats exiting with a bind error on a port
                    // the user may not have meant to use.
                    Err(e) => tracing::error!(
                        %address, error = %e,
                        "could not listen for workers; only in-process ones can run"
                    ),
                }
            });
        }

        (
            Self {
                commands: command_tx,
                events: event_tx,
            },
            handle,
        )
    }

    /// Send one command and await its reply. `None` means the supervisor is
    /// gone.
    async fn ask<T>(&self, build: impl FnOnce(oneshot::Sender<T>) -> Command) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.commands.send(build(tx)).await.ok()?;
        rx.await.ok()
    }
}

fn unavailable() -> ClusterError {
    ClusterError::Unavailable("cluster supervisor is not running".into())
}

/// Adapts the broadcast channel to a plain `Stream`.
///
/// A consumer that falls behind is skipped forward rather than being allowed
/// to hold the buffer open: for a live view, the newest events matter and the
/// missed ones are already in the persisted event log.
pub struct EventStream(BroadcastStream<EventRecord>);

impl Stream for EventStream {
    type Item = EventRecord;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventRecord>> {
        loop {
            return match Pin::new(&mut self.0).poll_next(cx) {
                Poll::Ready(Some(Ok(record))) => Poll::Ready(Some(record)),
                // Lagged: drop the gap and keep going.
                Poll::Ready(Some(Err(_))) => continue,
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

impl ClusterControl for LocalCluster {
    type Events = EventStream;

    async fn snapshot(&self) -> ClusterSnapshot {
        self.ask(Command::Snapshot).await.unwrap_or_default()
    }

    fn subscribe(&self) -> EventStream {
        EventStream(BroadcastStream::new(self.events.subscribe()))
    }

    async fn nodes(&self) -> Vec<Node> {
        self.ask(Command::Nodes).await.unwrap_or_default()
    }

    async fn node(&self, id: NodeId) -> Option<Node> {
        self.ask(|reply| Command::Node(id, reply)).await.flatten()
    }

    async fn recent_events(&self, limit: usize) -> Vec<EventRecord> {
        self.ask(|reply| Command::Events(limit, reply))
            .await
            .unwrap_or_default()
    }

    async fn jobs(&self, limit: usize) -> Vec<Job> {
        self.ask(|reply| Command::Jobs(limit, reply))
            .await
            .unwrap_or_default()
    }

    async fn job(&self, id: JobId) -> Option<JobDetail> {
        self.ask(|reply| Command::Job(id, reply)).await.flatten()
    }

    async fn submit_job(&self, spec: JobSpec) -> Result<JobId, ClusterError> {
        self.ask(|reply| Command::SubmitJob(spec, reply))
            .await
            .unwrap_or_else(|| Err(unavailable()))
    }

    async fn set_role(&self, node: NodeId, role: Role, enabled: bool) -> Result<(), ClusterError> {
        self.ask(|reply| Command::SetRole {
            node,
            role,
            enabled,
            reply,
        })
        .await
        .unwrap_or_else(|| Err(unavailable()))
    }

    async fn stop_node(&self, node: NodeId) -> Result<(), ClusterError> {
        self.ask(|reply| Command::StopNode(node, reply))
            .await
            .unwrap_or_else(|| Err(unavailable()))
    }

    async fn start_node(&self, node: NodeId) -> Result<(), ClusterError> {
        self.ask(|reply| Command::StartNode(node, reply))
            .await
            .unwrap_or_else(|| Err(unavailable()))
    }

    async fn pause_heartbeat(&self, node: NodeId, paused: bool) -> Result<(), ClusterError> {
        self.ask(|reply| Command::PauseHeartbeat(node, paused, reply))
            .await
            .unwrap_or_else(|| Err(unavailable()))
    }

    async fn inject_failures(&self, node: NodeId, count: u32) -> Result<(), ClusterError> {
        self.ask(|reply| Command::InjectFailures(node, count, reply))
            .await
            .unwrap_or_else(|| Err(unavailable()))
    }

    async fn set_task_delay(&self, node: NodeId, millis: u64) -> Result<(), ClusterError> {
        self.ask(|reply| Command::SetTaskDelay(node, millis, reply))
            .await
            .unwrap_or_else(|| Err(unavailable()))
    }
}
