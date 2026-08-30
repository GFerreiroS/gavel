//! The materialised rows say exactly what the request used to calculate.
//!
//! This is the test Phase 2 rests on. The phase moves *where* a reduction
//! happens and is explicit that it does not touch the definitions -- so the
//! only honest way to move it is to prove the two paths agree, market by
//! market, over a history with gaps and spikes in it. If a number differs, the
//! phase changed something it said it would not.

use app_core::market::materialise::{self, CHART_POINTS};
use app_core::market::window::{ROLLING_DAYS, Window};
use app_core::market::{Catalog, Copper, ItemId, Ladder, PriceSample, Region, analyse, downsample};
use cluster_core::Millis;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;
/// 2026-09-01T00:00:00Z, comfortably after the test catalogue's last patch.
const NOW: Millis = Millis(1_788_912_000_000);

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
               "ranks":[{"rank":1,"item_id":10},{"rank":2,"item_id":11}]}]}"#,
    )
    .unwrap()
}

/// Forty days of hourly observations, with gaps, a spike, and one hour where
/// the market was listed but empty. The same shape the characterization
/// fixture uses, for the same reason: a smooth line proves nothing.
fn history(item: ItemId) -> Vec<PriceSample> {
    let mut seed = 20_260_830u64;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) & 0xffff_ffff
    };
    let hours = 40 * 24;
    let mut price = 500_000u64;
    let mut samples = Vec::new();
    for hour in 0..hours {
        if next() % 9 == 0 {
            continue;
        }
        let drift = (next() % 20_001) as i64 - 10_000;
        price = (price as i64 + drift).clamp(50_000, 5_000_000) as u64;
        let observed = if hour == 500 { price * 3 } else { price };
        samples.push(PriceSample {
            item,
            region: Region::Eu,
            observed_at: Millis(NOW.get() - (hours - 1 - hour) * HOUR),
            min_unit_price: Copper(observed),
            p05_unit_price: Copper(observed + observed / 50),
            median_unit_price: Copper(observed + observed / 10),
            quantity: if hour == 100 {
                0
            } else {
                1_000 + next() % 40_000
            },
            listings: 1 + (next() % 400) as u32,
        });
    }
    samples
}

/// Every number the analysis page shows, from the stored row rather than from
/// the history. The page is not allowed to tell the difference.
#[test]
fn the_stored_state_is_what_analyse_would_have_said() {
    let catalog = catalog();
    let history = history(ItemId(10));
    let key = catalog.market_of(&history[0]);
    let expected = analyse(&history, NOW);

    let stored = materialise::commodity(
        key,
        &history,
        &Ladder::default(),
        &catalog,
        &Window::universal(),
        NOW,
    )
    .state;

    assert_eq!(stored.key, key);
    assert_eq!(stored.samples, expected.samples as u32);
    assert_eq!(stored.mean, expected.mean);
    assert_eq!(stored.median, expected.median);
    assert_eq!(stored.first_seen, expected.first_seen);
    assert_eq!(stored.low, expected.low.unwrap().price);
    assert_eq!(stored.low_at, expected.low.unwrap().at);
    assert_eq!(stored.high, expected.high.unwrap().price);
    assert_eq!(stored.high_at, expected.high.unwrap().at);
    assert_eq!(stored.volatility_percent, expected.volatility_percent);
    assert_eq!(stored.day, expected.day);
    assert_eq!(stored.week, expected.week);
    assert_eq!(stored.month, expected.month);
    assert_eq!(stored.by_hour, expected.by_hour);
    assert_eq!(stored.by_weekday, expected.by_weekday);
    assert_eq!(stored.best_hour, expected.best_hour);
    assert_eq!(stored.best_weekday, expected.best_weekday);

    // The chart is stored already thinned, which is the reduction that used to
    // happen per view.
    assert_eq!(stored.series, downsample(&expected.series, CHART_POINTS));
    assert!(stored.series.len() <= CHART_POINTS);

    // And the current observation is the newest one, not the last row of an
    // arbitrary ordering.
    let newest = history.iter().max_by_key(|s| s.observed_at).unwrap();
    assert_eq!(stored.observed_at, Some(newest.observed_at));
    assert_eq!(stored.price, newest.p05_unit_price);
    assert_eq!(stored.quantity, newest.quantity);
}

