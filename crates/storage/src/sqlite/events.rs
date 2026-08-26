use app_core::error::RepoResult;
use app_core::repo::EventRepository;
use cluster_core::{ClusterEvent, EventRecord, Millis};
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err};

#[derive(Clone)]
pub struct SqliteEvents {
    pool: Pool<Sqlite>,
}

impl SqliteEvents {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl EventRepository for SqliteEvents {
    async fn append(&self, record: &EventRecord) -> RepoResult<()> {
        let payload =
            serde_json::to_string(&record.event).map_err(|e| corrupt("event payload", e))?;
        // Events are replayed on restart, so a duplicate seq is a no-op rather
        // than an error.
        sqlx::query(
            "INSERT INTO cluster_events(seq, at, kind, node_id, payload_json)
             VALUES(?, ?, ?, ?, ?) ON CONFLICT(seq) DO NOTHING",
        )
        .bind(record.seq as i64)
        .bind(record.at.get() as i64)
        .bind(record.event.kind())
        .bind(record.event.node().map(|n| n.get() as i64))
        .bind(payload)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn recent(&self, limit: usize) -> RepoResult<Vec<EventRecord>> {
        let rows = sqlx::query(
            "SELECT seq, at, payload_json FROM cluster_events ORDER BY seq DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.into_iter()
            .map(|row| {
                let payload: String = row.get("payload_json");
                Ok(EventRecord {
                    seq: row.get::<i64, _>("seq") as u64,
                    at: Millis(row.get::<i64, _>("at") as u64),
                    event: serde_json::from_str::<ClusterEvent>(&payload)
                        .map_err(|e| corrupt("event payload", e))?,
                })
            })
            .collect()
    }

    async fn last_seq(&self) -> RepoResult<u64> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS seq FROM cluster_events")
            .fetch_one(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(row.get::<i64, _>("seq") as u64)
    }
}
