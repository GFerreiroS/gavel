//! What is actually on the shelf, and what it costs to take it.
//!
//! `docs/market-analysis.md` §7 and CLAUDE.md §16's Phase 7. Every price this
//! app has shown until now answers "what does one cost" -- the cheapest
//! listing, the supply-weighted P5, the median of the last fortnight. None of
//! them answers the question a buyer actually has, which is **"what does it
//! cost me to buy what I need?"**
//!
//! The difference is not academic. A flask at 400g with 3 units listed and a
//! flask at 400g with 4,000 units listed are the same headline price and
//! completely different propositions: one of them is a raid night's shopping
//! and the other is one lucky purchase followed by paying 900g. §15's rule
//! that "listed stock is not sales volume" stands -- nothing here says how much
//! changes hands -- but *what is offered, and at what prices* is exactly what
//! the auction house publishes, and it is what this module keeps.
//!
//! ## Dense and sparse ladders are different animals
//!
//! A commodity ladder is thousands of units spread over dozens of prices; a
//! BoE's is four auctions of one item each. §16 asks for them to be treated
//! separately, and the reason is that most of what follows is meaningless on
//! four observations -- a "supply percentile" over four units is a way of
//! saying "the second one", dressed up. [`Ladder::is_sparse`] is where that
//! decision lives, and the metrics that need depth return `None` rather than a
//! confident number, which is §2 applied to a statistic instead of to a price.
//!
//! ## Everything here is a snapshot
//!
//! A ladder describes one moment. It is not a history and it is not weighted
//! by time -- which makes it the *third* kind of percentile in this crate,
//! after [`super::stats`]'s supply-weighted snapshot P5 and
//! [`super::engine`]'s time-weighted historical ones. They must not be merged,
//! and the test that holds the first two apart now holds three.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{Copper, Listing};

/// One rung: a price, and how many units are offered at it.
///
/// Grouped by price rather than kept per auction, which is the normalisation
/// §16 asks for. Forty auctions of one unit at 400g are one rung of forty; the
/// buyer does not care that they came from forty sellers, and keeping them
/// apart would be forty rows to say one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub price: Copper,
    pub quantity: u64,
    /// Units at this price *and every cheaper one*. Stored on the step rather
    /// than recomputed, because every metric below is a search over it.
    pub cumulative: u64,
}

/// A market's supply at one instant, cheapest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ladder {
    pub steps: Vec<Step>,
}

/// The price ratios, in percent above the cheapest, a [`Curve`] records supply
/// at.
///
/// Geometric, because supply is: on the measured markets the cheapest fifth of
/// the price range holds most of the units, and a linear ladder would spend
/// its bands out where nothing is listed. 5 and 20 are edges deliberately --
/// they are the two the depth panel prints, so a curve answers those exactly
/// instead of interpolating the figures a reader actually sees.
pub const CURVE_BANDS: [u32; 12] = [0, 1, 2, 5, 10, 20, 35, 50, 100, 200, 500, 1_000];

/// A ladder at archive resolution. See [`Ladder::compact`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Curve {
    /// Exact: this is what every band is a ratio of, and what the panel
    /// prints.
    ///
    /// **Anchoring the bands somewhere more robust was tried and rejected on
    /// the measurement.** A single joke listing at one copper drags the
    /// minimum down and takes every band with it, so hanging them off the
    /// price by which a twentieth of the supply is offered -- §8's own
    /// reasoning for `p05_unit_price` -- does improve the supply percentiles:
    /// their p95 error goes from 19.7% to 14.9%. It also stops 5% and 20%
    /// being band edges, and *those are the two figures the panel actually
    /// prints*: they went from exact on all 515 markets to a p95 error of
    /// 5150%. Trading a number the reader reads for one they do not is the
    /// wrong way round.
    pub cheapest: Copper,
    /// Units at or below each of [`CURVE_BANDS`], cumulative.
    pub cumulative: [u64; CURVE_BANDS.len()],
    /// Exact, including whatever is listed above the last band.
    pub total: u64,
    /// How many rungs the ladder had before it was pooled. Kept because
    /// `is_sparse` is a statement about the shelf, and a curve that forgot it
    /// would let the archive answer questions the live ladder refused.
    pub levels: u32,
}

