//! Repository ports.
//!
//! Every method is `-> impl Future + Send` rather than `async fn` so the
//! traits stay allocation-free without an `async_trait` box. Implementations
//! write ordinary `async fn`.

use std::future::Future;

use cluster_core::Millis;

use crate::error::RepoResult;
use crate::market::catalog::CatalogStatus;
use crate::market::materialise::{MarketState, MarketSummary, MarketWindow, Materialised};
use crate::market::window::Window;
use crate::market::{
    Alert, ItemId, MarketEvent, MarketKey, PriceSample, Realm, RealmId, RealmSample, Region,
    WindowStats,
};

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

    /// Every live entry among `keys`, in one round trip.
    ///
    /// Not a convenience. A page draws hundreds of item cards and each one
    /// wants a cached tooltip; asking for them one at a time was 1316 point
    /// lookups per page load and the single largest cost in rendering one.
    /// Missing and expired keys are simply absent from the result, exactly as
    /// [`CacheStore::get`] returns `None` for them.
    fn get_many(
        &self,
        keys: &[String],
        now: Millis,
    ) -> impl Future<Output = RepoResult<Vec<(String, Vec<u8>)>>> + Send;
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

    /// Alerts raised at or after `since`, newest first.
    ///
    /// The page that shows these asks for one day. Older alerts are still in
    /// the table -- they are the price history's account of itself -- but a
    /// week-old "this was cheap on Tuesday" is not something anyone can act
    /// on, and showing it made the list read as a feed rather than a warning.
    fn alerts_since(
        &self,
        since: Millis,
        limit: usize,
    ) -> impl Future<Output = RepoResult<Vec<Alert>>> + Send;

    /// Every observation of every market in a region, oldest first.
    ///
    /// One query rather than one per item. It exists for the materialiser,
    /// which reduces a whole region at a time and would otherwise ask 515
    /// questions to answer one -- the shape §11b calls an N+1 whether or not
    /// it touches the database once per row.
    ///
    /// Not for a request. A handler that called this would be doing exactly
    /// what Phase 2 moved to the write path.
    fn history_in_region(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<PriceSample>>> + Send;

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

    /// Collapse each day of history older than `before` into one row.
    ///
    /// The alternative to pruning, and the reason retention can stay at "keep
    /// forever": an expansion's archive survives, at one row per item per
    /// region per day, instead of being deleted to save space. Returns the
    /// number of rows removed.
    fn downsample_before(&self, before: Millis) -> impl Future<Output = RepoResult<u64>> + Send;
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

    /// Turn collection on or off for one realm, without touching its history.
    fn set_realm_enabled(
        &self,
        region: Region,
        realm: RealmId,
        enabled: bool,
    ) -> impl Future<Output = RepoResult<()>> + Send;

    fn realms(&self) -> impl Future<Output = RepoResult<Vec<Realm>>> + Send;

    /// Collapse each day of history older than `before` into one row per
    /// (item, realm, variant). See [`PriceRepository::downsample_before`].
    fn downsample_before(&self, before: Millis) -> impl Future<Output = RepoResult<u64>> + Send;
}

/// What the tracker collects, as switches an administrator can flip.
///
/// Absent means on. A category added by a later release starts collected
/// rather than being silently ignored because no row existed for it, which is
/// the failure mode of storing "what is enabled" instead of "what was turned
/// off".
pub trait SettingsRepository: Send + Sync + 'static {
    fn set_enabled(&self, name: &str, enabled: bool)
    -> impl Future<Output = RepoResult<()>> + Send;

    /// Every switch that has been changed from its default.
    fn disabled(&self) -> impl Future<Output = RepoResult<Vec<String>>> + Send;
}

/// Bundles the repositories so callers take one type parameter instead of six.
/// One item a person asked to be told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    pub item: ItemId,
    pub region: Region,
    pub added_at: Millis,
}

/// What each person follows.
///
/// Its own port rather than a method on [`UserRepository`]: watching an item
/// is a market concern that happens to be keyed by a user, and the auth
/// service has no business growing a dependency on the catalogue.
pub trait WatchRepository: Send + Sync + 'static {
    /// Everything this person follows, most recently added first.
    fn watches(&self, user: UserId) -> impl Future<Output = RepoResult<Vec<Watch>>> + Send;

    /// Follow an item. Following one already followed is not an error: the
    /// button that does this is a toggle, and a double-click is not a fault.
    fn watch(
        &self,
        user: UserId,
        item: ItemId,
        region: Region,
        now: Millis,
    ) -> impl Future<Output = RepoResult<()>> + Send;

    /// Stop following. Equally idempotent.
    fn unwatch(
        &self,
        user: UserId,
        item: ItemId,
        region: Region,
    ) -> impl Future<Output = RepoResult<()>> + Send;
}

