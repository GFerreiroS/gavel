//! Deriving everything the item page shows from a series of observations.
//!
//! Pure and free of I/O: the whole module is a function from
//! `&[PriceSample]` to numbers. That keeps it testable without a database and
//! keeps the option open of running it on a node, where the alternative --
//! shipping thousands of rows to a browser to be reduced in JavaScript -- is
//! not available.

use std::collections::BTreeMap;

use cluster_core::Millis;

use crate::timing::{self, Stage};

use super::{Copper, PriceSample};

/// A price at a moment: what a chart plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub at: Millis,
    pub price: Copper,
    pub quantity: u64,
}

/// Change over a period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trend {
    pub from: Copper,
    pub to: Copper,
    /// Signed percentage; negative means it got cheaper.
    pub percent: i32,
    /// False when the window has no earlier observation to compare against.
    pub known: bool,
}

impl Trend {
    pub const UNKNOWN: Trend = Trend {
        from: Copper::ZERO,
        to: Copper::ZERO,
        percent: 0,
        known: false,
    };

    fn between(from: Copper, to: Copper) -> Trend {
        let percent = if from.get() == 0 {
            0
        } else {
            let delta = to.get() as i128 - from.get() as i128;
            ((delta * 100) / from.get() as i128) as i32
        };
        Trend {
            from,
            to,
            percent,
            known: true,
        }
    }
}

