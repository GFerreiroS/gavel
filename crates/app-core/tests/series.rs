//! The chart series: fixed resolution, honest gaps, a rolling band.
//!
//! Phase 6's charts are drawn from these, and the properties below are the
//! ones a chart can quietly violate without anybody noticing -- a line drawn
//! through a week nobody collected looks exactly like a line drawn through a
//! week that was flat.

use app_core::market::Copper;
use app_core::market::engine::Buckets;
use app_core::market::series::{BINS, ChartSeries, Histogram, Observation, RESOLUTION};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;

fn observation(hour: u64, price: u64) -> Observation {
    Observation {
        at: Millis(hour * HOUR),
        price: Copper(price),
        quantity: 100 + price,
        listings: (price % 50) as u32 + 1,
    }
}

/// A series has exactly [`RESOLUTION`] slots, whatever went into it.
///
/// This is what makes it a bounded column rather than one that grows with the
/// archive behind it -- and what lets a chart assume a slot is a known
/// fraction of the window rather than measuring one.
#[test]
fn a_series_is_the_same_length_however_much_history_it_covers() {
    let short = ChartSeries::over(
        (0..5).map(|h| observation(h, 100 + h)),
        Millis(0),
        Millis(5 * HOUR),
    );
    let long = ChartSeries::over(
        (0..5_000).map(|h| observation(h, 100 + h % 97)),
        Millis(0),
        Millis(5_000 * HOUR),
    );
    assert_eq!(short.points.len(), RESOLUTION);
    assert_eq!(long.points.len(), RESOLUTION);
}

/// Slots are equal *durations*, so a gap stays a gap and keeps its width.
#[test]
fn an_uncollected_slot_is_a_gap_rather_than_an_interpolation() {
    // Two clusters with a long silence between them, which is what a
    // collector outage looks like.
    let observations = (0..10).chain(90..100).map(|h| observation(h, 1_000 + h));
    let series = ChartSeries::over(observations, Millis(0), Millis(100 * HOUR));

    assert_eq!(series.points.len(), RESOLUTION);
    let gaps = series.points.iter().filter(|p| !p.observed).count();
    assert!(
        gaps > RESOLUTION / 2,
        "most of this window was not collected; {gaps} slots say so"
    );

    // Every gap claims nothing at all. A zero here is a chart drawing a market
    // crashing to free during an outage.
    for point in series.points.iter().filter(|p| !p.observed) {
        assert_eq!(point.price, Copper::ZERO);
        assert_eq!(point.median, Copper::ZERO);
        assert_eq!(point.quantity, 0);
    }

    // And a gap still has a place on the axis, so the break is drawn in the
    // right place rather than closed up.
    let times: Vec<u64> = series.points.iter().map(|p| p.at.get()).collect();
    assert!(
        times.windows(2).all(|w| w[1] > w[0]),
        "every slot has its own instant, gaps included"
    );
}

/// The band is the engine's quantiles over the trailing slots, and it is a
/// band: P25 <= median <= P75, always.
#[test]
fn the_rolling_band_encloses_its_median() {
    let series = ChartSeries::over(
        // A drift with a spike in it, so the band has something to be wider
        // than.
        (0..200).map(|h| observation(h, if h == 120 { 9_000 } else { 1_000 + h * 3 })),
        Millis(0),
        Millis(200 * HOUR),
    );
    for point in series.points.iter().filter(|p| p.observed) {
        assert!(
            point.p25 <= point.median && point.median <= point.p75,
            "band out of order at {:?}: {:?} {:?} {:?}",
            point.at,
            point.p25,
            point.median,
            point.p75
        );
    }

    // The spike shows in the raw line and is *contained* by the band, which is
    // the whole argument for drawing both.
    let spike = series
        .points
        .iter()
        .filter(|p| p.observed)
        .max_by_key(|p| p.price.get())
        .expect("a spike");
    assert!(
        spike.median < spike.price,
        "a rolling median absorbs a single spike rather than following it"
    );
}

