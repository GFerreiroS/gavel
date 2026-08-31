//! Chart-ready series, reduced once and sliced never.
//!
//! CLAUDE.md §16's Phase 6: "Fixed-resolution chart series for each named
//! window; SVG rendering may stay server-side, but series reduction does not
//! happen during the request." The item page used to call `downsample` on
//! every view -- a small reduction, but a reduction, and the one the phase
//! names.
//!
//! **Fixed resolution is the load-bearing word.** A series here has exactly
//! [`RESOLUTION`] slots whatever the window is, so a slot is a known fraction
//! of the window and the chart's horizontal really is time. That is the same
//! rule [`super::engine::Spark`] follows and for the same reason: the reader
//! takes the horizontal for time whether or not it is, and a series spaced by
//! observation draws a quiet fortnight and a busy afternoon at the same width.
//!
//! ## What a slot carries, and why each of it
//!
//! `docs/market-analysis.md` §6 asks the price panel for a rolling median with
//! a P25--P75 band around it, rather than a line through every observation.
//! The raw line answers "what was the price at 03:00 on Tuesday", which is a
//! question about one hour; the band answers "what has this been worth, and
//! how tightly", which is the question the page exists for. Both are here --
//! `price` is the observation, `median`/`p25`/`p75` are the rolling band --
//! because the raw line is what makes a spike visible and the band is what
//! makes it obviously a spike.
//!
//! `observed` is the other half of §2's rule about unavailable data. A slot
//! nothing was collected in is a *gap*, and the chart breaks its line there
//! rather than drawing straight through it. Interpolating would invent the
//! observation, and on a market that stopped being collected for a day it
//! would invent it in the most misleading possible place.

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

use super::Copper;
use super::engine::Buckets;

/// Slots in a stored chart series.
///
/// Fixed, so that a stored series has a known size whatever the window and
/// whatever the archive's age -- the property that makes this a bounded column
/// rather than one that grows with the history behind it. 96 is a little under
/// the horizontal pixels a chart has to draw them in at the width the page
/// gives it, so a finer series would be resolving detail the reader cannot see.
pub const RESOLUTION: usize = 96;

/// Slots the rolling band looks back over.
///
/// A fraction of the window rather than a fixed duration, because the band's
/// job is to be legible at whatever zoom the reader chose: a 30-day chart
/// wants a multi-day median, a 1-day chart wants an hourly one. Twelve of
/// ninety-six is an eighth of the window, which keeps roughly a dozen bands
/// across the chart -- enough to show a trend turning, not so many that the
/// band tracks every wobble and stops being a band.
pub const ROLLING_SLOTS: usize = 12;

/// One slot of a chart series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChartPoint {
    pub at: Millis,
    /// The observation in this slot. Meaningless when `observed` is false.
    pub price: Copper,
    /// The rolling band: the median of the trailing [`ROLLING_SLOTS`], and the
    /// quartiles around it. Computed by the same engine that decides every
    /// other percentile in the app.
    pub median: Copper,
    pub p25: Copper,
    pub p75: Copper,
    pub quantity: u64,
    /// Distinct auctions behind the quantity. Listed stock is not sales
    /// volume, and neither is this -- §15 -- but a price backed by one auction
    /// and a price backed by four hundred are different facts.
    pub listings: u32,
    /// Whether anything was collected in this slot. False is a gap, and a gap
    /// is drawn as a break rather than as a line through it.
    pub observed: bool,
}

/// One market's chart, at fixed resolution, over one window.
///
/// Carries the span it covers, so that decoding it needs nothing but the
/// stored string. The alternative was two more columns and a caller that had
/// to remember to pass them; a series that cannot say what interval it is of
/// is a series waiting to be drawn against the wrong axis.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChartSeries {
    pub from: Millis,
    pub until: Millis,
    pub points: Vec<ChartPoint>,
}

/// An observation going in: what the reducer needs and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub at: Millis,
    pub price: Copper,
    pub quantity: u64,
    pub listings: u32,
}

