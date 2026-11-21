//! PostgreSQL pool, migrations and small persistent settings.

use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::config::DatabaseConfig;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(config: &DatabaseConfig) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.connect_timeout_secs))
        .connect(&config.url)
        .await
        .context("cannot connect to PostgreSQL")
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("database migration failed")
}

pub async fn get_setting<T: DeserializeOwned>(
    pool: &PgPool,
    key: &str,
) -> anyhow::Result<Option<T>> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM gateway_settings WHERE key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(value.map(serde_json::from_value).transpose()?)
}

pub async fn set_setting<T: Serialize>(pool: &PgPool, key: &str, value: &T) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO gateway_settings (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(serde_json::to_value(value)?)
    .execute(pool)
    .await?;
    Ok(())
}
