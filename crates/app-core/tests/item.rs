//! Item tooltips: caching, and what happens when the upstream cannot answer.
//!
//! The cache is the point of this service -- the tooltip is fetched on hover,
//! so a page of icons must not turn into a page of upstream calls.

use std::sync::Mutex;

use app_core::error::{AppError, AppResult, RepoResult};
use app_core::item::{ItemDetailProvider, ItemQuality, ItemTooltip, LocalizedTooltips};
use app_core::locale::{ALL_LOCALES, Locale};
use app_core::market::{Copper, ItemId, Region};
use app_core::repo::CacheStore;
use app_core::service::{Freshness, ItemTooltipService};
use cluster_core::Millis;

const ITEM: ItemId = ItemId(212283);
const TTL: u64 = 7 * 24 * 60 * 60 * 1000;
const EN: Locale = Locale::EnGb;

/// The same flask in whichever language was asked for. Only the name moves:
/// the point of these tests is the caching, not the translation.
fn flask(locale: Locale, at: Millis) -> ItemTooltip {
    ItemTooltip {
        item: ITEM,
        locale,
        name: match locale {
            Locale::DeDe => "Fläschchen der alchemistischen Chaos".into(),
            _ => "Flask of Alchemical Chaos".into(),
        },
        quality: ItemQuality::Epic,
        item_level: Some("Item Level 80".into()),
        binding: None,
        unique: None,
        item_class: Some("Consumable".into()),
        item_subclass: Some("Flask".into()),
        subclass_hidden: true,
        required_level: Some("Requires Level 71".into()),
        required_item_level: None,
        stats: Vec::new(),
        effects: vec!["Use: Grants a random secondary stat.".into()],
        flavor: None,
        crafting_reagent: None,
        sell_price: Some(Copper(12_500)),
        sell_price_label: Some("Sell Price:".into()),
        fetched_at: at,
    }
}

#[derive(Default)]
struct FakeProvider {
    calls: Mutex<u32>,
    configured: bool,
    fails: bool,
    /// The upstream has no such item.
    unknown: bool,
}

impl FakeProvider {
    fn live() -> Self {
        Self {
            configured: true,
            ..Self::default()
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl ItemDetailProvider for FakeProvider {
    fn provider_name(&self) -> &'static str {
        "fake"
    }

    fn is_configured(&self) -> bool {
        self.configured
    }

    async fn tooltips(&self, _region: Region, item: ItemId) -> AppResult<LocalizedTooltips> {
        *self.calls.lock().unwrap() += 1;
        if self.fails {
            return Err(AppError::Integration("upstream is down".into()));
        }
        if self.unknown {
            return Err(AppError::NotFound);
        }
        // As the real adapter does: one call, every language there is.
        Ok(ALL_LOCALES
            .into_iter()
            .map(|locale| {
                let mut tooltip = flask(locale, Millis(1_000));
                tooltip.item = item;
                (locale, tooltip)
            })
            .collect())
    }
}

#[derive(Default)]
struct FakeCache {
    rows: Mutex<Vec<(String, Vec<u8>, Millis)>>,
}

impl CacheStore for FakeCache {
    async fn get(&self, key: &str, now: Millis) -> RepoResult<Option<Vec<u8>>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _, expires)| k == key && expires.get() > now.get())
            .map(|(_, value, _)| value.clone()))
    }

    async fn get_many(&self, keys: &[String], now: Millis) -> RepoResult<Vec<(String, Vec<u8>)>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _, expires)| keys.contains(k) && expires.get() > now.get())
            .map(|(k, value, _)| (k.clone(), value.clone()))
            .collect())
    }

    async fn put(&self, key: &str, value: &[u8], expires_at: Millis) -> RepoResult<()> {
        let mut rows = self.rows.lock().unwrap();
        rows.retain(|(k, _, _)| k != key);
        rows.push((key.to_string(), value.to_vec(), expires_at));
        Ok(())
    }

    async fn purge_expired(&self, now: Millis) -> RepoResult<u64> {
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|(_, _, expires)| expires.get() > now.get());
        Ok((before - rows.len()) as u64)
    }
}

#[tokio::test]
async fn second_hover_is_served_from_the_cache() {
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    let (first, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(0))
        .await;
    assert_eq!(freshness, Freshness::Fetched);
    assert_eq!(first.name, "Flask of Alchemical Chaos");

    let (second, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(5_000))
        .await;
    assert_eq!(freshness, Freshness::Cached);
    assert_eq!(second, first);
    assert_eq!(provider.calls(), 1, "the upstream is asked once, not twice");
}

#[tokio::test]
async fn one_fetch_serves_every_language_that_region_publishes() {
    // The upstream returns all locales in one response, so switching language
    // must not cost another call. This is the whole reason the provider port
    // hands back a set rather than one tooltip.
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    let (english, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(0))
        .await;
    assert_eq!(freshness, Freshness::Fetched);
    assert_eq!(english.name, "Flask of Alchemical Chaos");

    let (german, freshness) = service
        .lookup(Region::Eu, Locale::DeDe, ITEM, "Flask", Millis(0))
        .await;
    assert_eq!(freshness, Freshness::Cached);
    assert_eq!(german.name, "Fläschchen der alchemistischen Chaos");
    assert_eq!(german.locale, Locale::DeDe);

    assert_eq!(provider.calls(), 1, "twelve languages, one upstream call");
}

