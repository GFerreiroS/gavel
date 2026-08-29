//! Repository ports.
//!
//! Every method is `-> impl Future + Send` rather than `async fn` so the
//! traits stay allocation-free without an `async_trait` box. Implementations
//! write ordinary `async fn`.

use std::future::Future;

use cluster_core::Millis;

use crate::error::RepoResult;
use crate::market::{Alert, ItemId, PriceSample, Realm, RealmId, RealmSample, Region, WindowStats};

// Job and event persistence are cluster concepts, so their ports live in
// `cluster-core` where the runtime can reach them without depending on the
// application layer. They are re-exported here under repository names so that
// `Store` reads consistently.
use crate::model::{Credentials, LinkedAccount, Session, User, UserId};
pub use cluster_core::persist::{EventLog as EventRepository, JobStore as JobRepository};

/// Generic byte-oriented store.
///
/// SQLite implements it today; another database or service can implement it
/// later. Domain repositories may be built on top of it, but are
/// free to use a richer backing representation where that is clearly better.
pub trait KeyValueStore: Send + Sync + 'static {
    fn get(&self, key: &str) -> impl Future<Output = RepoResult<Option<Vec<u8>>>> + Send;
    fn put(&self, key: &str, value: &[u8]) -> impl Future<Output = RepoResult<()>> + Send;
    fn delete(&self, key: &str) -> impl Future<Output = RepoResult<()>> + Send;
}

/// Short-lived cache for upstream API responses.
pub trait CacheStore: Send + Sync + 'static {
    fn get(
        &self,
        key: &str,
        now: Millis,
    ) -> impl Future<Output = RepoResult<Option<Vec<u8>>>> + Send;
    fn put(
        &self,
        key: &str,
        value: &[u8],
        expires_at: Millis,
    ) -> impl Future<Output = RepoResult<()>> + Send;
    /// Drop expired rows; returns how many were removed.
    fn purge_expired(&self, now: Millis) -> impl Future<Output = RepoResult<u64>> + Send;
}

pub trait UserRepository: Send + Sync + 'static {
    fn create(
        &self,
        username: &str,
        password_hash: &str,
        now: Millis,
    ) -> impl Future<Output = RepoResult<User>> + Send;

    fn by_username(
        &self,
        username: &str,
    ) -> impl Future<Output = RepoResult<Option<Credentials>>> + Send;

    fn by_id(&self, id: UserId) -> impl Future<Output = RepoResult<Option<User>>> + Send;

    fn linked_accounts(
        &self,
        id: UserId,
    ) -> impl Future<Output = RepoResult<Vec<LinkedAccount>>> + Send;
}

/// Sessions have their own port so changing the shared-session backend does
/// not affect authentication services.
pub trait SessionRepository: Send + Sync + 'static {
    fn create(&self, session: &Session) -> impl Future<Output = RepoResult<()>> + Send;
    fn get(&self, id: &str) -> impl Future<Output = RepoResult<Option<Session>>> + Send;
    fn delete(&self, id: &str) -> impl Future<Output = RepoResult<()>> + Send;
    fn purge_expired(&self, now: Millis) -> impl Future<Output = RepoResult<u64>> + Send;
}

