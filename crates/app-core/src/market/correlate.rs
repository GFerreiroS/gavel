//! How a market moves, and what it moves with.
//!
//! CLAUDE.md §16's Phase 8. Everything here describes a *relationship* rather
//! than a level, and relationships are where a market-analysis product most
//! easily starts lying -- so the constraints come first.
//!
//! ## Three rules, and each is load-bearing
//!
//! **Nothing here is causation, and the wording is part of the code.** §16:
//! "wording is `associated with` or `observed after`". A price that fell after
//! a raid opened fell *after* a raid opened; whether the raid did it is not
//! something a rank correlation can know, and a page that said "because" would
//! be inventing a mechanism. [`Association::wording`] is the one place that
//! phrasing lives, so it cannot drift into a claim in one template.
//!
//! **Listed stock is not sales volume.** §15's rule, and it bites hardest here
//! because a "price versus volume" correlation is exactly what a reader will
//! assume they are looking at. What is correlated is price against *what is
//! listed*, which is supply on a shelf. [`Association::of`] takes stock under
//! that name and the panel says it out loud.
//!
//! **Evidence gates apply to a relationship as much as to a percentile.** A
//! correlation over six observations is a shape in noise. Every measure here
//! returns `None` below its gate rather than a number with a caveat beside it,
//! for the reason §5.3 gives about bands: a caveat is read past, and an absent
//! figure is not.
//!
//! ## Why rank correlation rather than Pearson
//!
//! Auction prices are heavy-tailed and occasionally absurd -- one seller at a
//! hundred times the market, one at a copper. Pearson's r is a statement about
//! a linear relationship between *levels*, and a single such listing moves it
//! bodily. Spearman's rho asks only whether the two move together in order,
//! which survives an outlier the way the median survives one and the mean does
//! not. It is the same argument §5.4 makes for IQR over range, applied to a
//! relationship instead of to a spread.

use std::collections::BTreeMap;

use cluster_core::Millis;

use super::Copper;
use super::engine::Buckets;

/// Paired observations needed before a rank correlation is reported.
///
/// Twenty, which is where a |rho| of about 0.45 clears the conventional 5%
/// two-sided threshold. Below that the coefficient is mostly a statement about
/// how few points there are. A round number chosen from a table rather than a
/// measurement -- there is no archive of *correlations* to calibrate against,
/// and pretending otherwise would be the mistake §16's Phase 5 already made
/// once with the evidence gates.
pub const MIN_PAIRS: usize = 20;

/// How strongly two series move together, in rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Association {
    /// Spearman's rho, scaled to -100..=100 so it stores and renders as an
    /// integer. Negative means one rises as the other falls.
    pub rho_percent: i32,
    /// Pairs behind it, so the reader can see what it is a correlation *of*.
    pub pairs: u32,
}

/// What an association is strong enough to be worth saying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    /// Below the point where the direction means anything.
    None,
    Weak,
    Moderate,
    Strong,
}

impl Association {
    /// Spearman's rho between two series, paired by position.
    ///
    /// `None` below [`MIN_PAIRS`], or where either series never moves -- a
    /// constant has no ranks to correlate, and dividing by its zero variance
    /// would produce a confident number about nothing.
    pub fn of(a: &[u64], b: &[u64]) -> Option<Association> {
        let n = a.len().min(b.len());
        if n < MIN_PAIRS {
            return None;
        }
        let (ra, rb) = (ranks(&a[..n]), ranks(&b[..n]));

        // Pearson over the ranks, which is what Spearman is. Computed the long
        // way rather than with the `1 - 6Σd²/n(n²-1)` shortcut, because that
        // form is only correct when there are no ties -- and a market that sat
        // at one price for six hours is nothing but ties.
        let mean = |xs: &[f64]| xs.iter().sum::<f64>() / xs.len() as f64;
        let (ma, mb) = (mean(&ra), mean(&rb));
        let mut cov = 0.0;
        let mut va = 0.0;
        let mut vb = 0.0;
        for i in 0..n {
            let (da, db) = (ra[i] - ma, rb[i] - mb);
            cov += da * db;
            va += da * da;
            vb += db * db;
        }
        if va == 0.0 || vb == 0.0 {
            return None;
        }
        let rho = cov / (va.sqrt() * vb.sqrt());
        Some(Association {
            rho_percent: (rho * 100.0).round().clamp(-100.0, 100.0) as i32,
            pairs: n as u32,
        })
    }

