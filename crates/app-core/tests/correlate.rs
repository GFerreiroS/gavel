//! Relationships, and the discipline about what they are allowed to claim.
//!
//! Phase 8's exit gate is that "every correlation exposes its scope/evidence,
//! and no chart labels listed stock as sales volume". Most of what follows is
//! about the *refusals*: a correlation that declines below its gate, a heatmap
//! that will not name a cheapest hour from a grid of holes, a before/after
//! comparison that says it is unsupported rather than being drawn small.

use app_core::market::Copper;
use app_core::market::correlate::{
    Association, BeforeAfter, Heatmap, MIN_EITHER_SIDE, MIN_HEATMAP_CELLS, MIN_PAIRS, Stability,
    Strength, Swings,
};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

/// Rank correlation finds a monotone relationship whether or not it is linear.
///
/// That is the whole reason for using it: price against stock is not a
/// straight line and nobody claimed it was, but "they move together in order"
/// is a real and checkable statement.
#[test]
fn a_rank_correlation_finds_a_monotone_relationship() {
    let n = 40;
    let price: Vec<u64> = (0..n).map(|i| 100 + i).collect();
    // Rises with price, and violently non-linear.
    let stock: Vec<u64> = (0..n).map(|i| (i + 1) * (i + 1) * 7).collect();

    let up = Association::of(&price, &stock).expect("enough pairs");
    assert_eq!(up.rho_percent, 100, "perfectly monotone, if not linear");
    assert_eq!(up.pairs, n as u32);
    assert_eq!(up.strength(), Strength::Strong);
    assert_eq!(up.wording(), "Higher prices associated with more stock");

    let falling: Vec<u64> = stock.iter().rev().copied().collect();
    let down = Association::of(&price, &falling).expect("enough pairs");
    assert_eq!(down.rho_percent, -100);
    assert_eq!(down.wording(), "Higher prices associated with lower stock");
}

/// One absurd listing does not move it, which is why it is not Pearson.
///
/// Auction data has a seller at a hundred times the market in it most days.
/// A linear correlation would be dragged bodily by that point; a rank one sees
/// one more observation in the same order.
#[test]
fn one_absurd_observation_does_not_move_a_rank_correlation() {
    let n = 40;
    let price: Vec<u64> = (0..n).map(|i| 100 + i).collect();
    let stock: Vec<u64> = (0..n).map(|i| 500 + i * 3).collect();
    let clean = Association::of(&price, &stock).expect("enough pairs");

    let mut spiked = price.clone();
    spiked[n as usize - 1] = 50_000_000;
    let with_spike = Association::of(&spiked, &stock).expect("enough pairs");

    assert_eq!(
        clean.rho_percent, with_spike.rho_percent,
        "the spike is still the largest value; its rank did not change"
    );
}

/// Ties share a rank, so a market that sat still is not given an ordering it
/// does not have.
#[test]
fn tied_values_share_their_rank() {
    let price = vec![100u64; 40];
    let stock: Vec<u64> = (0..40).collect();
    // Every price is tied, so there is no variance to correlate against.
    assert_eq!(
        Association::of(&price, &stock),
        None,
        "a constant has no ranks to correlate"
    );

    // Half tied, half moving: still computable, and not 1.0.
    let mut half = vec![100u64; 20];
    half.extend(120..140);
    let rho = Association::of(&half, &stock).expect("enough pairs");
    assert!(
        rho.rho_percent > 0 && rho.rho_percent < 100,
        "ties cost it some of the correlation: {}",
        rho.rho_percent
    );
}

/// Below the gate there is no number, rather than a number with a caveat.
#[test]
fn too_few_pairs_is_no_correlation_at_all() {
    let short: Vec<u64> = (0..(MIN_PAIRS as u64 - 1)).collect();
    assert_eq!(Association::of(&short, &short), None);

    let just_enough: Vec<u64> = (0..MIN_PAIRS as u64).collect();
    assert!(Association::of(&just_enough, &just_enough).is_some());
}