/// One catalogue's place in its life, as the database has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub catalog: String,
    pub state: CatalogStatus,
    pub changed_at: Millis,
    /// When it became active, if it ever did. Kept apart from `changed_at` so
    /// an archived tier can still say when it was the current one.
    pub activated_at: Option<Millis>,
    pub archived_at: Option<Millis>,
}

/// What one activation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub activated: String,
    /// The catalogue this activation archived, if there was one. §8: a new
    /// tier archives its predecessor, and the two happen together or not at
    /// all.
    pub archived: Option<String>,
}

/// The lifecycle of the shipped catalogues.
///
/// The catalogue's *content* -- items, tracks, bonus ids -- stays in
/// `catalogs.json`, reviewed in version control, which is where
/// `scripts/catalog-sync.py` writes it and where a patch's diff can be read.
/// Only the state lives here, because that is the part a person changes on a
/// running instance. It is the same split `SettingsRepository` already makes:
/// what exists is reviewed code, what is switched on is a runtime decision.
pub trait ReleaseRepository: Send + Sync + 'static {
    /// Every catalogue the database has a state for.
    fn releases(&self) -> impl Future<Output = RepoResult<Vec<Release>>> + Send;

    /// Record a starting state for catalogues the database has never seen.
    ///
    /// Never overwrites one: a state a person set outranks the one the binary
    /// shipped with, or upgrading would silently undo an activation. Returns
    /// how many rows were new.
    fn seed(
        &self,
        defaults: &[(String, CatalogStatus)],
        now: Millis,
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// Make one catalogue active and archive whatever was.
    ///
    /// One transaction, because §8 says so and because the alternative states
    /// -- none active, or two -- are both worse than the change not happening.
    /// Activating the already-active catalogue is not an error and archives
    /// nothing; the button that does this is one a person may press twice.
    fn activate(
        &self,
        catalog: &str,
        now: Millis,
    ) -> impl Future<Output = RepoResult<Activation>> + Send;
}

/// The timeline market movement is read against.
///
/// A separate port from [`EventRepository`], which is the *cluster's* event
/// log: a node going offline and a raid opening are both "events" and have
/// nothing else in common. One is how the deployment is doing; the other is
/// what happened in the game.
///
/// Phase 1 records them. Phase 8 correlates against them, and §11 is explicit
/// that an association is never described as a cause.
pub trait MarketEventRepository: Send + Sync + 'static {
    /// Write events that are not already there, by id.
    ///
    /// Idempotent on purpose: the catalogue's own events are re-derived at
    /// every start, and starting the server twice must not produce two copies
    /// of a patch release. Returns how many rows were new.
    fn record(&self, events: &[MarketEvent]) -> impl Future<Output = RepoResult<u64>> + Send;

    /// Events overlapping `[from, until)`, oldest first.
    ///
    /// `public_only` is not a convenience: an internal note and an event
    /// nobody has checked must not reach a visitor, and making the caller pass
    /// the audience keeps that decision at the boundary rather than in a
    /// filter somebody forgets.
    fn between(
        &self,
        from: Millis,
        until: Millis,
        public_only: bool,
    ) -> impl Future<Output = RepoResult<Vec<MarketEvent>>> + Send;
}

/// A candidate or published recalculation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisVersion {
    pub version: u64,
    pub state: VersionState,
    pub algorithm: u32,
    pub started_at: Millis,
    pub published_at: Option<Millis>,
    pub source_from: Option<Millis>,
    pub source_until: Option<Millis>,
    pub markets: u64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionState {
    /// Being built. Unreachable from any page, which is the guarantee.
    Staging,
    Published,
    /// Abandoned. Kept rather than deleted so operations can see that a
    /// recalculation failed and when (CLAUDE.md §15's failure contract, point
    /// five).
    Failed,
}

impl VersionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            VersionState::Staging => "staging",
            VersionState::Published => "published",
            VersionState::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<VersionState> {
        [
            VersionState::Staging,
            VersionState::Published,
            VersionState::Failed,
        ]
        .into_iter()
        .find(|s| s.as_str() == raw)
    }
}

