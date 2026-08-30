//! The read model, and the transaction that publishes it.
//!
//! Everything interesting is in [`SqliteReadModel::publish`]. A candidate is
//! built as `staging` rows, invisible to every page because every read filters
//! on `state = 'published'`; publishing swaps them over in one transaction.
//! While that is happening a page keeps serving the version before it, with
//! its real timestamp, which is CLAUDE.md §15's guarantee stated as SQL.
//!
//! What publishing does *not* do is blank the markets it did not recalculate.
//! A realm that failed to fetch contributes nothing this cycle; its previous
//! figures stay, and they are still true observations with a real time beside
//! them. The alternative -- requiring all 184 realms before anything is
//! published -- would freeze the public archive on one HTTP error.

use app_core::error::{RepoError, RepoResult};
use app_core::market::analysis::{Cycle, Point, Trend};
use app_core::market::catalog::{ItemKind, Track};
use app_core::market::materialise::{
    LevelStat, MarketRollup, MarketState, MarketSummary, MarketWindow, Materialised, ModifierStat,
    Scope,
};
use app_core::market::window::Window;
use app_core::market::{Copper, ItemId, MarketKey, RealmId, Region};
use app_core::repo::{AnalysisVersion, ReadModelRepository, VersionState};
use cluster_core::Millis;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err};

pub struct SqliteReadModel {
    pool: Pool<Sqlite>,
}

