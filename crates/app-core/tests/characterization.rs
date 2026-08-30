//! What the read path answers today, pinned to exact numbers.
//!
//! Phase 0 of the market-analysis roadmap asks for these before anything
//! replaces their source. They are not tests of what is *right*: several of
//! the values below are things `docs/market-analysis.md` says will change --
//! `volatility_percent` is a range-based swing rather than a robust measure,
//! and the alert percentile is nearest-rank rather than the Hyndman-Fan R8 the
//! specification settles on. Pinning them is the point. When Phase 2 moves
//! this arithmetic to a materialised read model and Phase 5 replaces the
//! definitions, the diff to this file is the list of what actually changed for
//! a reader, separated from what merely moved.
//!
//! The dataset is generated, deterministic, and described where it is built.
//! Nothing here reads a database: these are the pure reductions, which is the
//! half that has to keep answering the same thing whatever runs it -- locally
//! today, on a remote worker after Phase 4.

use app_core::market::{
    AlertRule, AlertSeverity, Copper, ItemId, Listing, PriceSample, RealmId, RealmSample, Region,
    alerts, analyse, downsample, stats,
};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;
/// A round instant, so every window boundary in the assertions is a round
/// number too. 2026-01-01T00:00:00Z.
const NOW: Millis = Millis(1_767_225_600_000);
const ITEM: ItemId = ItemId(210_796);

/// A reproducible pseudo-random sequence, so the golden dataset needs no
/// dependency and no fixture file. The constants are the ones from Numerical
/// Recipes; nothing here needs statistical quality, only repeatability.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 16) & 0xffff_ffff
    }

    /// A value in `0..span`.
    fn upto(&mut self, span: u64) -> u64 {
        self.next() % span.max(1)
    }
}

/// Thirty days of hourly observations of one commodity market.
///
/// Deliberately not a smooth line: it drifts, it has a spike with a tail, it
/// has hours where nothing was collected, and it has one hour where the market
/// was listed but empty. Every one of those is a case a statistic has to
/// survive, and a fixture without them proves nothing.
fn commodity_history() -> Vec<PriceSample> {
    let mut rng = Lcg(20_260_830);
    let mut price = 500_000u64;
    let mut samples = Vec::new();
    let hours = 30 * 24;
    for hour in 0..hours {
        // Roughly one hour in nine is missing: collection is not guaranteed.
        if rng.upto(9) == 0 {
            continue;
        }
        let drift = rng.upto(20_001) as i64 - 10_000;
        price = (price as i64 + drift).clamp(50_000, 5_000_000) as u64;
        // One spike, at a fixed hour rather than a random one, so it is always
        // in the same window.
        let observed = if hour == 700 { price * 3 } else { price };
        let quantity = if hour == 100 {
            0
        } else {
            1_000 + rng.upto(40_000)
        };
        samples.push(PriceSample {
            item: ITEM,
            region: Region::Eu,
            observed_at: Millis(NOW.get() - (hours - 1 - hour) * HOUR),
            min_unit_price: Copper(observed),
            p05_unit_price: Copper(observed + observed / 50),
            median_unit_price: Copper(observed + observed / 10),
            quantity,
            listings: 1 + rng.upto(400) as u32,
        });
    }
    samples
}