/// What a page reads instead of reducing a history.
///
/// CLAUDE.md §15's performance rule, as a port: collection and calculation are
/// the write path, HTTP is a read path. Every method below is either "stage
/// this candidate", "publish it", or "read the published one" -- there is
/// deliberately no method that reduces anything, because a handler that could
/// ask for one eventually would.
pub trait ReadModelRepository: Send + Sync + 'static {
    // --- the write path -----------------------------------------------------

    /// Open a candidate version and return its number.
    ///
    /// Also clears any staging rows left by an earlier candidate: a materialiser
    /// that died halfway leaves rows nobody will ever publish, and they must
    /// not be mistaken for this attempt's.
    fn begin(&self, algorithm: u32, now: Millis) -> impl Future<Output = RepoResult<u64>> + Send;

    /// Write part of a candidate. Called many times per version -- once per
    /// batch of markets -- so that a large rebuild does not hold one
    /// transaction open across the whole archive.
    fn stage(
        &self,
        version: u64,
        markets: &[Materialised],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// Make the candidate the published version, in one transaction.
    ///
    /// Markets this version did not recalculate keep the rows they had: a
    /// realm that failed to fetch contributes nothing rather than blanking
    /// itself, and its previous figures are still true observations with a
    /// real timestamp beside them.
    fn publish(
        &self,
        version: u64,
        source: (Option<Millis>, Option<Millis>),
        now: Millis,
    ) -> impl Future<Output = RepoResult<()>> + Send;

    /// Abandon a candidate, recording why. The published version is untouched.
    fn abandon(&self, version: u64, note: &str) -> impl Future<Output = RepoResult<()>> + Send;

    // --- the read path ------------------------------------------------------

    /// The version every page is currently serving, if there is one.
    fn published(&self) -> impl Future<Output = RepoResult<Option<AnalysisVersion>>> + Send;

    /// Recent versions, newest first. For the operations page.
    fn versions(
        &self,
        limit: usize,
    ) -> impl Future<Output = RepoResult<Vec<AnalysisVersion>>> + Send;

    /// How many published commodity markets a region holds, and when the
    /// newest of them was observed.
    ///
    /// One indexed query for the two figures an index page shows above the
    /// fold. It exists so that the shell -- which paints before any card
    /// arrives -- does not have to read every market to count them.
    fn commodity_summary(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<(u64, Option<Millis>)>> + Send;

    /// Every published commodity market in a region, ordered by item, as much
    /// of each as a card needs.
    ///
    /// Deliberately not [`MarketState`]: that carries the stored chart series,
    /// and a page drawing 515 cards and no charts would read megabytes of JSON
    /// to render a price and a quantity.
    fn commodities(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<MarketSummary>>> + Send;

    /// One published market.
    fn market(
        &self,
        key: MarketKey,
    ) -> impl Future<Output = RepoResult<Option<MarketState>>> + Send;

    /// Every published commodity market in a region over one window.
    fn commodity_windows(
        &self,
        region: Region,
        window: &Window,
    ) -> impl Future<Output = RepoResult<Vec<MarketWindow>>> + Send;

    /// Every window of one market, for its analysis page.
    fn windows_of(
        &self,
        key: MarketKey,
    ) -> impl Future<Output = RepoResult<Vec<MarketWindow>>> + Send;
}

pub trait Store: Send + Sync + 'static {
    type Users: UserRepository;
    type Sessions: SessionRepository;
    type Jobs: JobRepository;
    type Events: EventRepository;
    type Cache: CacheStore;
    type Kv: KeyValueStore;
    type Prices: PriceRepository;
    type RealmPrices: RealmPriceRepository;
    type Settings: SettingsRepository;
    type Watches: WatchRepository;
    type Releases: ReleaseRepository;
    type MarketEvents: MarketEventRepository;
    type ReadModel: ReadModelRepository;

    fn users(&self) -> &Self::Users;
    fn sessions(&self) -> &Self::Sessions;
    fn jobs(&self) -> &Self::Jobs;
    fn events(&self) -> &Self::Events;
    fn cache(&self) -> &Self::Cache;
    fn kv(&self) -> &Self::Kv;
    fn prices(&self) -> &Self::Prices;
    /// Gear prices, which are per connected realm.
    fn realm_prices(&self) -> &Self::RealmPrices;
    /// What the tracker collects.
    fn settings(&self) -> &Self::Settings;
    /// Which items each person asked to be told about.
    fn watches(&self) -> &Self::Watches;
    /// Where each catalogue is in its life.
    fn releases(&self) -> &Self::Releases;
    /// What happened in the game, and when.
    fn market_events(&self) -> &Self::MarketEvents;
    /// What a page reads instead of reducing a history.
    fn read_model(&self) -> &Self::ReadModel;
}
