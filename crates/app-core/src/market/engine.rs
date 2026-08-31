//! The one place a market's statistics are defined.
//!
//! `docs/market-analysis.md` §3: "Statistics are not created per item,
//! category, expansion, or patch. A common engine analyses a generic market."
//! Before this module there were three: `analysis.rs` reduced a commodity
//! series, `gear_stats.rs` had its own reduction for per-realm markets, and
//! `alerts.rs` had a third percentile with a fourth definition. They disagreed,
//! and the disagreement reached the reader as two pages calling different
//! numbers by the same name.
//!
//! Everything here is pure and takes prices rather than rows, so it says
//! nothing about which market it is describing. That is what makes it one
//! engine rather than a shape that happens to be reused.
//!
//! ## Three distinctions this module exists to keep
//!
//! **A snapshot percentile is not a historical percentile.** [`super::stats`]
//! computes a *supply-weighted* P5 inside one snapshot: what a buyer pays after
//! consuming that share of the currently listed quantity. The percentiles here
//! are over a market's own history and are *time*-weighted. §5.1 is explicit
//! that they must not be merged, and a test holds them apart.
//!
//! **Valuation is not anomaly.** `Very cheap` says a price sits in the lower
//! tail. An anomaly says it is unusually far from the body of the distribution.
//! A price can be both, either or neither, and §5.4 shows them separately.
//!
//! **Dispersion is not swing.** `(max - min) / mean` is dominated by two
//! observations and is named [`Swing`] here rather than volatility. IQR and MAD
//! are what a stable measure of spread looks like.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use cluster_core::Millis;

use super::Copper;

/// One hour. Snapshots are generated hourly, so an hour is the unit a
/// historical percentile weights equally.
pub const BUCKET_MS: u64 = 60 * 60 * 1000;

/// A market's prices, one per equal-duration bucket.
///
/// §5.1: "Each equal-duration time bucket has equal weight in a historical
/// percentile. Do not weight historical time by current listed quantity: that
/// would answer 'what price existed during high-stock periods?' rather than
/// 'where is today's price in this market's history?'"
///
/// It is also what stops a market that was collected twice in one hour from
/// counting twice, which raw observations would.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Buckets {
    prices: Vec<u64>,
}

impl Buckets {
    /// Reduce `(observed_at, price)` pairs to one price per hour.
    ///
    /// The last observation in a bucket wins: it is the market's state at the
    /// end of that hour, and a percentile over states is what "where is today's
    /// price in this market's history" asks for.
    pub fn from_observations(observations: impl IntoIterator<Item = (Millis, Copper)>) -> Buckets {
        let mut latest: BTreeMap<u64, (Millis, Copper)> = BTreeMap::new();
        for (at, price) in observations {
            let bucket = at.get() / BUCKET_MS;
            let slot = latest.entry(bucket).or_insert((at, price));
            if at >= slot.0 {
                *slot = (at, price);
            }
        }
        let mut prices: Vec<u64> = latest.into_values().map(|(_, price)| price.get()).collect();
        prices.sort_unstable();
        Buckets { prices }
    }