/// Auction-house price history and the alerts derived from it.
pub trait PriceRepository: Send + Sync + 'static {
    /// Append observations. One row per item per snapshot; re-recording the
    /// same instant is a no-op so a retried job cannot double-count.
    fn record_samples(
        &self,
        samples: &[PriceSample],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// The baseline window for one item.
    fn history(
        &self,
        item: ItemId,
        region: Region,
        since: Millis,
    ) -> impl Future<Output = RepoResult<Vec<PriceSample>>> + Send;

    /// The most recent observation of every tracked item in a region.
    fn latest(&self, region: Region) -> impl Future<Output = RepoResult<Vec<PriceSample>>> + Send;

    /// When the newest stored snapshot was generated, for `If-Modified-Since`.
    fn last_observed(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Option<Millis>>> + Send;

    fn record_alert(&self, alert: &Alert) -> impl Future<Output = RepoResult<()>> + Send;

    /// Supports the alert cooldown.
    fn last_alert_at(
        &self,
        item: ItemId,
        region: Region,
    ) -> impl Future<Output = RepoResult<Option<Millis>>> + Send;

    fn recent_alerts(&self, limit: usize) -> impl Future<Output = RepoResult<Vec<Alert>>> + Send;

    /// Low/high/mean per item over a half-open window, computed by the store
    /// rather than by pulling every row into memory.
    ///
    /// `until` is `None` for "up to now", which is what an open patch window
    /// and the all-time view both want.
    fn window_stats(
        &self,
        region: Region,
        since: Millis,
        until: Option<Millis>,
    ) -> impl Future<Output = RepoResult<Vec<WindowStats>>> + Send;

    /// Drop observations older than `before` when the configured retention
    /// policy asks for it.
    fn prune_before(&self, before: Millis) -> impl Future<Output = RepoResult<u64>> + Send;
}

/// Per-realm auction history: gear, which is not a commodity.
///
/// A separate port from [`PriceRepository`] rather than more methods on it,
/// because nothing about the two is interchangeable -- different key,
/// different price meaning, different table. A caller that wants a gear price
/// must say so.
pub trait RealmPriceRepository: Send + Sync + 'static {
    /// Append observations. Re-recording the same snapshot is a no-op, so a
    /// retried collection cannot double-count a realm.
    fn record_samples(
        &self,
        samples: &[RealmSample],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// The most recent observation of every tracked variant on one realm.
    fn latest(
        &self,
        region: Region,
        realm: RealmId,
    ) -> impl Future<Output = RepoResult<Vec<RealmSample>>> + Send;

    /// The most recent observation of every tracked variant across every
    /// realm in a region -- the cross-realm view, which is the landing state.
    fn latest_in_region(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<RealmSample>>> + Send;

    /// One item's history on one realm, for its detail page.
    fn history(
        &self,
        item: ItemId,
        region: Region,
        realm: RealmId,
        since: Millis,
    ) -> impl Future<Output = RepoResult<Vec<RealmSample>>> + Send;

    /// One item's history across every realm in a region, for the
    /// cross-realm view of its statistics page.
    fn history_in_region(
        &self,
        item: ItemId,
        region: Region,
        since: Millis,
    ) -> impl Future<Output = RepoResult<Vec<RealmSample>>> + Send;

    /// When this realm's newest stored snapshot was generated, for
    /// `If-Modified-Since`. Per realm, because realms are generated on their
    /// own schedules.
    fn last_observed(
        &self,
        region: Region,
        realm: RealmId,
    ) -> impl Future<Output = RepoResult<Option<Millis>>> + Send;

    /// Remember a realm's name, so the UI can say "Draenor" without asking
    /// the upstream, and so a realm dropped from the configuration keeps its
    /// history readable.
    fn record_realm(&self, realm: &Realm) -> impl Future<Output = RepoResult<()>> + Send;

    fn realms(&self) -> impl Future<Output = RepoResult<Vec<Realm>>> + Send;
}

/// Bundles the repositories so callers take one type parameter instead of six.
pub trait Store: Send + Sync + 'static {
    type Users: UserRepository;
    type Sessions: SessionRepository;
    type Jobs: JobRepository;
    type Events: EventRepository;
    type Cache: CacheStore;
    type Kv: KeyValueStore;
    type Prices: PriceRepository;
    type RealmPrices: RealmPriceRepository;

    fn users(&self) -> &Self::Users;
    fn sessions(&self) -> &Self::Sessions;
    fn jobs(&self) -> &Self::Jobs;
    fn events(&self) -> &Self::Events;
    fn cache(&self) -> &Self::Cache;
    fn kv(&self) -> &Self::Kv;
    fn prices(&self) -> &Self::Prices;
    /// Gear prices, which are per connected realm.
    fn realm_prices(&self) -> &Self::RealmPrices;
}
