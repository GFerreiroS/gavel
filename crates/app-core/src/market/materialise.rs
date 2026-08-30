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

use std::collections::{BTreeMap, BTreeSet};

use cluster_core::Millis;

use super::analysis::{self, Cycle, Point, Trend};
use super::catalog::{Catalog, ItemKind};
use super::engine::{Buckets, Distribution, Gates, Position, Spark, Swing};
use super::key::MarketKey;
use super::series::{ChartSeries, Histogram, Observation};
use super::window::Window;
use super::{Copper, ItemId, PriceSample};

/// Bumped whenever a definition here changes, so a stored row can say which
/// rules produced it and a rebuild can be told apart from a re-read.
///
/// Not the same as a catalogue version: that says what the market *was*, this
/// says how it was measured.
///
/// * **1** -- Phase 2. The reductions the request path used to perform, moved
///   here unchanged.
/// * **2** -- Phase 5. One engine: Hyndman-Fan R8 percentiles over
///   equal-duration buckets, valuation bands, IQR/MAD, evidence gates and the
///   card sparkline. A row written by 1 has none of those columns filled, so
///   the startup backfill treats it as absent rather than as analysis.
pub const ALGORITHM_VERSION: u32 = 2;

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
    /// The cheapest and dearest observation, with when. Distinct from the
    /// distribution's P5 and P95 below: an extreme is one hour that happened,
    /// a tail percentile is where the distribution thins out, and a card that
    /// showed one under the other's name would be wrong twice.
    pub low: Copper,
    pub low_at: Millis,
    pub high: Copper,
    pub high_at: Millis,
    pub mean: Copper,
    /// The five-number summary and the robust spreads, over equal-duration
    /// buckets. The engine's, so the median here is the median everywhere.
    pub distribution: Distribution,
    /// Where the market's current price sits in this window.
    pub position: Position,
    /// `(max - min) / mean`, named for what it is (§5.4).
    pub swing: Swing,
    /// The card's shape of this window: equal-duration slots, gaps kept.
    pub spark: Spark,
    /// The analysis page's chart, at fixed resolution: the observation, the
    /// rolling band around it, stock, listings, and which slots hold nothing.
    /// Reduced here so that drawing it is a read (§16, Phase 6).
    pub series: ChartSeries,
    /// How this window's prices were distributed -- the shape the valuation
    /// band is a rank inside. `None` where there is nothing to bin.
    pub histogram: Option<Histogram>,
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
        .filter_map(|window| summarise(key, window, history, Some(state.price), catalog, now))
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
    current: Option<Copper>,
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

    // One value per hour, which is what a historical percentile weights
    // equally (§5.1). The count below is therefore buckets rather than rows:
    // a market collected twice in an hour has one hour of evidence.
    let buckets =
        Buckets::from_observations(inside.iter().map(|s| (s.observed_at, s.p05_unit_price)));
    let distribution = Distribution::of(&buckets)?;

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

    let expected = window.expected_buckets(catalog, now);

    // Coverage for the *gate* is measured against the hours this market has
    // existed inside the window, not against the window's nominal length.
    //
    // The distinction is the one `largest_gap_ms` already makes below, and
    // leaving it out here was a bug with teeth: on a three-day-old archive,
    // every market's 14-day coverage is 57 hours out of 336, which is 17% --
    // under any sane threshold -- so every card on every page refused its
    // valuation band. Not because the data has holes in it, but because the
    // window is longer than the archive. A market that started trading
    // yesterday has not missed a fortnight; it has a fortnight of not
    // existing, and §2 does not let those be the same number.
    //
    // `expected_buckets` below is still the window's nominal length: that is
    // what the data-quality panel reports, and "57 of 336 hours" is a true
    // and useful sentence. It is just not the question the gate asks.
    let lived_from = from.max(inside[0].observed_at);
    let lived_until = until.unwrap_or(now);
    let lived_hours = lived_until
        .get()
        .saturating_sub(lived_from.get())
        .div_ceil(60 * 60 * 1000);
    let coverage =
        (lived_hours > 0).then(|| ((hours.len() as u64 * 100) / lived_hours).min(100) as u32);

    // Across the window the reader chose, not across what happened to be
    // observed: a card's line and a card's percentile describe the same
    // interval, or the shape is answering a different question from the
    // number beside it. `Window::All` starts at the epoch and has no drawable
    // span, so it starts where the market did.
    let spark_from = if from == Millis::ZERO {
        inside[0].observed_at
    } else {
        from
    };
    let spark = Spark::over(
        inside.iter().map(|s| (s.observed_at, s.p05_unit_price)),
        spark_from,
        until.unwrap_or(now),
    );

    // The analysis page's chart, over the same span the sparkline covers, so
    // that the small picture on the card and the large one on the page are the
    // same picture. Reduced once here rather than per view: the item page
    // called `downsample` on every request, which is a small reduction and
    // still the one Phase 6 names.
    let series = ChartSeries::over(
        inside.iter().map(|s| Observation {
            at: s.observed_at,
            price: s.p05_unit_price,
            quantity: s.quantity,
            listings: s.listings,
        }),
        spark_from,
        until.unwrap_or(now),
    );
    let histogram = Histogram::of(&buckets);

    Some(MarketWindow {
        key,
        window: window.clone(),
        low: low.p05_unit_price,
        low_at: low.observed_at,
        high: high.p05_unit_price,
        high_at: high.observed_at,
        mean,
        position: Position::of(
            // Where *today's* price sits in this window. A window with no
            // current price -- a market that has stopped trading -- still has
            // a distribution, and says it has no position rather than placing
            // a price it does not have.
            current.unwrap_or(distribution.median),
            &buckets,
            coverage,
            Gates::default(),
        ),
        distribution,
        swing: Swing::of(&buckets),
        spark,
        series,
        histogram,
        samples: inside.len() as u32,
        first_at: inside[0].observed_at,
        last_at: inside[inside.len() - 1].observed_at,
        expected_buckets: expected,
        observed_buckets: hours.len() as u32,
        largest_gap_ms,
    })
}