    /// How many buckets hold an observation. The evidence count every gate
    /// below is measured in.
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// The bucket prices, sorted. For a consumer that needs the shape rather
    /// than a statistic of it -- [`super::series::Histogram`] is the one.
    pub fn prices(&self) -> &[u64] {
        &self.prices
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// The Hyndman-Fan **type 8** sample quantile: the median-unbiased
    /// estimator, and the one `docs/market-analysis.md` §5.1 settles on for
    /// analysis, cards, alerts, tests and archive rebuilds alike.
    ///
    /// ```text
    /// h    = (n + 1/3) p + 1/3
    /// Q(p) = x[floor h] + (h - floor h) (x[floor h + 1] - x[floor h])
    /// ```
    ///
    /// One definition everywhere is the point. The nearest-rank percentile the
    /// alert rule used before is a different estimator, and two estimators mean
    /// a card and an alert can disagree about whether the same price is cheap.
    pub fn quantile(&self, p: f64) -> Option<Copper> {
        let n = self.prices.len();
        if n == 0 {
            return None;
        }
        if n == 1 {
            return Some(Copper(self.prices[0]));
        }
        let p = p.clamp(0.0, 1.0);
        let h = (n as f64 + 1.0 / 3.0) * p + 1.0 / 3.0;
        // 1-based in the definition, 0-based here.
        let lower = h.floor().clamp(1.0, n as f64) as usize;
        let upper = (lower + 1).min(n);
        let low = self.prices[lower - 1] as f64;
        let high = self.prices[upper - 1] as f64;
        let value = low + (h - lower as f64).clamp(0.0, 1.0) * (high - low);
        Some(Copper(value.round().max(0.0) as u64))
    }

    /// Where `price` sits in this history, as a percentage.
    ///
    /// The inverse of [`Self::quantile`] in intent rather than in arithmetic:
    /// a rank is a count, and interpolating a count would invent a precision
    /// the sample does not have.
    ///
    /// **Ties take the middle of the range they tie over**, which is the
    /// conventional mid-rank and is load-bearing here rather than a nicety. A
    /// plain "how many buckets are at or below" puts a market that has *never
    /// moved* at 100 -- every bucket is at or below, because every bucket is
    /// the same price -- and the card then reads `Very expensive` about a
    /// price that is exactly what it has always been. A steady market is the
    /// most ordinary thing an auction house contains, and it is the middle of
    /// its own history, which is what the mid-rank says.
    pub fn rank_of(&self, price: Copper) -> Option<u8> {
        if self.prices.is_empty() {
            return None;
        }
        let below = self.prices.partition_point(|p| *p < price.get());
        let at_or_below = self.prices.partition_point(|p| *p <= price.get());
        // The midpoint, in halves, so the division stays in integers.
        let midpoint = below + at_or_below;
        Some(((midpoint * 100) / (2 * self.prices.len())).min(100) as u8)
    }
}

/// The five-number summary and two robust spreads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distribution {
    pub p05: Copper,
    pub p25: Copper,
    pub median: Copper,
    pub p75: Copper,
    pub p95: Copper,
    /// P75 - P25. §5.4's stable measure of central spread.
    pub iqr: Copper,
    /// Median absolute deviation from the median. Stabler than the IQR on a
    /// small sample, and the basis of the robust score below.
    pub mad: Copper,
    /// Buckets behind all of the above.
    pub buckets: u32,
}

impl Distribution {
    pub fn of(buckets: &Buckets) -> Option<Distribution> {
        let median = buckets.quantile(0.50)?;
        let p25 = buckets.quantile(0.25)?;
        let p75 = buckets.quantile(0.75)?;

        let mut deviations: Vec<u64> = buckets
            .prices
            .iter()
            .map(|p| p.abs_diff(median.get()))
            .collect();
        deviations.sort_unstable();
        let mad = Buckets { prices: deviations }.quantile(0.50)?;

        Some(Distribution {
            p05: buckets.quantile(0.05)?,
            p25,
            median,
            p75,
            p95: buckets.quantile(0.95)?,
            iqr: Copper(p75.get().saturating_sub(p25.get())),
            mad,
            buckets: buckets.len() as u32,
        })
    }
}

/// The universal valuation bands: percentile ranks inside each market's own
/// distribution (§5.2).
///
/// Universal in the sense that the boundaries never move -- liquidity and
/// category do not secretly shift them. What they decide is whether the result
/// is *reliable*, which is [`Evidence`] below and is shown beside the label
/// rather than folded into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Valuation {
    VeryCheap,
    Cheap,
    /// Not "Fair". Listed prices do not establish intrinsic or transacted fair
    /// value, and calling the middle of a listing distribution fair would be
    /// the product claiming something its data cannot support.
    Typical,
    Expensive,
    VeryExpensive,
}

impl Valuation {
    pub const ALL: [Valuation; 5] = [
        Valuation::VeryCheap,
        Valuation::Cheap,
        Valuation::Typical,
        Valuation::Expensive,
        Valuation::VeryExpensive,
    ];

    /// The band a percentile rank falls in.
    pub const fn of_rank(rank: u8) -> Valuation {
        match rank {
            0..=5 => Valuation::VeryCheap,
            6..=25 => Valuation::Cheap,
            26..=75 => Valuation::Typical,
            76..=95 => Valuation::Expensive,
            _ => Valuation::VeryExpensive,
        }
    }

