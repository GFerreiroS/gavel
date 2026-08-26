//! Market tracking: price statistics, alert rules, the archival catalogs, and
//! one full collection pass against fakes.

use std::collections::BTreeMap;
use std::sync::Mutex;

use app_core::error::{AppResult, RepoResult};
use app_core::market::{
    Alert, AlertRule, AlertSeverity, Audience, Catalog, CatalogSet, Collector, CommodityProvider,
    Copper, ItemId, Listing, NullSink, Outcome, PriceSample, Region, Snapshot, WindowStats, alerts,
    summarise,
};
use app_core::repo::PriceRepository;
use cluster_core::Millis;

const ITEM: ItemId = ItemId(241324);
const HOUR: u64 = 60 * 60 * 1000;

fn listing(unit: u64, quantity: u64) -> Listing {
    Listing {
        item: ITEM,
        unit_price: Copper(unit),
        quantity,
    }
}

fn sample(at: u64, p05: u64, quantity: u64) -> PriceSample {
    PriceSample {
        item: ITEM,
        region: Region::Eu,
        observed_at: Millis(at),
        min_unit_price: Copper(p05),
        p05_unit_price: Copper(p05),
        median_unit_price: Copper(p05 * 2),
        quantity,
        listings: 10,
    }
}

// --- money ---------------------------------------------------------------

#[test]
fn copper_renders_as_gold_and_silver() {
    assert_eq!(Copper(12_345_600).to_string(), "1234g 56s");
    assert_eq!(Copper(4_500).to_string(), "45s 00c");
    assert_eq!(Copper(7).to_string(), "7c");
    assert_eq!(Copper(10_000).gold(), 1);
}

// --- price statistics ----------------------------------------------------

#[test]
fn percentiles_are_weighted_by_supply_not_by_listing() {
    // One unit at a silly price, then the real market. A listing-weighted
    // percentile would be dragged down by the outlier; a supply-weighted one
    // barely moves.
    let mut listings = vec![listing(1, 1), listing(1_000, 500), listing(1_100, 500)];
    let stats = summarise(&mut listings).unwrap();

    assert_eq!(stats.min, Copper(1), "min still sees the troll listing");
    assert_eq!(stats.p05, Copper(1_000), "p05 does not");
    assert_eq!(stats.median, Copper(1_000));
    assert_eq!(stats.quantity, 1_001);
    assert_eq!(stats.listings, 3);
}

#[test]
fn summarising_handles_a_one_listing_market() {
    let mut listings = vec![listing(500, 3)];
    let stats = summarise(&mut listings).unwrap();
    assert_eq!(stats.min, Copper(500));
    assert_eq!(stats.p05, Copper(500));
    assert_eq!(stats.median, Copper(500));
    assert_eq!(stats.quantity, 3);
}

#[test]
fn an_empty_market_produces_no_sample() {
    assert!(summarise(&mut []).is_none());
    assert!(summarise(&mut [listing(100, 0)]).is_none());
}

// --- alert rules ---------------------------------------------------------

#[test]
fn no_history_means_no_percentile_alert() {
    let rule = AlertRule::default();
    let current = sample(100 * HOUR, 100, 1_000);
    assert!(alerts::evaluate(&rule, &current, &[], None).is_none());
}

#[test]
fn a_hard_floor_fires_from_the_very_first_observation() {
    // This is what makes the tracker useful during the first week, before a
    // baseline exists.
    let rule = AlertRule::default();
    let current = sample(100 * HOUR, 400, 1_000);
    let alert = alerts::evaluate(&rule, &current, &[], Some(Copper(500))).unwrap();
    assert_eq!(alert.severity, AlertSeverity::VeryLow);
    assert_eq!(alert.current, Copper(400));
}

#[test]
fn a_price_below_the_baseline_percentile_alerts() {
    let rule = AlertRule {
        min_samples: 10,
        min_quantity: 10,
        ..AlertRule::default()
    };
    // A fortnight sitting around 1000.
    let history: Vec<PriceSample> = (0..100)
        .map(|i| sample(i * HOUR, 1_000 + (i % 7) * 10, 1_000))
        .collect();

    let cheap = sample(200 * HOUR, 500, 1_000);
    let alert = alerts::evaluate(&rule, &cheap, &history, None).expect("should alert");
    assert_eq!(alert.severity, AlertSeverity::VeryLow);
    assert!(
        alert.discount_percent >= 45,
        "got {}",
        alert.discount_percent
    );
    assert_eq!(alert.baseline, Copper(1_030));

    let normal = sample(200 * HOUR, 1_020, 1_000);
    assert!(alerts::evaluate(&rule, &normal, &history, None).is_none());
}

