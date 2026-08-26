//! Item analysis: the numbers behind the cards and the charts.

use app_core::market::{Copper, ItemId, PriceSample, Region, analysis, downsample};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

fn sample(at: u64, price: u64, quantity: u64) -> PriceSample {
    PriceSample {
        item: ItemId(1),
        region: Region::Eu,
        observed_at: Millis(at),
        min_unit_price: Copper(price),
        p05_unit_price: Copper(price),
        median_unit_price: Copper(price * 2),
        quantity,
        listings: 5,
    }
}

#[test]
fn an_empty_series_analyses_to_nothing_rather_than_panicking() {
    let a = analysis::analyse(&[], Millis(0));
    assert_eq!(a.samples, 0);
    assert!(a.current.is_none() && a.low.is_none() && a.high.is_none());
    assert_eq!(a.mean, Copper::ZERO);
    assert!(!a.day.known);
    assert!(a.series.is_empty());
    assert!(a.best_hour.is_none());
}

#[test]
fn extremes_carry_the_moment_they_happened() {
    // "Cheapest ever" is only actionable with a date attached.
    let samples = vec![
        sample(10 * HOUR, 900, 100),
        sample(20 * HOUR, 300, 100),
        sample(30 * HOUR, 1_500, 100),
        sample(40 * HOUR, 800, 100),
    ];
    let a = analysis::analyse(&samples, Millis(40 * HOUR));

    assert_eq!(a.low.unwrap().price, Copper(300));
    assert_eq!(a.low.unwrap().at, Millis(20 * HOUR));
    assert_eq!(a.high.unwrap().price, Copper(1_500));
    assert_eq!(a.high.unwrap().at, Millis(30 * HOUR));
    assert_eq!(
        a.current.unwrap().price,
        Copper(800),
        "newest, not last given"
    );
    assert_eq!(a.mean, Copper(875));
    assert_eq!(a.samples, 4);
}

#[test]
fn out_of_order_input_is_sorted_before_anything_is_derived() {
    let jumbled = vec![
        sample(30 * HOUR, 300, 1),
        sample(10 * HOUR, 100, 1),
        sample(20 * HOUR, 200, 1),
    ];
    let a = analysis::analyse(&jumbled, Millis(30 * HOUR));
    assert_eq!(a.current.unwrap().price, Copper(300));
    assert_eq!(a.first_seen, Some(Millis(10 * HOUR)));
    let times: Vec<u64> = a.series.iter().map(|p| p.at.get()).collect();
    assert_eq!(times, vec![10 * HOUR, 20 * HOUR, 30 * HOUR]);
}

#[test]
fn the_mean_and_median_diverge_when_a_spike_is_present() {
    // Reporting both is the point: a gap between them means an outlier.
    let mut samples: Vec<PriceSample> = (0..20).map(|i| sample(i * HOUR, 100, 1)).collect();
    samples.push(sample(21 * HOUR, 100_000, 1));
    let a = analysis::analyse(&samples, Millis(21 * HOUR));
    assert_eq!(a.median, Copper(100));
    assert!(
        a.mean.get() > 4_000,
        "mean dragged by the spike: {}",
        a.mean
    );
}

#[test]
fn trends_compare_against_the_oldest_point_inside_the_window() {
    let samples = vec![
        sample(0, 1_000, 1),       // 40h ago: outside the 24h window
        sample(20 * HOUR, 800, 1), // 20h ago: inside it
        sample(40 * HOUR, 400, 1), // now
    ];
    let a = analysis::analyse(&samples, Millis(40 * HOUR));

    assert!(a.day.known);
    assert_eq!(a.day.from, Copper(800), "the 24h window starts at 16h");
    assert_eq!(a.day.to, Copper(400));
    assert_eq!(a.day.percent, -50);

    assert!(a.week.known);
    assert_eq!(a.week.from, Copper(1_000), "a week reaches the first point");
    assert_eq!(a.week.percent, -60);
}

#[test]
fn a_single_observation_has_no_trend() {
    let a = analysis::analyse(&[sample(5 * HOUR, 100, 1)], Millis(5 * HOUR));
    assert!(!a.day.known, "nothing to compare against");
    assert_eq!(a.current.unwrap().price, Copper(100));
}

#[test]
fn the_cheapest_hour_of_day_is_found() {
    // Thirty days, with a deliberate dip between 02:00 and 05:00.
    let samples: Vec<PriceSample> = (0..30 * 24)
        .map(|i| {
            let at = i * HOUR;
            let hour = (at / HOUR) % 24;
            let price = if (2..=5).contains(&hour) { 500 } else { 1_000 };
            sample(at, price, 100)
        })
        .collect();
    let a = analysis::analyse(&samples, Millis(30 * DAY));

    assert_eq!(a.by_hour.len(), 24);
    assert_eq!(a.by_hour[3].mean, Copper(500), "03:00 sits in the dip");
    assert_eq!(a.by_hour[12].mean, Copper(1_000));
    assert!(
        matches!(a.best_hour, Some(h) if (2..=5).contains(&h)),
        "got {:?}",
        a.best_hour
    );
    assert_eq!(a.by_weekday.len(), 7);
    assert!(a.best_weekday.is_some(), "30 days covers every weekday");
}

#[test]
fn a_thin_cycle_refuses_to_name_a_best_hour() {
    // Two days cannot establish an hourly pattern; claiming one would be
    // reporting noise as a recommendation.
    let samples: Vec<PriceSample> = (0..48).map(|i| sample(i * HOUR, 100 + i, 1)).collect();
    let a = analysis::analyse(&samples, Millis(48 * HOUR));
    assert!(a.best_hour.is_none(), "only two observations per hour");
}

#[test]
fn volatility_is_the_swing_relative_to_the_mean() {
    let samples = vec![
        sample(HOUR, 500, 1),
        sample(2 * HOUR, 1_500, 1),
        sample(3 * HOUR, 1_000, 1),
    ];
    let a = analysis::analyse(&samples, Millis(3 * HOUR));
    // (1500 - 500) / 1000 = 100%
    assert_eq!(a.volatility_percent, 100);
}

// --- downsampling --------------------------------------------------------

#[test]
fn downsampling_preserves_the_dips() {
    // A chart of "what could I have paid" must not average away the bargain.
    let mut points: Vec<analysis::Point> = (0..1_000)
        .map(|i| analysis::Point {
            at: Millis(i * HOUR),
            price: Copper(1_000),
            quantity: 10,
        })
        .collect();
    points[500].price = Copper(1);

    let reduced = downsample(&points, 50);
    assert_eq!(reduced.len(), 50);
    assert!(
        reduced.iter().any(|p| p.price == Copper(1)),
        "the single cheap point survived"
    );
}

#[test]
fn downsampling_a_short_series_is_a_no_op() {
    let points: Vec<analysis::Point> = (0..10)
        .map(|i| analysis::Point {
            at: Millis(i),
            price: Copper(i),
            quantity: 1,
        })
        .collect();
    assert_eq!(downsample(&points, 50), points);
    assert_eq!(
        downsample(&points, 0),
        points,
        "a zero target is not a crash"
    );
}