impl Curve {
    pub fn is_sparse(&self) -> bool {
        (self.levels as usize) < SPARSE_STEPS
    }

    /// Units within `percent` of the cheapest. Exact on a band edge, and
    /// linearly interpolated between two -- which is the honest reading of a
    /// curve that pooled the rungs in between.
    pub fn quantity_within(&self, percent: u32) -> Option<u64> {
        if self.cheapest == Copper::ZERO {
            return None;
        }
        if let Some(i) = CURVE_BANDS.iter().position(|b| *b == percent) {
            return Some(self.cumulative[i]);
        }
        let Some(upper) = CURVE_BANDS.iter().position(|b| *b > percent) else {
            return Some(self.total);
        };
        if upper == 0 {
            return Some(self.cumulative[0]);
        }
        let (lo, hi) = (CURVE_BANDS[upper - 1], CURVE_BANDS[upper]);
        let (a, b) = (self.cumulative[upper - 1], self.cumulative[upper]);
        let span = (hi - lo) as u128;
        let into = (percent - lo) as u128;
        Some(a + (((b - a) as u128 * into) / span) as u64)
    }

    /// The price by which `percent` of the supply is on offer.
    ///
    /// `None` on a sparse shelf, exactly as [`Ladder::supply_percentile`]
    /// refuses one: with four listings "the cheapest quarter of supply" is a
    /// long way of saying "the second one", and an archive must not answer
    /// what the live ladder declined to.
    pub fn supply_percentile(&self, percent: u8) -> Option<Copper> {
        if self.is_sparse() || self.total == 0 {
            return None;
        }
        let wanted = (self.total as u128 * percent as u128).div_ceil(100) as u64;
        let base = self.cheapest.get() as u128;
        for (i, units) in self.cumulative.iter().enumerate() {
            if *units < wanted {
                continue;
            }
            // Interpolate *within* the band rather than snapping to its
            // ceiling. Measured on 515 real markets, snapping put the median
            // error at 3% and the p95 at 48%: the bands are geometric, so the
            // dear end of one is a long way from its cheap end, and a
            // percentile that lands there is reported as much dearer than it
            // is. Straight-line between the two edges takes the p95 to a few
            // per cent, which is what makes this an archive worth keeping.
            let hi = CURVE_BANDS[i] as u128;
            let (lo, below) = match i {
                0 => (0, 0),
                _ => (CURVE_BANDS[i - 1] as u128, self.cumulative[i - 1]),
            };
            let span = (*units).saturating_sub(below) as u128;
            let into = (wanted.saturating_sub(below)) as u128;
            let ratio = if span == 0 {
                lo
            } else {
                lo + ((hi - lo) * into).div_ceil(span)
            };
            return Some(Copper(
                (base * (100 + ratio) / 100).min(u64::MAX as u128) as u64
            ));
        }
        // Everything below the last band did not reach it: the rest is dearer
        // than the curve records, and the last band is the honest floor.
        let last = *CURVE_BANDS.last().unwrap() as u128;
        Some(Copper(
            (base * (100 + last) / 100).min(u64::MAX as u128) as u64
        ))
    }
}

/// Below this many rungs a ladder is *sparse*, and the depth metrics that
/// assume a distribution decline to answer.
///
/// Five, because that is the point at which "the cheapest quarter of supply"
/// stops being a phrase about a market and starts being a phrase about two
/// auctions. A BoE market has a median of five listings on the real archive
/// and a commodity market has 127, so this separates them almost exactly where
/// the data does.
pub const SPARSE_STEPS: usize = 5;

