//! Telling the long-lived connections to let go.
//!
//! Graceful shutdown means "stop accepting, then wait for the requests already
//! in flight". That is exactly right for a request, and exactly wrong for a
//! response that never ends: the SSE stream at `/events/stream` is held open
//! for as long as the tab is, so **one open browser tab made Ctrl+C hang for
//! ever** and the only way out was closing the terminal.
//!
//! The stream is not in flight in any meaningful sense -- it is a connection
//! waiting for something to happen -- so it is the stream's job to notice the
//! server is going away and end. This is how it is told.
//!
//! A `watch` rather than a broadcast: every holder needs the *latest* state,
//! not every message, and a receiver that was created after the flag was set
//! must still see it. A stream that opens during shutdown ends immediately
//! rather than holding the door.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::watch;
use tokio_stream::Stream;

/// A handle the long-lived handlers hold, and wait on.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    pub fn new(receiver: watch::Receiver<bool>) -> Self {
        Self(receiver)
    }

    /// A `Shutdown` that never fires, for tests and for anything that builds a
    /// router without a process to stop.
    ///
    /// One sender for the whole program, kept alive here: dropping it would
    /// read as "the server is going away", which is the opposite of what this
    /// means.
    pub fn never() -> Self {
        static NEVER: std::sync::OnceLock<watch::Sender<bool>> = std::sync::OnceLock::new();
        Self(NEVER.get_or_init(|| watch::channel(false).0).subscribe())
    }

    /// Resolves when the server has begun shutting down.
    ///
    /// Also resolves if the sender is gone, because a process that dropped it
    /// is not a process that is still serving.
    pub async fn wait(mut self) {
        if *self.0.borrow_and_update() {
            return;
        }
        while self.0.changed().await.is_ok() {
            if *self.0.borrow() {
                return;
            }
        }
    }
}

/// A stream that ends when `shutdown` fires, whichever comes first.
///
/// Hand-written rather than `futures_util::StreamExt::take_until`, because
/// that is the only thing this crate would want from `futures-util` and
/// `tokio-stream` has no equivalent. Both halves are boxed so the whole thing
/// is `Unpin` and needs no projection -- this crate forbids `unsafe`, and one
/// allocation per SSE connection is nothing next to the connection.
pub fn until<T: 'static>(
    stream: impl Stream<Item = T> + Send + 'static,
    shutdown: Shutdown,
) -> impl Stream<Item = T> + Send + 'static {
    Until {
        stream: Box::pin(stream),
        shutdown: Box::pin(shutdown.wait()),
        stopped: false,
    }
}

struct Until<T> {
    stream: Pin<Box<dyn Stream<Item = T> + Send>>,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    stopped: bool,
}

impl<T> Stream for Until<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if self.stopped {
            return Poll::Ready(None);
        }
        // Shutdown first: once it has fired there is nothing worth waiting for
        // on the stream, and a pending item must not keep the connection open.
        if self.shutdown.as_mut().poll(cx).is_ready() {
            self.stopped = true;
            return Poll::Ready(None);
        }
        self.stream.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waiting_ends_when_the_flag_is_set() {
        let (tx, rx) = watch::channel(false);
        let waiting = tokio::spawn(Shutdown::new(rx).wait());
        assert!(!waiting.is_finished());
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("wait should end")
            .unwrap();
    }

    /// The case that would otherwise hold the door open: a stream that opens
    /// *after* the signal has already gone out.
    #[tokio::test]
    async fn a_late_holder_does_not_wait_at_all() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), Shutdown::new(rx).wait())
            .await
            .expect("a shutdown already begun is still a shutdown");
    }

    /// A sender that was dropped is a server that is not serving.
    #[tokio::test]
    async fn a_dropped_sender_counts_as_shutdown() {
        let (tx, rx) = watch::channel(false);
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(1), Shutdown::new(rx).wait())
            .await
            .expect("wait should end");
    }

    /// The whole point: a stream that would never end on its own does, and
    /// promptly.
    #[tokio::test]
    async fn a_stream_ends_when_the_server_does() {
        use tokio_stream::StreamExt;

        let (tx, rx) = watch::channel(false);
        // A stream with nothing on it and no end -- an idle SSE connection.
        let forever = futures_never();
        let mut stream = Box::pin(until(forever, Shutdown::new(rx)));

        let polled =
            tokio::time::timeout(std::time::Duration::from_millis(50), stream.next()).await;
        assert!(polled.is_err(), "nothing to send, so nothing yet");

        tx.send(true).unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("the stream must end when told");
        assert!(ended.is_none(), "ended, rather than yielding something");
    }

    /// And a stream that ends on its own still does.
    #[tokio::test]
    async fn a_finished_stream_is_not_held_open() {
        use tokio_stream::StreamExt;

        let items = tokio_stream::iter([1, 2, 3]);
        let collected: Vec<i32> = until(items, Shutdown::never()).collect().await;
        assert_eq!(collected, [1, 2, 3]);
    }

    /// A stream created after the signal ends at once rather than holding the
    /// door for the grace period.
    #[tokio::test]
    async fn a_late_stream_ends_immediately() {
        use tokio_stream::StreamExt;

        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let mut stream = Box::pin(until(futures_never(), Shutdown::new(rx)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
                .await
                .expect("must not wait")
                .is_none()
        );
    }

    /// A stream that yields nothing and never finishes: an idle SSE
    /// connection, which is what a browser tab holds.
    ///
    /// The first attempt built one from an `mpsc` receiver whose sender was
    /// dropped on the spot, which is a stream that has *already ended* -- the
    /// opposite of the case under test, and the assertion above caught it.
    fn futures_never() -> impl Stream<Item = i32> + Send + 'static {
        tokio_stream::pending()
    }

    #[tokio::test]
    async fn never_does_not_fire() {
        let waiting = tokio::spawn(Shutdown::never().wait());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
    }
}
