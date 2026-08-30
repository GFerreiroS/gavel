//! The game's own timeline.
//!
//! Distinct from `events.rs`, which is the cluster's log. A node going offline
//! and a raid opening are both "events" and have nothing else in common: one
//! says how the deployment is doing, the other says what happened in the game.

use app_core::error::RepoResult;
use app_core::market::catalog::ItemKind;
use app_core::market::event::{EventKind, EventScope, Provenance, Validation, Visibility};
use app_core::market::{ItemId, MarketEvent, Region};
use app_core::repo::MarketEventRepository;
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err};

pub struct SqliteMarketEvents {
    pool: Pool<Sqlite>,
}

impl SqliteMarketEvents {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

fn regions_json(regions: &[Region]) -> String {
    serde_json::to_string(regions).unwrap_or_else(|_| "[]".to_string())
}

fn event_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<MarketEvent> {
    let kind: String = row.get("kind");
    let provenance: String = row.get("provenance");
    let validation: String = row.get("validation");
    let visibility: String = row.get("visibility");
    let regions: String = row.get("regions");
    let category: Option<String> = row.get("category");
    let market: Option<String> = row.get("market_key");

    Ok(MarketEvent {
        id: row.get("id"),
        kind: EventKind::parse(&kind).ok_or_else(|| corrupt("event kind", kind))?,
        title: row.get("title"),
        notes: row.get("notes"),
        starts_at: Millis(row.get::<i64, _>("starts_at") as u64),
        ends_at: row
            .get::<Option<i64>, _>("ends_at")
            .map(|v| Millis(v as u64)),
        scope: EventScope {
            // A malformed region list is the one thing here worth surviving:
            // the event still happened, and dropping it because its scope
            // failed to parse would lose more than it protects. Empty means
            // every region, which is also the safe reading.
            regions: serde_json::from_str(&regions).unwrap_or_default(),
            expansion: row.get("expansion"),
            patch: row.get("patch"),
            tier: row.get("tier"),
            category: category
                .as_deref()
                .and_then(|c| ItemKind::ALL.into_iter().find(|k| k.as_str() == c)),
            item: row
                .get::<Option<i64>, _>("item_id")
                .map(|id| ItemId(id as u32)),
            market: market.as_deref().and_then(|k| k.parse().ok()),
        },
        provenance: Provenance::parse(&provenance)
            .ok_or_else(|| corrupt("event provenance", provenance))?,
        validation: Validation::parse(&validation)
            .ok_or_else(|| corrupt("event validation", validation))?,
        visibility: Visibility::parse(&visibility)
            .ok_or_else(|| corrupt("event visibility", visibility))?,
    })
}

impl MarketEventRepository for SqliteMarketEvents {
    async fn record(&self, events: &[MarketEvent]) -> RepoResult<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0u64;
        for event in events {
            // `DO NOTHING`: the catalogue's events are re-derived at every
            // start, and starting twice must not produce two patch releases.
            // It also means an administrator's later edit is not overwritten
            // by a seed, which is the same rule the release states follow.
            let result = sqlx::query(
                "INSERT INTO market_events
                   (id, kind, title, notes, starts_at, ends_at, regions, expansion,
                    patch, tier, category, item_id, market_key, provenance,
                    validation, visibility, recorded_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&event.id)
            .bind(event.kind.as_str())
            .bind(&event.title)
            .bind(event.notes.as_deref())
            .bind(event.starts_at.get() as i64)
            .bind(event.ends_at.map(|v| v.get() as i64))
            .bind(regions_json(&event.scope.regions))
            .bind(event.scope.expansion.as_deref())
            .bind(event.scope.patch.as_deref())
            .bind(event.scope.tier.as_deref())
            .bind(event.scope.category.map(|c| c.as_str()))
            .bind(event.scope.item.map(|i| i.get() as i64))
            .bind(event.scope.market.map(|m| m.to_string()))
            .bind(event.provenance.as_str())
            .bind(event.validation.as_str())
            .bind(event.visibility.as_str())
            .bind(event.starts_at.get() as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += result.rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn between(
        &self,
        from: Millis,
        until: Millis,
        public_only: bool,
    ) -> RepoResult<Vec<MarketEvent>> {
        // An event with no end overlaps the window if it starts inside it; one
        // with an end overlaps if the two intervals touch at all. Written as
        // one predicate rather than two queries because a caller asking "what
        // was going on then" wants both kinds in one ordered list.
        let sql = "SELECT * FROM market_events
                    WHERE starts_at < ?
                      AND (ends_at IS NULL OR ends_at >= ?)
                      AND starts_at >= CASE WHEN ends_at IS NULL THEN ? ELSE 0 END
                      AND (? = 0 OR (visibility = 'public' AND validation = 'validated'))
                    ORDER BY starts_at, id";
        let rows = sqlx::query(sql)
            .bind(until.get() as i64)
            .bind(from.get() as i64)
            .bind(from.get() as i64)
            .bind(i64::from(public_only))
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(event_from_row).collect()
    }

    async fn recent(&self, limit: usize) -> RepoResult<Vec<MarketEvent>> {
        let rows = sqlx::query("SELECT * FROM market_events ORDER BY starts_at DESC, id LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(event_from_row).collect()
    }

    async fn review(
        &self,
        id: &str,
        validation: Validation,
        visibility: Visibility,
    ) -> RepoResult<bool> {
        // Both columns in one statement, because they are one decision: a
        // separate update per column would allow "published and unchecked" to
        // exist between them, which is the state that must never be reachable.
        let result =
            sqlx::query("UPDATE market_events SET validation = ?, visibility = ? WHERE id = ?")
                .bind(validation.as_str())
                .bind(visibility.as_str())
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(map_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn forget(&self, id: &str) -> RepoResult<bool> {
        // The provenance filter is in the statement rather than in a check
        // before it: a catalogue or calendar event is re-derived at every
        // start, so deleting one would delete it until the next restart put it
        // back -- a button that appears not to work.
        let result =
            sqlx::query("DELETE FROM market_events WHERE id = ? AND provenance = 'administrator'")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(map_err)?;
        Ok(result.rows_affected() > 0)
    }
}