    /// The English label, which is also the source string templates translate.
    pub const fn as_str(self) -> &'static str {
        match self {
            Valuation::VeryCheap => "Very cheap",
            Valuation::Cheap => "Cheap",
            Valuation::Typical => "Typical",
            Valuation::Expensive => "Expensive",
            Valuation::VeryExpensive => "Very expensive",
        }
    }

    /// A slug for a CSS class, which must not be a translated word.
    pub const fn slug(self) -> &'static str {
        match self {
            Valuation::VeryCheap => "very-cheap",
            Valuation::Cheap => "cheap",
            Valuation::Typical => "typical",
            Valuation::Expensive => "expensive",
            Valuation::VeryExpensive => "very-expensive",
        }
    }
}

/// Whether a value is far from the body of the distribution, which is a
/// different question from where it ranks in it (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anomaly {
    /// Inside Tukey's inner fences.
    Ordinary,
    /// Beyond 1.5 IQR of the nearer quartile.
    Mild,
    /// Beyond 3 IQR. NIST calls these extreme outliers.
    Extreme,
}

impl Anomaly {
    /// Tukey's fences, which need a spread to be measured against.
    ///
    /// A market whose IQR is zero -- a price that has not moved -- has no
    /// scale, so nothing can be far from it. Reporting every different price
    /// as extreme there would be an anomaly detector that fires on a market
    /// being calm.
    pub fn of(price: Copper, distribution: &Distribution) -> Anomaly {
        let iqr = distribution.iqr.get() as i128;
        if iqr == 0 {
            return Anomaly::Ordinary;
        }
        let price = price.get() as i128;
        let (low, high) = (
            distribution.p25.get() as i128,
            distribution.p75.get() as i128,
        );
        let beyond =
            |factor: i128| price < low - factor * iqr / 2 || price > high + factor * iqr / 2;
        // 3.0 and 1.5 IQR, expressed in halves to stay in integers.
        if beyond(6) {
            Anomaly::Extreme
        } else if beyond(3) {
            Anomaly::Mild
        } else {
            Anomaly::Ordinary
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Anomaly::Ordinary => "Ordinary",
            Anomaly::Mild => "Unusual",
            Anomaly::Extreme => "Extreme",
        }
    }
}

/// `(max - min) / mean`, and its name.
///
/// §5.4: "Do not use `(maximum - minimum) / mean` as volatility: that is a
/// range-based swing, is dominated by two observations, and should be named
/// **Swing** if retained." It is retained, under that name, because it is
/// legible in a way a MAD is not -- and it sits beside the robust measures
/// rather than instead of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swing(pub u32);

impl Swing {
    pub fn of(buckets: &Buckets) -> Swing {
        let (Some(low), Some(high)) = (buckets.prices.first(), buckets.prices.last()) else {
            return Swing(0);
        };
        let total: u128 = buckets.prices.iter().map(|p| *p as u128).sum();
        let mean = total / buckets.prices.len() as u128;
        if mean == 0 {
            return Swing(0);
        }
        Swing((((high - low) as u128 * 100) / mean) as u32)
    }
}

/// Why a market has no valuation to show.
///
/// §5.3: "Do not emit a valuation or anomaly label when its evidence gate
/// fails. Show `Not enough history` and the reason instead." The reason is a
/// value rather than a sentence so the page can translate it and a test can
/// assert it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Insufficient {
    /// Fewer buckets than the statistic needs.
    NotEnoughHistory { have: u32, need: u32 },
    /// Enough buckets, but too few of the ones the window expected: a market
    /// observed for six hours of a month is not a month of evidence.
    TooManyGaps { coverage: u32, need: u32 },
}

