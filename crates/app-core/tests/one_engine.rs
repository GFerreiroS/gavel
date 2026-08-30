//! Phase 5's exit gate: one golden dataset, one set of answers.
//!
//! The roadmap asks that "one golden dataset produces the same percentile,
//! label, median, and availability on card, analysis page, alert evaluation,
//! local rebuild, and remote rebuild". Four of those five are here; the fifth
//! is Phase 4's and is deferred with it, which is exactly why the assertions
//! below are written against the *pure* engine rather than against a database.
//! A remote worker runs this arithmetic and nothing else, so a rebuild that
//! agrees with these numbers locally is the thing Phase 4 will have to match.
//!
//! What each leg means:
//!
//! * **Card** and **analysis page** read the same materialised
//!   [`MarketWindow`]. They cannot disagree by construction -- there is one
//!   row -- so what is worth asserting is that the row holds what the engine
//!   says, which is the *local rebuild* leg.
//! * **Alert evaluation** is the leg that used to disagree, and the one this
//!   phase existed to fix. It had its own nearest-rank percentile over raw
//!   rows; it now calls the same estimator over the same buckets.
//!
//! The alert and the card do not summarise the *same observations*, and that
//! is deliberate rather than a crack in the gate: an alert excludes the price
//! that triggered it from its own baseline, because a price compared against a
//! window containing itself is graded on a curve it moved. So the assertions
//! feed both legs one explicit slice and check they answer identically over
//! it -- one definition, not one accident of which rows each happened to see.

use app_core::market::engine::{Buckets, Gates, Position, Valuation};
use app_core::market::materialise;
use app_core::market::window::Window;
use app_core::market::{
    AlertRule, Catalog, Copper, ItemId, MarketKey, PriceSample, Region, alerts,
};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;
/// 2026-09-01T00:00:00Z, after the test catalogue's last patch.
const NOW: Millis = Millis(1_788_912_000_000);
const ITEM: ItemId = ItemId(210_796);

fn catalog() -> Catalog {
    Catalog::from_json(
        r#"{"id":"midnight","expansion":"Midnight",
            "patches":[
              {"patch":"12.0","name":"Launch","started":"2026-03-02"},
              {"patch":"12.1","name":"The Curse","started":"2026-08-11"}],
            "raid_tiers":[
              {"id":"abyss","name":"The Venomous Abyss","patch":"12.1",
               "opened":"2026-08-18","season":2}],
            "items":[
              {"name":"Potion","category":"flask","audience":"common","stat":"haste",
               "ranks":[{"rank":1,"item_id":210796}]}]}"#,
    )
    .unwrap()
}

/// Thirty days of hourly observations, with gaps and a spike.
///
/// The same shape as the characterization fixture, and for the same reason: a
/// smooth line agrees with itself under any definition, so it would prove
/// nothing about two definitions having been made one.
fn history() -> Vec<PriceSample> {
    let mut seed = 20_260_830u64;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) & 0xffff_ffff
    };
    let hours = 30 * 24;
    let mut price = 500_000u64;
    let mut samples = Vec::new();
    for hour in 0..hours {
        // Roughly one hour in nine is missing: collection is not guaranteed,
        // and a gate that only ever saw complete windows would not be a gate.
        if next() % 9 == 0 {
            continue;
        }
        let drift = next() % 20_001;
        price = (price as i64 + drift as i64 - 10_000).clamp(50_000, 5_000_000) as u64;
        // A spike with a tail, at a fixed hour so it is always in the same
        // window: this is what separates a robust measure from a fragile one.
        let observed = if hour == 500 { price * 3 } else { price };
        samples.push(PriceSample {
            item: ITEM,
            region: Region::Eu,
            observed_at: Millis(NOW.get() - (hours - 1 - hour) * HOUR),
            min_unit_price: Copper(observed),
            p05_unit_price: Copper(observed + observed / 50),
            median_unit_price: Copper(observed + observed / 10),
            quantity: 1_000 + next() % 40_000,
            listings: 1 + (next() % 400) as u32,
        });
    }
    samples
}

/// The observations an alert fired at `current` would use as its baseline.
///
/// Everything strictly before it, within the rule's lookback. Written out here
/// rather than reached for through the alert module because the whole point is
/// to hand *both* legs the identical slice.
fn baseline_of(
    rule: &AlertRule,
    history: &[PriceSample],
    current: &PriceSample,
) -> Vec<PriceSample> {
    history
        .iter()
        .filter(|s| s.observed_at < current.observed_at)
        .filter(|s| current.observed_at.since(s.observed_at) <= rule.lookback_ms)
        .cloned()
        .collect()
}