// --- per-realm markets, rolled up ------------------------------------------
//
// A commodity market is one price for a region, so its stored row is the whole
// answer. A gear or recipe market is one price *per connected realm*, and both
// the card and the analysis page ask about a region's worth of them at once:
// "what is the cheapest Veteran copy anywhere in EU, at what item levels, with
// how many listings behind it". That question is a roll-up over markets rather
// than a market, so it is a read-model row of its own -- `docs/market-analysis`
// calls these category-card facts, and §3 keeps `MarketKey` for real markets.
//
// The same row shape serves one realm, because "one realm" is the same
// question with one market in it. That is what stops the page having two
// implementations of everything it shows.

use super::Region;
use super::catalog::{ItemLevel, Track};
use super::realm::{RealmId, RealmSample};

/// One item level inside a track, and what it costs.
///
/// The track is the market and the ranks inside it are not (§8), so these are
/// a breakdown of one market rather than several. Levels the catalogue cannot
/// name are left out rather than shown as zero: the sync script resolves them,
/// and a level of 0 is a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelStat {
    pub item_level: u16,
    /// "Champion 2/6", the catalogue's own wording.
    pub upgrade: String,
    pub cheapest: Copper,
    pub highest: Copper,
    pub listings: u32,
    /// How many connected realms list it. Always 1 in a realm-scoped roll-up.
    pub realms: u32,
}

/// How common one socket or tertiary is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifierStat {
    pub name: String,
    /// Listings carrying it in the newest snapshot.
    pub now: u32,
    /// Listings carrying it across the window.
    pub seen: u32,
}

/// What a roll-up covers.
///
/// A sentinel rather than a nullable realm: this is half a primary key, and
/// SQLite treats NULLs in a unique index as distinct, which would let the same
/// region's roll-up be written twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Every connected realm in the region.
    Region,
    /// One connected realm.
    Realm(RealmId),
}

