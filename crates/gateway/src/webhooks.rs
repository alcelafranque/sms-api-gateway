//! Outgoing webhooks: event enqueueing, HMAC signing and the dispatcher.
//!
//! Each event is a row of `webhook_deliveries` whose id is the event id sent
//! to the receiver (`X-Webhook-Id` header and `id` field) for dedup. The
//! signature is `X-Signature: sha256=<hex(hmac_sha256(secret,
//! "<X-Timestamp>.<raw body>"))>`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use sqlx::{PgConnection, PgPool};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::WebhookConfig;
use crate::messages::{Message, MessageView};
use crate::metrics as m;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Received,
    Sent,
    Failed,
    Unknown,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::Received => "message.received",
            EventType::Sent => "message.sent",
            EventType::Failed => "message.failed",
            EventType::Unknown => "message.unknown",
        }
    }
}

#[derive(Serialize)]
struct Event<'a> {
    id: Uuid,
    #[serde(rename = "type")]
    event_type: &'a str,
    created_at: DateTime<Utc>,
    data: MessageView,
}

/// Queues `event` for `message` if its app has a webhook configured.
/// Returns whether a delivery was queued.
pub async fn enqueue(
    conn: &mut PgConnection,
    event_type: EventType,
    message: &Message,
) -> sqlx::Result<bool> {
    let Some(app_id) = message.app_id else {
        return Ok(false);
    };
    let id = Uuid::now_v7();
    let payload = serde_json::to_value(Event {
        id,
        event_type: event_type.as_str(),
        created_at: Utc::now(),
        data: MessageView::from(message),
    })
    .expect("serializable event");
    let res = sqlx::query(
        "INSERT INTO webhook_deliveries (id, app_id, message_id, event_type, payload)
         SELECT $1, a.id, $3, $4, $5 FROM apps a
         WHERE a.id = $2 AND a.enabled AND a.webhook_url IS NOT NULL",
    )
    .bind(id)
    .bind(app_id)
    .bind(message.id)
    .bind(event_type.as_str())
    .bind(payload)
    .execute(conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[derive(sqlx::FromRow)]
struct Claimed {
    id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    attempts: i32,
    created_at: DateTime<Utc>,
    enabled: bool,
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
}

pub struct Dispatcher {
    pool: PgPool,
    config: WebhookConfig,
    client: reqwest::Client,
    wake: Arc<Notify>,
}

impl Dispatcher {
    pub fn new(pool: PgPool, config: WebhookConfig, wake: Arc<Notify>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("sms-gateway/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            pool,
            config,
            client,
            wake,
        })
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let this = Arc::new(self);
        let idle = Duration::from_millis(this.config.poll_interval_ms);
        loop {
            let busy = match this.dispatch_once().await {
                Ok(n) => n > 0,
                Err(e) => {
                    tracing::error!(error = %e, "webhook claim failed");
                    false
                }
            };
            if shutdown.is_cancelled() {
                break;
            }
            if !busy {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = this.wake.notified() => {}
                    _ = tokio::time::sleep(idle) => {}
                }
            }
        }
        tracing::info!("webhook dispatcher stopped");
    }

    /// Attempts one batch of due deliveries; returns how many were
    /// attempted. In-flight deliveries are short (bounded by the HTTP
    /// timeout) and always run to completion so their outcome is recorded.
    pub async fn dispatch_once(self: &Arc<Self>) -> sqlx::Result<usize> {
        let claimed = self.claim().await?;
        let n = claimed.len();
        let mut tasks = JoinSet::new();
        for delivery in claimed {
            let this = self.clone();
            tasks.spawn(async move { this.deliver(delivery).await });
        }
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                tracing::error!(error = %e, "webhook task panicked");
            }
        }
        Ok(n)
    }

    /// Leases due deliveries: pushing `next_attempt_at` forward hides them
    /// from other dispatchers while being attempted and lets them come back
    /// if this process dies.
    async fn claim(&self) -> sqlx::Result<Vec<Claimed>> {
        let lease = (self.config.timeout_secs + self.config.connect_timeout_secs + 30) as f64;
        sqlx::query_as(
            "WITH due AS (
                SELECT id FROM webhook_deliveries
                WHERE status = 'pending' AND next_attempt_at <= now()
                ORDER BY next_attempt_at
                LIMIT $1
                FOR UPDATE SKIP LOCKED
             )
             UPDATE webhook_deliveries d
             SET next_attempt_at = now() + make_interval(secs => $2), updated_at = now()
             FROM due, apps a
             WHERE d.id = due.id AND a.id = d.app_id
             RETURNING d.id, d.event_type, d.payload, d.attempts, d.created_at,
                       a.enabled, a.webhook_url, a.webhook_secret",
        )
        .bind(self.config.concurrency as i64)
        .bind(lease)
        .fetch_all(&self.pool)
        .await
    }

    async fn deliver(&self, d: Claimed) {
        let (url, secret) = match (d.enabled, d.webhook_url, d.webhook_secret) {
            (true, Some(url), Some(secret)) => (url, secret),
            _ => {
                self.finish(
                    d.id,
                    d.attempts,
                    "failed",
                    None,
                    Some("app disabled or webhook removed"),
                    None,
                )
                .await;
                metrics::counter!(m::WEBHOOK_DELIVERIES, "outcome" => "dropped").increment(1);
                return;
            }
        };
        let body = serde_json::to_vec(&d.payload).expect("serializable payload");
        let timestamp = Utc::now().timestamp();
        let started = Instant::now();
        let result = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Webhook-Id", d.id.to_string())
            .header("X-Event-Type", &d.event_type)
            .header("X-Timestamp", timestamp.to_string())
            .header("X-Signature", sign(&secret, timestamp, &body))
            .body(body)
            .send()
            .await;
        let elapsed = started.elapsed();
        let (status_code, error) = match result {
            Ok(resp) if resp.status().is_success() => (Some(resp.status().as_u16()), None),
            Ok(resp) => (
                Some(resp.status().as_u16()),
                Some(format!("HTTP {}", resp.status())),
            ),
            Err(e) => (None, Some(describe_reqwest_error(e))),
        };
        let attempts = d.attempts + 1;
        if let Err(e) = sqlx::query(
            "INSERT INTO webhook_attempts (delivery_id, status_code, error, duration_ms)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(d.id)
        .bind(status_code.map(i32::from))
        .bind(&error)
        .bind(elapsed.as_millis().min(i32::MAX as u128) as i32)
        .execute(&self.pool)
        .await
        {
            tracing::error!(error = %e, "cannot record webhook attempt");
        }

        let Some(error) = error else {
            tracing::info!(event_id = %d.id, event = %d.event_type, attempts, "webhook delivered");
            metrics::counter!(m::WEBHOOK_DELIVERIES, "outcome" => "delivered").increment(1);
            self.finish(d.id, attempts, "delivered", status_code, None, None)
                .await;
            return;
        };

        let schedule = &self.config.retry_schedule_secs;
        let delay = schedule[(d.attempts as usize).min(schedule.len() - 1)];
        let next = Utc::now() + chrono::Duration::seconds(delay as i64);
        let deadline = d.created_at + chrono::Duration::seconds(self.config.max_age_secs as i64);
        if next > deadline {
            tracing::warn!(event_id = %d.id, attempts, %error, "webhook abandoned");
            metrics::counter!(m::WEBHOOK_DELIVERIES, "outcome" => "failed").increment(1);
            self.finish(d.id, attempts, "failed", status_code, Some(&error), None)
                .await;
        } else {
            tracing::info!(event_id = %d.id, attempts, %error, retry_in_secs = delay, "webhook failed, will retry");
            metrics::counter!(m::WEBHOOK_DELIVERIES, "outcome" => "retry").increment(1);
            self.finish(
                d.id,
                attempts,
                "pending",
                status_code,
                Some(&error),
                Some(next),
            )
            .await;
        }
    }

    async fn finish(
        &self,
        id: Uuid,
        attempts: i32,
        status: &str,
        status_code: Option<u16>,
        error: Option<&str>,
        next: Option<DateTime<Utc>>,
    ) {
        let res = sqlx::query(
            "UPDATE webhook_deliveries
             SET status = $2, attempts = $3, last_status_code = $4, last_error = $5,
                 next_attempt_at = COALESCE($6, next_attempt_at),
                 delivered_at = CASE WHEN $2 = 'delivered' THEN now() END,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(status)
        .bind(attempts)
        .bind(status_code.map(i32::from))
        .bind(error)
        .bind(next)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "cannot update webhook delivery");
        }
    }
}

fn describe_reqwest_error(e: reqwest::Error) -> String {
    // The URL may embed credentials: never include it.
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_hmac_sha256_of_timestamp_dot_body() {
        // Reference value computed independently:
        // python3 -c 'import hmac,hashlib;print(hmac.new(b"whsec_test",
        //   b"1700000000.{\"a\":1}", hashlib.sha256).hexdigest())'
        assert_eq!(
            sign("whsec_test", 1_700_000_000, br#"{"a":1}"#),
            "sha256=38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789"
        );
    }
}