/// The gate itself: one dataset, one percentile, one label, one median.
#[test]
fn the_card_and_the_alert_answer_the_same_question_the_same_way() {
    let history = history();
    let (prior, last) = history.split_at(history.len() - 1);
    let current = &last[0];

    // The default rule looks back a fortnight, which is also one of the five
    // windows every card offers. That coincidence is not what makes them
    // agree -- the estimator is -- but it is what lets the two be compared at
    // all without arguing about which interval each meant.
    let rule = AlertRule::default();
    assert_eq!(rule.lookback_ms, 14 * DAY, "the windows must be comparable");

    let baseline = baseline_of(&rule, prior, current);
    let buckets =
        Buckets::from_observations(baseline.iter().map(|s| (s.observed_at, s.p05_unit_price)));

    // --- the alert leg ---------------------------------------------------
    let alert = alerts::evaluate(&rule, current, prior, None);

    // --- the card and analysis-page leg ----------------------------------
    // One materialisation over the same observations, at the same instant.
    // The card reads `distribution.median` and `position`; the analysis page
    // reads the same row. There is no third path to check because Phase 2
    // removed it.
    let key = MarketKey::commodity(Region::Eu, ITEM, 1);
    let rebuilt = materialise::commodity(
        key,
        &baseline,
        &catalog(),
        &[Window::Days(14)],
        current.observed_at,
    );
    let window = rebuilt
        .windows
        .first()
        .expect("a fortnight of observations summarises");

    // --- the median ------------------------------------------------------
    let median = buckets.quantile(0.50).expect("a populated window");
    assert_eq!(
        window.distribution.median, median,
        "the card's median is the engine's"
    );
    if let Some(alert) = &alert {
        assert_eq!(
            alert.baseline, median,
            "the alert's baseline is the same median the card prints"
        );
    }

    // --- the percentile --------------------------------------------------
    // The threshold an alert fires at is the tenth percentile of that window.
    // Before Phase 5 this was a nearest-rank percentile: a different estimator
    // over a differently weighted sample, which is how an alert could call a
    // price cheap that the card beside it called typical.
    if let Some(alert) = &alert {
        assert_eq!(
            alert.threshold,
            buckets.quantile(0.10).expect("a populated window"),
            "the alert's threshold is Hyndman-Fan R8, like everything else"
        );
    }

    // --- the percentile a card prints ------------------------------------
    // The rank is where the market's *current* price sits in that window --
    // the number printed beside the band, not the median's own rank. Placed
    // by the same `Buckets` the alert's threshold came out of, which is the
    // claim: two figures on two pages, one estimator, one sample.
    let rank = window.position.rank.expect("a fortnight places a rank");
    assert_eq!(
        Some(rank),
        buckets.rank_of(rebuilt.state.price),
        "the card's percentile is the engine placing the card's price"
    );

    // --- the label -------------------------------------------------------
    // A band is the rank's band and nothing else. Recomputing it here from
    // the rank is the assertion: if `Position` ever decided a label by any
    // other route, these two would part company.
    assert_eq!(
        window.position.valuation,
        Some(Valuation::of_rank(rank)),
        "the label is the band of the rank, not a second opinion"
    );

    // --- availability ----------------------------------------------------
    // A band and a refusal are exclusive, in both directions. §5.3 does not
    // let a card show a label it has also said it lacks the evidence for, and
    // §2 does not let it go quiet instead of saying why.
    assert!(
        window.position.valuation.is_some() != window.position.insufficient.is_some(),
        "exactly one of a band and a reason"
    );
}

/// Rebuilding the same observations twice produces the same row.
///
/// The property Phase 4 needs and cannot add later: a partition recalculated
/// on another machine, or re-run after a worker died holding it, has to be
/// byte-equivalent to the one it replaces, or a retry would republish a
/// different market.
#[test]
fn a_rebuild_is_the_same_rebuild() {
    let history = history();
    let key = MarketKey::commodity(Region::Eu, ITEM, 1);
    let windows = Window::universal();

    let once = materialise::commodity(key, &history, &catalog(), &windows, NOW);
    // Shuffled, because `commodity` documents that history arrives in any
    // order. An ordering-dependent statistic would be a rebuild that depends
    // on how the rows came back out of SQLite.
    let mut reversed = history.clone();
    reversed.reverse();
    let twice = materialise::commodity(key, &reversed, &catalog(), &windows, NOW);

    assert_eq!(once, twice, "the same observations, the same answer");
}

/// The gate refuses, and says what it was short of.
///
/// The other half of availability: a market with a day of history has a
/// median and no band, and the reason carries the two numbers a card prints
/// rather than a bare `None`.
#[test]
fn a_thin_market_gets_a_median_and_a_reason_rather_than_a_band() {
    let history = history();
    // Eighteen hours: past the median gate and short of the tail gate. Enough
    // to place a price, not enough to call it cheap. The assertion below is
    // what keeps that true if the gates are recalibrated again -- they have
    // been once, and this fixture silently stopped being thin.
    let thin: Vec<PriceSample> = history.iter().rev().take(18).rev().cloned().collect();

    let buckets =
        Buckets::from_observations(thin.iter().map(|s| (s.observed_at, s.p05_unit_price)));
    let held = buckets.len() as u32;
    assert!(
        Gates::default().admits_median(held) && held < Gates::default().tails,
        "the fixture has to sit between the two gates, not past both: {held} buckets"
    );

    let position = Position::of(
        thin.last().expect("not empty").p05_unit_price,
        &buckets,
        None,
        Gates::default(),
    );

    assert!(position.rank.is_some(), "a rank is placed");
    assert!(position.from_median_percent.is_some(), "so is a distance");
    assert_eq!(position.valuation, None, "but no band");
    assert!(
        position.insufficient.is_some(),
        "and the card is told why, so it can say so"
    );
}

/// The fixture has to actually fire an alert.
///
/// Without this the gate above is three `if let Some(alert)` blocks that a
/// silently non-firing rule would skip, and the leg the phase existed to fix
/// would be the one leg not being checked.
#[test]
fn the_golden_dataset_fires_an_alert_to_check() {
    let history = history();
    let (prior, last) = history.split_at(history.len() - 1);
    assert!(
        alerts::evaluate(&AlertRule::default(), &last[0], prior, None).is_some(),
        "the fixture must end on a price the rule calls cheap"
    );
}