    pub fn strength(&self) -> Strength {
        match self.rho_percent.abs() {
            0..=19 => Strength::None,
            20..=39 => Strength::Weak,
            40..=69 => Strength::Moderate,
            _ => Strength::Strong,
        }
    }

    /// The sentence this is allowed to be put in.
    ///
    /// §16: `associated with`, never `causes`. Kept here rather than in a
    /// template so that there is one wording to review and no second one to
    /// drift into a claim. A relationship too weak to describe says so instead
    /// of being described weakly.
    pub const fn wording(&self) -> &'static str {
        match (self.rho_percent < 0, self.rho_percent.abs()) {
            (_, 0..=19) => "No association in this window",
            (true, _) => "Higher prices associated with lower stock",
            (false, _) => "Higher prices associated with more stock",
        }
    }
}

/// Ranks, with ties sharing their average rank.
///
/// The tie handling is the same idea as [`super::engine::Buckets::rank_of`]'s
/// mid-rank, and it is needed for the same reason: a market that did not move
/// for six hours is six tied values, and giving them different ranks would
/// invent an ordering the data does not have.
fn ranks(values: &[u64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by_key(|i| values[*i]);

    let mut out = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && values[order[j + 1]] == values[order[i]] {
            j += 1;
        }
        // Average rank across the tied run, 1-based.
        let rank = (i + j) as f64 / 2.0 + 1.0;
        for slot in &order[i..=j] {
            out[*slot] = rank;
        }
        i = j + 1;
    }
    out
}

/// The worst fall and the best rise a market made inside a window.
///
/// Peak-to-trough and trough-to-peak, in order -- not simply the high and the
/// low. A market that opened cheap, rose and fell back has a drawdown; one
/// that fell then rose has a rise; the extremes alone cannot tell those apart,
/// and "how far did this drop from its own peak" is the question somebody
/// holding stock is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Swings {
    /// Largest fall from a running peak, as a percentage of that peak.
    pub drawdown_percent: u32,
    /// Largest rise from a running trough, as a percentage of that trough.
    pub rise_percent: u32,
}

impl Swings {
    /// `prices` in time order. Order is the whole point: this is not a
    /// statistic of the set, it is a statistic of the *path*.
    ///
    /// The two are tracked independently, and an earlier version that had a
    /// new extreme on one side reset the other got it wrong: on
    /// `100, 140, 200, 180, 150` it reported a 16% drawdown instead of 25%,
    /// because reaching a new trough at 180 threw away the peak of 200 that
    /// the fall should be measured from. A drawdown is measured from the
    /// highest price *so far*, full stop.
    pub fn of(prices: &[u64]) -> Swings {
        let mut peak = 0u64;
        let mut trough = u64::MAX;
        let mut swings = Swings::default();
        for price in prices.iter().copied().filter(|p| *p > 0) {
            if price < peak {
                let fall = ((peak - price) as u128 * 100 / peak as u128) as u32;
                swings.drawdown_percent = swings.drawdown_percent.max(fall);
            }
            if price > trough {
                let rise = ((price - trough) as u128 * 100 / trough as u128) as u32;
                swings.rise_percent = swings.rise_percent.max(rise);
            }
            peak = peak.max(price);
            trough = trough.min(price);
        }
        swings
    }
}