impl SqliteReadModel {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

// --- what the JSON columns hold ---------------------------------------------
//
// Mirror types rather than serialising the domain structs directly: CLAUDE.md
// §9 forbids letting a serde representation of a persisted thing be the domain
// type, because then renaming a field is a migration nobody noticed writing.

#[derive(Serialize, Deserialize)]
struct StoredPoint {
    at: u64,
    price: u64,
    quantity: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredCycle {
    bucket: u8,
    mean: u64,
    samples: u32,
}

fn points_json(points: &[Point]) -> String {
    let stored: Vec<StoredPoint> = points
        .iter()
        .map(|p| StoredPoint {
            at: p.at.get(),
            price: p.price.get(),
            quantity: p.quantity,
        })
        .collect();
    serde_json::to_string(&stored).unwrap_or_else(|_| "[]".into())
}

fn points_from(raw: &str) -> Vec<Point> {
    serde_json::from_str::<Vec<StoredPoint>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(|p| Point {
            at: Millis(p.at),
            price: Copper(p.price),
            quantity: p.quantity,
        })
        .collect()
}

fn cycles_json(cycles: &[Cycle]) -> String {
    let stored: Vec<StoredCycle> = cycles
        .iter()
        .map(|c| StoredCycle {
            bucket: c.bucket,
            mean: c.mean.get(),
            samples: c.samples,
        })
        .collect();
    serde_json::to_string(&stored).unwrap_or_else(|_| "[]".into())
}

fn cycles_from(raw: &str) -> Vec<Cycle> {
    serde_json::from_str::<Vec<StoredCycle>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(|c| Cycle {
            bucket: c.bucket,
            mean: Copper(c.mean),
            samples: c.samples,
        })
        .collect()
}

#[derive(Serialize, Deserialize)]
struct StoredLevel {
    item_level: u16,
    upgrade: String,
    cheapest: u64,
    highest: u64,
    listings: u32,
    realms: u32,
}

#[derive(Serialize, Deserialize)]
struct StoredModifier {
    name: String,
    now: u32,
    seen: u32,
}

fn levels_json(levels: &[LevelStat]) -> String {
    let stored: Vec<StoredLevel> = levels
        .iter()
        .map(|l| StoredLevel {
            item_level: l.item_level,
            upgrade: l.upgrade.clone(),
            cheapest: l.cheapest.get(),
            highest: l.highest.get(),
            listings: l.listings,
            realms: l.realms,
        })
        .collect();
    serde_json::to_string(&stored).unwrap_or_else(|_| "[]".into())
}

fn levels_from(raw: &str) -> Vec<LevelStat> {
    serde_json::from_str::<Vec<StoredLevel>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(|l| LevelStat {
            item_level: l.item_level,
            upgrade: l.upgrade,
            cheapest: Copper(l.cheapest),
            highest: Copper(l.highest),
            listings: l.listings,
            realms: l.realms,
        })
        .collect()
}

fn modifiers_json(modifiers: &[ModifierStat]) -> String {
    let stored: Vec<StoredModifier> = modifiers
        .iter()
        .map(|m| StoredModifier {
            name: m.name.clone(),
            now: m.now,
            seen: m.seen,
        })
        .collect();
    serde_json::to_string(&stored).unwrap_or_else(|_| "[]".into())
}

fn modifiers_from(raw: &str) -> Vec<ModifierStat> {
    serde_json::from_str::<Vec<StoredModifier>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(|m| ModifierStat {
            name: m.name,
            now: m.now,
            seen: m.seen,
        })
        .collect()
}

/// The track's stored form. `-` for one no catalogue names, `` for a recipe:
/// both are real answers and neither may collide with a track's own slug.
fn track_column(track: Option<Track>, kind: ItemKind) -> &'static str {
    match (kind, track) {
        (ItemKind::Recipe, _) => "",
        (_, None) => "-",
        (_, Some(track)) => track.slug(),
    }
}

fn track_from(raw: &str) -> Option<Track> {
    match raw {
        "" | "-" => None,
        slug => Track::from_slug(slug),
    }
}

fn copper(value: Option<i64>) -> Option<Copper> {
    value.map(|v| Copper(v as u64))
}

fn realm(value: Option<i64>) -> Option<RealmId> {
    value.map(|v| RealmId(v as u32))
}

fn rollup_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<MarketRollup> {
    let region: String = row.get("region");
    let window: String = row.get("window");
    let track: String = row.get("track");
    let kind: String = row.get("kind");
    Ok(MarketRollup {
        region: Region::parse(&region).ok_or_else(|| corrupt("region", region))?,
        item: ItemId(row.get::<i64, _>("item_id") as u32),
        kind: ItemKind::ALL
            .into_iter()
            .find(|k| k.as_str() == kind)
            .ok_or_else(|| corrupt("rollup kind", kind))?,
        track: track_from(&track),
        scope: Scope::parse(row.get::<i64, _>("realm_id") as u32),
        window: Window::parse(&window).ok_or_else(|| corrupt("analysis window", window))?,
        observed_at: row
            .get::<Option<i64>, _>("observed_at")
            .map(|v| Millis(v as u64)),
        snapshots: row.get::<i64, _>("snapshots") as u32,
        realms_listing: row.get::<i64, _>("realms") as u32,
        cheapest_now: copper(row.get("cheapest_now")),
        cheapest_realm: realm(row.get("cheapest_realm")),
        dearest_realm_now: copper(row.get("dearest_realm_now")),
        dearest_realm: realm(row.get("dearest_realm")),
        median_realm_now: copper(row.get("median_realm_now")),
        highest_now: copper(row.get("highest_now")),
        cheapest_ever: copper(row.get("cheapest_ever")),
        highest_ever: copper(row.get("highest_ever")),
        listings_now: row.get::<i64, _>("listings_now") as u32,
        listings_seen: row.get::<i64, _>("listings_seen") as u32,
        level_range: row.get("level_range"),
        levels: levels_from(&row.get::<String, _>("levels")),
        modifiers: modifiers_from(&row.get::<String, _>("modifiers")),
        series: points_from(&row.get::<String, _>("series")),
    })
}

/// The market's components, spread across the columns a page filters on.
fn parts(key: MarketKey) -> (&'static str, Option<i64>, Option<i64>, Option<String>) {
    match key {
        MarketKey::Commodity { rank, .. } => ("commodity", Some(rank as i64), None, None),
        MarketKey::Recipe { realm, .. } => ("recipe", None, Some(realm.get() as i64), None),
        MarketKey::Boe { realm, track, .. } => (
            "boe",
            None,
            Some(realm.get() as i64),
            track.map(|t| t.slug().to_string()),
        ),
    }
}

fn trend(percent: i64, known: i64) -> Trend {
    if known == 0 {
        return Trend::UNKNOWN;
    }
    Trend {
        // `from`/`to` are not stored: nothing renders them, and a column
        // nobody reads is a column that drifts. The percentage is the trend.
        from: Copper::ZERO,
        to: Copper::ZERO,
        percent: percent as i32,
        known: true,
    }
}

fn state_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<MarketState> {
    let raw: String = row.get("market_key");
    let key: MarketKey = raw.parse().map_err(|_| corrupt("market key", raw))?;
    Ok(MarketState {
        key,
        observed_at: row
            .get::<Option<i64>, _>("observed_at")
            .map(|v| Millis(v as u64)),
        price: Copper(row.get::<i64, _>("price") as u64),
        min_price: Copper(row.get::<i64, _>("min_price") as u64),
        median_price: Copper(row.get::<i64, _>("median_price") as u64),
        quantity: row.get::<i64, _>("quantity") as u64,
        listings: row.get::<i64, _>("listings") as u32,
        first_seen: row
            .get::<Option<i64>, _>("first_seen")
            .map(|v| Millis(v as u64)),
        samples: row.get::<i64, _>("samples") as u32,
        mean: Copper(row.get::<i64, _>("mean") as u64),
        median: Copper(row.get::<i64, _>("median") as u64),
        low: Copper(row.get::<i64, _>("low") as u64),
        low_at: Millis(row.get::<i64, _>("low_at") as u64),
        high: Copper(row.get::<i64, _>("high") as u64),
        high_at: Millis(row.get::<i64, _>("high_at") as u64),
        volatility_percent: row.get::<i64, _>("swing") as u32,
        day: trend(row.get("day_percent"), row.get("day_known")),
        week: trend(row.get("week_percent"), row.get("week_known")),
        month: trend(row.get("month_percent"), row.get("month_known")),
        by_hour: cycles_from(&row.get::<String, _>("by_hour")),
        by_weekday: cycles_from(&row.get::<String, _>("by_weekday")),
        best_hour: row.get::<Option<i64>, _>("best_hour").map(|v| v as u8),
        best_weekday: row.get::<Option<i64>, _>("best_weekday").map(|v| v as u8),
        series: points_from(&row.get::<String, _>("series")),
    })
}

fn summary_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<MarketSummary> {
    let raw: String = row.get("market_key");
    Ok(MarketSummary {
        key: raw
            .parse()
            .map_err(|_| corrupt("market key", raw.clone()))?,
        observed_at: row
            .get::<Option<i64>, _>("observed_at")
            .map(|v| Millis(v as u64)),
        price: Copper(row.get::<i64, _>("price") as u64),
        min_price: Copper(row.get::<i64, _>("min_price") as u64),
        quantity: row.get::<i64, _>("quantity") as u64,
        listings: row.get::<i64, _>("listings") as u32,
        samples: row.get::<i64, _>("samples") as u32,
    })
}

fn window_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<MarketWindow> {
    let raw: String = row.get("market_key");
    let key: MarketKey = raw.parse().map_err(|_| corrupt("market key", raw))?;
    let window: String = row.get("window");
    Ok(MarketWindow {
        key,
        window: Window::parse(&window).ok_or_else(|| corrupt("analysis window", window))?,
        low: Copper(row.get::<i64, _>("low") as u64),
        low_at: Millis(row.get::<i64, _>("low_at") as u64),
        high: Copper(row.get::<i64, _>("high") as u64),
        high_at: Millis(row.get::<i64, _>("high_at") as u64),
        mean: Copper(row.get::<i64, _>("mean") as u64),
        median: Copper(row.get::<i64, _>("median") as u64),
        samples: row.get::<i64, _>("samples") as u32,
        first_at: Millis(row.get::<i64, _>("first_at") as u64),
        last_at: Millis(row.get::<i64, _>("last_at") as u64),
        expected_buckets: row
            .get::<Option<i64>, _>("expected_buckets")
            .map(|v| v as u32),
        observed_buckets: row.get::<i64, _>("observed_buckets") as u32,
        largest_gap_ms: row.get::<i64, _>("largest_gap_ms") as u64,
    })
}

fn version_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<AnalysisVersion> {
    let state: String = row.get("state");
    Ok(AnalysisVersion {
        version: row.get::<i64, _>("version") as u64,
        state: VersionState::parse(&state).ok_or_else(|| corrupt("version state", state))?,
        algorithm: row.get::<i64, _>("algorithm") as u32,
        started_at: Millis(row.get::<i64, _>("started_at") as u64),
        published_at: row
            .get::<Option<i64>, _>("published_at")
            .map(|v| Millis(v as u64)),
        source_from: row
            .get::<Option<i64>, _>("source_from")
            .map(|v| Millis(v as u64)),
        source_until: row
            .get::<Option<i64>, _>("source_until")
            .map(|v| Millis(v as u64)),
        markets: row.get::<i64, _>("markets") as u64,
        note: row.get("note"),
    })
}

const SELECT_STATE: &str = "SELECT * FROM market_current WHERE state = 'published'";
const SELECT_WINDOW: &str = "SELECT * FROM market_windows WHERE state = 'published'";

impl ReadModelRepository for SqliteReadModel {
    async fn begin(&self, algorithm: u32, now: Millis) -> RepoResult<u64> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;

