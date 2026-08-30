//! One engine, and the three distinctions it exists to keep.
//!
//! Phase 5's exit gate is that one golden dataset produces the same percentile,
//! label, median and availability everywhere it is shown. These tests are the
//! definitions half of that: they hold the arithmetic against the published
//! definition rather than against whatever the code happened to do.

use app_core::market::engine::{
    Anomaly, Buckets, Distribution, Gates, Insufficient, Position, SPARK_SLOTS, Spark, Swing,
    Valuation,
};
use app_core::market::{Copper, ItemId, Listing, stats};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;

fn buckets(prices: &[u64]) -> Buckets {
    Buckets::from_observations(
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| (Millis(i as u64 * HOUR), Copper(*p))),
    )
}

/// Hyndman and Fan's type 8, checked against the definition rather than
/// against this implementation.
///
/// `h = (n + 1/3) p + 1/3`, then linear interpolation between the neighbouring
/// order statistics. Worked by hand for a sample small enough to check:
/// n = 5, p = 0.5 gives h = 3.0 exactly, so the median is the third value.
#[test]
fn the_quantile_is_hyndman_fan_type_eight() {
    let b = buckets(&[10, 20, 30, 40, 50]);
    assert_eq!(b.quantile(0.50), Some(Copper(30)), "h = 3.0");

    // p = 0.25: h = (5 + 1/3)(0.25) + 1/3 = 1.6667, so
    // x[1] + 0.6667 (x[2] - x[1]) = 10 + 0.6667 * 10 = 16.67 -> 17.
    assert_eq!(b.quantile(0.25), Some(Copper(17)));
    // p = 0.75: h = 4.3333 -> 40 + 0.3333 * 10 = 43.33 -> 43.
    assert_eq!(b.quantile(0.75), Some(Copper(43)));

    // The ends clamp rather than extrapolating past the data. An estimator
    // that returned a price below the cheapest ever seen would be inventing
    // one.
    assert_eq!(b.quantile(0.0), Some(Copper(10)));
    assert_eq!(b.quantile(1.0), Some(Copper(50)));

    // Degenerate inputs answer or refuse, never panic.
    assert_eq!(buckets(&[]).quantile(0.5), None);
    assert_eq!(buckets(&[42]).quantile(0.5), Some(Copper(42)));
}

/// §5.1: each equal-duration bucket weighs the same. Two observations in one
/// hour are one hour of evidence, not two.
#[test]
fn an_hour_counts_once_however_often_it_was_collected() {
    let busy = Buckets::from_observations([
        (Millis(0), Copper(100)),
        (Millis(60_000), Copper(200)),
        (Millis(120_000), Copper(300)),
        (Millis(HOUR), Copper(900)),
    ]);
    assert_eq!(busy.len(), 2, "two hours, four observations");
    // The last observation in an hour is that hour's state.
    assert_eq!(busy.quantile(0.0), Some(Copper(300)));
    assert_eq!(busy.quantile(1.0), Some(Copper(900)));
}

/// §5.1 again, and the one this whole module is arranged around: the
/// supply-weighted percentile inside a snapshot and the time-weighted
/// percentile over a history are different measures with the same word in
/// their names.
#[test]
fn a_snapshot_percentile_is_not_a_historical_one() {
    // One snapshot: a troll listing of one unit at 1 copper, and real supply.
    let mut listings = vec![
        Listing {
            item: ItemId(1),
            unit_price: Copper(1),
            quantity: 1,
        },
        Listing {
            item: ItemId(1),
            unit_price: Copper(1_000),
            quantity: 500,
        },
        Listing {
            item: ItemId(1),
            unit_price: Copper(1_100),
            quantity: 500,
        },
    ];
    let snapshot = stats::summarise(&mut listings).expect("a snapshot");
    assert_eq!(
        snapshot.p05,
        Copper(1_000),
        "supply-weighted: the one-unit listing barely moves it"
    );

    // The same three numbers as a *history* of three hours weigh equally,
    // because an hour is an hour whatever was listed during it.
    let history = buckets(&[1, 1_000, 1_100]);
    assert_eq!(
        history.quantile(0.05),
        Some(Copper(1)),
        "time-weighted: an hour at 1 copper is an hour"
    );
    assert_ne!(snapshot.p05, history.quantile(0.05).unwrap());
}