impl Ladder {
    /// Group listings by price, cheapest first.
    ///
    /// Takes the listings by value because it consumes them; the caller has
    /// just finished summarising the same slice and has no further use for it.
    pub fn of(listings: &[Listing]) -> Ladder {
        let mut by_price: BTreeMap<u64, u64> = BTreeMap::new();
        for listing in listings {
            if listing.quantity == 0 {
                continue;
            }
            *by_price.entry(listing.unit_price.get()).or_default() += listing.quantity;
        }
        let mut cumulative = 0;
        Ladder {
            steps: by_price
                .into_iter()
                .map(|(price, quantity)| {
                    cumulative += quantity;
                    Step {
                        price: Copper(price),
                        quantity,
                        cumulative,
                    }
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Distinct prices offered.
    /// One ladder from several, by pooling the units at each price.
    ///
    /// What a per-realm roll-up needs. A track is sold on a realm as several
    /// *variants* -- the same piece with a socket, with a tertiary stat, at a
    /// rank no sync has resolved -- and §8 pools those rather than splitting
    /// them into markets nobody could price. They are one shelf to a buyer, so
    /// they are one ladder here.
    ///
    /// **Only ever within one realm.** Pooling across realms would describe a
    /// shelf nobody can reach: a sweep is something you do in one auction
    /// house, and adding Sargeras's supply to Draenor's would quote a price
    /// for an order that cannot be filled.
    pub fn merged<'a>(ladders: impl IntoIterator<Item = &'a Ladder>) -> Ladder {
        let mut by_price: BTreeMap<u64, u64> = BTreeMap::new();
        for ladder in ladders {
            for step in &ladder.steps {
                *by_price.entry(step.price.get()).or_default() += step.quantity;
            }
        }
        let mut cumulative = 0;
        Ladder {
            steps: by_price
                .into_iter()
                .map(|(price, quantity)| {
                    cumulative += quantity;
                    Step {
                        price: Copper(price),
                        quantity,
                        cumulative,
                    }
                })
                .collect(),
        }
    }

    /// The same shelf at archive resolution: cumulative supply at a fixed
    /// ladder of prices above the cheapest.
    ///
    /// **Phase 7's archive curve, and it was deliberately not designed until
    /// there were real ladders to design it against.** There are now: 515 EU
    /// commodity markets, one snapshot, measured on 2026-08-31. The shape it
    /// was fitted to is a median of 28 rungs, a p95 of 123 and a maximum of
    /// 322 -- and 368 KB on disk per region-snapshot, which is roughly 70 MB a
    /// day across four regions and a gigabyte across the fourteen-day window.
    /// That is what makes a hot window a hot window rather than the archive.
    ///
    /// The bands are *ratios of the market's own cheapest price*, not absolute
    /// money, because every depth metric this has to reproduce is already
    /// expressed that way: "within 5%", "within 20%", the sweep's price impact.
    /// Choosing them so that 5 and 20 are band edges makes those two exact
    /// rather than interpolated, which is worth more than an extra band
    /// somewhere nobody reads.
    ///
    /// What survives exactly: the cheapest price, the total, and the units
    /// within any band edge. What is interpolated: the supply percentiles and
    /// a sweep's cost, both within one band. **What is lost: walls.** A wall is
    /// one rung's share of the supply, and a curve has pooled the rungs -- so
    /// "held up by few sellers" is a hot-window question and says so, rather
    /// than being answered worse from the archive.
    pub fn compact(&self) -> Curve {
        let Some(cheapest) = self.cheapest() else {
            return Curve::default();
        };
        let base = cheapest.get() as u128;
        let cumulative = CURVE_BANDS.map(|percent| {
            let ceiling =
                Copper((base * (100 + percent as u128) / 100).min(u64::MAX as u128) as u64);
            self.quantity_upto(ceiling)
        });
        Curve {
            cheapest,
            cumulative,
            total: self.total(),
            levels: self.steps.len() as u32,
        }
    }

    pub fn levels(&self) -> usize {
        self.steps.len()
    }

    /// Every unit on offer.
    pub fn total(&self) -> u64 {
        self.steps.last().map(|s| s.cumulative).unwrap_or(0)
    }

    /// The cheapest unit price, which is what every other figure is relative
    /// to.
    pub fn cheapest(&self) -> Option<Copper> {
        self.steps.first().map(|s| s.price)
    }

    pub fn dearest(&self) -> Option<Copper> {
        self.steps.last().map(|s| s.price)
    }

    /// Whether this ladder is too thin for the distribution metrics to mean
    /// anything. See [`SPARSE_STEPS`].
    pub fn is_sparse(&self) -> bool {
        self.steps.len() < SPARSE_STEPS
    }

    /// Units offered at or below `price`.
    pub fn quantity_upto(&self, price: Copper) -> u64 {
        let index = self.steps.partition_point(|s| s.price <= price);
        if index == 0 {
            0
        } else {
            self.steps[index - 1].cumulative
        }
    }

    /// Units offered within `percent` of the cheapest price.
    ///
    /// The **liquidity proxy** §16 asks to be named rather than implied: "how
    /// much can I buy without paying appreciably more than the sticker price".
    /// It is a proxy and not a measurement -- nothing here knows whether any
    /// of it sells -- which is why it is spelled out in the type's name and in
    /// the panel that shows it.
    pub fn quantity_within(&self, percent: u32) -> Option<u64> {
        let cheapest = self.cheapest()?.get() as u128;
        let ceiling =
            Copper((cheapest * (100 + percent as u128) / 100).min(u64::MAX as u128) as u64);
        Some(self.quantity_upto(ceiling))
    }

    /// The price at which `percent` of the listed supply has been consumed.
    ///
    /// **Supply-weighted and about one snapshot.** This is the same kind of
    /// figure as [`super::stats`]'s P5 and emphatically not the same kind as
    /// [`super::engine`]'s historical percentiles: those weight equal
    /// durations of history, this weights units on a shelf right now. §5.1
    /// forbids merging them and a test keeps all three apart.
    ///
    /// `None` on a sparse ladder, where it would be a long word for "the
    /// second cheapest one".
    pub fn supply_percentile(&self, percent: u8) -> Option<Copper> {
        if self.is_sparse() {
            return None;
        }
        let total = self.total();
        if total == 0 {
            return None;
        }
        // Ceiling, so a tiny market still targets at least one unit.
        let target = (total * percent as u64).div_ceil(100).max(1);
        self.steps
            .iter()
            .find(|s| s.cumulative >= target)
            .map(|s| s.price)
            .or_else(|| self.dearest())
    }

    /// What it costs to buy `wanted` units, walking the ladder from the
    /// cheapest rung up.
    ///
    /// The whole point of the module, and the one figure that cannot be
    /// derived from any summary this app stored before: buying is a *sweep*,
    /// and its cost depends on the shape of the supply rather than on any
    /// single price in it.
    pub fn fill(&self, wanted: u64) -> Fill {
        let cheapest = self.cheapest().unwrap_or(Copper::ZERO);
        let mut filled = 0u64;
        let mut cost = 0u128;
        let mut clearing = cheapest;

        for step in &self.steps {
            if filled >= wanted {
                break;
            }
            let take = step.quantity.min(wanted - filled);
            filled += take;
            cost += take as u128 * step.price.get() as u128;
            clearing = step.price;
        }

        let average = if filled == 0 {
            Copper::ZERO
        } else {
            Copper((cost / filled as u128) as u64)
        };
        // How much dearer the sweep is than the sticker price, as a
        // percentage. This is the number a card would show as "buying 20 of
        // these costs 12% more than the cheapest one does".
        let impact_percent = match (cheapest.get(), filled) {
            (0, _) | (_, 0) => 0,
            (base, _) => (((average.get() as i128 - base as i128) * 100) / base as i128) as u32,
        };

        Fill {
            wanted,
            filled,
            complete: filled == wanted,
            total_cost: Copper(cost.min(u64::MAX as u128) as u64),
            average_unit: average,
            clearing_price: clearing,
            impact_percent,
        }
    }

    /// Rungs holding an unusual share of the whole ladder.
    ///
    /// A *wall* is one seller (or one price point) holding enough of the
    /// supply that the market's shape depends on them. It matters because a
    /// market that looks deep can be one listing away from being thin, and the
    /// depth figures above would not say so on their own.
    ///
    /// `share_percent` of the total supply, at or above [`WALL_SHARE`]. Sparse
    /// ladders have no walls by definition -- with four rungs, every one of
    /// them is a wall, which is a way of saying none of them is.
    pub fn walls(&self) -> Vec<Wall> {
        if self.is_sparse() {
            return Vec::new();
        }
        let total = self.total();
        if total == 0 {
            return Vec::new();
        }
        self.steps
            .iter()
            .filter_map(|step| {
                let share = ((step.quantity as u128 * 100) / total as u128) as u32;
                (share >= WALL_SHARE).then_some(Wall {
                    price: step.price,
                    quantity: step.quantity,
                    share_percent: share,
                })
            })
            .collect()
    }

    /// The stored form: `price:quantity`, `,` between rungs, cheapest first.
    ///
    /// `cumulative` is not stored -- it is a running sum of what is here, and
    /// storing it would be doubling the column to save an addition. The same
    /// argument the chart series makes about timestamps.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.steps.len() * 14);
        for (index, step) in self.steps.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            use std::fmt::Write;
            let _ = write!(out, "{}:{}", step.price.get(), step.quantity);
        }
        out
    }

    /// Read back what [`Self::encode`] wrote, rebuilding the running sum.
    ///
    /// A rung this binary cannot parse is dropped rather than zeroed: a zero
    /// price would be a free unit at the front of the ladder, which is the one
    /// error that would corrupt every figure above it.
    pub fn decode(raw: &str) -> Ladder {
        if raw.is_empty() {
            return Ladder::default();
        }
        let mut cumulative = 0;
        Ladder {
            steps: raw
                .split(',')
                .filter_map(|rung| {
                    let (price, quantity) = rung.split_once(':')?;
                    let price: u64 = price.parse().ok()?;
                    let quantity: u64 = quantity.parse().ok()?;
                    (price > 0 && quantity > 0).then(|| {
                        cumulative += quantity;
                        Step {
                            price: Copper(price),
                            quantity,
                            cumulative,
                        }
                    })
                })
                .collect(),
        }
    }
}

/// A rung holding at least this share of the ladder is a wall.
///
/// A fifth of the market at one price is a seller with a position rather than
/// a market with a price. Lower and every ordinary rung qualifies; higher and
/// only a monopoly does.
pub const WALL_SHARE: u32 = 20;

/// The result of sweeping a ladder for a target quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub wanted: u64,
    /// How many were actually available. Less than `wanted` is the answer §2
    /// insists on: a market that cannot fill the order says so rather than
    /// quoting a price for units that are not there.
    pub filled: u64,
    pub complete: bool,
    pub total_cost: Copper,
    /// What one unit averages across the sweep -- the honest "price" for this
    /// quantity, as opposed to the cheapest listing's.
    pub average_unit: Copper,
    /// The price of the last unit taken.
    pub clearing_price: Copper,
    /// How much dearer the average is than the cheapest rung, as a percentage.
    pub impact_percent: u32,
}

