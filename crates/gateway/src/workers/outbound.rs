//! Outbound worker: claims queued messages and sends them through the modem.
//!
//! Retry semantics follow [`modem::ModemError::is_retryable`]: only
//! `Unavailable` (nothing reached the device) is retried. `Unknown` ends in
//! status `unknown` and is never re-sent; `Rejected` / `Protocol` end in
//! `failed`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use modem::{ModemError, SendOutcome};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::OutboundConfig;
use crate::messages::{MESSAGE_COLUMNS, Message, Status};
use crate::metrics as m;
use crate::modem_ctl::{ModemHandle, error_kind};
use crate::phone::Masked;
use crate::webhooks::{self, EventType};

pub struct OutboundWorker {
    pub pool: PgPool,
    pub modem: Arc<ModemHandle>,
    pub config: OutboundConfig,
    pub wake: Arc<Notify>,
    pub webhook_wake: Arc<Notify>,
}

const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Result of one worker iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The modem is paused: nothing was claimed.
    Paused,
    /// No message is due.
    Idle,
    /// One message was attempted and now has this status.
    Attempted { status: Status, retryable: bool },
}

/// What to record after a send attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub status: Status,
    /// Delay before the next attempt (status `queued` only).
    pub retry_in: Option<Duration>,
    pub event: Option<EventType>,
    /// Label of the `sms_outbound_sends_total` metric.
    pub outcome: &'static str,
}

/// Exponential backoff: `base * 2^(attempt - 1)`, capped at `max`.
pub fn backoff(base_secs: u64, max_secs: u64, attempt: u32) -> Duration {
    let base = base_secs.max(1);
    let secs = base.saturating_mul(1u64 << attempt.saturating_sub(1).min(20));
    Duration::from_secs(secs.min(max_secs.max(base)))
}

/// Maps a send result to the message's next state. `attempts` counts the
/// attempt just made.
pub fn decide(
    result: &modem::Result<SendOutcome>,
    attempts: i32,
    config: &OutboundConfig,
) -> Decision {
    let (status, retry_in, event, outcome) = match result {
        Ok(SendOutcome::Sent) => (Status::Sent, None, Some(EventType::Sent), "sent"),
        Ok(SendOutcome::Submitted) => (Status::Submitted, None, Some(EventType::Sent), "submitted"),
        Err(e) if e.is_retryable() && attempts < config.max_attempts => {
            let delay = backoff(
                config.retry_base_secs,
                config.retry_max_secs,
                attempts.max(1) as u32,
            );
            (Status::Queued, Some(delay), None, "retry")
        }
        Err(ModemError::Unknown(_)) => (Status::Unknown, None, Some(EventType::Unknown), "unknown"),
        Err(_) => (Status::Failed, None, Some(EventType::Failed), "failed"),
    };
    Decision {
        status,
        retry_in,
        event,
        outcome,
    }
}