impl Scope {
    /// The stored form. Realm ids start well above zero, so zero is free to
    /// mean "all of them".
    pub const fn realm_id(self) -> u32 {
        match self {
            Scope::Region => 0,
            Scope::Realm(realm) => realm.get(),
        }
    }

    pub const fn parse(realm_id: u32) -> Scope {
        match realm_id {
            0 => Scope::Region,
            id => Scope::Realm(RealmId(id)),
        }
    }
}

/// A region's -- or one realm's -- worth of one per-realm market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketRollup {
    pub region: Region,
    pub item: ItemId,
    /// Which per-realm market this is. Carried rather than inferred: a recipe
    /// and a BoE on a track no catalogue names both have no track, and they
    /// are not the same thing.
    pub kind: ItemKind,
    /// `None` for a recipe, which has one version of itself, and for a track
    /// no catalogue names.
    pub track: Option<Track>,
    pub scope: Scope,
    /// The interval the figures cover.
    pub window: Window,

    /// The newest observation anywhere in scope.
    pub observed_at: Option<Millis>,
    /// Distinct snapshot instants in the window. What "how much evidence"
    /// means for a market collected per realm on its own schedule.
    pub snapshots: u32,
    /// Connected realms with a listing in the newest snapshot.
    pub realms_listing: u32,

    /// Over the newest snapshot of each realm.
    ///
    /// Three different questions, and the page asks all three. A *realm's*
    /// price is its cheapest copy -- that is what you would pay there -- so
    /// the cheapest, the median and the dearest of those describe the spread
    /// *across realms*, which is what a card shows and which realm to fly to.
    /// The dearest *listing* is a different fact: it is the spread *within*
    /// the market, which is all there is to see once a realm is chosen.
    /// Collapsing them would make a card name a realm for a price nobody
    /// there is charging.
    pub cheapest_now: Option<Copper>,
    pub cheapest_realm: Option<RealmId>,
    /// The dearest of the realms' cheapest copies.
    pub dearest_realm_now: Option<Copper>,
    pub dearest_realm: Option<RealmId>,
    /// The median of the realms' cheapest copies: a card's headline figure.
    pub median_realm_now: Option<Copper>,
    /// The dearest listing anywhere in scope.
    pub highest_now: Option<Copper>,
    /// Over the whole window.
    pub cheapest_ever: Option<Copper>,
    pub highest_ever: Option<Copper>,
    pub listings_now: u32,
    /// Every listing seen across the window: the denominator a modifier's
    /// share is a share of.
    pub listings_seen: u32,

    /// "279-285", or empty where no level resolves.
    pub level_range: String,
    pub levels: Vec<LevelStat>,
    pub modifiers: Vec<ModifierStat>,
    /// One point per snapshot: the price is the median of what the realms in
    /// scope charge for the cheapest copy, the quantity is their listings
    /// summed. Both charts on the page are drawn from this one series.
    pub series: Vec<Point>,
    /// The same five-number summary and robust spreads a commodity window
    /// carries, over the same equal-duration buckets. A per-realm market had
    /// its own reduction and no percentile at all before Phase 5; now a gear
    /// card and a consumable card mean the same thing by "cheap".
    ///
    /// `None` where the window holds nothing to summarise.
    pub distribution: Option<Distribution>,
    /// Where the cheapest copy now sits in that history.
    pub position: Option<Position>,
    pub swing: Swing,
}

