//! Database-backed tests. Each test gets a fresh database created by
//! `#[sqlx::test]` from `DATABASE_URL` (a PostgreSQL role allowed to create
//! databases), with the gateway migrations applied.

mod api;
mod inbound;
mod outbound;
mod webhooks;

use std::sync::Arc;
use std::time::Duration;

use gateway::api::dev::DevModem;
use gateway::apps::{self, App, CreateApp};
use gateway::config::{
    Config, DatabaseConfig, InboundConfig, OutboundConfig, RetentionConfig, ValidationConfig,
    WebhookConfig,
};
use gateway::messages::{self, InsertOutcome, MESSAGE_COLUMNS, Message, NewOutbound};
use gateway::modem_ctl::{ModemHandle, PauseState};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

pub async fn create_app(
    pool: &PgPool,
    name: &str,
    webhook_url: Option<&str>,
    keywords: &[&str],
    admin: bool,
) -> (App, String) {
    apps::create(
        pool,
        CreateApp {
            name: name.into(),
            admin,
            webhook_url: webhook_url.map(Into::into),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
        },
    )
    .await
    .unwrap()
}

pub fn modem() -> (Arc<DevModem>, Arc<ModemHandle>) {
    let dev = Arc::new(DevModem::default());
    let handle = Arc::new(ModemHandle::new(
        dev.clone(),
        PauseState::default(),
        Duration::from_secs(5),
    ));
    (dev, handle)
}

pub fn wake() -> Arc<Notify> {
    Arc::new(Notify::new())
}

pub fn config() -> Config {
    Config {
        server: Default::default(),
        metrics: Default::default(),
        log: Default::default(),
        database: DatabaseConfig {
            url: String::new(),
            max_connections: 1,
            connect_timeout_secs: 1,
            migrate_on_start: false,
        },
        modem: None,
        outbound: OutboundConfig {
            min_interval_ms: 0,
            max_attempts: 3,
            ..Default::default()
        },
        inbound: InboundConfig::default(),
        validation: ValidationConfig::default(),
        webhooks: WebhookConfig::default(),
        retention: RetentionConfig::default(),
    }
}

pub async fn queue(pool: &PgPool, app_id: Uuid, to: &str, text: &str) -> Message {
    let outcome = messages::insert_outbound(
        pool,
        NewOutbound {
            app_id,
            to,
            text,
            client_ref: None,
            idempotency_key: None,
        },
    )
    .await
    .unwrap();
    match outcome {
        InsertOutcome::Created(message) => message,
        InsertOutcome::Existing { .. } => unreachable!("no idempotency key"),
    }
}

pub async fn message(pool: &PgPool, id: Uuid) -> Message {
    sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Makes every scheduled retry due now.
pub async fn make_due(pool: &PgPool) {
    sqlx::query("UPDATE messages SET next_attempt_at = now() WHERE status = 'queued'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() WHERE status = 'pending'")
        .execute(pool)
        .await
        .unwrap();
}

/// `(event_type, app_id, message_id)` of every webhook delivery, oldest
/// first.
pub async fn deliveries(pool: &PgPool) -> Vec<(String, Uuid, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT event_type, app_id, message_id FROM webhook_deliveries ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}