/// A rank is a count of buckets, not an interpolation -- and a tie takes the
/// middle of the range it ties over.
///
/// Ten buckets, ten apart. The cheapest is one of ten, so it sits half a
/// bucket in: 5, not 10. That half-bucket is the whole point of the next test.
#[test]
fn a_rank_counts_rather_than_interpolates() {
    let b = buckets(&[10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
    assert_eq!(b.rank_of(Copper(10)), Some(5));
    assert_eq!(b.rank_of(Copper(50)), Some(45));
    assert_eq!(b.rank_of(Copper(100)), Some(95));
    assert_eq!(b.rank_of(Copper(0)), Some(0), "cheaper than anything seen");
    assert_eq!(b.rank_of(Copper(999)), Some(100));
    assert_eq!(buckets(&[]).rank_of(Copper(1)), None);
}

/// A market that has never moved is in the middle of its own history.
///
/// Counting "buckets at or below" puts it at 100, because every bucket is at
/// or below every other -- and the card then calls a price that is exactly
/// what it has always been `Very expensive`. A steady market is the most
/// ordinary thing an auction house contains.
#[test]
fn a_market_that_never_moved_is_typical() {
    let flat = buckets(&[500; 40]);
    assert_eq!(flat.rank_of(Copper(500)), Some(50));
    assert_eq!(
        Valuation::of_rank(flat.rank_of(Copper(500)).unwrap()),
        Valuation::Typical
    );

    // And a price that really is outside it still says so.
    assert_eq!(flat.rank_of(Copper(499)), Some(0));
    assert_eq!(flat.rank_of(Copper(501)), Some(100));
}

/// §5.2's table, and the boundaries in it.
#[test]
fn the_bands_are_the_published_ones() {
    assert_eq!(Valuation::of_rank(0), Valuation::VeryCheap);
    assert_eq!(Valuation::of_rank(5), Valuation::VeryCheap);
    assert_eq!(Valuation::of_rank(6), Valuation::Cheap);
    assert_eq!(Valuation::of_rank(25), Valuation::Cheap);
    assert_eq!(Valuation::of_rank(26), Valuation::Typical);
    assert_eq!(Valuation::of_rank(75), Valuation::Typical);
    assert_eq!(Valuation::of_rank(76), Valuation::Expensive);
    assert_eq!(Valuation::of_rank(95), Valuation::Expensive);
    assert_eq!(Valuation::of_rank(96), Valuation::VeryExpensive);

    // "Typical", not "Fair". Listed prices do not establish fair value, and
    // the word is the claim.
    assert_eq!(Valuation::Typical.as_str(), "Typical");
    assert!(
        Valuation::ALL.iter().all(|v| v.as_str() != "Fair"),
        "nothing here may be called fair"
    );
}

/// Valuation and anomaly are different statements, and a price can be one
/// without the other.
#[test]
fn valuation_and_anomaly_are_separate() {
    // A tight market with one wild hour. The wild hour is the dearest, so it
    // ranks in the top band -- and it is also far outside the body.
    let mut prices: Vec<u64> = (0..40).map(|i| 1_000 + i).collect();
    prices.push(50_000);
    let b = buckets(&prices);
    let d = Distribution::of(&b).unwrap();

    assert_eq!(Anomaly::of(Copper(50_000), &d), Anomaly::Extreme);
    // The dearest *ordinary* price is top-band but not an anomaly: expensive
    // and unusual are different words for different facts.
    assert_eq!(
        Valuation::of_rank(b.rank_of(Copper(1_039)).unwrap()),
        Valuation::VeryExpensive
    );
    assert_eq!(Anomaly::of(Copper(1_039), &d), Anomaly::Ordinary);

    // And a market that has not moved has no scale, so nothing is far from it.
    let flat = buckets(&[500; 30]);
    let flat_d = Distribution::of(&flat).unwrap();
    assert_eq!(flat_d.iqr, Copper(0));
    assert_eq!(Anomaly::of(Copper(9_999), &flat_d), Anomaly::Ordinary);
}

/// IQR and MAD are the stable spreads; Swing is the legible one and says so in
/// its name.
#[test]
fn the_spreads_are_named_for_what_they_are() {
    let b = buckets(&[10, 20, 30, 40, 50]);
    let d = Distribution::of(&b).unwrap();
    assert_eq!(d.median, Copper(30));
    assert_eq!(d.iqr, Copper(26), "43 - 17");
    // Deviations from the median are 20, 10, 0, 10, 20; their median is 10.
    assert_eq!(d.mad, Copper(10));

    // Swing is (max - min) / mean: 40 / 30 = 133%.
    assert_eq!(Swing::of(&b), Swing(133));
    // And it is dominated by two observations, which is why it is not called
    // volatility: one wild hour in forty triples it.
    let mut spiky: Vec<u64> = vec![30; 40];
    spiky.push(3_000);
    assert!(Swing::of(&buckets(&spiky)).0 > 1_000);
}

/// §5.3: a tail needs more evidence than a median, and a refusal names its
/// reason rather than showing a band nobody should act on.
///
/// Eighteen buckets: past [`Gates::median`] and short of [`Gates::tails`].
/// The fixture has to sit *between* the two or it is not testing that there
/// are two -- which is how it read when the gates were 24 and 72 and this
/// used thirty.
#[test]
fn a_thin_history_places_a_median_but_refuses_a_band() {
    let gates = Gates::default();
    let held: u64 = 18;
    assert!(
        held >= gates.median as u64 && held < gates.tails as u64,
        "the fixture must sit between the gates, not past both"
    );

    let thin = buckets(&(0..held).map(|i| 1_000 + i).collect::<Vec<u64>>());
    let position = Position::of(Copper(1_005), &thin, None, gates);

    assert_eq!(position.rank, Some(30), "eighteen hours is enough to rank");
    assert!(position.from_median_percent.is_some());
    assert_eq!(position.valuation, None, "but not enough for a band");
    assert_eq!(
        position.insufficient,
        Some(Insufficient::NotEnoughHistory {
            have: held as u32,
            need: gates.tails
        })
    );
}

#[test]
fn a_history_full_of_holes_refuses_a_band_too() {
    let gates = Gates::default();
    let plenty = buckets(&(0..200).map(|i| 1_000 + i).collect::<Vec<u64>>());
    let position = Position::of(Copper(1_005), &plenty, Some(10), gates);

    assert_eq!(
        position.insufficient,
        Some(Insufficient::TooManyGaps {
            coverage: 10,
            need: gates.coverage
        }),
        "two hundred hours out of two thousand is not two thousand hours of evidence"
    );
    assert_eq!(position.valuation, None);
    // 1000..1199 holds five values below 1005 and six at or below, out of two
    // hundred: the mid-rank of that tie is 2.
    assert_eq!(position.rank, Some(2), "the rank is still a fact");
}

/// "Everything ever" has no datable start, so it has no coverage to fail --
/// which is not the same as failing it.
#[test]
fn a_window_with_no_expected_length_is_not_penalised() {
    let gates = Gates::default();
    let plenty = buckets(&(0..200).map(|i| 1_000 + i).collect::<Vec<u64>>());
    let position = Position::of(Copper(1_005), &plenty, None, gates);
    assert_eq!(position.valuation, Some(Valuation::VeryCheap));
    assert_eq!(position.insufficient, None);
}

/// The whole answer travels together: §5.2 says the valuation is never shown
/// alone, so the rank and the distance from the median come with it.
#[test]
fn a_position_carries_what_the_band_must_be_shown_with() {
    let gates = Gates::default();
    // 1000, 1010, ... 1990: a hundred hours, ten copper apart.
    let b = buckets(&(0..100).map(|i| 1_000 + i * 10).collect::<Vec<u64>>());
    let position = Position::of(Copper(1_100), &b, Some(90), gates);

    // Ten of the hundred hours were below 1100 and eleven at or below it, so
    // the mid-rank of that tie is 10.
    assert_eq!(position.rank, Some(10));
    assert_eq!(position.valuation, Some(Valuation::Cheap));
    // h = (100 + 1/3)(0.5) + 1/3 = 50.5, so the median is 1490 + 0.5(10) =
    // 1495, and 1100 is 26% under it. Signed, so the sign means cheaper.
    assert_eq!(position.from_median_percent, Some(-26));
    assert_eq!(position.anomaly, Anomaly::Ordinary);
    assert_eq!(position.insufficient, None);
}

// --- the sparkline -------------------------------------------------------

/// The slots are equal *durations*, not equal numbers of observations.
///
/// A sparkline carries no axis, so a reader reads the horizontal as time. Two
/// observations an hour apart and two a week apart must not be drawn the same
/// width, which is the whole reason this is a resampling rather than a list.
#[test]
fn a_spark_slot_is_a_duration_not_an_observation() {
    let from = Millis(0);
    let until = Millis(SPARK_SLOTS as u64 * HOUR);

    // Every observation crowded into the first two hours. A list of points
    // would spread them across the whole line; slots leave the rest empty,
    // because nothing was observed there.
    let crowded = Spark::over(
        (0..8).map(|i| (Millis(i * HOUR / 4), Copper(100 + i))),
        from,
        until,
    );
    assert_eq!(crowded.slots.len(), SPARK_SLOTS);
    assert!(
        crowded.slots[2..].iter().all(|s| s.is_none()),
        "a quiet fortnight is drawn as quiet, not squeezed out by a busy hour"
    );

    // The last observation in a slot wins: a slot is the market's state at
    // the end of it, which is `Buckets`' rule and has to stay the same one.
    assert_eq!(crowded.slots[0], Some(Copper(103)));
}

/// A gap is a gap. The line breaks; it is not drawn through.
#[test]
fn an_unobserved_slot_is_none_rather_than_an_interpolation() {
    let until = Millis(SPARK_SLOTS as u64 * HOUR);
    // The first slot and the last, nothing in between.
    let spark = Spark::over(
        [
            (Millis(0), Copper(100)),
            (Millis(until.get() - 1), Copper(900)),
        ],
        Millis(0),
        until,
    );

    assert_eq!(spark.slots[0], Some(Copper(100)));
    assert_eq!(spark.slots[SPARK_SLOTS - 1], Some(Copper(900)));
    assert!(
        spark.slots[1..SPARK_SLOTS - 1].iter().all(|s| s.is_none()),
        "§2: unavailable data is never invented, including by a straight line"
    );
}

/// The final instant belongs to the last slot, not to one past the end.
#[test]
fn the_end_of_the_window_lands_inside_it() {
    let until = Millis(SPARK_SLOTS as u64 * HOUR);
    let spark = Spark::over([(until, Copper(7))], Millis(0), until);
    assert_eq!(spark.slots.len(), SPARK_SLOTS);
    assert_eq!(spark.slots[SPARK_SLOTS - 1], Some(Copper(7)));
}

/// One observation is not a line, and says so.
#[test]
fn a_single_observation_is_not_a_shape() {
    let until = Millis(SPARK_SLOTS as u64 * HOUR);
    let one = Spark::over([(Millis(0), Copper(100))], Millis(0), until);
    assert!(one.is_empty(), "a card draws nothing rather than a dot");

    let two = Spark::over(
        [(Millis(0), Copper(100)), (Millis(5 * HOUR), Copper(200))],
        Millis(0),
        until,
    );
    assert!(!two.is_empty());
}

/// The stored form round-trips, and an unreadable field is a gap.
///
/// The direction that matters is the second one: a field this binary cannot
/// parse must draw nothing, never zero. A zero is a market crashing to free,
/// which is a chart telling a lie about a column it could not read.
#[test]
fn the_stored_spark_round_trips_and_fails_to_a_gap() {
    let spark = Spark {
        slots: vec![Some(Copper(1_200)), None, Some(Copper(1_250))],
    };
    assert_eq!(spark.encode(), "1200,,1250");
    assert_eq!(Spark::decode(&spark.encode()), spark);

    assert_eq!(Spark::decode("").slots, Vec::new());
    assert_eq!(
        Spark::decode("1200,rubbish,1250").slots,
        vec![Some(Copper(1_200)), None, Some(Copper(1_250))],
    );
}

/// A window with no span has no shape, rather than a division by zero.
#[test]
fn a_window_of_no_duration_draws_nothing() {
    let spark = Spark::over([(Millis(5), Copper(1))], Millis(5), Millis(5));
    assert!(spark.slots.is_empty());
    assert!(spark.is_empty());
}