impl ChartSeries {
    /// Reduce observations to [`RESOLUTION`] slots across `[from, until]`.
    ///
    /// The last observation in a slot wins, which is the rule everywhere else
    /// in this crate: a slot describes the market's state at the end of it.
    pub fn over(
        observations: impl IntoIterator<Item = Observation>,
        from: Millis,
        until: Millis,
    ) -> ChartSeries {
        let span = until.get().saturating_sub(from.get());
        if span == 0 {
            return ChartSeries::default();
        }

        let mut slots: Vec<Option<Observation>> = vec![None; RESOLUTION];
        for observation in observations {
            if observation.at < from || observation.at > until {
                continue;
            }
            let offset = observation.at.get() - from.get();
            // The final instant belongs to the last slot rather than to a
            // ninety-seventh one that does not exist.
            let slot = (((offset * RESOLUTION as u64) / span) as usize).min(RESOLUTION - 1);
            match &mut slots[slot] {
                Some(held) if held.at > observation.at => {}
                held => *held = Some(observation),
            }
        }

        let points: Vec<ChartPoint> = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                // The slot's own instant, so that a gap still has a place on
                // the axis. Taking the observation's timestamp instead would
                // leave a gap with no time at all.
                let at = Millis(from.get() + (span * index as u64) / RESOLUTION as u64);

                // The rolling band, over the trailing slots that hold
                // something. A window's worth of leading gaps narrows the band
                // rather than shifting it: fewer observations behind it, which
                // is what the coverage panel is for saying out loud.
                let start = index.saturating_sub(ROLLING_SLOTS - 1);
                let trailing = Buckets::from_observations(
                    slots[start..=index]
                        .iter()
                        .flatten()
                        .map(|o| (o.at, o.price)),
                );

                match slot {
                    Some(observation) => ChartPoint {
                        at,
                        price: observation.price,
                        median: trailing.quantile(0.50).unwrap_or(observation.price),
                        p25: trailing.quantile(0.25).unwrap_or(observation.price),
                        p75: trailing.quantile(0.75).unwrap_or(observation.price),
                        quantity: observation.quantity,
                        listings: observation.listings,
                        observed: true,
                    },
                    // A gap keeps its place on the axis and claims nothing.
                    None => ChartPoint {
                        at,
                        ..ChartPoint::default()
                    },
                }
            })
            .collect();

        ChartSeries {
            from,
            until,
            points,
        }
    }

    /// Slots that hold an observation.
    pub fn observed(&self) -> usize {
        self.points.iter().filter(|p| p.observed).count()
    }

    /// Whether there is a line to draw. One point is not a line.
    pub fn is_empty(&self) -> bool {
        self.observed() < 2
    }

    /// The stored form: one record per slot, `;` between slots.
    ///
    /// A string rather than JSON for the reason [`super::engine::Spark`] gives
    /// -- this is a column on every window of every market, and the field
    /// names would be most of the bytes. A gap is an empty record, so a market
    /// nobody collected for a week costs a week of semicolons.
    ///
    /// A slot's `at` is not stored. It is `from + index * span / RESOLUTION`
    /// by construction, so the span goes in the header and ninety-six
    /// timestamps do not go in at all.
    pub fn encode(&self) -> String {
        if self.points.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(self.points.len() * 24);
        use std::fmt::Write;
        let _ = write!(out, "{},{}", self.from.get(), self.until.get());
        for point in &self.points {
            out.push(';');
            if !point.observed {
                continue;
            }
            let _ = write!(
                out,
                "{},{},{},{},{},{}",
                point.price.get(),
                point.median.get(),
                point.p25.get(),
                point.p75.get(),
                point.quantity,
                point.listings,
            );
        }
        out
    }

    /// Read back what [`Self::encode`] wrote, restoring each slot's instant
    /// from the span in the header.
    ///
    /// Forgiving in one direction only: a record this binary cannot parse
    /// becomes a gap, which draws nothing, rather than a zero, which would
    /// draw a market crashing to free.
    pub fn decode(raw: &str) -> ChartSeries {
        let mut parts = raw.split(';');
        let Some(header) = parts.next() else {
            return ChartSeries::default();
        };
        let mut bounds = header.split(',');
        let (Some(Ok(from)), Some(Ok(until))) = (
            bounds.next().map(str::parse::<u64>),
            bounds.next().map(str::parse::<u64>),
        ) else {
            return ChartSeries::default();
        };
        let (from, until) = (Millis(from), Millis(until));

        let slots: Vec<&str> = parts.collect();
        if slots.is_empty() {
            return ChartSeries::default();
        }
        let span = until.get().saturating_sub(from.get());
        let total = slots.len() as u64;
        let points = slots
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let at = Millis(from.get() + (span * index as u64) / total);
                let mut fields = record.split(',');
                let mut next = || fields.next().and_then(|f| f.parse::<u64>().ok());
                match (next(), next(), next(), next(), next(), next()) {
                    (
                        Some(price),
                        Some(median),
                        Some(p25),
                        Some(p75),
                        Some(quantity),
                        Some(listings),
                    ) => ChartPoint {
                        at,
                        price: Copper(price),
                        median: Copper(median),
                        p25: Copper(p25),
                        p75: Copper(p75),
                        quantity,
                        listings: listings as u32,
                        observed: true,
                    },
                    _ => ChartPoint {
                        at,
                        ..ChartPoint::default()
                    },
                }
            })
            .collect();
        ChartSeries {
            from,
            until,
            points,
        }
    }
}

