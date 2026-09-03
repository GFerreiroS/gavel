//! Repository ports.
//!
//! Every method is `-> impl Future + Send` rather than `async fn` so the
//! traits stay allocation-free without an `async_trait` box. Implementations
//! write ordinary `async fn`.

use std::future::Future;

use cluster_core::Millis;

use crate::error::RepoResult;
use crate::market::catalog::{CatalogStatus, ItemKind, Track};
use crate::market::event::{Validation, Visibility};
use crate::market::materialise::{
    MarketRollup, MarketState, MarketSummary, MarketWindow, Materialised, Scope,
};
use crate::market::window::Window;
use crate::market::{
    Alert, ItemId, Ladder, MarketEvent, MarketKey, PriceSample, Realm, RealmId, RealmSample,
    Region, TsmCommoditySample, TsmContrast, TsmRegionDaily, WindowStats,
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
        _username: &str,
        _password_hash: &str,
        _now: Millis,
    ) -> impl Future<Output = RepoResult<User>> + Send;

    /// Create the explicitly bootstrapped administrator, but only while the
    /// installation has no administrator. Implementations must make the check
    /// and insert atomically.
    fn bootstrap_admin(
        &self,
        _username: &str,
        _password_hash: &str,
        _now: Millis,
    ) -> impl Future<Output = RepoResult<Option<User>>> + Send {
        async { Ok(None) }
    }

    fn by_username(
        &self,
        username: &str,
    ) -> impl Future<Output = RepoResult<Option<Credentials>>> + Send;

    fn by_id(&self, id: UserId) -> impl Future<Output = RepoResult<Option<User>>> + Send;

    /// The webhook a person configured for their own Discord notifications,
    /// kept off `User` for the same reason `password_hash` is: a credential,
    /// not a profile fact to be rendered or logged. Defaults to "nobody has
    /// one configured", same neutral shape as `bootstrap_admin` and
    /// `delete` above, so a store that does not care about this need not
    /// implement it.
    fn discord_webhook(
        &self,
        _id: UserId,
    ) -> impl Future<Output = RepoResult<Option<String>>> + Send {
        async { Ok(None) }
    }

    /// Replace it, or clear it with `None`. Validation of what a "real"
    /// webhook URL looks like is the caller's job -- this port only stores
    /// what it is given.
    fn set_discord_webhook(
        &self,
        _id: UserId,
        _webhook: Option<&str>,
    ) -> impl Future<Output = RepoResult<()>> + Send {
        async { Ok(()) }
    }

    fn linked_accounts(
        &self,
        id: UserId,
    ) -> impl Future<Output = RepoResult<Vec<LinkedAccount>>> + Send;

    /// Delete one account and all user-owned rows through foreign-key
    /// cascades. Operational history contains no username and is retained.
    fn delete(&self, _id: UserId) -> impl Future<Output = RepoResult<bool>> + Send {
        async { Ok(false) }
    }
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
    /// Persist the summary and depth of one upstream snapshot as one unit.
    fn record_snapshot(
        &self,
        samples: &[PriceSample],
        region: Region,
        observed_at: Millis,
        ladders: &[(ItemId, Ladder)],
    ) -> impl Future<Output = RepoResult<(u64, u64)>> + Send {
        async move {
            let samples = self.record_samples(samples).await?;
            let ladders = self.record_ladders(region, observed_at, ladders).await?;
            Ok((samples, ladders))
        }
    }
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

    // --- market depth (Phase 7) -------------------------------------------
    //
    // Kept on this port rather than a new one, because a ladder is the same
    // observation as the sample beside it -- same item, same region, same
    // instant, written by the same collection pass. Splitting them would let
    // one be recorded without the other.

    /// Store one snapshot's ladders. Re-recording an instant is a no-op, for
    /// the reason `record_samples` is: a retried collection must not double.
    fn record_ladders(
        &self,
        region: Region,
        observed_at: Millis,
        ladders: &[(ItemId, Ladder)],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// The newest ladder of every market in a region.
    ///
    /// What the materialiser sweeps. One query rather than one per item, for
    /// the reason `history_in_region` gives.
    fn latest_ladders(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<(ItemId, Millis, Ladder)>>> + Send;

    /// Drop ladders older than `before`.
    ///
    /// Separate from `prune_before` because these are kept to a different
    /// policy: price history is the archive and is kept for ever, while
    /// ladders are bulky and are kept for a *hot window*. §16 is explicit that
    /// the compact historical encoding must not be chosen before there is real
    /// depth data to prove which analyses survive it -- so until then this is
    /// a plain window with an honest name, not a compaction.
    fn prune_ladders_before(&self, before: Millis) -> impl Future<Output = RepoResult<u64>> + Send;
}

/// One region's WoW Token price at an upstream-provided instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WowTokenPrice {
    pub region: Region,
    pub observed_at: Millis,
    pub price: crate::market::Copper,
}