/// How much a market moves from one observation to the next, robustly.
///
/// The median absolute *change*, as a percentage -- not the spread of its
/// levels. A market that drifts steadily from 100 to 200 has a wide spread and
/// is perfectly calm; one that alternates 140/160 every hour has a narrow
/// spread and is not. §5.4 asks for a robust stability measure, and this is
/// the one that answers "is this a market I can act on tomorrow".
///
/// `None` below [`MIN_PAIRS`] changes: a volatility from four observations is
/// a statement about four observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stability {
    /// Median absolute change between consecutive observations, as a
    /// percentage of the earlier one.
    pub typical_move_percent: u32,
    pub changes: u32,
}

impl Stability {
    pub fn of(prices: &[u64]) -> Option<Stability> {
        let changes: Vec<u64> = prices
            .windows(2)
            .filter(|pair| pair[0] > 0)
            .map(|pair| (pair[0].abs_diff(pair[1]) as u128 * 100 / pair[0] as u128) as u64)
            .collect();
        if changes.len() < MIN_PAIRS {
            return None;
        }
        // The engine's median, over the changes rather than over the levels.
        // One estimator, wherever a median is taken.
        let buckets = Buckets::from_observations(
            changes
                .iter()
                .enumerate()
                .map(|(i, change)| (Millis(i as u64 * 3_600_000), Copper(*change))),
        );
        Some(Stability {
            typical_move_percent: buckets.quantile(0.50)?.get() as u32,
            changes: changes.len() as u32,
        })
    }
}

/// Cells needed in an hour-by-weekday grid before it is shown at all.
///
/// The grid has 168 cells. Below a third of them filled it is mostly holes,
/// and a reader looking at a mostly-empty heatmap reads the holes as cheapness
/// -- the one failure mode this panel has.
pub const MIN_HEATMAP_CELLS: usize = 56;

/// Median price by hour of the week: 7 weekdays by 24 hours.
///
/// §16 asks for the two separate cycle charts to become one grid, and the
/// reason is that they cannot answer the question together. "Cheapest at 04:00"
/// and "cheapest on Tuesday" do not compose into "cheapest at 04:00 on
/// Tuesday" -- the whole point of a weekly market rhythm is that the hour and
/// the day interact, because a reset happens at one hour on one day.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Heatmap {
    /// 168 cells, weekday-major: `cells[weekday * 24 + hour]`. `None` where
    /// that hour of the week was never observed -- a hole, drawn as a hole.
    pub cells: Vec<Option<Copper>>,
    /// Observations behind the whole grid.
    pub samples: u32,
    /// Cells that hold anything.
    pub filled: u32,
}

impl Heatmap {
    /// `observations` in any order.
    pub fn of(observations: impl IntoIterator<Item = (Millis, Copper)>) -> Heatmap {
        const DAY_MS: u64 = 24 * 60 * 60 * 1000;
        let mut grouped: BTreeMap<usize, Vec<(Millis, Copper)>> = BTreeMap::new();
        let mut samples = 0u32;
        for (at, price) in observations {
            let hour = (at.get() / 3_600_000 % 24) as usize;
            // Monday = 0; 1970-01-01 was a Thursday.
            let weekday = (((at.get() / DAY_MS) + 3) % 7) as usize;
            grouped
                .entry(weekday * 24 + hour)
                .or_default()
                .push((at, price));
            samples += 1;
        }

        let mut cells = vec![None; 7 * 24];
        let mut filled = 0;
        for (cell, values) in grouped {
            // The median of that hour-of-week, by the same estimator as
            // everything else -- not the mean, which the old cycle charts used
            // and which one spike drags.
            let buckets = Buckets::from_observations(values);
            if let Some(median) = buckets.quantile(0.50) {
                cells[cell] = Some(median);
                filled += 1;
            }
        }
        Heatmap {
            cells,
            samples,
            filled: filled as u32,
        }
    }

    /// Whether there is enough of the week here to draw.
    pub fn is_usable(&self) -> bool {
        self.filled as usize >= MIN_HEATMAP_CELLS
    }

    pub fn range(&self) -> Option<(Copper, Copper)> {
        let lo = self.cells.iter().flatten().min()?;
        let hi = self.cells.iter().flatten().max()?;
        Some((*lo, *hi))
    }

