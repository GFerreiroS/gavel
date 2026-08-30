//! Turning a history into the rows a page reads.
//!
//! This is the write path CLAUDE.md §15 draws: collection persists
//! observations, this reduces them, and the result is published as a version
//! an HTTP request only ever reads. Phase 2's exit condition is that no
//! handler calls [`super::analyse`] or scans a history -- so this is where
//! that call moved to, not a second implementation of it.
//!
//! **The arithmetic is deliberately unchanged.** Everything here is either
//! [`super::analyse`] or the same reduction `PriceRepository::window_stats`
//! performs in SQL. Phase 2 moves *where* the calculation happens; Phase 5 is
//! where the definitions change. Keeping those apart is what makes
//! `crates/app-core/tests/characterization.rs` mean something: if a number
//! moves in this phase, it moved by accident.

use std::collections::BTreeSet;

use cluster_core::Millis;

use super::analysis::{self, Cycle, Point, Trend};
use super::catalog::Catalog;
use super::key::MarketKey;
use super::window::Window;
use super::{Copper, PriceSample};

/// Bumped whenever a definition here changes, so a stored row can say which
/// rules produced it and a rebuild can be told apart from a re-read.
///
/// Not the same as a catalogue version: that says what the market *was*, this
/// says how it was measured.
pub const ALGORITHM_VERSION: u32 = 1;

/// Points kept in a stored chart series.
///
/// The chart is drawn from this rather than from the history, so the reduction
/// happens once per collection instead of once per view. It is also the number
/// that stops a market with four months of hourly observations from putting
/// three thousand points into an SVG.
pub const CHART_POINTS: usize = 120;

/// One market's current state and its whole summary.
///
/// Everything the analysis page's header and charts show, and everything a
/// card needs beyond its window comparison. One row, so a page that draws six
/// hundred cards reads six hundred rows rather than reducing six hundred
/// histories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketState {
    pub key: MarketKey,
    /// The newest observation. `None` for a market that has never been seen,
    /// which is a market with a catalogue entry and no listings.
    pub observed_at: Option<Millis>,
    /// The price a buyer acts on. Supply-weighted P5 for a commodity, which is
    /// the distinction §5.1 insists is not a historical percentile.
    pub price: Copper,
    pub min_price: Copper,
    pub median_price: Copper,
    pub quantity: u64,
    pub listings: u32,

    pub first_seen: Option<Millis>,
    pub samples: u32,
    pub mean: Copper,
    pub median: Copper,
    pub low: Copper,
    pub low_at: Millis,
    pub high: Copper,
    pub high_at: Millis,
    /// `(high - low) / mean`. §5.4 renames this Swing and replaces it with a
    /// robust measure in Phase 5; it is carried unchanged so that the
    /// replacement is visibly a replacement.
    pub volatility_percent: u32,

    pub day: Trend,
    pub week: Trend,
    pub month: Trend,

    pub by_hour: Vec<Cycle>,
    pub by_weekday: Vec<Cycle>,
    pub best_hour: Option<u8>,
    pub best_weekday: Option<u8>,

    /// Chart-ready and already thinned.
    pub series: Vec<Point>,
}

impl MarketState {
    /// A market nothing has been recorded for.
    ///
    /// A tracked item with no listings is a real answer -- §2's "rendered as
    /// unavailable" -- and a page needs one shape whether or not the read
    /// model has a row for it, rather than two branches per figure.
    pub fn empty(key: MarketKey) -> MarketState {
        MarketState {
            key,
            observed_at: None,
            price: Copper::ZERO,
            min_price: Copper::ZERO,
            median_price: Copper::ZERO,
            quantity: 0,
            listings: 0,
            first_seen: None,
            samples: 0,
            mean: Copper::ZERO,
            median: Copper::ZERO,
            low: Copper::ZERO,
            low_at: Millis::ZERO,
            high: Copper::ZERO,
            high_at: Millis::ZERO,
            volatility_percent: 0,
            day: Trend::UNKNOWN,
            week: Trend::UNKNOWN,
            month: Trend::UNKNOWN,
            by_hour: Vec::new(),
            by_weekday: Vec::new(),
            best_hour: None,
            best_weekday: None,
            series: Vec::new(),
        }
    }