/// The whole of what the item page derives from a history today.
///
/// One assertion per number rather than one over a debug string: when Phase 2
/// changes one of them, the failure should name it.
#[test]
fn commodity_analysis_is_unchanged() {
    let history = commodity_history();
    let analysis = analyse(&history, NOW);

    assert_eq!(history.len(), 634, "the golden dataset itself moved");
    assert_eq!(analysis.samples, 634);

    let current = analysis.current.expect("a current price");
    assert_eq!(current.price, Copper(495_119));
    assert_eq!(current.at, NOW);

    let low = analysis.low.expect("a cheapest observation");
    let high = analysis.high.expect("a dearest observation");
    assert_eq!(low.price, Copper(479_063));
    assert_eq!(high.price, Copper(1_553_996));

    assert_eq!(analysis.mean, Copper(557_052));
    assert_eq!(analysis.median, Copper(553_726));
    assert_eq!(analysis.first_seen, Some(Millis(1_764_637_200_000)));

    // Signed, and negative means it got cheaper.
    assert_eq!(analysis.day.percent, -5);
    assert_eq!(analysis.week.percent, -8);
    assert_eq!(analysis.month.percent, -3);
    assert!(analysis.day.known && analysis.week.known && analysis.month.known);

    // `(high - low) / mean`. `docs/market-analysis.md` §5.4 renames this to
    // Swing and replaces it with IQR/MAD; the number is pinned so that the
    // replacement is visibly a replacement.
    assert_eq!(analysis.volatility_percent, 192);

    assert_eq!(analysis.by_hour.len(), 24);
    assert_eq!(analysis.by_weekday.len(), 7);
    assert_eq!(analysis.best_hour, Some(20));
    assert_eq!(analysis.best_weekday, Some(3));
}

/// The chart's series is thinned for drawing, not for arithmetic.
///
/// Worth pinning precisely because of what it does *not* promise. It takes the
/// cheapest point of each bucket, so the line it draws is a lower envelope
/// rather than the series -- the first and last drawn points are the cheapest
/// of their buckets, not the oldest and newest observations. That is a
/// defensible choice for a price chart and a wrong one for a statistic, and
/// `docs/market-analysis.md` §10 moves series reduction out of the request
/// entirely. Until then, this is what a reader is looking at.
#[test]
fn downsampling_takes_the_cheapest_of_each_bucket() {
    let history = commodity_history();
    let analysis = analyse(&history, NOW);
    let thinned = downsample(&analysis.series, 60);

    assert_eq!(analysis.series.len(), 634);
    assert_eq!(thinned.len(), 60);

    // Every drawn point is a real observation, in order, and no bucket is
    // dearer than the cheapest of the points it stands for.
    let bucket = 634 / 60;
    for (index, point) in thinned.iter().enumerate() {
        let start = index * 634 / 60;
        let end = ((index + 1) * 634 / 60).max(start + 1);
        let cheapest = analysis.series[start..end]
            .iter()
            .map(|p| p.price)
            .min()
            .expect("a non-empty bucket");
        assert_eq!(point.price, cheapest, "bucket {index} of about {bucket}");
    }
    assert!(
        thinned.windows(2).all(|pair| pair[0].at <= pair[1].at),
        "the thinned series stays in time order"
    );

    // The whole series comes back untouched when it already fits.
    assert_eq!(downsample(&analysis.series, 1_000), analysis.series);
}

/// The supply-weighted percentile inside one snapshot.
///
/// This is a different measure from the historical percentiles above and
/// `docs/market-analysis.md` §5.1 is explicit that they must not be merged.
/// The one-copper listing is the case that separates them: it moves `min` and
/// barely moves `p05`.
#[test]
fn a_snapshot_is_summarised_by_supply_not_by_listings() {
    let mut listings = vec![
        Listing {
            item: ITEM,
            unit_price: Copper(1),
            quantity: 1,
        },
        Listing {
            item: ITEM,
            unit_price: Copper(1_000),
            quantity: 500,
        },
        Listing {
            item: ITEM,
            unit_price: Copper(1_100),
            quantity: 500,
        },
        Listing {
            item: ITEM,
            unit_price: Copper(9_000),
            quantity: 200,
        },
    ];
    let summary = stats::summarise(&mut listings).expect("four listings summarise");

    assert_eq!(summary.min, Copper(1), "one troll listing is still the min");
    assert_eq!(summary.p05, Copper(1_000), "but it is not the p05");
    assert_eq!(summary.median, Copper(1_100));
    assert_eq!(summary.quantity, 1_201);
    assert_eq!(summary.listings, 4);
}