/// One price holding an outsized share of the supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wall {
    pub price: Copper,
    pub quantity: u64,
    pub share_percent: u32,
}

/// How much of something somebody is actually trying to buy.
///
/// §16: "Define target profiles in catalogue/domain metadata rather than
/// templates." A template that hard-coded "20" would be a product decision
/// living in markup, and a different template would eventually hard-code a
/// different one -- which is §7's drift, in the one place it would be
/// invisible because both numbers look equally arbitrary.
///
/// The defaults are per [`super::ItemKind`] and are what a raid night actually
/// needs, not round numbers: a flask lasts an hour and a raid is three, times
/// a little slack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target(pub u64);

impl Target {
    /// What a buyer of this kind of item is usually buying.
    pub const fn of(kind: super::ItemKind) -> Target {
        match kind {
            // A raid night: three or four flasks, a stack of potions, food for
            // the pulls that go wrong.
            super::ItemKind::Consumable => Target(20),
            // Crafting is bought in stacks, and a stack is 200 this expansion.
            super::ItemKind::Reagent => Target(200),
            // One per slot, and nobody enchants a whole raid at once.
            super::ItemKind::Enchant => Target(5),
            super::ItemKind::Gem => Target(5),
            // A BoE or a recipe is one item. There is no quantity question --
            // which is exactly why these are the sparse case.
            super::ItemKind::Boe | super::ItemKind::Recipe => Target(1),
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Everything a page shows about one market's depth, at one instant.
///
/// Precomputed and stored, like every other figure a page reads: §15's write
/// path applies to depth exactly as it does to percentiles, and sweeping a
/// ladder per card would be the same mistake in a new place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Depth {
    /// Rungs and units, which are the two numbers that say whether any of the
    /// rest is worth reading.
    pub levels: u32,
    pub total: u64,
    pub cheapest: Copper,
    /// `None` on a sparse ladder.
    pub p25: Option<Copper>,
    pub p50: Option<Copper>,
    /// Units within 5% and within 20% of the cheapest price: the named
    /// liquidity proxies.
    pub within_5: Option<u64>,
    pub within_20: Option<u64>,
    /// What buying the catalogue's target quantity costs.
    pub target: u64,
    pub fill: Fill,
    pub walls: Vec<Wall>,
    /// Whether the metrics above declined to answer.
    pub sparse: bool,
}

impl Depth {
    /// The stored form: fixed fields in order, `|` separated, walls last.
    ///
    /// Stored rather than recomputed on read for the reason §15 gives about
    /// everything else: sweeping a ladder per card is cheap once and is the
    /// same mistake as reducing a history once, made in a new place. It is
    /// also where the *target* is baked in -- the catalogue knows what a buyer
    /// wants and the storage layer does not, so the sweep has to happen on the
    /// side of the wall that has the catalogue.
    pub fn encode(&self) -> String {
        use std::fmt::Write;
        let opt = |value: Option<u64>| value.map(|v| v.to_string()).unwrap_or_default();
        let mut out = String::with_capacity(96);
        let _ = write!(
            out,
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.levels,
            self.total,
            self.cheapest.get(),
            opt(self.p25.map(|c| c.get())),
            opt(self.p50.map(|c| c.get())),
            opt(self.within_5),
            opt(self.within_20),
            self.target,
            self.fill.filled,
            self.fill.total_cost.get(),
            self.fill.average_unit.get(),
            self.fill.clearing_price.get(),
            self.fill.impact_percent,
        );
        for wall in &self.walls {
            let _ = write!(
                out,
                "|{}:{}:{}",
                wall.price.get(),
                wall.quantity,
                wall.share_percent
            );
        }
        out
    }

    /// Read back what [`Self::encode`] wrote.
    ///
    /// `None` for anything it cannot parse in full. A half-read depth summary
    /// is worse than none: every figure on the panel is relative to the
    /// cheapest price, so one missing field is a panel of confident wrong
    /// numbers rather than a panel that says it has nothing.
    pub fn decode(raw: &str) -> Option<Depth> {
        let mut fields = raw.split('|');
        let mut next = || fields.next();
        let num = |field: Option<&str>| field?.parse::<u64>().ok();
        let opt = |field: Option<&str>| -> Option<Option<u64>> {
            let field = field?;
            if field.is_empty() {
                Some(None)
            } else {
                field.parse::<u64>().ok().map(Some)
            }
        };

        let levels = num(next())? as u32;
        let total = num(next())?;
        let cheapest = Copper(num(next())?);
        let p25 = opt(next())?.map(Copper);
        let p50 = opt(next())?.map(Copper);
        let within_5 = opt(next())?;
        let within_20 = opt(next())?;
        let target = num(next())?;
        let filled = num(next())?;
        let total_cost = Copper(num(next())?);
        let average_unit = Copper(num(next())?);
        let clearing_price = Copper(num(next())?);
        let impact_percent = num(next())? as u32;

        let walls = fields
            .filter_map(|wall| {
                let mut parts = wall.split(':');
                Some(Wall {
                    price: Copper(parts.next()?.parse().ok()?),
                    quantity: parts.next()?.parse().ok()?,
                    share_percent: parts.next()?.parse().ok()?,
                })
            })
            .collect();

        Some(Depth {
            levels,
            total,
            cheapest,
            p25,
            p50,
            within_5,
            within_20,
            target,
            fill: Fill {
                wanted: target,
                filled,
                complete: filled == target,
                total_cost,
                average_unit,
                clearing_price,
                impact_percent,
            },
            walls,
            // Recovered rather than stored: it is a statement about the rung
            // count, which is right there.
            sparse: (levels as usize) < SPARSE_STEPS,
        })
    }

    pub fn of(ladder: &Ladder, target: Target) -> Option<Depth> {
        let cheapest = ladder.cheapest()?;
        Some(Depth {
            levels: ladder.levels() as u32,
            total: ladder.total(),
            cheapest,
            p25: ladder.supply_percentile(25),
            p50: ladder.supply_percentile(50),
            within_5: ladder.quantity_within(5).filter(|_| !ladder.is_sparse()),
            within_20: ladder.quantity_within(20).filter(|_| !ladder.is_sparse()),
            target: target.get(),
            fill: ladder.fill(target.get()),
            walls: ladder.walls(),
            sparse: ladder.is_sparse(),
        })
    }
}