impl OutboundWorker {
    pub async fn run(self, shutdown: CancellationToken) {
        let idle = Duration::from_millis(self.config.idle_poll_ms);
        let min_interval = Duration::from_millis(self.config.min_interval_ms);
        let mut pause_rx = self.modem.subscribe_pause();
        let mut last_attempt: Option<Instant> = None;
        let mut last_reap: Option<Instant> = None;
        // Consecutive `Unavailable` errors: the device is probably down, so
        // slow down instead of burning the attempts of every queued message.
        let mut unavailable_streak: u32 = 0;

        loop {
            if shutdown.is_cancelled() {
                break;
            }
            if last_reap.is_none_or(|t| t.elapsed() >= REAP_INTERVAL) {
                if let Err(e) = self.reap_stale().await {
                    tracing::error!(error = %e, "cannot reap stale sends");
                }
                last_reap = Some(Instant::now());
            }

            // Pacing between two sends, and backoff while the device is down.
            let mut wait = Duration::ZERO;
            if let Some(t) = last_attempt {
                wait = min_interval.saturating_sub(t.elapsed());
            }
            if unavailable_streak > 0 {
                wait = wait.max(backoff(
                    self.config.retry_base_secs,
                    self.config.retry_max_secs,
                    unavailable_streak,
                ));
            }
            if !wait.is_zero() {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(wait) => {}
                }
            }

            match self.step().await {
                Ok(Step::Attempted { retryable, .. }) => {
                    last_attempt = Some(Instant::now());
                    unavailable_streak = if retryable {
                        unavailable_streak.saturating_add(1)
                    } else {
                        0
                    };
                }
                Ok(Step::Paused) => {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = pause_rx.changed() => {}
                        _ = tokio::time::sleep(idle) => {}
                    }
                }
                Ok(Step::Idle) => {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(idle) => {}
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "outbound worker database error");
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(idle) => {}
                    }
                }
            }
        }
        tracing::info!("outbound worker stopped");
    }

    /// Claims the oldest due message, if any, and attempts to send it.
    pub async fn step(&self) -> sqlx::Result<Step> {
        let Ok(session) = self.modem.session().await else {
            return Ok(Step::Paused);
        };
        let Some(message) = self.claim().await? else {
            return Ok(Step::Idle);
        };
        let result = match message.body.as_deref() {
            Some(body) => session.send(&message.peer, body).await,
            None => Err(ModemError::Rejected("message body missing".into())),
        };
        drop(session);
        let retryable = matches!(&result, Err(e) if e.is_retryable());
        let status = self.complete(&message, result).await?;
        Ok(Step::Attempted { status, retryable })
    }

    /// Moves the oldest due message to `sending` (attempts + 1).
    async fn claim(&self) -> sqlx::Result<Option<Message>> {
        sqlx::query_as(&format!(
            "UPDATE messages SET status = 'sending', attempts = attempts + 1, updated_at = now()
             WHERE id = (
                SELECT id FROM messages
                WHERE status = 'queued' AND next_attempt_at <= now()
                ORDER BY next_attempt_at, created_at
                LIMIT 1
                FOR UPDATE SKIP LOCKED
             )
             RETURNING {MESSAGE_COLUMNS}"
        ))
        .fetch_optional(&self.pool)
        .await
    }

    async fn complete(
        &self,
        message: &Message,
        result: modem::Result<SendOutcome>,
    ) -> sqlx::Result<Status> {
        let decision = decide(&result, message.attempts, &self.config);
        let error = result.as_ref().err().map(ToString::to_string);
        let to = Masked(&message.peer);
        metrics::counter!(m::OUTBOUND_SENDS, "outcome" => decision.outcome).increment(1);
        match &result {
            Ok(_) => tracing::info!(
                message_id = %message.id, %to, attempt = message.attempts,
                status = decision.status.as_str(), "message sent"
            ),
            Err(e) => tracing::warn!(
                message_id = %message.id, %to, attempt = message.attempts,
                kind = error_kind(e), error = %e, status = decision.status.as_str(),
                "send failed"
            ),
        }

        let retry_at = decision.retry_in.map(|d| chrono::Utc::now() + d);
        let mut tx = self.pool.begin().await?;
        let updated: Message = sqlx::query_as(&format!(
            "UPDATE messages
             SET status = $2, last_error = $3,
                 next_attempt_at = COALESCE($4, next_attempt_at),
                 sent_at = CASE WHEN $2 IN ('sent', 'submitted') THEN now() ELSE sent_at END,
                 updated_at = now()
             WHERE id = $1
             RETURNING {MESSAGE_COLUMNS}"
        ))
        .bind(message.id)
        .bind(decision.status.as_str())
        .bind(&error)
        .bind(retry_at)
        .fetch_one(&mut *tx)
        .await?;
        let mut queued = false;
        if let Some(event) = decision.event {
            queued = webhooks::enqueue(&mut tx, event, &updated).await?;
        }
        tx.commit().await?;
        if queued {
            self.webhook_wake.notify_one();
        }
        Ok(decision.status)
    }

    /// A message stuck in `sending` means a gateway died during the device
    /// call: whether the SMS left is unknown, so it is never re-sent.
    /// Returns the number of messages moved to `unknown`.
    pub async fn reap_stale(&self) -> sqlx::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let stale: Vec<Message> = sqlx::query_as(&format!(
            "UPDATE messages
             SET status = 'unknown', updated_at = now(),
                 last_error = 'gateway interrupted during the send'
             WHERE status = 'sending'
               AND updated_at < now() - make_interval(secs => $1)
             RETURNING {MESSAGE_COLUMNS}"
        ))
        .bind(self.config.sending_stale_secs as f64)
        .fetch_all(&mut *tx)
        .await?;
        for message in &stale {
            tracing::warn!(message_id = %message.id, "stale send moved to unknown");
            metrics::counter!(m::OUTBOUND_SENDS, "outcome" => "unknown").increment(1);
            webhooks::enqueue(&mut tx, EventType::Unknown, message).await?;
        }
        tx.commit().await?;
        if !stale.is_empty() {
            self.webhook_wake.notify_one();
        }
        Ok(stale.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OutboundConfig {
        OutboundConfig {
            max_attempts: 3,
            retry_base_secs: 10,
            retry_max_secs: 60,
            ..OutboundConfig::default()
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let secs: Vec<u64> = (1..=5).map(|a| backoff(10, 60, a).as_secs()).collect();
        assert_eq!(secs, [10, 20, 40, 60, 60]);
        // Huge attempt counts must not overflow.
        assert_eq!(backoff(10, 60, u32::MAX).as_secs(), 60);
    }

    #[test]
    fn send_outcomes_map_to_distinct_statuses() {
        let sent = decide(&Ok(SendOutcome::Sent), 1, &config());
        assert_eq!(
            (sent.status, sent.event),
            (Status::Sent, Some(EventType::Sent))
        );
        let submitted = decide(&Ok(SendOutcome::Submitted), 1, &config());
        assert_eq!(
            (submitted.status, submitted.event),
            (Status::Submitted, Some(EventType::Sent))
        );
    }

    #[test]
    fn only_unavailable_is_retried_until_max_attempts() {
        let unavailable = || Err(ModemError::Unavailable("down".into()));
        let first = decide(&unavailable(), 1, &config());
        assert_eq!(first.status, Status::Queued);
        assert_eq!(first.retry_in, Some(Duration::from_secs(10)));
        assert_eq!(first.event, None);
        assert_eq!(
            decide(&unavailable(), 2, &config()).retry_in,
            Some(Duration::from_secs(20))
        );
        let last = decide(&unavailable(), 3, &config());
        assert_eq!(
            (last.status, last.event),
            (Status::Failed, Some(EventType::Failed))
        );

        let unknown = decide(&Err(ModemError::Unknown("timeout".into())), 1, &config());
        assert_eq!(
            (unknown.status, unknown.retry_in, unknown.event),
            (Status::Unknown, None, Some(EventType::Unknown))
        );
        for e in [
            ModemError::Rejected("bad number".into()),
            ModemError::Protocol("garbage".into()),
        ] {
            let d = decide(&Err(e), 1, &config());
            assert_eq!(
                (d.status, d.retry_in, d.event),
                (Status::Failed, None, Some(EventType::Failed))
            );
        }
    }
}