    /// Whether anything has ever been recorded for this market.
    pub fn has_data(&self) -> bool {
        self.samples > 0
    }
}

/// What a card needs, and nothing else.
///
/// `docs/market-analysis.md` lists "category-card facts" as their own storage
/// responsibility, separate from the full state, and the reason turns out to
/// be measurable: a category page draws 515 cards and no charts, but reading
/// [`MarketState`] for each of them drags 515 stored chart series across --
/// megabytes of JSON to render a number and a quantity. Half the remaining
/// database time on a card page was that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketSummary {
    pub key: MarketKey,
    /// When this market was last seen. Freshness is a fact the page is
    /// entitled to, not an implementation detail (§15).
    pub observed_at: Option<Millis>,
    /// The price a buyer acts on.
    pub price: Copper,
    /// The cheapest single listing, which is what a watchlist row shows.
    pub min_price: Copper,
    pub quantity: u64,
    pub listings: u32,
    pub samples: u32,
}

impl MarketSummary {
    pub fn has_data(&self) -> bool {
        self.samples > 0
    }
}

/// One market over one interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketWindow {
    pub key: MarketKey,
    pub window: Window,
    pub low: Copper,
    pub low_at: Millis,
    pub high: Copper,
    pub high_at: Millis,
    pub mean: Copper,
    pub median: Copper,
    pub samples: u32,
    pub first_at: Millis,
    pub last_at: Millis,
    /// How many hourly observations a complete window would hold. `None` where
    /// the window has no datable start -- there is nothing to be a fraction of.
    pub expected_buckets: Option<u32>,
    /// How many distinct hours actually hold one.
    pub observed_buckets: u32,
    /// The longest run with no observation, between the first and the last one
    /// inside the window. Deliberately not measured from the window's edges: a
    /// market that started trading yesterday has not got a month-long gap, it
    /// has a month of not existing, and §2 does not let those be the same
    /// number.
    pub largest_gap_ms: u64,
}

impl MarketWindow {
    /// What fraction of the window was observed, as a percentage.
    ///
    /// `None` where there is nothing to be a fraction of, which is the honest
    /// answer and not zero.
    pub fn coverage_percent(&self) -> Option<u32> {
        let expected = self.expected_buckets?;
        if expected == 0 {
            return None;
        }
        Some(((self.observed_buckets as u64 * 100) / expected as u64).min(100) as u32)
    }
}

/// Everything one market's history produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialised {
    pub state: MarketState,
    pub windows: Vec<MarketWindow>,
}

/// Reduce one commodity market.
///
/// `history` is every observation of this market, in any order. `windows` is
/// what to summarise it over -- normally [`Window::all_for`], but a caller
/// rebuilding one page's worth may pass fewer.
pub fn commodity(
    key: MarketKey,
    history: &[PriceSample],
    catalog: &Catalog,
    windows: &[Window],
    now: Millis,
) -> Materialised {
    let analysis = analysis::analyse(history, now);
    let newest = history.iter().max_by_key(|s| s.observed_at);

    let state = MarketState {
        key,
        observed_at: newest.map(|s| s.observed_at),
        // The executable price is the supply-weighted P5, which is what the
        // card and the alert already act on.
        price: newest.map(|s| s.p05_unit_price).unwrap_or(Copper::ZERO),
        min_price: newest.map(|s| s.min_unit_price).unwrap_or(Copper::ZERO),
        median_price: newest.map(|s| s.median_unit_price).unwrap_or(Copper::ZERO),
        quantity: newest.map(|s| s.quantity).unwrap_or(0),
        listings: newest.map(|s| s.listings).unwrap_or(0),

        first_seen: analysis.first_seen,
        samples: analysis.samples as u32,
        mean: analysis.mean,
        median: analysis.median,
        low: analysis.low.map(|p| p.price).unwrap_or(Copper::ZERO),
        low_at: analysis.low.map(|p| p.at).unwrap_or(Millis::ZERO),
        high: analysis.high.map(|p| p.price).unwrap_or(Copper::ZERO),
        high_at: analysis.high.map(|p| p.at).unwrap_or(Millis::ZERO),
        volatility_percent: analysis.volatility_percent,

        day: analysis.day,
        week: analysis.week,
        month: analysis.month,

        by_hour: analysis.by_hour,
        by_weekday: analysis.by_weekday,
        best_hour: analysis.best_hour,
        best_weekday: analysis.best_weekday,

        series: analysis::downsample(&analysis.series, CHART_POINTS),
    };

    let windows = windows
        .iter()
        .filter_map(|window| summarise(key, window, history, catalog, now))
        .collect();

    Materialised { state, windows }
}

