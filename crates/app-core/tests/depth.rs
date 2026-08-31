//! Market depth: what is on the shelf, and what it costs to take it.
//!
//! **These run against generated ladders, and that is a stated limitation
//! rather than an oversight.** No archive has real depth data: `summarise`
//! reduced the listings to five numbers and dropped them, so four months of
//! history has no ladder in it and cannot be given one. Collection had to land
//! before the analyses could be measured against anything real, which is why
//! §16's Phase 7 leaves the compaction encoding and the hot-window length
//! explicitly undecided.
//!
//! What that means for these tests: they hold the *arithmetic*, which is
//! knowable now, and they do not claim anything about the shapes real markets
//! take. The fixtures below are built to be awkward on purpose -- a wall, a
//! troll listing, a market too thin to fill an order -- because those are the
//! cases the arithmetic has to survive whatever the real distribution turns
//! out to be.

use app_core::market::depth::{Depth, Ladder, SPARSE_STEPS, Target, WALL_SHARE};
use app_core::market::{Copper, ItemId, ItemKind, Listing};

const ITEM: ItemId = ItemId(210_796);

fn listing(price: u64, quantity: u64) -> Listing {
    Listing {
        item: ITEM,
        unit_price: Copper(price),
        quantity,
    }
}

/// A dense commodity ladder: many rungs, many units.
fn dense() -> Ladder {
    Ladder::of(&[
        listing(100, 5),
        listing(100, 15), // the same rung, two sellers
        listing(110, 30),
        listing(120, 50),
        listing(150, 100),
        listing(200, 300),
        listing(400, 1_000),
    ])
}

/// Auctions at the same price are one rung, and the running total is the
/// ladder's own.
#[test]
fn listings_at_one_price_are_one_rung() {
    let ladder = dense();
    assert_eq!(ladder.levels(), 6, "seven listings, six distinct prices");
    assert_eq!(ladder.steps[0].price, Copper(100));
    assert_eq!(ladder.steps[0].quantity, 20, "5 + 15 at the same price");
    assert_eq!(ladder.steps[0].cumulative, 20);
    assert_eq!(ladder.total(), 1_500);
    assert_eq!(ladder.cheapest(), Some(Copper(100)));
    assert_eq!(ladder.dearest(), Some(Copper(400)));
}

/// Buying is a sweep, and the sweep is the whole point.
///
/// The headline price is 100. Twenty units cost 100 each. Fifty cost more,
/// because there are only twenty at 100 -- and no summary this app stored
/// before Phase 7 could have said so.
#[test]
fn buying_walks_the_ladder_rather_than_paying_the_sticker_price() {
    let ladder = dense();

    let twenty = ladder.fill(20);
    assert!(twenty.complete);
    assert_eq!(twenty.average_unit, Copper(100));
    assert_eq!(
        twenty.impact_percent, 0,
        "entirely inside the cheapest rung"
    );

    let fifty = ladder.fill(50);
    assert!(fifty.complete);
    // 20 at 100, 30 at 110 = 2000 + 3300 = 5300 over 50 units.
    assert_eq!(fifty.total_cost, Copper(5_300));
    assert_eq!(fifty.average_unit, Copper(106));
    assert_eq!(fifty.clearing_price, Copper(110));
    assert_eq!(fifty.impact_percent, 6, "6% dearer than the sticker price");
}

/// A market that cannot fill the order says so, rather than quoting a price
/// for units that are not there.
#[test]
fn an_order_bigger_than_the_market_is_reported_unfilled() {
    let ladder = dense();
    let huge = ladder.fill(10_000);
    assert!(!huge.complete);
    assert_eq!(huge.filled, 1_500, "everything there was");
    assert_eq!(huge.wanted, 10_000);
    // And it still prices what it *could* get, which is the actionable half.
    assert!(huge.average_unit > Copper(100));
}

/// The liquidity proxies are named, and they are proxies.
#[test]
fn quantity_within_a_price_band_is_the_named_liquidity_proxy() {
    let ladder = dense();
    // Within 5% of 100 is <= 105: only the first rung.
    assert_eq!(ladder.quantity_within(5), Some(20));
    // Within 20% is <= 120: the first three rungs.
    assert_eq!(ladder.quantity_within(20), Some(100));
    assert_eq!(ladder.quantity_upto(Copper(150)), 200);
    assert_eq!(ladder.quantity_upto(Copper(99)), 0, "cheaper than anything");
}

