//! Where a request's time went.
//!
//! Phase 0 of the market-analysis roadmap needs a change to be able to prove
//! it improved the read path, and "the page felt quicker" is not that proof.
//! What is needed is per-stage time -- database, cache, analysis, template --
//! for one request, on the real archive, in a release build.
//!
//! The awkward part is that the stages do not share a call frame. A category
//! page's database time is spent inside `storage`, several ports below the
//! handler that renders the template, and threading a timer parameter through
//! every port would put a measurement concern into the domain's signatures
//! for ever. So the accumulator is *ambient*: the request scope installs one,
//! anything under it that wants to be counted asks for it, and code that runs
//! outside a request finds nothing there and does nothing.
//!
//! Plain atomics and a hand-rolled scope, for the same reason
//! [`crate::metrics`] is plain atomics: the alternative is a tracing/metrics
//! framework in the domain crate, and this needs six counters.
//!
//! Statements are counted differently, and the reason is worth writing down
//! because it is not obvious and it was found the hard way. The SQLite driver
//! runs every statement on a thread of its own, one per pooled connection, and
//! reports it from there. That report therefore arrives on a thread which is
//! not the one serving the request, and no per-request context -- not this
//! one, not a task-local, not a tracing span -- reaches it. So statements are
//! counted into a process-wide [`DATABASE`] instead, and a request takes the
//! difference across itself. Served sequentially -- which is how a benchmark
//! asks -- that difference is exactly this request's work. Under concurrent
//! traffic it is the process's database work during this request, which is
//! still worth reading and is not the same claim.
//!
//! Nothing here is on unless [`crate::WebConfig::server_timing`] is, because
//! per-stage timings describe how the deployment is doing, which is
//! operations rather than product (CLAUDE.md §7).

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

/// The stages a request is broken into.
///
/// They are *not* a partition of the total: a stage may contain another. Cache
/// reads are database reads, and an analysis pass that reads history spends
/// most of its time in the database. Each entry answers "how long was spent
/// inside this kind of work", and the sum can exceed the total. That is
/// deliberate -- the alternative is subtracting nested time and reporting a
/// number that matches no clock anybody can check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Time inside the storage adapter, measured by the driver.
    Db,
    /// Time inside the cache port, which is also database time.
    Cache,
    /// Time reducing observations into statistics: what Phase 2 moves out of
    /// the request path entirely, so this is the number that must go to zero.
    Analysis,
    /// Time rendering templates.
    Template,
}

impl Stage {
    /// The `Server-Timing` metric name. Short, because it is on the wire.
    const fn key(self) -> &'static str {
        match self {
            Stage::Db => "db",
            Stage::Cache => "cache",
            Stage::Analysis => "calc",
            Stage::Template => "tpl",
        }
    }
}

/// One request's accounting. Shared, never copied.
#[derive(Debug, Default)]
pub struct Timings {
    db_micros: AtomicU64,
    cache_micros: AtomicU64,
    analysis_micros: AtomicU64,
    template_micros: AtomicU64,
    /// Statements executed. The count is the point: a page that asks 1316
    /// questions is slow in a way no single query's duration shows
    /// (CLAUDE.md §11b).
    queries: AtomicU64,
    /// Rows the driver decoded. Distinguishes "many queries" from "one query
    /// dragging the whole archive back".
    rows: AtomicU64,
}

impl Timings {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn slot(&self, stage: Stage) -> &AtomicU64 {
        match stage {
            Stage::Db => &self.db_micros,
            Stage::Cache => &self.cache_micros,
            Stage::Analysis => &self.analysis_micros,
            Stage::Template => &self.template_micros,
        }
    }

    pub fn record(&self, stage: Stage, micros: u64) {
        self.slot(stage).fetch_add(micros, Ordering::Relaxed);
    }

    /// Charge this request with the database work measured across it.
    pub fn absorb(&self, database: DatabaseWork) {
        self.queries
            .fetch_add(database.statements, Ordering::Relaxed);
        self.rows.fetch_add(database.rows, Ordering::Relaxed);
        self.db_micros.fetch_add(database.micros, Ordering::Relaxed);
    }

    pub fn micros(&self, stage: Stage) -> u64 {
        self.slot(stage).load(Ordering::Relaxed)
    }