#[test]
fn thin_markets_are_ignored_however_cheap() {
    // Four units at a bargain is noise, not a buying opportunity.
    let rule = AlertRule {
        min_samples: 1,
        min_quantity: 20,
        ..AlertRule::default()
    };
    let history: Vec<PriceSample> = (0..50).map(|i| sample(i * HOUR, 1_000, 1_000)).collect();
    let thin = sample(200 * HOUR, 1, 4);
    assert!(alerts::evaluate(&rule, &thin, &history, Some(Copper(500))).is_none());
}

#[test]
fn stale_history_falls_out_of_the_baseline_window() {
    let rule = AlertRule {
        lookback_ms: 10 * HOUR,
        min_samples: 5,
        min_quantity: 1,
        ..AlertRule::default()
    };
    // All of this is older than the window and must not count.
    let ancient: Vec<PriceSample> = (0..50).map(|i| sample(i * HOUR, 1_000, 1_000)).collect();
    let current = sample(500 * HOUR, 10, 1_000);
    assert!(alerts::evaluate(&rule, &current, &ancient, None).is_none());
}

// --- catalog -------------------------------------------------------------

#[test]
fn the_shipped_catalog_parses_and_is_coherent() {
    let catalogs = CatalogSet::embedded();
    let catalog = catalogs.active().expect("one expansion must be active");
    assert!(!catalog.season.is_empty());
    assert!(catalog.items.len() >= 20, "got {}", catalog.items.len());

    let ids = catalog.tracked_ids();
    let mut deduped = ids.clone();
    deduped.dedup();
    assert_eq!(ids.len(), deduped.len(), "an item id appears twice");

    for item in &catalog.items {
        assert!(!item.ranks.is_empty(), "{} has no ranks", item.name);
        assert!(!item.name.is_empty());
        // Ranks are numbered from 1 upwards with no gaps, because the display
        // name renders the number directly.
        let mut ranks: Vec<u8> = item.ranks.iter().map(|r| r.rank).collect();
        ranks.sort_unstable();
        assert_eq!(
            ranks,
            (1..=item.ranks.len() as u8).collect::<Vec<_>>(),
            "{} has odd rank numbering",
            item.name
        );
    }

    // The audience split the tracker is organised around must be populated.
    for audience in [Audience::Melee, Audience::Caster, Audience::Common] {
        assert!(
            catalog.by_audience(audience).count() > 0,
            "{audience:?} bucket is empty"
        );
    }
}

#[test]
fn catalog_lookups_resolve_ranks() {
    let catalogs = CatalogSet::embedded();
    let catalog = catalogs.active().unwrap();

    let flask = catalog.find(ItemId(241324)).expect("flask is tracked");
    assert_eq!(flask.audience, Audience::Common);
    assert!(flask.rank_of(ItemId(241324)).is_some());

    // Mana potions are the genuine caster-only case.
    let mana = catalog
        .find(ItemId(241300))
        .expect("mana potion is tracked");
    assert_eq!(mana.audience, Audience::Caster);

    // Weapon stones are the genuine melee-only case.
    let stone = catalog
        .find(ItemId(237369))
        .expect("weightstone is tracked");
    assert_eq!(stone.audience, Audience::Melee);

    assert!(catalog.find(ItemId(1)).is_none());
    assert_eq!(catalog.index().len(), catalog.tracked_ids().len());
}

#[test]
fn rank_is_shown_only_when_an_item_has_several() {
    let single = Catalog::from_json(
        r#"{"id":"t","expansion":"T","season":"t","items":[
            {"name":"Solo","category":"flask","audience":"common","stat":"haste",
             "ranks":[{"rank":1,"item_id":10}]},
            {"name":"Dual","category":"flask","audience":"common","stat":"haste",
             "ranks":[{"rank":1,"item_id":20},{"rank":2,"item_id":21}]}]}"#,
    )
    .unwrap();

    let solo = single.find(ItemId(10)).unwrap();
    assert_eq!(solo.display_name(ItemId(10)), "Solo");

    let dual = single.find(ItemId(21)).unwrap();
    assert_eq!(dual.display_name(ItemId(21)), "Dual (R2)");
}

// --- archival behaviour --------------------------------------------------

#[test]
fn exactly_one_expansion_is_collected() {
    let catalogs = CatalogSet::embedded();
    let active: Vec<&str> = catalogs
        .catalogs
        .iter()
        .filter(|c| c.is_active())
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        active.len(),
        1,
        "exactly one catalog may be active, got {active:?}"
    );

    // Ids are how archived pages are addressed, so they must be unique.
    let mut ids: Vec<&str> = catalogs.catalogs.iter().map(|c| c.id.as_str()).collect();
    ids.sort();
    let unique = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), unique, "duplicate catalog id");
}