/// Token history stays separate from catalogue markets: it has no item id,
/// realm, alert, ladder, or materialised market state.
pub trait TokenPriceRepository: Send + Sync + 'static {
    /// Returns false when the upstream instant was already recorded.
    fn record(&self, token: &WowTokenPrice) -> impl Future<Output = RepoResult<bool>> + Send;

    /// One region's complete recorded history, oldest first.
    fn history(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<WowTokenPrice>>> + Send;
}

/// TSM's independent commodity and completed-sales figures.
///
/// This stays apart from [`PriceRepository`]: these tables are source data,
/// not variants of our own auction measurements.
pub trait TsmRepository: Send + Sync + 'static {
    fn record_region_daily(
        &self,
        samples: &[TsmRegionDaily],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    fn record_commodity_samples(
        &self,
        samples: &[TsmCommoditySample],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// Whether this upstream snapshot is already present. TSM offers no
    /// conditional request mechanism; `updatedAt` is its replacement.
    fn has_commodity_snapshot(
        &self,
        region: Region,
        observed_at: Millis,
    ) -> impl Future<Output = RepoResult<bool>> + Send;

    /// Compare a TSM snapshot only where all local samples in its ±90-minute
    /// alignment window held the same minimum price.
    fn contrast(
        &self,
        region: Region,
        observed_at: Millis,
    ) -> impl Future<Output = RepoResult<Vec<TsmContrast>>> + Send;
}

/// Per-realm auction history: gear, which is not a commodity.
///
/// A separate port from [`PriceRepository`] rather than more methods on it,
/// because nothing about the two is interchangeable -- different key,
/// different price meaning, different table. A caller that wants a gear price
/// must say so.
pub trait RealmPriceRepository: Send + Sync + 'static {
    /// Persist one realm snapshot atomically across summary and depth tables.
    fn record_snapshot(
        &self,
        samples: &[RealmSample],
        region: Region,
        realm: RealmId,
        observed_at: Millis,
        ladders: &[(ItemId, String, Ladder)],
    ) -> impl Future<Output = RepoResult<(u64, u64)>> + Send {
        async move {
            let samples = self.record_samples(samples).await?;
            let ladders = self
                .record_ladders(region, realm, observed_at, ladders)
                .await?;
            Ok((samples, ladders))
        }
    }
    /// Append observations. Re-recording the same snapshot is a no-op, so a
    /// retried collection cannot double-count a realm.
    fn record_samples(
        &self,
        samples: &[RealmSample],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// Store one realm snapshot's ladders, keyed by item and variant.
    ///
    /// These are the *sparse* ladders: a BoE is four auctions of one item
    /// each, so a rung is usually one unit and a ladder is usually four rungs.
    /// Tiny rows, and a great many of them.
    fn record_ladders(
        &self,
        region: Region,
        realm: RealmId,
        observed_at: Millis,
        ladders: &[(ItemId, String, Ladder)],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

    /// The newest ladder of every variant of one item across a region.
    ///
    /// Per item rather than per region: the analysis page asks about one BoE,
    /// and a region-wide sweep of these would be 35,720 markets to answer a
    /// question about one.
    fn latest_ladders_for(
        &self,
        region: Region,
        item: ItemId,
    ) -> impl Future<Output = RepoResult<Vec<(RealmId, String, Millis, Ladder)>>> + Send;

    /// The newest ladder of every tracked variant in a region, on every realm.
    ///
    /// What the materialiser sweeps, and the reason it is separate from
    /// [`Self::latest_ladders_for`]: that one answers a page's question about
    /// one item, and calling it per item on the write path would be one query
    /// per tracked piece per cycle -- §11b's N+1, on the half of the app where
    /// it is least visible because no reader is waiting for it.
    fn latest_ladders_in_region(
        &self,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<(RealmId, ItemId, String, Ladder)>>> + Send;

    /// Drop realm ladders older than `before`. See
    /// [`PriceRepository::prune_ladders_before`].
    fn prune_ladders_before(&self, before: Millis) -> impl Future<Output = RepoResult<u64>> + Send;

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

    /// Every per-realm observation in a region since `since`.
    ///
    /// One query rather than one per item or one per realm. It exists for the
    /// materialiser, which rolls a whole region up at a time; a handler that
    /// called this would be doing what Phase 2 moved to the write path.
    fn window_in_region(
        &self,
        region: Region,
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

    /// The other direction: who to tell when this market moves. Raising an
    /// alert needs this and only this -- not the added-at ordering `watches`
    /// carries, which is a page's concern and not a notifier's.
    fn watchers(
        &self,
        item: ItemId,
        region: Region,
    ) -> impl Future<Output = RepoResult<Vec<UserId>>> + Send;
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
    ///
    /// **A shipped `Active` is the default for an empty database, not a
    /// claim.** On an instance that is already collecting something, a
    /// newcomer is seeded as `DraftPtr` and waits for somebody to activate it.
    /// The alternative was found by running a rollover: the second active row
    /// hit the partial unique index, seeding returned the conflict, and the
    /// server refused to start. Nothing was broken -- a person had simply not
    /// chosen yet -- and §8 makes that choice deliberate for exactly this
    /// reason.
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

    /// Every event, newest first. The administrator's list.
    ///
    /// No `public_only`: this is the one read that is *meant* to see the
    /// internal and the unvalidated, because reviewing them is what the
    /// administrator is there to do. It has no public route, which is what
    /// keeps that from being a leak (§7's operations gate).
    fn recent(&self, limit: usize) -> impl Future<Output = RepoResult<Vec<MarketEvent>>> + Send;

    /// Set an event's validation and visibility.
    ///
    /// The two together rather than one each, because they are one decision:
    /// "this is true, and people may see it". Splitting them would allow the
    /// state that must never exist -- published and unchecked -- to be reached
    /// by doing the halves in the wrong order.
    fn review(
        &self,
        id: &str,
        validation: Validation,
        visibility: Visibility,
    ) -> impl Future<Output = RepoResult<bool>> + Send;

    /// Remove an event an administrator added.
    ///
    /// Only an `administrator` one: a patch release is re-derived from the
    /// catalogue at every start, so deleting one would delete it until the
    /// next restart put it back -- a button that appears not to work. Returns
    /// whether a row went.
    fn forget(&self, id: &str) -> impl Future<Output = RepoResult<bool>> + Send;
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

    /// Write part of a candidate's per-realm roll-ups.
    ///
    /// Separate from [`Self::stage`] rather than folded into it because they
    /// are different shapes with different volumes: 2,042 commodity markets
    /// against 35,720 realm ones, rolled up two ways. Both land in the same
    /// version and are published by the same transaction.
    fn stage_rollups(
        &self,
        version: u64,
        rollups: &[MarketRollup],
    ) -> impl Future<Output = RepoResult<u64>> + Send;

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

    /// Every published roll-up of one kind in a region at one scope, ordered
    /// by item.
    ///
    /// What a gear or recipe card page reads: a few hundred rows rather than
    /// the eighteen thousand markets behind them.
    fn rollups(
        &self,
        region: Region,
        kind: ItemKind,
        scope: Scope,
    ) -> impl Future<Output = RepoResult<Vec<MarketRollup>>> + Send;

    /// One roll-up: one item's track, in a region or on a realm.
    fn rollup(
        &self,
        region: Region,
        item: ItemId,
        track: Option<Track>,
        scope: Scope,
    ) -> impl Future<Output = RepoResult<Option<MarketRollup>>> + Send;
}

pub trait Store: Send + Sync + 'static {
    type Users: UserRepository;
    type Sessions: SessionRepository;
    type Jobs: JobRepository;
    type Events: EventRepository;
    type Cache: CacheStore;
    type Kv: KeyValueStore;
    type Prices: PriceRepository + TokenPriceRepository;
    type RealmPrices: RealmPriceRepository;
    type Tsm: TsmRepository;
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
    /// Independent TSM data; never merged into our own price archive.
    fn tsm(&self) -> &Self::Tsm;
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
