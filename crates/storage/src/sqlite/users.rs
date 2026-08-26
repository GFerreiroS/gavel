use app_core::error::RepoResult;
use app_core::model::{Credentials, LinkedAccount, User, UserId};
use app_core::repo::UserRepository;
use cluster_core::Millis;
use sqlx::sqlite::SqliteRow;
use sqlx::{Pool, Row, Sqlite};

use super::map_err;

#[derive(Clone)]
pub struct SqliteUsers {
    pool: Pool<Sqlite>,
}

impl SqliteUsers {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

fn user_from_row(row: &SqliteRow) -> User {
    User {
        id: row.get::<i64, _>("id"),
        username: row.get::<String, _>("username"),
        created_at: Millis(row.get::<i64, _>("created_at") as u64),
    }
}

impl UserRepository for SqliteUsers {
    async fn create(&self, username: &str, password_hash: &str, now: Millis) -> RepoResult<User> {
        let row = sqlx::query(
            "INSERT INTO users(username, password_hash, created_at) VALUES(?, ?, ?)
             RETURNING id, username, created_at",
        )
        .bind(username)
        .bind(password_hash)
        .bind(now.get() as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(user_from_row(&row))
    }

    async fn by_username(&self, username: &str) -> RepoResult<Option<Credentials>> {
        let row = sqlx::query(
            "SELECT id, username, created_at, password_hash FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(row.map(|row| Credentials {
            user: user_from_row(&row),
            password_hash: row.get::<String, _>("password_hash"),
        }))
    }

    async fn by_id(&self, id: UserId) -> RepoResult<Option<User>> {
        let row = sqlx::query("SELECT id, username, created_at FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(row.as_ref().map(user_from_row))
    }

    async fn linked_accounts(&self, id: UserId) -> RepoResult<Vec<LinkedAccount>> {
        let rows = sqlx::query(
            "SELECT user_id, provider, external_id, display_name, linked_at
             FROM linked_accounts WHERE user_id = ? ORDER BY provider",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .into_iter()
            .map(|row| LinkedAccount {
                user_id: row.get::<i64, _>("user_id"),
                provider: row.get::<String, _>("provider"),
                external_id: row.get::<String, _>("external_id"),
                display_name: row.get::<String, _>("display_name"),
                linked_at: Millis(row.get::<i64, _>("linked_at") as u64),
            })
            .collect())
    }
}