/// The stored form round-trips, gaps and span included.
#[test]
fn a_stored_series_comes_back_the_same() {
    let series = ChartSeries::over(
        (0..40)
            .filter(|h| h % 7 != 0)
            .map(|h| observation(h, 500 + h)),
        Millis(0),
        Millis(40 * HOUR),
    );
    let back = ChartSeries::decode(&series.encode());
    assert_eq!(back, series);
    assert_eq!(back.from, series.from);
    assert_eq!(back.until, series.until);
}

/// An unreadable record is a gap, never a zero.
#[test]
fn an_unreadable_slot_decodes_to_a_gap() {
    let raw = "0,3600000;100,100,100,100,5,1;rubbish;300,300,300,300,7,2";
    let back = ChartSeries::decode(raw);
    assert_eq!(back.points.len(), 3);
    assert!(back.points[0].observed);
    assert!(!back.points[1].observed, "unparseable is a gap");
    assert_eq!(back.points[1].price, Copper::ZERO);
    assert!(back.points[2].observed);
}

/// Nothing to draw is nothing, rather than a line at zero.
#[test]
fn an_empty_series_is_empty() {
    assert_eq!(ChartSeries::decode(""), ChartSeries::default());
    assert!(ChartSeries::default().is_empty());
    let one = ChartSeries::over([observation(1, 500)], Millis(0), Millis(10 * HOUR));
    assert!(one.is_empty(), "one observation is not a line");
}

/// A window of no duration has no series, rather than a division by zero.
#[test]
fn a_window_of_no_duration_has_no_series() {
    let series = ChartSeries::over([observation(0, 5)], Millis(5), Millis(5));
    assert!(series.points.is_empty());
}

// --- the distribution ----------------------------------------------------

/// Every bucket lands in exactly one bin, so the bars count the whole history.
#[test]
fn a_histogram_bins_every_hour_exactly_once() {
    let buckets = Buckets::from_observations(
        (0..500).map(|h| (Millis(h * HOUR), Copper(1_000 + (h * 7) % 400))),
    );
    let histogram = Histogram::of(&buckets).expect("a populated market");

    assert_eq!(histogram.bins.len(), BINS);
    assert_eq!(
        histogram.bins.iter().sum::<u32>() as usize,
        buckets.len(),
        "the bars account for every hour, no more and no less"
    );
    assert_eq!(histogram.lo.get(), 1_000);
}

/// A market that never moved is one bar, not a division by zero.
#[test]
fn a_flat_market_is_one_bar_in_the_middle() {
    let buckets = Buckets::from_observations((0..40).map(|h| (Millis(h * HOUR), Copper(700))));
    let histogram = Histogram::of(&buckets).expect("a populated market");
    assert_eq!(histogram.lo, histogram.hi);
    assert_eq!(histogram.bins[BINS / 2], 40);
    assert_eq!(histogram.bins.iter().sum::<u32>(), 40);
    assert_eq!(histogram.bin_of(Copper(700)), Some(BINS / 2));
}

/// A price outside the history has no bin, which is a real answer.
///
/// It is also the anomaly case: today's price beyond everything ever recorded
/// is exactly what the panel is for showing, and marking it against the last
/// bar would be putting it inside a range it is not in.
#[test]
fn a_price_outside_the_history_has_no_bin() {
    let buckets =
        Buckets::from_observations((0..40).map(|h| (Millis(h * HOUR), Copper(1_000 + h))));
    let histogram = Histogram::of(&buckets).expect("a populated market");
    assert_eq!(histogram.bin_of(Copper(999)), None);
    assert_eq!(histogram.bin_of(Copper(100_000)), None);
    assert!(histogram.bin_of(Copper(1_020)).is_some());
}

/// The stored form round-trips, and a corrupt one is no histogram at all.
#[test]
fn a_stored_histogram_comes_back_the_same() {
    let buckets =
        Buckets::from_observations((0..90).map(|h| (Millis(h * HOUR), Copper(400 + h * 3))));
    let histogram = Histogram::of(&buckets).expect("a populated market");
    assert_eq!(Histogram::decode(&histogram.encode()), Some(histogram));

    assert_eq!(Histogram::decode(""), None);
    assert_eq!(
        Histogram::decode("1,2,3"),
        None,
        "too few bins is not a histogram"
    );
}