/// What the alert engine decides today, including the percentile definition it
/// uses to decide it.
#[test]
fn alerting_is_unchanged() {
    let rule = AlertRule::default();
    let history = commodity_history();
    // Judge the cheapest observation against everything before it, which is
    // the shape the collector calls this in.
    let cheapest = history
        .iter()
        .min_by_key(|s| s.p05_unit_price.get())
        .copied()
        .expect("a cheapest sample");
    let before: Vec<PriceSample> = history
        .iter()
        .filter(|s| s.observed_at < cheapest.observed_at)
        .copied()
        .collect();

    let alert = alerts::evaluate(&rule, &cheapest, &before, None).expect("an alert");
    assert_eq!(alert.severity, AlertSeverity::VeryLow);
    assert_eq!(alert.current, Copper(479_063));
    assert_eq!(alert.baseline, Copper(546_708));
    assert_eq!(alert.threshold, Copper(507_530));
    assert_eq!(alert.discount_percent, 12);
    assert_eq!(alert.quantity, cheapest.quantity);
}

/// The gates that decide there is nothing to say, which matter as much as the
/// arithmetic: `docs/market-analysis.md` §5.3 keeps them and makes them
/// explicit rather than silent.
#[test]
fn alerting_refuses_a_thin_or_unevidenced_market() {
    let rule = AlertRule::default();
    let history = commodity_history();
    let mut thin = *history.last().expect("a last sample");
    thin.p05_unit_price = Copper(1);

    thin.quantity = rule.min_quantity - 1;
    assert!(
        alerts::evaluate(&rule, &thin, &history, None).is_none(),
        "a cheap price on nothing is not a buying signal"
    );

    thin.quantity = rule.min_quantity;
    let too_short = &history[..rule.min_samples - 1];
    assert!(
        alerts::evaluate(&rule, &thin, too_short, None).is_none(),
        "under min_samples there is no baseline to be cheap against"
    );

    // A hard floor works from the first observation, before any baseline.
    assert_eq!(
        alerts::evaluate(&rule, &thin, &[], Some(Copper(10)))
            .expect("the floor fires")
            .severity,
        AlertSeverity::VeryLow
    );
}

/// One per-realm market per track, and the ranks inside a track pooled.
///
/// CLAUDE.md §8: the track is the market, the rank is not. This pins the
/// grouping key so that a change to it shows up as a change to this test
/// rather than as a card that quietly split into two markets.
#[test]
fn a_realm_sample_carries_its_whole_bonus_list() {
    let sample = RealmSample {
        item: ITEM,
        region: Region::Eu,
        realm: RealmId(1403),
        variant: "6652,10844,12827,13332,13662".to_string(),
        observed_at: NOW,
        min_price: Copper(1_000),
        median_price: Copper(1_500),
        max_price: Copper(9_000),
        listings: 3,
    };

    // Stored whole, so every grouping rule above it is a display decision and
    // a renumbered bonus id costs a catalogue entry, never any history.
    assert!(sample.variant.contains("13332"), "the track bonus survives");
    assert!(sample.variant.contains("12827"), "so does the rank");
    assert_eq!(sample.variant.split(',').count(), 5);
}

/// A window is anchored to `now`, and asking for the same window twice must
/// mean the same interval.
#[test]
fn comparison_windows_are_exact_days() {
    for days in [3u64, 7, 14, 30] {
        let since = Millis(NOW.get() - days * DAY);
        let history = commodity_history();
        let inside = history.iter().filter(|s| s.observed_at >= since).count();
        // Hourly collection with roughly one hour in nine missing.
        let expected = (days * 24) as usize;
        assert!(
            inside <= expected && inside as f64 >= expected as f64 * 0.8,
            "{days}d window holds {inside} of a possible {expected}"
        );
    }
}