/// Bins in a distribution histogram.
///
/// Odd, so one bin straddles the middle and the median has somewhere to sit
/// rather than falling on a boundary. Small enough that every bin is a
/// readable bar at a card's width.
pub const BINS: usize = 21;

/// How a market's prices were distributed across a window.
///
/// §5.4's panel: the shape the valuation band is a rank *inside*. A reader
/// shown "Cheap, P12" learns where today sits; shown the distribution as well,
/// they learn whether the market is tight or spread out -- which is the
/// difference between a band worth acting on and one that is noise.
///
/// Over the same equal-duration buckets every other historical statistic uses,
/// so a bar here counts hours and not observations.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Histogram {
    /// Cheapest and dearest bucket price, which are the axis.
    pub lo: Copper,
    pub hi: Copper,
    /// Hours in each bin, cheapest first.
    pub bins: Vec<u32>,
}

impl Histogram {
    pub fn of(buckets: &Buckets) -> Option<Histogram> {
        let prices = buckets.prices();
        let (lo, hi) = (*prices.first()?, *prices.last()?);
        let mut bins = vec![0u32; BINS];
        for price in prices {
            // A market that never moved lands wholly in the middle bin, which
            // is what it looks like: one bar, and the distribution is a point.
            let bin = if hi == lo {
                BINS / 2
            } else {
                (((price - lo) as u128 * (BINS as u128 - 1)) / (hi - lo) as u128) as usize
            };
            bins[bin.min(BINS - 1)] += 1;
        }
        Some(Histogram {
            lo: Copper(lo),
            hi: Copper(hi),
            bins,
        })
    }

    /// Which bin a price falls in, for marking "you are here". `None` when it
    /// is outside the range the histogram covers -- which is a real answer,
    /// and the one an anomaly produces.
    pub fn bin_of(&self, price: Copper) -> Option<usize> {
        let (lo, hi) = (self.lo.get(), self.hi.get());
        if price.get() < lo || price.get() > hi {
            return None;
        }
        if hi == lo {
            return Some(BINS / 2);
        }
        Some(
            ((((price.get() - lo) as u128 * (BINS as u128 - 1)) / (hi - lo) as u128) as usize)
                .min(BINS - 1),
        )
    }

    pub fn tallest(&self) -> u32 {
        self.bins.iter().copied().max().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.bins.iter().all(|count| *count == 0)
    }

    /// The stored form: `lo,hi,` then one count per bin.
    pub fn encode(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = format!("{},{}", self.lo.get(), self.hi.get());
        for count in &self.bins {
            out.push(',');
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{count}"));
        }
        out
    }

    pub fn decode(raw: &str) -> Option<Histogram> {
        let mut fields = raw.split(',');
        let lo = fields.next()?.parse().ok()?;
        let hi = fields.next()?.parse().ok()?;
        let bins: Vec<u32> = fields.map(|f| f.parse().unwrap_or(0)).collect();
        (bins.len() == BINS).then_some(Histogram {
            lo: Copper(lo),
            hi: Copper(hi),
            bins,
        })
    }
}