impl MarketRollup {
    /// A market nothing has ever been listed on.
    ///
    /// A tracked piece with no auctions anywhere is a real answer, and the
    /// page needs one shape whether or not the read model has a row for it --
    /// two branches per figure is how a page ends up rendering a zero.
    pub fn empty(
        region: Region,
        item: ItemId,
        kind: ItemKind,
        track: Option<Track>,
    ) -> MarketRollup {
        MarketRollup {
            region,
            item,
            kind,
            track,
            scope: Scope::Region,
            window: Window::All,
            observed_at: None,
            snapshots: 0,
            realms_listing: 0,
            cheapest_now: None,
            cheapest_realm: None,
            dearest_realm_now: None,
            dearest_realm: None,
            median_realm_now: None,
            highest_now: None,
            cheapest_ever: None,
            highest_ever: None,
            listings_now: 0,
            listings_seen: 0,
            level_range: String::new(),
            levels: Vec::new(),
            modifiers: Vec::new(),
            series: Vec::new(),
            distribution: None,
            position: None,
            swing: Swing(0),
        }
    }
}

/// Roll up one region's per-realm history.
///
/// `history` is every observation in the window for the markets being rolled
/// up, in any order. Grouped by item and track here rather than by the caller,
/// because which track a variant belongs to is a catalogue rule and this is
/// the side of the wall the catalogue is on.
///
/// Produces one row for the region and one for each realm that has any
/// history, for every (item, track) present. `window` names the interval the
/// caller already narrowed `history` to; nothing here filters by time, because
/// the store did it with an index.
pub fn rollups(history: &[RealmSample], catalog: &Catalog, window: &Window) -> Vec<MarketRollup> {
    let mut grouped: BTreeMap<(Region, ItemId, ItemKind, Option<Track>), Vec<&RealmSample>> =
        BTreeMap::new();
    for sample in history {
        // An item the catalogue no longer lists is treated as gear, because
        // that is the shape the per-realm table holds. Its history stays
        // addressable either way, which is what an archive is for.
        let kind = catalog
            .find(sample.item)
            .map(|e| e.kind)
            .unwrap_or(ItemKind::Boe);
        let track = match kind {
            ItemKind::Recipe => None,
            _ => catalog.track_in(&sample.variant),
        };
        grouped
            .entry((sample.region, sample.item, kind, track))
            .or_default()
            .push(sample);
    }

    // When each realm last had a snapshot of each item, across every track.
    //
    // Computed here rather than inside a group, and that distinction is the
    // whole of "is this still on sale". One snapshot covers a realm's whole
    // auction house, so a variant whose newest row is older than its realm's
    // newest row for the item was *delisted* -- somebody bought it or pulled
    // it. Deciding within a track cannot see that: a track nobody lists any
    // more would keep reporting its last known listings for ever, which is
    // what both pages used to do.
    let mut newest: BTreeMap<(Region, ItemId, RealmId), Millis> = BTreeMap::new();
    for sample in history {
        let at = newest
            .entry((sample.region, sample.item, sample.realm))
            .or_insert(sample.observed_at);
        *at = (*at).max(sample.observed_at);
    }

    let mut out = Vec::new();
    for ((region, item, kind, track), samples) in grouped {
        out.push(roll(
            region,
            item,
            kind,
            track,
            Scope::Region,
            &samples,
            &newest,
            catalog,
            window,
        ));

        let mut realms: Vec<RealmId> = samples.iter().map(|s| s.realm).collect();
        realms.sort();
        realms.dedup();
        for realm in realms {
            let mine: Vec<&RealmSample> = samples
                .iter()
                .copied()
                .filter(|s| s.realm == realm)
                .collect();
            out.push(roll(
                region,
                item,
                kind,
                track,
                Scope::Realm(realm),
                &mine,
                &newest,
                catalog,
                window,
            ));
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn roll(
    region: Region,
    item: ItemId,
    kind: ItemKind,
    track: Option<Track>,
    scope: Scope,
    samples: &[&RealmSample],
    newest: &BTreeMap<(Region, ItemId, RealmId), Millis>,
    catalog: &Catalog,
    window: &Window,
) -> MarketRollup {
    // "Now" is per realm, because realms are generated on their own schedules
    // and the newest observation overall would silently drop every realm that
    // had not refreshed yet. It is per realm across *every* track of the item,
    // because one snapshot covers the whole auction house: a variant older
    // than its realm's newest row for this item is one nobody is selling.
    let current: Vec<&RealmSample> = samples
        .iter()
        .copied()
        .filter(|s| newest.get(&(s.region, s.item, s.realm)) == Some(&s.observed_at))
        .collect();

    // A realm's price is its cheapest copy. One realm may list the same track
    // several times, so this is per realm before it is anything else.
    let mut per_realm: BTreeMap<RealmId, Copper> = BTreeMap::new();
    for sample in &current {
        let slot = per_realm.entry(sample.realm).or_insert(sample.min_price);
        *slot = (*slot).min(sample.min_price);
    }
    let mut across: Vec<Copper> = per_realm.values().copied().collect();
    across.sort_unstable();
    let cheapest_realm = per_realm
        .iter()
        .min_by_key(|(realm, price)| (price.get(), realm.get()))
        .map(|(realm, _)| *realm);
    // Ties broken by realm id at both ends, so the answer is stable rather
    // than whichever the map happened to yield. Which realm is named when two
    // charge the same is arbitrary; that it is the same one every time is not.
    let dearest_realm = per_realm
        .iter()
        .max_by_key(|(realm, price)| (price.get(), realm.get()))
        .map(|(realm, _)| *realm);

    let cheapest_now = across.first().copied();
    // Rows written before `max_price` existed carry zero, which is not a
    // price: fall back to the cheapest rather than reporting nothing.
    let highest_now = current
        .iter()
        .map(|s| s.max_price)
        .max()
        .filter(|p| p.get() > 0)
        .or(cheapest_now);
    let cheapest_ever = samples.iter().map(|s| s.min_price).min();
    let highest_ever = samples
        .iter()
        .map(|s| s.max_price)
        .max()
        .filter(|p| p.get() > 0)
        .or(cheapest_ever);

    // The median of what the realms in scope charge for their cheapest copy:
    // a card's headline figure, and the one the distribution below is built
    // from. Hoisted out of the struct because the position has to rank the
    // same number the cell prints.
    let median_realm_now = across.get(across.len() / 2).copied();

    // One point per snapshot, before thinning: a percentile is over every
    // bucket, and thinning first would be measuring the chart rather than the
    // market.
    let points = series(samples);
    let buckets = Buckets::from_observations(points.iter().map(|p| (p.at, p.price)));
    let distribution = Distribution::of(&buckets);

    MarketRollup {
        region,
        item,
        kind,
        track,
        scope,
        window: window.clone(),
        observed_at: samples.iter().map(|s| s.observed_at).max(),
        snapshots: samples
            .iter()
            .map(|s| s.observed_at)
            .collect::<BTreeSet<_>>()
            .len() as u32,
        realms_listing: current
            .iter()
            .map(|s| s.realm)
            .collect::<BTreeSet<_>>()
            .len() as u32,
        cheapest_now,
        cheapest_realm,
        dearest_realm_now: across.last().copied(),
        dearest_realm,
        median_realm_now,
        highest_now,
        cheapest_ever,
        highest_ever,
        listings_now: current.iter().map(|s| s.listings).sum(),
        listings_seen: samples.iter().map(|s| s.listings).sum(),
        level_range: catalog.level_range(current.iter().map(|s| s.variant.as_str())),
        levels: levels(&current, catalog),
        modifiers: modifiers(samples, &current, catalog),
        series: analysis::downsample(&points, CHART_POINTS),
        distribution,
        position: distribution.map(|distribution| {
            Position::of(
                // **The figure the card headlines, not the cheapest one.**
                //
                // `buckets` is built from `series`, whose price is the median
                // of what the realms in scope charge for their cheapest copy.
                // Ranking `cheapest_now` inside that distribution compares the
                // cheapest realm against a history of median realms, which is
                // two different questions -- and it answers the same way every
                // time, because the cheapest of a set is below its median by
                // construction. On the real archive it made all 27 gear cells
                // read "Very cheap, P0", which is a card with a verdict that
                // carries no information.
                //
                // `median_realm_now` is the price the cell prints across
                // realms, and on a single realm it *is* that realm's cheapest,
                // so one rule serves both scopes -- which is what stops the
                // page having two implementations of everything it shows.
                median_realm_now
                    .or(cheapest_now)
                    .unwrap_or(distribution.median),
                &buckets,
                None,
                Gates::default(),
            )
        }),
        swing: Swing::of(&buckets),
    }
}

/// The track broken apart by item level.
///
/// ilvl 311 going for less than an ilvl 305 is exactly the thing worth seeing,
/// and it is a breakdown rather than a split: they are one market.
fn levels(current: &[&RealmSample], catalog: &Catalog) -> Vec<LevelStat> {
    let mut by_level: BTreeMap<u16, (&ItemLevel, Vec<&RealmSample>)> = BTreeMap::new();
    for sample in current {
        let Some(level) = catalog.rank_in(&sample.variant) else {
            continue;
        };
        by_level
            .entry(level.item_level)
            .or_insert_with(|| (level, Vec::new()))
            .1
            .push(sample);
    }

    by_level
        .into_iter()
        .map(|(item_level, (level, samples))| {
            let cheapest = samples
                .iter()
                .map(|s| s.min_price)
                .min()
                .unwrap_or_default();
            let highest = samples
                .iter()
                .map(|s| s.max_price)
                .max()
                .filter(|p| p.get() > 0)
                .unwrap_or(cheapest);
            LevelStat {
                item_level,
                upgrade: level.upgrade.clone(),
                cheapest,
                highest,
                listings: samples.iter().map(|s| s.listings).sum(),
                realms: samples
                    .iter()
                    .map(|s| s.realm)
                    .collect::<BTreeSet<_>>()
                    .len() as u32,
            }
        })
        .collect()
}

/// How common each socket or tertiary is: in the newest snapshot, and across
/// the window.
fn modifiers(
    samples: &[&RealmSample],
    current: &[&RealmSample],
    catalog: &Catalog,
) -> Vec<ModifierStat> {
    let mut now: BTreeMap<&str, u32> = BTreeMap::new();
    for sample in current {
        for name in catalog.modifier_names(&sample.variant) {
            *now.entry(name).or_default() += sample.listings;
        }
    }
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    for sample in samples {
        for name in catalog.modifier_names(&sample.variant) {
            *seen.entry(name).or_default() += sample.listings;
        }
    }
    seen.into_iter()
        .map(|(name, count)| ModifierStat {
            name: name.to_string(),
            now: now.get(name).copied().unwrap_or(0),
            seen: count,
        })
        .collect()
}

/// One point per snapshot: the median of what the realms in scope charge for
/// the cheapest copy, and their listings summed.
///
/// The median rather than the minimum, because a line of "the single cheapest
/// realm" is a line about whichever realm was having a bad day. Returned
/// un-thinned: the caller thins it for the chart and takes percentiles over
/// all of it, which are different needs.
fn series(samples: &[&RealmSample]) -> Vec<Point> {
    let mut by_instant: BTreeMap<Millis, Vec<&RealmSample>> = BTreeMap::new();
    for sample in samples {
        by_instant
            .entry(sample.observed_at)
            .or_default()
            .push(sample);
    }
    let points: Vec<Point> = by_instant
        .into_iter()
        .map(|(at, at_instant)| {
            let mut cheapest: Vec<Copper> = at_instant.iter().map(|s| s.min_price).collect();
            cheapest.sort_unstable();
            Point {
                at,
                price: cheapest
                    .get(cheapest.len() / 2)
                    .copied()
                    .unwrap_or_default(),
                quantity: at_instant.iter().map(|s| s.listings as u64).sum(),
            }
        })
        .collect();
    points
}