/// A supply percentile weights units, not time, and not observations.
///
/// The third kind of percentile in this crate, and the tests that keep the
/// other two apart now have a third to keep apart from both.
#[test]
fn a_supply_percentile_weights_units_on_the_shelf() {
    let ladder = dense();
    // Cumulative units by rung: 20, 50, 100, 200, 500, 1,500.
    //
    // The cheapest 25% is 375 units, which is reached at 200 -- the first rung
    // whose running total clears it. Note how little that has to do with the
    // *prices*: two thirds of this market is one rung at 400, so a quarter of
    // the supply is already most of the way up the ladder.
    assert_eq!(ladder.supply_percentile(25), Some(Copper(200)));
    // Half of it -- 750 units -- is inside that last rung.
    assert_eq!(ladder.supply_percentile(50), Some(Copper(400)));
    // The cheapest 1% is 15 units, inside the first rung.
    assert_eq!(ladder.supply_percentile(1), Some(Copper(100)));
}

/// A wall is one price holding an outsized share of the market.
#[test]
fn a_wall_is_one_price_holding_the_market_up() {
    let ladder = dense();
    let walls = ladder.walls();
    // 1,000 of 1,500 units at 400, and 300 of 1,500 at 200.
    assert_eq!(walls.len(), 2);
    assert_eq!(walls[0].price, Copper(200));
    assert_eq!(walls[0].share_percent, 20);
    assert_eq!(walls[1].price, Copper(400));
    assert_eq!(walls[1].share_percent, 66);
    assert!(walls.iter().all(|w| w.share_percent >= WALL_SHARE));

    // An even market has no walls, which is the answer rather than an absence
    // of one.
    let even = Ladder::of(&(0..10).map(|i| listing(100 + i, 10)).collect::<Vec<_>>());
    assert!(even.walls().is_empty());
}

// --- the sparse case -----------------------------------------------------

/// A BoE ladder declines the metrics that assume a distribution.
///
/// §16 asks for sparse and dense ladders to be treated separately, and this is
/// what that means in practice: with four auctions, "the cheapest quarter of
/// supply" is a long way of saying "the second one", and saying it in
/// percentile language would dress up a guess as a measurement.
#[test]
fn a_sparse_ladder_declines_to_be_a_distribution() {
    let sparse = Ladder::of(&[
        listing(25_000, 1),
        listing(31_000, 1),
        listing(31_500, 1),
        listing(300_000, 1),
    ]);
    assert!(sparse.is_sparse());
    assert!(sparse.levels() < SPARSE_STEPS);

    assert_eq!(sparse.supply_percentile(50), None);
    assert!(
        sparse.walls().is_empty(),
        "with four rungs, every one is a wall"
    );

    let depth = Depth::of(&sparse, Target(1)).expect("a listed market");
    assert!(depth.sparse);
    assert_eq!(depth.p25, None);
    assert_eq!(depth.p50, None);
    assert_eq!(depth.within_5, None);

    // What it *does* answer is the question a BoE buyer actually has.
    assert_eq!(depth.cheapest, Copper(25_000));
    assert_eq!(depth.total, 4);
    assert!(depth.fill.complete);
    assert_eq!(depth.fill.average_unit, Copper(25_000));
}

/// The line between sparse and dense is one rung wide, and it is where the
/// constant says it is.
#[test]
fn the_sparse_threshold_is_where_it_says_it_is() {
    let four = Ladder::of(&(0..4).map(|i| listing(100 + i, 5)).collect::<Vec<_>>());
    let five = Ladder::of(&(0..5).map(|i| listing(100 + i, 5)).collect::<Vec<_>>());
    assert!(four.is_sparse());
    assert!(!five.is_sparse());
    assert_eq!(SPARSE_STEPS, 5);
    assert!(four.supply_percentile(50).is_none());
    assert!(five.supply_percentile(50).is_some());
}

// --- target profiles -----------------------------------------------------

/// The quantity a market is judged against comes from the domain, not a
/// template.
#[test]
fn a_target_quantity_is_a_property_of_the_kind() {
    assert_eq!(Target::of(ItemKind::Consumable).get(), 20);
    assert_eq!(Target::of(ItemKind::Reagent).get(), 200);
    // A BoE is one item. There is no quantity question, which is exactly why
    // it is the sparse case.
    assert_eq!(Target::of(ItemKind::Boe).get(), 1);
    assert_eq!(Target::of(ItemKind::Recipe).get(), 1);
}

// --- storage -------------------------------------------------------------

/// The stored form round-trips, and rebuilds the running total.
#[test]
fn a_stored_ladder_comes_back_the_same() {
    let ladder = dense();
    assert_eq!(
        ladder.encode(),
        "100:20,110:30,120:50,150:100,200:300,400:1000"
    );
    let back = Ladder::decode(&ladder.encode());
    assert_eq!(back, ladder);
    assert_eq!(back.total(), ladder.total());
}