/// How much evidence a window holds, and what it is therefore allowed to say.
///
/// Two gates rather than one `min_samples`, because §5.3 is explicit that
/// "tail percentiles such as P5/P95 need more evidence than a median" -- market
/// observations are serially dependent, so a hundred adjacent hours do not
/// carry the information of a hundred independent samples.
///
/// The thresholds are measured, and the measurement is worth writing down
/// because the first set of numbers here was not.
///
/// They were 24 and 72, on the reasoning that a day of buckets supports a
/// median and three days support a tail. Run against the real archive, every
/// card on every page refused its band -- 49 columns out of 49 on the
/// consumables page. The archive is 76 hours long and collection is not
/// continuous, so **no market anywhere in it holds 72 hourly buckets**: the
/// gate could not be met by construction, and a statistic nobody can ever see
/// is not a conservative statistic, it is an absent one.
///
/// What the archive actually holds, per market, in a 14-day window:
///
/// | Buckets | Commodity markets (of 2,042) | Per-realm (of 28,284) |
/// |---|---:|---:|
/// | >= 12 | 2,024 | 15,900 |
/// | >= 24 | 1,984 | 1,280 |
/// | >= 36 | 1,418 | 0 |
/// | >= 48 | 46 | 0 |
///
/// So: **12 for a median, 24 for a tail.** A day of hourly buckets behind a
/// band, half a day behind a rank, and the tail gate still twice the median
/// one -- which is what §5.3 actually asks for ("tail percentiles such as
/// P5/P95 need more evidence than a median"), rather than a specific number it
/// never names. Twenty-four buckets resolve a percentile to about four points,
/// which is coarse for a number and ample for five bands -- and the rank is
/// printed beside the word precisely so the reader can see how coarse.
///
/// Per-realm markets are thinner than commodity ones because realms are
/// collected on their own schedules, so fewer of them earn a band. That is the
/// gate working: a gear market with 13 hours behind it has less evidence, and
/// says so.
///
/// Re-measure these against a mature archive. The shape above is a
/// three-day-old one, and the honest reading of it is "what can be supported
/// today", not "what is enough for ever".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    /// Buckets needed before a median, an IQR or a rank is shown.
    pub median: u32,
    /// Buckets needed before P5/P95 and a valuation band are shown.
    pub tails: u32,
    /// Percentage of the window's expected buckets that must be observed.
    pub coverage: u32,
}

impl Default for Gates {
    fn default() -> Self {
        Gates {
            median: 12,
            tails: 24,
            coverage: 25,
        }
    }
}

impl Gates {
    /// Whether this evidence supports a band, and why not when it does not.
    ///
    /// `coverage` is `None` for a window with no datable start -- "everything
    /// ever" -- where there is nothing to be a fraction of, and a missing
    /// fraction is not a failed one.
    pub fn admit(&self, buckets: u32, coverage: Option<u32>) -> Result<(), Insufficient> {
        if buckets < self.tails {
            return Err(Insufficient::NotEnoughHistory {
                have: buckets,
                need: self.tails,
            });
        }
        if let Some(coverage) = coverage
            && coverage < self.coverage
        {
            return Err(Insufficient::TooManyGaps {
                coverage,
                need: self.coverage,
            });
        }
        Ok(())
    }

    /// The weaker gate: enough to place a median and a rank, not enough for a
    /// band.
    pub fn admits_median(&self, buckets: u32) -> bool {
        buckets >= self.median
    }
}

/// Where one price sits in one market's history.
///
/// The whole answer, including the refusal to answer. §5.2: "The valuation is
/// never shown alone", so everything it must be shown with is in the same
/// value -- rank, distance from the median, and the evidence that decided
/// whether there is a band at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Percentage of buckets at or below the current price. `None` when there
    /// is not even enough for a median.
    pub rank: Option<u8>,
    /// The band, or `None` with a reason.
    pub valuation: Option<Valuation>,
    pub insufficient: Option<Insufficient>,
    /// Signed percentage from the window's median; negative is cheaper.
    pub from_median_percent: Option<i32>,
    /// Whether the price is far from the body of the distribution, which is a
    /// separate statement from the band.
    pub anomaly: Anomaly,
}