#[test]
fn patch_windows_are_contiguous_and_end_open() {
    let catalogs = CatalogSet::embedded();
    let catalog = catalogs.active().unwrap();
    let windows = catalog.patch_windows();
    assert!(windows.len() >= 2, "need patches to segment history");

    for pair in windows.windows(2) {
        let (_, _, first_end) = pair[0];
        let (_, second_start, _) = pair[1];
        assert_eq!(
            first_end,
            Some(second_start),
            "a gap or overlap between patch windows would lose samples"
        );
    }
    assert!(
        windows.last().unwrap().2.is_none(),
        "the newest patch window must stay open"
    );
    assert_eq!(catalog.span_start(), windows[0].1);
}

#[test]
fn archived_catalogs_are_selectable_but_not_active() {
    let set = CatalogSet::from_json(
        r#"{"catalogs":[
            {"id":"old","expansion":"Old","season":"s","status":"archived",
             "patches":[{"patch":"1.0","name":"a","started":"2024-01-01"}],
             "items":[{"name":"X","category":"flask","audience":"common","stat":"haste",
                       "ranks":[{"rank":1,"item_id":1}]}]},
            {"id":"new","expansion":"New","season":"s","status":"active",
             "patches":[{"patch":"2.0","name":"b","started":"2026-01-01"}],
             "items":[{"name":"Y","category":"flask","audience":"common","stat":"haste",
                       "ranks":[{"rank":1,"item_id":2}]}]}]}"#,
    )
    .unwrap();

    assert_eq!(set.active().unwrap().id, "new");
    assert!(set.by_id("old").is_some(), "archives stay addressable");
    assert!(!set.by_id("old").unwrap().is_active());

    // Display order puts the live one first, then newest archive.
    let order: Vec<&str> = set.ordered().iter().map(|c| c.id.as_str()).collect();
    assert_eq!(order, vec!["new", "old"]);

    // The cross-expansion index still resolves an archived item, which is what
    // lets an old alert render its name.
    assert_eq!(set.index().get(&ItemId(1)).unwrap().1.name, "X");
    assert_eq!(set.index().len(), 2);
}

#[test]
fn an_expansion_with_nothing_active_still_reads() {
    let set = CatalogSet::from_json(
        r#"{"catalogs":[
            {"id":"old","expansion":"Old","season":"s","status":"archived",
             "patches":[],"items":[]}]}"#,
    )
    .unwrap();
    assert!(set.active().is_none(), "nothing to collect");
    assert_eq!(set.ordered().len(), 1, "but still browsable");
}

// --- a full collection pass ----------------------------------------------

struct FakeProvider {
    snapshot: Mutex<Snapshot>,
    calls: Mutex<Vec<Option<Millis>>>,
}

impl CommodityProvider for FakeProvider {
    fn provider_name(&self) -> &'static str {
        "fake"
    }

    async fn commodities(
        &self,
        _region: Region,
        _wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> AppResult<Snapshot> {
        self.calls.lock().unwrap().push(if_modified_since);
        Ok(self.snapshot.lock().unwrap().clone())
    }
}

#[derive(Default)]
struct FakePrices {
    samples: Mutex<BTreeMap<(u32, u64), PriceSample>>,
    alerts: Mutex<Vec<Alert>>,
}

impl PriceRepository for FakePrices {
    async fn record_samples(&self, samples: &[PriceSample]) -> RepoResult<u64> {
        let mut store = self.samples.lock().unwrap();
        let mut written = 0;
        for sample in samples {
            let key = (sample.item.get(), sample.observed_at.get());
            if store.insert(key, *sample).is_none() {
                written += 1;
            }
        }
        Ok(written)
    }

    async fn history(
        &self,
        item: ItemId,
        _region: Region,
        since: Millis,
    ) -> RepoResult<Vec<PriceSample>> {
        Ok(self
            .samples
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.item == item && s.observed_at >= since)
            .copied()
            .collect())
    }

    async fn latest(&self, _region: Region) -> RepoResult<Vec<PriceSample>> {
        Ok(self.samples.lock().unwrap().values().copied().collect())
    }

    async fn last_observed(&self, _region: Region) -> RepoResult<Option<Millis>> {
        Ok(self
            .samples
            .lock()
            .unwrap()
            .values()
            .map(|s| s.observed_at)
            .max())
    }

    async fn record_alert(&self, alert: &Alert) -> RepoResult<()> {
        self.alerts.lock().unwrap().push(alert.clone());
        Ok(())
    }

    async fn last_alert_at(&self, item: ItemId, _region: Region) -> RepoResult<Option<Millis>> {
        Ok(self
            .alerts
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.item == item)
            .map(|a| a.observed_at)
            .max())
    }

    async fn recent_alerts(&self, limit: usize) -> RepoResult<Vec<Alert>> {
        Ok(self
            .alerts
            .lock()
            .unwrap()
            .iter()
            .take(limit)
            .cloned()
            .collect())
    }

    async fn window_stats(
        &self,
        _region: Region,
        since: Millis,
        until: Option<Millis>,
    ) -> RepoResult<Vec<WindowStats>> {
        let store = self.samples.lock().unwrap();
        let end = until.unwrap_or(Millis(u64::MAX));
        let mut by_item: BTreeMap<ItemId, Vec<u64>> = BTreeMap::new();
        for sample in store
            .values()
            .filter(|s| s.observed_at >= since && s.observed_at < end)
        {
            by_item
                .entry(sample.item)
                .or_default()
                .push(sample.p05_unit_price.get());
        }
        Ok(by_item
            .into_iter()
            .map(|(item, prices)| WindowStats {
                item,
                low: Copper(prices.iter().copied().min().unwrap_or(0)),
                low_at: Millis(0),
                high: Copper(prices.iter().copied().max().unwrap_or(0)),
                high_at: Millis(0),
                mean: Copper(prices.iter().sum::<u64>() / prices.len() as u64),
                samples: prices.len() as u32,
            })
            .collect())
    }

    async fn prune_before(&self, _before: Millis) -> RepoResult<u64> {
        Ok(0)
    }
}