/// The window rows have to say what `PriceRepository::window_stats` says, or
/// every card's comparison silently moves. Same measure -- the supply-weighted
/// P5 -- and the same reduction.
#[test]
fn a_window_row_is_what_window_stats_reduces() {
    let catalog = catalog();
    let history = history(ItemId(10));
    let key = catalog.market_of(&history[0]);
    let windows = Window::universal();
    let stored =
        materialise::commodity(key, &history, &Ladder::default(), &catalog, &windows, NOW).windows;

    for days in ROLLING_DAYS {
        let window = Window::Days(days);
        let row = stored
            .iter()
            .find(|w| w.window == window)
            .unwrap_or_else(|| panic!("no row for {window}"));

        let since = Millis(NOW.get() - days * DAY);
        let inside: Vec<&PriceSample> = history.iter().filter(|s| s.observed_at >= since).collect();

        assert_eq!(row.samples as usize, inside.len(), "{window}");
        assert_eq!(
            row.low,
            inside.iter().map(|s| s.p05_unit_price).min().unwrap(),
            "{window}"
        );
        assert_eq!(
            row.high,
            inside.iter().map(|s| s.p05_unit_price).max().unwrap(),
            "{window}"
        );
        let total: u128 = inside.iter().map(|s| s.p05_unit_price.get() as u128).sum();
        assert_eq!(
            row.mean,
            Copper((total / inside.len() as u128) as u64),
            "{window}"
        );
    }
}

/// The five rolling windows are the five a card offers. They are two lists in
/// two crates, and this is what keeps them one list.
#[test]
fn the_rolling_windows_are_the_ones_a_card_offers() {
    // `prefs::BASELINE_CHOICES`, which app-core cannot import.
    assert_eq!(ROLLING_DAYS, [1, 3, 7, 14, 30]);
}

/// A window with nothing in it gets no row, not a row of zeroes. §2: an
/// unavailable fact is rendered unavailable, and a stored zero is a price
/// somebody eventually plots.
#[test]
fn an_empty_window_is_absent_rather_than_zero() {
    let catalog = catalog();
    // One observation, four months ago: inside the expansion, outside every
    // rolling window.
    let old = vec![PriceSample {
        item: ItemId(10),
        region: Region::Eu,
        observed_at: Millis(NOW.get() - 120 * DAY),
        min_unit_price: Copper(1_000),
        p05_unit_price: Copper(1_100),
        median_unit_price: Copper(1_200),
        quantity: 50,
        listings: 2,
    }];
    let key = catalog.market_of(&old[0]);
    let out = materialise::commodity(
        key,
        &old,
        &Ladder::default(),
        &catalog,
        &Window::all_for(&catalog),
        NOW,
    );

    let present: Vec<String> = out.windows.iter().map(|w| w.window.key()).collect();
    for days in ROLLING_DAYS {
        assert!(
            !present.contains(&format!("{days}d")),
            "{days}d should have no row: {present:?}"
        );
    }
    assert!(present.contains(&"all".to_string()));
    assert!(present.contains(&"expansion".to_string()));
    // It landed inside 12.0 and outside 12.1 and the tier.
    assert!(present.contains(&"patch:12.0".to_string()));
    assert!(!present.contains(&"patch:12.1".to_string()));
    assert!(!present.contains(&"tier:abyss".to_string()));
}

/// Coverage is a fraction of something, and the row stores both halves. A
/// sample count that cannot say what it is a count out of is not evidence.
#[test]
fn a_window_says_how_much_of_itself_was_observed() {
    let catalog = catalog();
    let history = history(ItemId(10));
    let key = catalog.market_of(&history[0]);
    let stored = materialise::commodity(
        key,
        &history,
        &Ladder::default(),
        &catalog,
        &Window::universal(),
        NOW,
    )
    .windows;

    let week = stored.iter().find(|w| w.window == Window::Days(7)).unwrap();
    assert_eq!(week.expected_buckets, Some(7 * 24));
    assert!(week.observed_buckets > 0 && week.observed_buckets <= 7 * 24);
    let coverage = week
        .coverage_percent()
        .expect("a fraction of a known whole");
    assert!((80..=100).contains(&coverage), "coverage was {coverage}%");
    // Roughly one hour in nine is missing, so there is a gap and it is small.
    assert!(week.largest_gap_ms >= HOUR);
    assert!(week.largest_gap_ms < 12 * HOUR);

    // "Everything ever" has no datable start, so there is nothing for it to be
    // a fraction of, and it says so rather than claiming 100%.
    let all = stored.iter().find(|w| w.window == Window::All).unwrap();
    assert_eq!(all.expected_buckets, None);
    assert_eq!(all.coverage_percent(), None);
}

/// A window is a key, and the key round-trips: it is a column on every row.
#[test]
fn every_window_survives_being_stored() {
    let catalog = catalog();
    for window in Window::all_for(&catalog) {
        let key = window.key();
        assert_eq!(Window::parse(&key), Some(window.clone()), "{key}");
    }
    for bad in ["", "d", "xd", "patch:", "tier:", "days", "7"] {
        assert!(Window::parse(bad).is_none(), "{bad:?}");
    }
}