/// A weak association says so instead of being described weakly.
#[test]
fn a_weak_association_declines_to_be_described() {
    let weak = Association {
        rho_percent: 8,
        pairs: 100,
    };
    assert_eq!(weak.strength(), Strength::None);
    assert_eq!(weak.wording(), "No association in this window");
    // And the wording never claims a mechanism, at any strength.
    for rho in [-100, -50, -20, 20, 50, 100] {
        let a = Association {
            rho_percent: rho,
            pairs: 100,
        };
        let words = a.wording();
        assert!(
            words.contains("associated with"),
            "{words:?} must say associated, never caused"
        );
        assert!(!words.contains("because") && !words.contains("caused"));
    }
}

// --- swings --------------------------------------------------------------

/// A drawdown is about the path, not about the extremes.
///
/// Up then down and down then up have the same high and the same low, and
/// they are opposite things to somebody holding stock.
#[test]
fn a_drawdown_is_a_property_of_the_path() {
    // 100 up to 200, back to 150: a 25% fall from the peak.
    let up_then_down = Swings::of(&[100, 140, 200, 180, 150]);
    assert_eq!(up_then_down.drawdown_percent, 25);
    assert_eq!(up_then_down.rise_percent, 100, "100 to 200 on the way up");

    // The same set of prices, reversed: it never falls from a peak it made.
    let down_then_up = Swings::of(&[150, 180, 200, 140, 100]);
    assert_eq!(down_then_up.rise_percent, 33, "150 to 200");
    assert_eq!(down_then_up.drawdown_percent, 50, "200 down to 100");

    // A market that only rises has no drawdown, which is not zero by accident.
    let only_up = Swings::of(&[100, 110, 120, 130]);
    assert_eq!(only_up.drawdown_percent, 0);
    assert_eq!(only_up.rise_percent, 30);
}

// --- stability -----------------------------------------------------------

/// Stability is about movement between observations, not the spread of them.
///
/// The distinction this test exists for: a market that drifts steadily from
/// 100 to 200 has a wide spread and is calm; one that alternates 140/160 has a
/// narrow spread and is not. A measure that used the spread would rank these
/// exactly the wrong way round.
#[test]
fn stability_measures_movement_rather_than_spread() {
    let drifting: Vec<u64> = (0..60).map(|i| 100 + i).collect();
    let jumpy: Vec<u64> = (0..60)
        .map(|i| if i % 2 == 0 { 140 } else { 160 })
        .collect();

    let calm = Stability::of(&drifting).expect("enough changes");
    let wild = Stability::of(&jumpy).expect("enough changes");

    assert!(
        calm.typical_move_percent < wild.typical_move_percent,
        "a steady drift is calmer than an alternation: {} vs {}",
        calm.typical_move_percent,
        wild.typical_move_percent
    );
    assert_eq!(calm.typical_move_percent, 0, "under one percent a step");
    assert!(wild.typical_move_percent >= 12);
}

/// And it declines below its gate too.
#[test]
fn too_few_changes_is_no_stability_figure() {
    // From one rather than zero: a change is measured as a percentage of the
    // earlier price, so a zero has nothing to be a percentage of and is not a
    // change at all.
    let short: Vec<u64> = (1..=MIN_PAIRS as u64).collect();
    // n values give n-1 changes, which is one short.
    assert_eq!(Stability::of(&short), None);
    let enough: Vec<u64> = (1..=MIN_PAIRS as u64 + 1).collect();
    assert!(Stability::of(&enough).is_some());
}

// --- the heatmap ---------------------------------------------------------

fn week(hours: u64) -> Vec<(Millis, Copper)> {
    // A market with a genuine weekly rhythm: cheapest in the small hours.
    (0..hours)
        .map(|h| {
            let hour_of_day = h % 24;
            let price = if (2..6).contains(&hour_of_day) {
                800
            } else {
                1_000
            };
            (Millis(h * HOUR), Copper(price))
        })
        .collect()
}

