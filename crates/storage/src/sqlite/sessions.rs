use app_core::error::RepoResult;
use app_core::model::Session;
use app_core::repo::SessionRepository;
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::map_err;

#[derive(Clone)]
pub struct SqliteSessions {
    pool: Pool<Sqlite>,
}

impl SqliteSessions {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl SessionRepository for SqliteSessions {
    async fn create(&self, session: &Session) -> RepoResult<()> {
        sqlx::query("INSERT INTO sessions(id, user_id, created_at, expires_at) VALUES(?, ?, ?, ?)")
            .bind(&session.id)
            .bind(session.user_id)
            .bind(session.created_at.get() as i64)
            .bind(session.expires_at.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn get(&self, id: &str) -> RepoResult<Option<Session>> {
        let row =
            sqlx::query("SELECT id, user_id, created_at, expires_at FROM sessions WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_err)?;
        Ok(row.map(|row| Session {
            id: row.get::<String, _>("id"),
            user_id: row.get::<i64, _>("user_id"),
            created_at: Millis(row.get::<i64, _>("created_at") as u64),
            expires_at: Millis(row.get::<i64, _>("expires_at") as u64),
        }))
    }

    async fn delete(&self, id: &str) -> RepoResult<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn purge_expired(&self, now: Millis) -> RepoResult<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(now.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(result.rows_affected())
    }
}