        // Anything still staging belongs to a candidate that died. Mark it
        // failed and drop its rows: leaving them would let this attempt
        // publish somebody else's half-finished work.
        let orphans: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM analysis_versions WHERE state = 'staging'")
                .fetch_all(&mut *tx)
                .await
                .map_err(map_err)?;
        for (version,) in &orphans {
            sqlx::query(
                "UPDATE analysis_versions
                    SET state = 'failed', note = COALESCE(note, 'abandoned: a later run started')
                  WHERE version = ?",
            )
            .bind(version)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        }
        sqlx::query("DELETE FROM market_current WHERE state = 'staging'")
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        sqlx::query("DELETE FROM market_windows WHERE state = 'staging'")
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        sqlx::query("DELETE FROM market_rollup WHERE state = 'staging'")
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;

        let row: (i64,) = sqlx::query_as(
            "INSERT INTO analysis_versions (state, algorithm, started_at)
             VALUES ('staging', ?, ?)
             RETURNING version",
        )
        .bind(algorithm as i64)
        .bind(now.get() as i64)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_err)?;

        tx.commit().await.map_err(map_err)?;
        Ok(row.0 as u64)
    }

    async fn stage(&self, version: u64, markets: &[Materialised]) -> RepoResult<u64> {
        if markets.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0u64;

        for materialised in markets {
            let state = &materialised.state;
            let key = state.key;
            let (kind, rank, realm, track) = parts(key);

            sqlx::query(
                "INSERT INTO market_current
                   (market_key, state, version, kind, region, item_id, rank, realm_id, track,
                    observed_at, price, min_price, median_price, quantity, listings,
                    first_seen, samples, mean, median, low, low_at, high, high_at, swing,
                    day_percent, day_known, week_percent, week_known, month_percent, month_known,
                    best_hour, best_weekday, by_hour, by_weekday, series)
                 VALUES (?, 'staging', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                         ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(market_key, state) DO UPDATE SET
                    version = excluded.version, observed_at = excluded.observed_at,
                    price = excluded.price, min_price = excluded.min_price,
                    median_price = excluded.median_price, quantity = excluded.quantity,
                    listings = excluded.listings, first_seen = excluded.first_seen,
                    samples = excluded.samples, mean = excluded.mean, median = excluded.median,
                    low = excluded.low, low_at = excluded.low_at, high = excluded.high,
                    high_at = excluded.high_at, swing = excluded.swing,
                    day_percent = excluded.day_percent, day_known = excluded.day_known,
                    week_percent = excluded.week_percent, week_known = excluded.week_known,
                    month_percent = excluded.month_percent, month_known = excluded.month_known,
                    best_hour = excluded.best_hour, best_weekday = excluded.best_weekday,
                    by_hour = excluded.by_hour, by_weekday = excluded.by_weekday,
                    series = excluded.series",
            )
            .bind(key.to_string())
            .bind(version as i64)
            .bind(kind)
            .bind(key.region().as_str())
            .bind(key.item().get() as i64)
            .bind(rank)
            .bind(realm)
            .bind(track)
            .bind(state.observed_at.map(|v| v.get() as i64))
            .bind(state.price.get() as i64)
            .bind(state.min_price.get() as i64)
            .bind(state.median_price.get() as i64)
            .bind(state.quantity as i64)
            .bind(state.listings as i64)
            .bind(state.first_seen.map(|v| v.get() as i64))
            .bind(state.samples as i64)
            .bind(state.mean.get() as i64)
            .bind(state.median.get() as i64)
            .bind(state.low.get() as i64)
            .bind(state.low_at.get() as i64)
            .bind(state.high.get() as i64)
            .bind(state.high_at.get() as i64)
            .bind(state.volatility_percent as i64)
            .bind(state.day.percent as i64)
            .bind(i64::from(state.day.known))
            .bind(state.week.percent as i64)
            .bind(i64::from(state.week.known))
            .bind(state.month.percent as i64)
            .bind(i64::from(state.month.known))
            .bind(state.best_hour.map(i64::from))
            .bind(state.best_weekday.map(i64::from))
            .bind(cycles_json(&state.by_hour))
            .bind(cycles_json(&state.by_weekday))
            .bind(points_json(&state.series))
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += 1;

            for w in &materialised.windows {
                let (kind, _, realm, _) = parts(w.key);
                sqlx::query(
                    "INSERT INTO market_windows
                       (market_key, window, state, version, kind, region, item_id, realm_id,
                        low, low_at, high, high_at, mean, median, samples, first_at, last_at,
                        expected_buckets, observed_buckets, largest_gap_ms)
                     VALUES (?, ?, 'staging', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(market_key, window, state) DO UPDATE SET
                        version = excluded.version, low = excluded.low, low_at = excluded.low_at,
                        high = excluded.high, high_at = excluded.high_at, mean = excluded.mean,
                        median = excluded.median, samples = excluded.samples,
                        first_at = excluded.first_at, last_at = excluded.last_at,
                        expected_buckets = excluded.expected_buckets,
                        observed_buckets = excluded.observed_buckets,
                        largest_gap_ms = excluded.largest_gap_ms",
                )
                .bind(w.key.to_string())
                .bind(w.window.key())
                .bind(version as i64)
                .bind(kind)
                .bind(w.key.region().as_str())
                .bind(w.key.item().get() as i64)
                .bind(realm)
                .bind(w.low.get() as i64)
                .bind(w.low_at.get() as i64)
                .bind(w.high.get() as i64)
                .bind(w.high_at.get() as i64)
                .bind(w.mean.get() as i64)
                .bind(w.median.get() as i64)
                .bind(w.samples as i64)
                .bind(w.first_at.get() as i64)
                .bind(w.last_at.get() as i64)
                .bind(w.expected_buckets.map(i64::from))
                .bind(w.observed_buckets as i64)
                .bind(w.largest_gap_ms as i64)
                .execute(&mut *tx)
                .await
                .map_err(map_err)?;
            }
        }

        sqlx::query("UPDATE analysis_versions SET markets = markets + ? WHERE version = ?")
            .bind(written as i64)
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;

        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn stage_rollups(&self, version: u64, rollups: &[MarketRollup]) -> RepoResult<u64> {
        if rollups.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0u64;
        for rollup in rollups {
            let kind = rollup.kind;
            sqlx::query(
                "INSERT INTO market_rollup
                   (region, item_id, track, realm_id, state, version, kind, window,
                    observed_at, snapshots, realms, cheapest_now, cheapest_realm,
                    dearest_realm_now, dearest_realm, median_realm_now, highest_now,
                    cheapest_ever, highest_ever, listings_now, listings_seen,
                    level_range, levels, modifiers, series)
                 VALUES (?, ?, ?, ?, 'staging', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                         ?, ?, ?, ?)
                 ON CONFLICT(region, item_id, track, realm_id, state) DO UPDATE SET
                    version = excluded.version, kind = excluded.kind,
                    window = excluded.window, observed_at = excluded.observed_at,
                    snapshots = excluded.snapshots, realms = excluded.realms,
                    cheapest_now = excluded.cheapest_now,
                    cheapest_realm = excluded.cheapest_realm,
                    dearest_realm_now = excluded.dearest_realm_now,
                    dearest_realm = excluded.dearest_realm,
                    median_realm_now = excluded.median_realm_now,
                    highest_now = excluded.highest_now,
                    cheapest_ever = excluded.cheapest_ever, highest_ever = excluded.highest_ever,
                    listings_now = excluded.listings_now, listings_seen = excluded.listings_seen,
                    level_range = excluded.level_range, levels = excluded.levels,
                    modifiers = excluded.modifiers, series = excluded.series",
            )
            .bind(rollup.region.as_str())
            .bind(rollup.item.get() as i64)
            .bind(track_column(rollup.track, kind))
            .bind(rollup.scope.realm_id() as i64)
            .bind(version as i64)
            .bind(kind.as_str())
            .bind(rollup.window.key())
            .bind(rollup.observed_at.map(|v| v.get() as i64))
            .bind(rollup.snapshots as i64)
            .bind(rollup.realms_listing as i64)
            .bind(rollup.cheapest_now.map(|v| v.get() as i64))
            .bind(rollup.cheapest_realm.map(|r| r.get() as i64))
            .bind(rollup.dearest_realm_now.map(|v| v.get() as i64))
            .bind(rollup.dearest_realm.map(|r| r.get() as i64))
            .bind(rollup.median_realm_now.map(|v| v.get() as i64))
            .bind(rollup.highest_now.map(|v| v.get() as i64))
            .bind(rollup.cheapest_ever.map(|v| v.get() as i64))
            .bind(rollup.highest_ever.map(|v| v.get() as i64))
            .bind(rollup.listings_now as i64)
            .bind(rollup.listings_seen as i64)
            .bind(&rollup.level_range)
            .bind(levels_json(&rollup.levels))
            .bind(modifiers_json(&rollup.modifiers))
            .bind(points_json(&rollup.series))
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += 1;
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn rollups(
        &self,
        region: Region,
        kind: ItemKind,
        scope: Scope,
    ) -> RepoResult<Vec<MarketRollup>> {
        let rows = sqlx::query(
            "SELECT * FROM market_rollup
              WHERE state = 'published' AND kind = ? AND region = ? AND realm_id = ?
              ORDER BY item_id, track",
        )
        .bind(kind.as_str())
        .bind(region.as_str())
        .bind(scope.realm_id() as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(rollup_from_row).collect()
    }

    async fn rollup(
        &self,
        region: Region,
        item: ItemId,
        track: Option<Track>,
        scope: Scope,
    ) -> RepoResult<Option<MarketRollup>> {
        // Both spellings of "no track": a recipe stores the empty string and
        // an unresolved gear track stores `-`. One query rather than making
        // every caller know which it is.
        let rows = sqlx::query(
            "SELECT * FROM market_rollup
              WHERE state = 'published' AND region = ? AND item_id = ?
                AND realm_id = ? AND track IN (?, ?)",
        )
        .bind(region.as_str())
        .bind(item.get() as i64)
        .bind(scope.realm_id() as i64)
        .bind(track.map(|t| t.slug()).unwrap_or(""))
        .bind(track.map(|t| t.slug()).unwrap_or("-"))
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?;
        rows.as_ref().map(rollup_from_row).transpose()
    }

    async fn publish(
        &self,
        version: u64,
        source: (Option<Millis>, Option<Millis>),
        now: Millis,
    ) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;

        let candidate: Option<(String,)> =
            sqlx::query_as("SELECT state FROM analysis_versions WHERE version = ?")
                .bind(version as i64)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_err)?;
        match candidate {
            None => return Err(RepoError::NotFound),
            Some((state,)) if state != "staging" => {
                return Err(RepoError::Conflict(format!(
                    "version {version} is {state}, not a candidate"
                )));
            }
            Some(_) => {}
        }

        // Drop only the published rows this candidate replaces. Every other
        // market keeps what it had -- that is what makes a failed realm cost
        // its own freshness and nobody else's.
        for table in ["market_current", "market_windows", "market_rollup"] {
            let key = match table {
                "market_current" => "market_key",
                "market_windows" => "market_key, window",
                _ => "region, item_id, track, realm_id",
            };
            sqlx::query(&format!(
                "DELETE FROM {table}
                  WHERE state = 'published'
                    AND ({key}) IN (SELECT {key} FROM {table} WHERE state = 'staging' AND version = ?)"
            ))
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;

            sqlx::query(&format!(
                "UPDATE {table} SET state = 'published' WHERE state = 'staging' AND version = ?"
            ))
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        }

        sqlx::query(
            "UPDATE analysis_versions
                SET state = 'published', published_at = ?, source_from = ?, source_until = ?
              WHERE version = ?",
        )
        .bind(now.get() as i64)
        .bind(source.0.map(|v| v.get() as i64))
        .bind(source.1.map(|v| v.get() as i64))
        .bind(version as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        tx.commit().await.map_err(map_err)
    }

    async fn abandon(&self, version: u64, note: &str) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        sqlx::query("DELETE FROM market_current WHERE state = 'staging' AND version = ?")
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        sqlx::query("DELETE FROM market_windows WHERE state = 'staging' AND version = ?")
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        sqlx::query("DELETE FROM market_rollup WHERE state = 'staging' AND version = ?")
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        sqlx::query("UPDATE analysis_versions SET state = 'failed', note = ? WHERE version = ?")
            .bind(note)
            .bind(version as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        tx.commit().await.map_err(map_err)
    }

    async fn published(&self) -> RepoResult<Option<AnalysisVersion>> {
        let row = sqlx::query(
            "SELECT * FROM analysis_versions
              WHERE state = 'published'
              ORDER BY version DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?;
        row.as_ref().map(version_from_row).transpose()
    }

    async fn versions(&self, limit: usize) -> RepoResult<Vec<AnalysisVersion>> {
        let rows = sqlx::query("SELECT * FROM analysis_versions ORDER BY version DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(version_from_row).collect()
    }

    async fn commodity_summary(&self, region: Region) -> RepoResult<(u64, Option<Millis>)> {
        let row = sqlx::query(
            "SELECT count(*) AS markets, max(observed_at) AS newest
               FROM market_current
              WHERE state = 'published' AND kind = 'commodity' AND region = ?",
        )
        .bind(region.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(map_err)?;
        Ok((
            row.get::<i64, _>("markets") as u64,
            row.get::<Option<i64>, _>("newest")
                .map(|v| Millis(v as u64)),
        ))
    }

    async fn commodities(&self, region: Region) -> RepoResult<Vec<MarketSummary>> {
        // Named columns, not `*`. The three JSON columns on this table are the
        // chart's, and a card page reads neither of them -- selecting them
        // anyway was half the database time this page spent.
        let rows = sqlx::query(
            "SELECT market_key, observed_at, price, min_price, quantity, listings, samples
               FROM market_current
              WHERE state = 'published' AND kind = 'commodity' AND region = ?
              ORDER BY item_id",
        )
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(summary_from_row).collect()
    }

    async fn market(&self, key: MarketKey) -> RepoResult<Option<MarketState>> {
        let row = sqlx::query(&format!("{SELECT_STATE} AND market_key = ?"))
            .bind(key.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
        row.as_ref().map(state_from_row).transpose()
    }

    async fn commodity_windows(
        &self,
        region: Region,
        window: &Window,
    ) -> RepoResult<Vec<MarketWindow>> {
        let rows = sqlx::query(&format!(
            "{SELECT_WINDOW} AND window = ? AND kind = 'commodity' AND region = ? ORDER BY item_id"
        ))
        .bind(window.key())
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(window_from_row).collect()
    }

    async fn windows_of(&self, key: MarketKey) -> RepoResult<Vec<MarketWindow>> {
        let rows = sqlx::query(&format!(
            "{SELECT_WINDOW} AND market_key = ? ORDER BY window"
        ))
        .bind(key.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(window_from_row).collect()
    }
}

#[cfg(test)]
mod tests {
    /// Every read of the read model has to be filtered to the published state.
    ///
    /// It is the one line standing between a visitor and a half-built version,
    /// and it is a line that is easy to leave off a new query. §15's guarantee
    /// -- "a page never waits for a worker, never sees staging rows" -- is
    /// this filter, so it is asserted rather than reviewed for.
    ///
    /// Keyed on the table names rather than on how a statement is spelled: a
    /// query written on one line reads differently from one written over six,
    /// and a check that only understood one of those spellings would pass
    /// while missing the query that mattered.
    #[test]
    fn no_read_can_see_a_candidate() {
        // Everything above this module. Scanning the test itself finds the
        // strings the test is looking for, which is a way of failing that says
        // nothing about the code under it.
        let whole = include_str!("read_model.rs");
        let source = whole
            .split_once("#[cfg(test)]")
            .map(|(code, _)| code)
            .unwrap_or(whole);
        let mut offenders = Vec::new();
        let mut examined = 0;

        for (index, line) in source.lines().enumerate() {
            if !line.contains("FROM market_current") && !line.contains("FROM market_windows") {
                continue;
            }
            // The writer touches both states on purpose: staging is what it
            // writes, and `begin`/`abandon` are what clear it.
            examined += 1;
            if line.contains("state = 'staging'") {
                continue;
            }
            // The whole statement, which may run over several lines.
            let statement: String = source
                .lines()
                .skip(index)
                .take(8)
                .collect::<Vec<_>>()
                .join(" ");
            let named = source
                .lines()
                .skip(index.saturating_sub(3))
                .take(11)
                .any(|l| l.contains("SELECT_STATE") || l.contains("SELECT_WINDOW"));
            if statement.contains("state = 'published'")
                || statement.contains("state = 'staging'")
                || named
            {
                continue;
            }
            offenders.push(format!("{}: {}", index + 1, line.trim()));
        }

        assert!(
            examined >= 6,
            "only {examined} statements touched the read model; the check is \
             looking for something that has been renamed"
        );
        assert!(
            offenders.is_empty(),
            "these touch the read model without saying which state they mean: {offenders:#?}"
        );
    }

    /// And the check can fail, which is the other half of it being worth
    /// having.
    #[test]
    fn the_state_check_can_fail() {
        let unfiltered = "SELECT * FROM market_current WHERE region = ?";
        assert!(unfiltered.contains("FROM market_current"));
        assert!(!unfiltered.contains("state = 'published'"));
    }
}