    pub fn queries(&self) -> u64 {
        self.queries.load(Ordering::Relaxed)
    }

    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    /// The `Server-Timing` field value, with `total` last.
    ///
    /// Durations are milliseconds with three decimals, which is what the
    /// header's grammar wants and what a browser's network panel reads.
    /// Query and row counts ride along as zero-duration metrics with a
    /// description, because they are the counts §11b says to watch and there
    /// is no other header that would carry them.
    pub fn header(&self, total_micros: u64) -> String {
        let mut out = String::with_capacity(160);
        for stage in [Stage::Db, Stage::Cache, Stage::Analysis, Stage::Template] {
            let micros = self.micros(stage);
            push_metric(&mut out, stage.key(), micros);
        }
        push_count(&mut out, "q", self.queries());
        push_count(&mut out, "rows", self.rows());
        push_metric(&mut out, "total", total_micros);
        out
    }
}

fn separate(out: &mut String) {
    if !out.is_empty() {
        out.push_str(", ");
    }
}

fn push_metric(out: &mut String, key: &str, micros: u64) {
    separate(out);
    out.push_str(key);
    out.push_str(";dur=");
    out.push_str(&format_millis(micros));
}

fn push_count(out: &mut String, key: &str, count: u64) {
    separate(out);
    out.push_str(key);
    out.push_str(";desc=\"");
    out.push_str(&count.to_string());
    out.push('"');
}

/// Microseconds as milliseconds, three decimals, no floating point.
fn format_millis(micros: u64) -> String {
    format!("{}.{:03}", micros / 1_000, micros % 1_000)
}

/// Statements the process has run, and what they cost.
///
/// Process-wide because the driver reports from its own threads; see the
/// module comment. Fed by the composition root, which is the crate that knows
/// which database this is.
#[derive(Debug, Default)]
pub struct Database {
    statements: AtomicU64,
    rows: AtomicU64,
    micros: AtomicU64,
}

impl Database {
    /// One statement finished: how long it took, and the rows it decoded.
    pub fn observe(&self, micros: u64, rows: u64) {
        self.statements.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(rows, Ordering::Relaxed);
        self.micros.fetch_add(micros, Ordering::Relaxed);
    }

    pub fn read(&self) -> DatabaseWork {
        DatabaseWork {
            statements: self.statements.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            micros: self.micros.load(Ordering::Relaxed),
        }
    }
}

/// What the database did over some interval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatabaseWork {
    pub statements: u64,
    pub rows: u64,
    pub micros: u64,
}

impl DatabaseWork {
    /// The work done between two readings.
    ///
    /// Saturating, so two readings taken out of order report nothing rather
    /// than a request that ran nine quintillion statements.
    pub fn since(self, earlier: DatabaseWork) -> DatabaseWork {
        DatabaseWork {
            statements: self.statements.saturating_sub(earlier.statements),
            rows: self.rows.saturating_sub(earlier.rows),
            micros: self.micros.saturating_sub(earlier.micros),
        }
    }
}

/// The process's statement counters. Nothing writes to them unless the
/// composition root installed something that does.
pub static DATABASE: Database = Database {
    statements: AtomicU64::new(0),
    rows: AtomicU64::new(0),
    micros: AtomicU64::new(0),
};

thread_local! {
    /// The accounting for whatever request this thread is currently polling.
    ///
    /// A thread-local is the right shape here even though tasks move between
    /// threads: [`Scoped`] installs it around each `poll` and removes it
    /// afterwards, so it is set exactly while this request's future is running
    /// and never while another's is. Work `spawn`ed away from the request
    /// deliberately does not inherit it -- it is no longer this request's
    /// time.
    static CURRENT: RefCell<Option<Arc<Timings>>> = const { RefCell::new(None) };
}

/// The accounting for the request being polled, if any.
pub fn current() -> Option<Arc<Timings>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Start timing a stage. `None` when nothing is collecting, which is the
/// default and costs one thread-local read.
///
/// Hold the guard for the work; dropping it records the elapsed time.
#[must_use = "the stage is recorded when the guard is dropped"]
pub fn start(stage: Stage) -> Option<Guard> {
    current().map(|timings| Guard {
        timings,
        stage,
        at: Instant::now(),
    })
}

