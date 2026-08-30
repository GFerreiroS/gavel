//! Counting statements without touching the storage adapter.
//!
//! CLAUDE.md §11b's first rule for a slow page is to count the queries: the
//! 1316 cache reads behind one page were "invisible in a profile and obvious
//! in a log". The log is where they still are -- SQLx emits one `tracing`
//! event per statement, carrying its elapsed time and the rows it decoded --
//! so this layer reads that stream and adds each statement to whichever
//! request is being served.
//!
//! The alternative was wrapping 63 call sites in the storage adapter, which
//! would have to be maintained by everyone who ever adds a query and would
//! still miss the ones sqlx issues on its own. This misses nothing, and the
//! storage crate does not learn that anybody is measuring it.
//!
//! What it cannot do is say *whose* statement it was. The SQLite driver runs
//! each statement on a thread of its own -- one per pooled connection -- and
//! reports it from there, so by the time this layer sees the event the request
//! that caused it is on another thread entirely. Hence the process-wide
//! counter and the difference the request middleware takes across itself; see
//! [`app_core::timing`] for what that difference does and does not claim.
//!
//! It lives in `server` because `server` is the crate that knows the database
//! is SQLx (CLAUDE.md §3); `app-core` only knows that something records
//! statements against the current request.

use app_core::timing;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, filter_fn};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// The target SQLx logs every statement under.
const SQLX_QUERY: &str = "sqlx::query";

/// Adds each statement to [`app_core::timing::DATABASE`].
///
/// Statements from collection and housekeeping land there too. They are the
/// process's database work, which is what the counter says it is; a request
/// only ever reports the difference across itself.
pub struct QueryTiming;

impl<S> Layer<S> for QueryTiming
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = Statement::default();
        event.record(&mut visitor);
        timing::DATABASE.observe(visitor.micros, visitor.rows);
    }
}

/// The two fields worth reading off a statement event.
///
/// `elapsed_secs` is what SQLx measured around the statement itself, so it
/// excludes waiting for a pooled connection. A page that is slow because it
/// queues behind the pool will show that as total time this does not account
/// for, which is the honest way round.
#[derive(Default)]
struct Statement {
    micros: u64,
    rows: u64,
}

impl Visit for Statement {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "rows_returned" {
            self.rows = value;
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "elapsed_secs" && value.is_finite() && value > 0.0 {
            self.micros = (value * 1_000_000.0) as u64;
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// The layer, filtered to statement events alone.
///
/// The filter is the layer's own rather than the process's: the fmt layer
/// keeps whatever `--log` asked for, so turning measurement on does not also
/// print every statement to the console. The max-level hint keeps the rest of
/// the process from having to build TRACE events nobody wants.
pub fn layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    QueryTiming.with_filter(
        filter_fn(|metadata| metadata.target() == SQLX_QUERY)
            .with_max_level_hint(LevelFilter::DEBUG),
    )
}