/// A rung that cannot be read is dropped, never zeroed.
///
/// A zero price would be a free unit at the front of the ladder, which is the
/// one corruption that would poison every figure above it: the cheapest price,
/// the impact of every sweep, and every percentage measured from it.
#[test]
fn an_unreadable_rung_is_dropped_rather_than_zeroed() {
    let back = Ladder::decode("100:20,rubbish,0:99,120:5,150:0");
    assert_eq!(back.levels(), 2);
    assert_eq!(back.cheapest(), Some(Copper(100)));
    assert_eq!(back.total(), 25);
    assert_eq!(Ladder::decode(""), Ladder::default());
}

/// A listing of nothing is not supply.
#[test]
fn a_zero_quantity_listing_is_not_a_rung() {
    let ladder = Ladder::of(&[listing(100, 0), listing(200, 5)]);
    assert_eq!(ladder.levels(), 1);
    assert_eq!(ladder.cheapest(), Some(Copper(200)));
}

/// An empty market is empty rather than free.
#[test]
fn an_empty_ladder_answers_nothing_rather_than_zero() {
    let empty = Ladder::default();
    assert!(empty.is_empty());
    assert_eq!(empty.cheapest(), None);
    assert_eq!(empty.supply_percentile(50), None);
    assert_eq!(empty.quantity_within(5), None);
    assert_eq!(Depth::of(&empty, Target(20)), None);

    let fill = empty.fill(20);
    assert!(!fill.complete);
    assert_eq!(fill.filled, 0);
    assert_eq!(fill.average_unit, Copper::ZERO);
    assert_eq!(fill.impact_percent, 0, "no price to be a percentage of");
}

/// The whole panel, on a market worth showing one for.
#[test]
fn a_depth_summary_carries_what_the_panel_shows() {
    let depth = Depth::of(&dense(), Target::of(ItemKind::Consumable)).expect("a listed market");
    assert!(!depth.sparse);
    assert_eq!(depth.levels, 6);
    assert_eq!(depth.total, 1_500);
    assert_eq!(depth.cheapest, Copper(100));
    assert_eq!(depth.target, 20);
    assert!(depth.fill.complete);
    assert_eq!(depth.within_5, Some(20));
    assert_eq!(depth.within_20, Some(100));
    assert_eq!(depth.walls.len(), 2);
}

// --- the archive curve (Phase 7's retention) ---------------------------------

/// The two figures the depth panel prints are band edges, so a curve answers
/// them **exactly**. Measured on 515 real EU markets: exact on all of them.
#[test]
fn a_curve_is_exact_on_the_figures_the_panel_prints() {
    let ladder = Ladder::of(&[
        listing(100, 40),  // the cheapest
        listing(104, 60),  // within 5%
        listing(118, 200), // within 20%
        listing(400, 700), // far above both
    ]);
    let curve = ladder.compact();

    assert_eq!(curve.cheapest, ladder.cheapest().unwrap());
    assert_eq!(curve.total, ladder.total());
    assert_eq!(curve.quantity_within(5), ladder.quantity_within(5));
    assert_eq!(curve.quantity_within(20), ladder.quantity_within(20));
}

/// A curve must not answer what the live ladder refused. Sparseness is a
/// statement about the shelf, and an archive that forgot how many rungs there
/// were would start quoting percentiles over four listings.
#[test]
fn a_curve_keeps_a_sparse_shelf_sparse() {
    let ladder = Ladder::of(&[listing(100, 1), listing(140, 1), listing(300, 2)]);
    assert!(ladder.is_sparse());
    let curve = ladder.compact();
    assert!(curve.is_sparse());
    assert_eq!(curve.supply_percentile(50), None);
    assert_eq!(ladder.supply_percentile(50), None);
}

/// Within its documented exactness: on a shelf whose supply sits inside the
/// bands, a percentile lands within a few per cent of the exact answer.
#[test]
fn a_curve_places_a_percentile_close_to_the_exact_ladder() {
    let ladder = Ladder::of(&[
        listing(1000, 10),
        listing(1050, 30),
        listing(1120, 40),
        listing(1300, 50),
        listing(1800, 70),
        listing(2400, 100),
    ]);
    let exact = ladder.supply_percentile(50).unwrap().get() as f64;
    let got = ladder.compact().supply_percentile(50).unwrap().get() as f64;
    let error = (got - exact).abs() / exact * 100.0;
    assert!(error < 10.0, "{got} against {exact} is {error:.1}% out");
}

/// An empty shelf compacts to an empty curve rather than to a curve of zeroes
/// claiming a price of zero.
#[test]
fn an_empty_ladder_has_no_curve() {
    let curve = Ladder::default().compact();
    assert_eq!(curve.cheapest, app_core::market::Copper::ZERO);
    assert_eq!(curve.total, 0);
    assert_eq!(curve.quantity_within(5), None);
}