/// Records its stage's elapsed time when dropped -- including when dropped by
/// a `?` returning early, which is the case an explicit stop call gets wrong.
#[derive(Debug)]
pub struct Guard {
    timings: Arc<Timings>,
    stage: Stage,
    at: Instant,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.timings
            .record(self.stage, self.at.elapsed().as_micros() as u64);
    }
}

/// Run a future with `timings` installed as the ambient accounting.
pub fn scope<F: Future>(timings: Arc<Timings>, future: F) -> Scoped<F> {
    Scoped {
        timings,
        inner: Box::pin(future),
    }
}

/// A future polled with an ambient [`Timings`] installed.
///
/// The inner future is boxed so that the projection needed to poll it is safe
/// code; this crate forbids `unsafe`, and one allocation per request is not
/// worth an exception.
pub struct Scoped<F> {
    timings: Arc<Timings>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        // A guard rather than a straight-line restore: a panic inside the
        // handler must not leave this request's accounting installed on a
        // thread that goes on to serve somebody else's.
        let _installed = Installed::set(Arc::clone(&me.timings));
        me.inner.as_mut().poll(cx)
    }
}

struct Installed(Option<Arc<Timings>>);

impl Installed {
    fn set(timings: Arc<Timings>) -> Self {
        Installed(CURRENT.with(|slot| slot.borrow_mut().replace(timings)))
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|slot| *slot.borrow_mut() = previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_collected_outside_a_request() {
        assert!(current().is_none());
        assert!(start(Stage::Db).is_none());
    }

    #[tokio::test]
    async fn a_stage_is_recorded_when_its_guard_is_dropped() {
        let timings = Timings::new();
        scope(Arc::clone(&timings), async {
            {
                let _guard = start(Stage::Template);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(start(Stage::Db).is_some());
        })
        .await;

        assert!(timings.micros(Stage::Template) >= 2_000);
        assert_eq!(timings.micros(Stage::Analysis), 0);
    }

    /// The scope has to survive the `.await` points inside it, which is the
    /// whole reason it is a future wrapper rather than a plain guard.
    #[tokio::test]
    async fn the_scope_spans_await_points() {
        let timings = Timings::new();
        scope(Arc::clone(&timings), async {
            tokio::task::yield_now().await;
            current()
                .expect("still inside the request")
                .absorb(DatabaseWork {
                    statements: 1,
                    rows: 7,
                    micros: 500,
                });
        })
        .await;

        assert_eq!(timings.queries(), 1);
        assert_eq!(timings.rows(), 7);
        assert_eq!(timings.micros(Stage::Db), 500);
    }

    /// Leaving the scope has to leave the thread clean, or the next request
    /// polled on this thread inherits somebody else's accounting.
    #[tokio::test]
    async fn the_thread_is_left_clean() {
        let timings = Timings::new();
        scope(timings, async { tokio::task::yield_now().await }).await;
        assert!(current().is_none());
    }

    /// The difference across a request is what the middleware charges it
    /// with, and it has to survive a counter that never resets.
    #[test]
    fn database_work_is_the_difference_between_two_readings() {
        let database = Database::default();
        database.observe(1_000, 10);
        let before = database.read();
        database.observe(400, 3);
        database.observe(600, 4);

        let work = database.read().since(before);
        assert_eq!(
            work,
            DatabaseWork {
                statements: 2,
                rows: 7,
                micros: 1_000
            }
        );
    }

    /// Two readings the wrong way round is a bug, not a reason to report a
    /// request that ran nine quintillion statements.
    #[test]
    fn readings_out_of_order_report_nothing() {
        let later = DatabaseWork {
            statements: 5,
            rows: 5,
            micros: 5,
        };
        assert_eq!(
            DatabaseWork::default().since(later),
            DatabaseWork::default()
        );
    }

    #[test]
    fn the_header_reports_milliseconds_and_counts() {
        let timings = Timings::default();
        timings.record(Stage::Analysis, 12_345);
        timings.absorb(DatabaseWork {
            statements: 2,
            rows: 50,
            micros: 2_000,
        });

        let header = timings.header(80_000);
        assert_eq!(
            header,
            "db;dur=2.000, cache;dur=0.000, calc;dur=12.345, tpl;dur=0.000, \
             q;desc=\"2\", rows;desc=\"50\", total;dur=80.000"
        );
    }
}