impl Position {
    /// Place `price` in `buckets`, under `gates`.
    pub fn of(price: Copper, buckets: &Buckets, coverage: Option<u32>, gates: Gates) -> Position {
        let held = buckets.len() as u32;
        let Some(distribution) = Distribution::of(buckets) else {
            return Position {
                rank: None,
                valuation: None,
                insufficient: Some(Insufficient::NotEnoughHistory {
                    have: 0,
                    need: gates.tails,
                }),
                from_median_percent: None,
                anomaly: Anomaly::Ordinary,
            };
        };

        let rank = gates
            .admits_median(held)
            .then(|| buckets.rank_of(price))
            .flatten();
        let from_median_percent = gates.admits_median(held).then(|| {
            let median = distribution.median.get() as i128;
            if median == 0 {
                0
            } else {
                (((price.get() as i128 - median) * 100) / median) as i32
            }
        });

        let admitted = gates.admit(held, coverage);
        Position {
            rank,
            valuation: match (&admitted, rank) {
                (Ok(()), Some(rank)) => Some(Valuation::of_rank(rank)),
                _ => None,
            },
            insufficient: match admitted {
                Ok(()) if rank.is_none() => Some(Insufficient::NotEnoughHistory {
                    have: held,
                    need: gates.median,
                }),
                Ok(()) => None,
                Err(reason) => Some(reason),
            },
            from_median_percent,
            // Anomaly needs a spread, not a gate: it is a statement about the
            // distribution's own shape, and a market with two observations has
            // no shape to be far from -- which `Anomaly::of` handles by having
            // no IQR.
            anomaly: Anomaly::of(price, &distribution),
        }
    }
}

/// Slots in a card's sparkline.
///
/// Small on purpose. This is drawn several hundred times on a category page,
/// so every slot is markup multiplied by the size of the grid; sixteen is
/// enough to show a shape and little enough that replacing the four figure
/// rows it stands in for costs the page nothing. The analysis page's chart is
/// [`super::materialise::CHART_POINTS`] and is a different picture for a
/// different question.
pub const SPARK_SLOTS: usize = 16;

/// A card-sized shape of a market over one window.
///
/// **Equal-duration slots, like [`Buckets`], and for the same reason.** A
/// sparkline has no axis on it, so a reader takes the horizontal as time
/// whether or not it is; spacing the points by observation rather than by
/// clock would draw a quiet fortnight and a busy afternoon at the same width.
/// A slot nothing was observed in is [`None`] rather than an interpolation --
/// §2's rule that unavailable data is never invented, applied to a line that
/// would otherwise be drawn straight through the gap.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Spark {
    /// One value per slot, oldest first.
    pub slots: Vec<Option<Copper>>,
}

impl Spark {
    /// Reduce observations to [`SPARK_SLOTS`] slots across `[from, until]`.
    ///
    /// The last observation in a slot wins, which is [`Buckets`]'s rule: a
    /// slot describes the market's state at the end of it.
    pub fn over(
        observations: impl IntoIterator<Item = (Millis, Copper)>,
        from: Millis,
        until: Millis,
    ) -> Spark {
        let span = until.get().saturating_sub(from.get());
        if span == 0 {
            return Spark::default();
        }
        let mut slots: Vec<Option<(Millis, Copper)>> = vec![None; SPARK_SLOTS];
        for (at, price) in observations {
            if at < from || at > until {
                continue;
            }
            let offset = at.get() - from.get();
            // The final instant belongs to the last slot rather than to a
            // seventeenth one that does not exist.
            let slot = ((offset * SPARK_SLOTS as u64) / span) as usize;
            let slot = slot.min(SPARK_SLOTS - 1);
            match &mut slots[slot] {
                Some(held) if held.0 > at => {}
                held => *held = Some((at, price)),
            }
        }
        Spark {
            slots: slots.into_iter().map(|s| s.map(|(_, p)| p)).collect(),
        }
    }

    /// Whether there is any shape here at all. One point is not a line.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().filter(|s| s.is_some()).count() < 2
    }

    /// The stored form: one value per slot, comma separated, empty for a gap.
    ///
    /// A string rather than JSON because this is a column on every window of
    /// every market, and `[1200,null,1250]` is twice the bytes of
    /// `1200,,1250` for the same fact.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.slots.len() * 8);
        for (index, slot) in self.slots.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            if let Some(price) = slot {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{}", price.get()));
            }
        }
        out
    }

    /// Read back what [`Self::encode`] wrote.
    ///
    /// Forgiving in one direction only: a field this binary cannot parse is a
    /// gap, which draws nothing, rather than a zero, which would draw a market
    /// crashing to free.
    pub fn decode(raw: &str) -> Spark {
        if raw.is_empty() {
            return Spark::default();
        }
        Spark {
            slots: raw
                .split(',')
                .take(SPARK_SLOTS)
                .map(|field| field.parse().ok().map(Copper))
                .collect(),
        }
    }
}
