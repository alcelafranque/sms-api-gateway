//! Background tasks.

pub mod inbound;
pub mod outbound;

use std::sync::Arc;
use std::time::Duration;

use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::config::RetentionConfig;
use crate::messages;
use crate::metrics as m;
use crate::modem_ctl::ModemHandle;

/// Periodically refreshes the radio status (signal level...).
pub async fn status_refresher(
    modem: Arc<ModemHandle>,
    interval: Duration,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if let Ok(session) = modem.session().await
            && let Err(e) = session.status().await
        {
            tracing::warn!(error = %e, "cannot read modem status");
        }
    }
}

/// Publishes queue gauges and runs exporter upkeep.
pub async fn stats(pool: PgPool, metrics: PrometheusHandle, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        match messages::queue_depth(&pool).await {
            Ok(depth) => {
                metrics::gauge!(m::OUTBOUND_QUEUE_DEPTH).set(depth.queued as f64);
                metrics::gauge!(m::WEBHOOK_QUEUE_DEPTH).set(depth.webhooks_pending as f64);
            }
            Err(e) => tracing::warn!(error = %e, "cannot compute queue depth"),
        }
        metrics.run_upkeep();
    }
}

/// Purges old message bodies, messages and webhook deliveries.
pub async fn retention(pool: PgPool, config: RetentionConfig, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs.max(60)));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if let Err(e) = purge(&pool, &config).await {
            tracing::error!(error = %e, "retention job failed");
        }
    }
}

async fn purge(pool: &PgPool, config: &RetentionConfig) -> sqlx::Result<()> {
    // Messages still in flight are never touched.
    if config.message_body_days > 0 {
        let days = config.message_body_days as i32;
        let bodies = sqlx::query(
            "UPDATE messages SET body = NULL, body_purged_at = now(), updated_at = now()
             WHERE body IS NOT NULL AND status NOT IN ('queued', 'sending')
               AND created_at < now() - make_interval(days => $1)",
        )
        .bind(days)
        .execute(pool)
        .await?
        .rows_affected();
        let payloads = sqlx::query(
            "UPDATE webhook_deliveries
             SET payload = jsonb_set(payload, '{data,text}', 'null'), updated_at = now()
             WHERE status <> 'pending' AND payload #>> '{data,text}' IS NOT NULL
               AND created_at < now() - make_interval(days => $1)",
        )
        .bind(days)
        .execute(pool)
        .await?
        .rows_affected();
        record("message_bodies", bodies + payloads);
    }
    if config.message_days > 0 {
        let n = sqlx::query(
            "DELETE FROM messages
             WHERE status NOT IN ('queued', 'sending')
               AND created_at < now() - make_interval(days => $1)",
        )
        .bind(config.message_days as i32)
        .execute(pool)
        .await?
        .rows_affected();
        record("messages", n);
    }
    if config.webhook_days > 0 {
        let n = sqlx::query(
            "DELETE FROM webhook_deliveries
             WHERE status <> 'pending' AND created_at < now() - make_interval(days => $1)",
        )
        .bind(config.webhook_days as i32)
        .execute(pool)
        .await?
        .rows_affected();
        record("webhook_deliveries", n);
    }
    Ok(())
}

fn record(kind: &'static str, n: u64) {
    if n > 0 {
        tracing::info!(kind, rows = n, "retention purge");
        metrics::counter!(m::RETENTION_PURGED, "kind" => kind).increment(n);
    }
}