/// Average price bucketed by a repeating cycle -- hour of day, day of week.
///
/// The question it answers is "when should I buy", which is the one piece of
/// analysis the auction house itself will never show you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// Bucket index (0-23 for hours, 0-6 for days, Monday first).
    pub bucket: u8,
    pub mean: Copper,
    pub samples: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemAnalysis {
    pub current: Option<Point>,
    pub low: Option<Point>,
    pub high: Option<Point>,
    pub mean: Copper,
    /// Median is reported alongside the mean because a single spike drags the
    /// mean and not the median; a gap between them is itself informative.
    pub median: Copper,
    pub samples: usize,
    pub first_seen: Option<Millis>,
    pub day: Trend,
    pub week: Trend,
    pub month: Trend,
    /// (high - low) as a percentage of the mean.
    pub volatility_percent: u32,
    pub by_hour: Vec<Cycle>,
    pub by_weekday: Vec<Cycle>,
    /// Cheapest hour of day, when there is enough data to say.
    pub best_hour: Option<u8>,
    pub best_weekday: Option<u8>,
    pub series: Vec<Point>,
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Reduce a series of observations to everything the item page needs.
///
/// `samples` may arrive in any order; `now` anchors the trend windows.
pub fn analyse(samples: &[PriceSample], now: Millis) -> ItemAnalysis {
    // The whole reduction is charged here. The roadmap's Phase 2 exit gate is
    // that no handler reaches this function at all, so the honest way to watch
    // that happen is a stage that has to fall to zero rather than a stage that
    // has to get quicker.
    let _timing = timing::start(Stage::Analysis);
    let mut points: Vec<Point> = samples
        .iter()
        .map(|s| Point {
            at: s.observed_at,
            price: s.p05_unit_price,
            quantity: s.quantity,
        })
        .collect();
    points.sort_by_key(|p| p.at.get());

    if points.is_empty() {
        return ItemAnalysis {
            current: None,
            low: None,
            high: None,
            mean: Copper::ZERO,
            median: Copper::ZERO,
            samples: 0,
            first_seen: None,
            day: Trend::UNKNOWN,
            week: Trend::UNKNOWN,
            month: Trend::UNKNOWN,
            volatility_percent: 0,
            by_hour: Vec::new(),
            by_weekday: Vec::new(),
            best_hour: None,
            best_weekday: None,
            series: Vec::new(),
        };
    }

    let current = points.last().copied();
    let low = points.iter().min_by_key(|p| p.price.get()).copied();
    let high = points.iter().max_by_key(|p| p.price.get()).copied();

    let total: u128 = points.iter().map(|p| p.price.get() as u128).sum();
    let mean = Copper((total / points.len() as u128) as u64);

    let mut sorted: Vec<u64> = points.iter().map(|p| p.price.get()).collect();
    sorted.sort_unstable();
    let median = Copper(sorted[sorted.len() / 2]);

    let volatility_percent = match (low, high) {
        (Some(l), Some(h)) if mean.get() > 0 => {
            (((h.price.get() - l.price.get()) as u128 * 100) / mean.get() as u128) as u32
        }
        _ => 0,
    };

    ItemAnalysis {
        current,
        low,
        high,
        mean,
        median,
        samples: points.len(),
        first_seen: points.first().map(|p| p.at),
        day: trend_over(&points, now, DAY_MS),
        week: trend_over(&points, now, 7 * DAY_MS),
        month: trend_over(&points, now, 30 * DAY_MS),
        volatility_percent,
        by_hour: cycle(&points, 24, hour_of_day),
        by_weekday: cycle(&points, 7, weekday),
        best_hour: cheapest_bucket(&cycle(&points, 24, hour_of_day)),
        best_weekday: cheapest_bucket(&cycle(&points, 7, weekday)),
        series: points,
    }
}

/// Compare the newest price against the oldest one still inside the window.
fn trend_over(points: &[Point], now: Millis, window_ms: u64) -> Trend {
    let cutoff = now.get().saturating_sub(window_ms);
    let Some(newest) = points.last() else {
        return Trend::UNKNOWN;
    };
    match points.iter().find(|p| p.at.get() >= cutoff) {
        // Only meaningful if the reference point is genuinely older.
        Some(oldest) if oldest.at != newest.at => Trend::between(oldest.price, newest.price),
        _ => Trend::UNKNOWN,
    }
}

fn cycle(points: &[Point], buckets: u8, key: fn(Millis) -> u8) -> Vec<Cycle> {
    let mut totals: BTreeMap<u8, (u128, u32)> = BTreeMap::new();
    for point in points {
        let entry = totals.entry(key(point.at)).or_insert((0, 0));
        entry.0 += point.price.get() as u128;
        entry.1 += 1;
    }
    (0..buckets)
        .map(|bucket| match totals.get(&bucket) {
            Some((sum, count)) if *count > 0 => Cycle {
                bucket,
                mean: Copper((sum / *count as u128) as u64),
                samples: *count,
            },
            _ => Cycle {
                bucket,
                mean: Copper::ZERO,
                samples: 0,
            },
        })
        .collect()
}

/// The cheapest bucket, but only once every bucket has been observed a few
/// times -- otherwise it reports whichever hour happened to catch a dip.
fn cheapest_bucket(cycles: &[Cycle]) -> Option<u8> {
    const MIN_PER_BUCKET: u32 = 3;
    if cycles.iter().any(|c| c.samples < MIN_PER_BUCKET) {
        return None;
    }
    cycles
        .iter()
        .filter(|c| c.mean.get() > 0)
        .min_by_key(|c| c.mean.get())
        .map(|c| c.bucket)
}

fn hour_of_day(at: Millis) -> u8 {
    ((at.get() / (60 * 60 * 1000)) % 24) as u8
}

/// Monday = 0. 1970-01-01 was a Thursday, hence the offset.
fn weekday(at: Millis) -> u8 {
    (((at.get() / DAY_MS) + 3) % 7) as u8
}

pub const WEEKDAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Reduce a series to at most `target` points for plotting.
///
/// Buckets by position and keeps the cheapest point in each: a chart of "what
/// could I have paid" should preserve the dips, which averaging would erase.
pub fn downsample(points: &[Point], target: usize) -> Vec<Point> {
    if points.len() <= target || target == 0 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(target);
    for bucket in 0..target {
        let start = bucket * points.len() / target;
        let end = ((bucket + 1) * points.len() / target).max(start + 1);
        if let Some(cheapest) = points[start..end.min(points.len())]
            .iter()
            .min_by_key(|p| p.price.get())
        {
            out.push(*cheapest);
        }
    }
    out
}