fn test_catalog() -> Catalog {
    Catalog::from_json(
        r#"{"id":"test","expansion":"Test","season":"test","items":[
            {"name":"Flask of the Blood Knights","category":"flask","audience":"common",
             "stat":"haste","ranks":[{"rank":1,"item_id":241324}],"floor_copper":600}]}"#,
    )
    .unwrap()
}

#[tokio::test]
async fn a_collection_pass_stores_samples_and_raises_a_floor_alert() {
    let provider = FakeProvider {
        snapshot: Mutex::new(Snapshot::Fresh {
            generated_at: Millis(50 * HOUR),
            listings: vec![listing(500, 400), listing(520, 600)],
        }),
        calls: Mutex::new(Vec::new()),
    };
    let prices = FakePrices::default();
    let catalog = test_catalog();
    let collector = Collector::new(
        &provider,
        &prices,
        &NullSink,
        &catalog,
        AlertRule::default(),
    );

    let report = collector
        .collect(Region::Eu, Millis(50 * HOUR))
        .await
        .unwrap();
    assert_eq!(report.generated_at, Millis(50 * HOUR));

    let Outcome::Collected {
        samples,
        written,
        alerts,
    } = report.outcome
    else {
        panic!("expected a collection, got {:?}", report.outcome);
    };
    assert_eq!(samples, 1);
    assert_eq!(written, 1);
    assert_eq!(alerts.len(), 1, "500c is under the 600c floor");
    assert_eq!(alerts[0].item, ITEM);

    // The first call had nothing to send; the next one must offer the stored
    // timestamp so the expensive endpoint can answer 304.
    assert_eq!(provider.calls.lock().unwrap().as_slice(), &[None]);
    let second = collector
        .collect(Region::Eu, Millis(51 * HOUR))
        .await
        .unwrap();
    assert_eq!(second.outcome, Outcome::AlreadyRecorded, "same snapshot");
    assert_eq!(
        provider.calls.lock().unwrap()[1],
        Some(Millis(50 * HOUR)),
        "If-Modified-Since is sent"
    );
}

#[tokio::test]
async fn an_unchanged_snapshot_costs_nothing_downstream() {
    let provider = FakeProvider {
        snapshot: Mutex::new(Snapshot::NotModified),
        calls: Mutex::new(Vec::new()),
    };
    let prices = FakePrices::default();
    let catalog = test_catalog();
    let collector = Collector::new(
        &provider,
        &prices,
        &NullSink,
        &catalog,
        AlertRule::default(),
    );

    let report = collector.collect(Region::Eu, Millis(HOUR)).await.unwrap();
    assert_eq!(report.outcome, Outcome::NotModified);
    assert!(prices.latest(Region::Eu).await.unwrap().is_empty());
}

#[tokio::test]
async fn re_collecting_the_same_hour_does_not_alert_twice() {
    let provider = FakeProvider {
        snapshot: Mutex::new(Snapshot::Fresh {
            generated_at: Millis(50 * HOUR),
            listings: vec![listing(100, 5_000)],
        }),
        calls: Mutex::new(Vec::new()),
    };
    let prices = FakePrices::default();
    let catalog = test_catalog();
    let collector = Collector::new(
        &provider,
        &prices,
        &NullSink,
        &catalog,
        AlertRule::default(),
    );

    collector
        .collect(Region::Eu, Millis(50 * HOUR))
        .await
        .unwrap();
    let before = prices.alerts.lock().unwrap().len();

    // A restart inside the same hour must be a no-op, not a second ping.
    collector
        .collect(Region::Eu, Millis(50 * HOUR))
        .await
        .unwrap();
    assert_eq!(prices.alerts.lock().unwrap().len(), before);
}