#[tokio::test]
async fn any_language_works_in_any_region() {
    // Reading Korean prices in German is an ordinary thing to want, and the
    // upstream has no objection: every host returns every language.
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    let (tooltip, _) = service
        .lookup(Region::Kr, Locale::DeDe, ITEM, "Flask", Millis(0))
        .await;

    assert_eq!(tooltip.locale, Locale::DeDe);
    assert_eq!(tooltip.name, "Fläschchen der alchemistischen Chaos");
}

#[tokio::test]
async fn the_cache_expires_with_the_ttl() {
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(0))
        .await;
    let (_, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(TTL + 1))
        .await;

    assert_eq!(freshness, Freshness::Fetched);
    assert_eq!(provider.calls(), 2);
}

#[tokio::test]
async fn regions_share_one_copy_of_the_text() {
    // Item text is identical from every regional host, so switching region
    // must not re-fetch it -- and must not store it a second time.
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(0))
        .await;
    let (_, freshness) = service
        .lookup(Region::Kr, EN, ITEM, "Flask", Millis(0))
        .await;

    assert_eq!(freshness, Freshness::Cached);
    assert_eq!(provider.calls(), 1, "one fetch serves every region");
}

#[tokio::test]
async fn an_upstream_failure_still_renders_a_name() {
    let provider = FakeProvider {
        configured: true,
        fails: true,
        ..FakeProvider::default()
    };
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    let (tooltip, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask of Alchemical Chaos", Millis(0))
        .await;

    assert_eq!(freshness, Freshness::Unavailable);
    assert_eq!(tooltip.name, "Flask of Alchemical Chaos");
    assert!(!tooltip.is_detailed());
    // A failure must not be cached: the next hover should try again.
    assert!(cache.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_item_the_upstream_does_not_know_is_only_asked_for_once() {
    // Old expansions lose item ids. Without caching the "no such item"
    // answer, every hover would re-ask Battle.net for ever.
    let provider = FakeProvider {
        configured: true,
        unknown: true,
        ..FakeProvider::default()
    };
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    let (first, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(0))
        .await;
    assert_eq!(freshness, Freshness::Missing);
    assert_eq!(first.name, "Flask");

    let (_, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(1))
        .await;
    assert_eq!(freshness, Freshness::Cached);
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn an_unconfigured_provider_is_never_called() {
    let provider = FakeProvider::default();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    let (tooltip, freshness) = service
        .lookup(Region::Eu, EN, ITEM, "Flask", Millis(0))
        .await;

    assert_eq!(freshness, Freshness::Unavailable);
    assert_eq!(tooltip.name, "Flask");
    assert_eq!(provider.calls(), 0);
}

/// A page asks for hundreds of tooltips at once. The batch has to return
/// exactly what the one-at-a-time path would: every live entry, nothing
/// expired, and nothing for an item that was never cached.
#[tokio::test]
async fn a_batch_of_tooltips_matches_asking_one_at_a_time() {
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);

    for id in [10u32, 20, 30] {
        service
            .lookup(Region::Eu, Locale::EnGb, ItemId(id), "fallback", Millis(0))
            .await;
    }

    let wanted = [ItemId(10), ItemId(20), ItemId(30), ItemId(999)];
    let batch = service.cached_many(Locale::EnGb, wanted, Millis(500)).await;

    let mut got: Vec<u32> = batch.iter().map(|(item, _)| item.get()).collect();
    got.sort_unstable();
    assert_eq!(got, vec![10, 20, 30], "999 was never cached");

    for (item, tooltip) in &batch {
        let one = service
            .cached(Locale::EnGb, *item, Millis(500))
            .await
            .expect("also cached one at a time");
        assert_eq!(one.item, tooltip.item);
        assert_eq!(one.name, tooltip.name);
    }

    // Past the TTL the batch goes empty, exactly as the point lookups do.
    assert!(
        service
            .cached_many(Locale::EnGb, wanted, Millis(TTL + 1))
            .await
            .is_empty()
    );
}

/// Duplicate ids are what a catalog with shared reagents actually produces.
#[tokio::test]
async fn a_repeated_item_is_asked_for_once_and_returned_once() {
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);
    service
        .lookup(Region::Eu, Locale::EnGb, ItemId(7), "fallback", Millis(0))
        .await;

    let batch = service
        .cached_many(Locale::EnGb, [ItemId(7), ItemId(7), ItemId(7)], Millis(1))
        .await;
    assert_eq!(batch.len(), 1);
}

/// An empty ask must not become a query with no `IN` list.
#[tokio::test]
async fn asking_for_nothing_returns_nothing() {
    let provider = FakeProvider::live();
    let cache = FakeCache::default();
    let service = ItemTooltipService::new(&provider, &cache, TTL);
    assert!(
        service
            .cached_many(Locale::EnGb, [], Millis(1))
            .await
            .is_empty()
    );
}