/// One market over one window, or `None` when the window holds nothing.
///
/// A window with no observations gets no row rather than a row of zeroes:
/// §2's rule is that an unavailable fact is rendered unavailable, and a stored
/// zero is a price somebody will eventually plot.
fn summarise(
    key: MarketKey,
    window: &Window,
    history: &[PriceSample],
    catalog: &Catalog,
    now: Millis,
) -> Option<MarketWindow> {
    let (from, until) = window.bounds(catalog, now)?;
    let end = until.map(|u| u.get()).unwrap_or(u64::MAX);

    let mut inside: Vec<&PriceSample> = history
        .iter()
        .filter(|s| s.observed_at >= from && s.observed_at.get() < end)
        .collect();
    if inside.is_empty() {
        return None;
    }
    inside.sort_by_key(|s| s.observed_at);

    // The same measure `window_stats` reduces in SQL: the supply-weighted P5,
    // not the bare minimum. Changing which column this reads would change
    // every card's comparison, which is Phase 5's business and not this one's.
    let prices: Vec<u64> = inside.iter().map(|s| s.p05_unit_price.get()).collect();

    // The earliest observation at the extreme, where SQL's
    // bare-column-with-MIN picked arbitrarily among ties. A tightening rather
    // than a change: the price was always the same, only the timestamp beside
    // it could vary between runs.
    //
    // `inside` is already in time order, so the first match is the earliest.
    let cheapest = prices.iter().copied().min().expect("non-empty");
    let dearest = prices.iter().copied().max().expect("non-empty");
    let low = inside
        .iter()
        .find(|s| s.p05_unit_price.get() == cheapest)
        .expect("the minimum came from this slice");
    let high = inside
        .iter()
        .find(|s| s.p05_unit_price.get() == dearest)
        .expect("the maximum came from this slice");

    let total: u128 = prices.iter().map(|p| *p as u128).sum();
    let mean = Copper((total / prices.len() as u128) as u64);

    let mut sorted = prices.clone();
    sorted.sort_unstable();
    let median = Copper(sorted[sorted.len() / 2]);

    let hours: BTreeSet<u64> = inside
        .iter()
        .map(|s| s.observed_at.get() / (60 * 60 * 1000))
        .collect();

    let largest_gap_ms = inside
        .windows(2)
        .map(|pair| {
            pair[1]
                .observed_at
                .get()
                .saturating_sub(pair[0].observed_at.get())
        })
        .max()
        .unwrap_or(0);

    Some(MarketWindow {
        key,
        window: window.clone(),
        low: low.p05_unit_price,
        low_at: low.observed_at,
        high: high.p05_unit_price,
        high_at: high.observed_at,
        mean,
        median,
        samples: inside.len() as u32,
        first_at: inside[0].observed_at,
        last_at: inside[inside.len() - 1].observed_at,
        expected_buckets: window.expected_buckets(catalog, now),
        observed_buckets: hours.len() as u32,
        largest_gap_ms,
    })
}