    /// The cheapest hour of the week, as (weekday, hour).
    ///
    /// `None` on an unusable grid: naming an hour from a grid that is mostly
    /// holes is naming the hour that happened to be collected.
    pub fn cheapest(&self) -> Option<(u8, u8)> {
        if !self.is_usable() {
            return None;
        }
        let (index, _) = self
            .cells
            .iter()
            .enumerate()
            .filter_map(|(i, cell)| cell.map(|price| (i, price)))
            .min_by_key(|(_, price)| price.get())?;
        Some(((index / 24) as u8, (index % 24) as u8))
    }

    /// The stored form: 168 fields, comma separated, empty for a hole.
    pub fn encode(&self) -> String {
        if self.filled == 0 {
            return String::new();
        }
        let mut out = String::with_capacity(self.cells.len() * 7);
        for (index, cell) in self.cells.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            if let Some(price) = cell {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{}", price.get()));
            }
        }
        // The sample count rides on the end, after a `;`, because a grid that
        // could not say how much is behind it would fail its own evidence gate
        // on the way back out of storage.
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!(";{}", self.samples));
        out
    }

    pub fn decode(raw: &str) -> Heatmap {
        let (grid, samples) = raw.split_once(';').unwrap_or((raw, "0"));
        if grid.is_empty() {
            return Heatmap::default();
        }
        let cells: Vec<Option<Copper>> = grid
            .split(',')
            .take(7 * 24)
            .map(|field| field.parse().ok().map(Copper))
            .collect();
        let filled = cells.iter().flatten().count() as u32;
        Heatmap {
            cells,
            samples: samples.parse().unwrap_or(0),
            filled,
        }
    }
}

/// A market before and after a moment.
///
/// §16's pre/post-event comparison, and the whole of what this app is willing
/// to say about an event: two medians, the depth behind each, and how many
/// observations went into them. Explicitly *not* a test, a p-value, or a claim
/// that the event moved the price -- the wording is `observed after`, and
/// [`Self::is_supported`] is what stops a comparison from four observations
/// being rendered as one from four hundred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeforeAfter {
    pub before_median: Copper,
    pub after_median: Copper,
    pub before_samples: u32,
    pub after_samples: u32,
    /// Signed percentage; negative means cheaper afterwards.
    pub change_percent: i32,
}

/// Observations either side needed before a comparison is shown.
///
/// A day of hourly collection each way, which is also the median gate
/// [`super::engine::Gates`] uses for a rank -- the same evidence buying the
/// same kind of claim.
pub const MIN_EITHER_SIDE: u32 = 12;

impl BeforeAfter {
    /// Compare the window either side of `at`.
    ///
    /// `observations` may be in any order and may extend well beyond the
    /// window; the caller's `span` is what decides how much either side counts,
    /// so that "before" and "after" are the same length and the comparison is
    /// not between a fortnight and an afternoon.
    pub fn of(
        observations: impl IntoIterator<Item = (Millis, Copper)>,
        at: Millis,
        span: u64,
    ) -> Option<BeforeAfter> {
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for (when, price) in observations {
            if when < at && at.get().saturating_sub(when.get()) <= span {
                before.push((when, price));
            } else if when >= at && when.get().saturating_sub(at.get()) <= span {
                after.push((when, price));
            }
        }
        let (b, a) = (
            Buckets::from_observations(before),
            Buckets::from_observations(after),
        );
        let (before_median, after_median) = (b.quantile(0.50)?, a.quantile(0.50)?);
        let base = before_median.get() as i128;
        Some(BeforeAfter {
            before_median,
            after_median,
            before_samples: b.len() as u32,
            after_samples: a.len() as u32,
            change_percent: if base == 0 {
                0
            } else {
                (((after_median.get() as i128 - base) * 100) / base) as i32
            },
        })
    }

    /// Whether both sides carry enough to be worth putting side by side.
    pub fn is_supported(&self) -> bool {
        self.before_samples >= MIN_EITHER_SIDE && self.after_samples >= MIN_EITHER_SIDE
    }
}