/// The grid is 168 cells and a hole stays a hole.
#[test]
fn a_heatmap_is_a_week_of_hours_with_holes_kept() {
    let map = Heatmap::of(week(24 * 14));
    assert_eq!(map.cells.len(), 7 * 24);
    assert_eq!(map.filled, 168, "a fortnight fills every hour of the week");
    assert!(map.is_usable());

    // A single day fills 24 cells and leaves 144 holes -- and says so rather
    // than letting a reader take the holes for cheapness.
    let one_day = Heatmap::of(week(24));
    assert_eq!(one_day.filled, 24);
    assert!(!one_day.is_usable());
    assert!(one_day.filled < MIN_HEATMAP_CELLS as u32);
    assert_eq!(
        one_day.cheapest(),
        None,
        "naming an hour from a grid of holes names the hour that was collected"
    );
}

/// The cheapest hour of the week is an hour *and* a day.
///
/// Which is the reason the grid replaced two separate charts: "cheapest at
/// 04:00" and "cheapest on Tuesday" do not compose into "cheapest at 04:00 on
/// Tuesday", and a weekly reset happens at one hour on one day.
#[test]
fn the_cheapest_cell_names_both_an_hour_and_a_day() {
    let map = Heatmap::of(week(24 * 14));
    let (weekday, hour) = map.cheapest().expect("a full week");
    assert!(
        (2..6).contains(&(hour as u64)),
        "the cheap window, got {hour}"
    );
    assert!(weekday < 7);

    let (lo, hi) = map.range().expect("a populated grid");
    assert_eq!(lo, Copper(800));
    assert_eq!(hi, Copper(1_000));
}

/// The stored form keeps the holes and the sample count.
#[test]
fn a_stored_heatmap_comes_back_the_same() {
    let map = Heatmap::of(week(24 * 3));
    let back = Heatmap::decode(&map.encode());
    assert_eq!(back.cells, map.cells);
    assert_eq!(back.samples, map.samples);
    assert_eq!(back.filled, map.filled);
    assert_eq!(Heatmap::decode(""), Heatmap::default());
}

// --- before and after ----------------------------------------------------

/// A comparison is over equal spans either side, and says how much is behind
/// each.
#[test]
fn a_before_and_after_compares_equal_spans() {
    let at = Millis(100 * HOUR);
    // 1,000 before, 800 after: cheaper afterwards.
    let observations: Vec<(Millis, Copper)> = (0..200)
        .map(|h| {
            let when = Millis(h * HOUR);
            let price = if when < at { 1_000 } else { 800 };
            (when, Copper(price))
        })
        .collect();

    let split = BeforeAfter::of(observations, at, 2 * DAY).expect("both sides populated");
    assert_eq!(split.before_median, Copper(1_000));
    assert_eq!(split.after_median, Copper(800));
    assert_eq!(split.change_percent, -20, "signed, and negative is cheaper");
    assert!(split.is_supported());
    assert_eq!(split.before_samples, 48);
    assert_eq!(
        split.after_samples, 49,
        "the instant itself counts as after"
    );
}

/// Too little either side is unsupported, and says so rather than being drawn
/// small.
#[test]
fn a_thin_side_makes_the_comparison_unsupported() {
    let at = Millis(100 * HOUR);
    let observations: Vec<(Millis, Copper)> = (95..103)
        .map(|h| (Millis(h * HOUR), Copper(1_000)))
        .collect();

    let split = BeforeAfter::of(observations, at, 2 * DAY).expect("something either side");
    assert!(!split.is_supported());
    assert!(split.before_samples < MIN_EITHER_SIDE || split.after_samples < MIN_EITHER_SIDE);
}

/// Nothing on one side is no comparison at all.
#[test]
fn nothing_after_is_no_comparison() {
    let at = Millis(100 * HOUR);
    let only_before: Vec<(Millis, Copper)> =
        (0..50).map(|h| (Millis(h * HOUR), Copper(1_000))).collect();
    assert_eq!(BeforeAfter::of(only_before, at, 2 * DAY), None);
}
