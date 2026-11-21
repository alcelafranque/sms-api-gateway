//! Inbound poller: reads the device inbox, stores new messages, deletes them
//! from the device and queues `message.received` webhooks.
//!
//! Order: insert (one transaction, webhook deliveries included) → delete
//! from the device → wake the webhook dispatcher. A crash between two steps
//! loses nothing: messages still on the device are deduplicated on
//! `(sender, device reception time, sha256(text))` at the next scan.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use modem::InboundSms;
use sqlx::{PgConnection, PgPool};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::apps::{self, App};
use crate::config::InboundConfig;
use crate::messages::{MESSAGE_COLUMNS, Message, sha256};
use crate::metrics as m;
use crate::modem_ctl::{ModemHandle, ModemSession};
use crate::phone::{Masked, NumberPolicy};
use crate::webhooks::{self, EventType};

pub struct InboundPoller {
    pub pool: PgPool,
    pub modem: Arc<ModemHandle>,
    pub config: InboundConfig,
    pub timezone: Tz,
    pub policy: Arc<NumberPolicy>,
    pub webhook_wake: Arc<Notify>,
}

#[derive(Default)]
pub struct PollState {
    last_full_scan: Option<Instant>,
    /// Unread count seen at the last scan, to avoid rescanning an unchanged
    /// inbox when messages are kept on the device.
    unread_at_last_scan: Option<u32>,
}

impl InboundPoller {
    pub async fn run(self, shutdown: CancellationToken) {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut state = PollState::default();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            if let Err(e) = self.poll_once(&mut state).await {
                tracing::warn!(error = %e, "inbox poll failed");
            }
        }
        tracing::info!("inbound poller stopped");
    }

    /// One poll cycle; returns `false` when skipped because the modem is
    /// paused.
    pub async fn poll_once(&self, state: &mut PollState) -> anyhow::Result<bool> {
        let Ok(session) = self.modem.session().await else {
            return Ok(false);
        };
        self.poll(&session, state).await?;
        self.modem.set_last_poll(Utc::now());
        Ok(true)
    }

    async fn poll(&self, session: &ModemSession<'_>, state: &mut PollState) -> anyhow::Result<()> {
        let unread = session.unread_count().await?;
        let full_due = state.last_full_scan.is_none_or(|t| {
            t.elapsed() >= Duration::from_secs(self.config.full_scan_interval_secs)
        });
        let news = unread > 0
            && (self.config.delete_after_ingest || state.unread_at_last_scan != Some(unread));
        if !news && !full_due {
            return Ok(());
        }

        let mut inbox = session.inbox().await?;
        state.last_full_scan = Some(Instant::now());
        state.unread_at_last_scan = Some(unread);
        if inbox.is_empty() {
            return Ok(());
        }

        let queued_webhooks = self.store(&inbox).await?;

        if self.config.delete_after_ingest {
            // Highest index first, in case the device renumbers the
            // remaining entries after a deletion.
            inbox.sort_by_key(|sms| std::cmp::Reverse(sms.inbox_ref.index));
            for sms in &inbox {
                if let Err(e) = session.delete(&sms.inbox_ref).await {
                    tracing::warn!(index = sms.inbox_ref.index, error = %e, "cannot delete SMS from the device");
                    if e.is_retryable() {
                        break; // device unreachable: retry at the next scan
                    }
                }
            }
        }
        if queued_webhooks {
            self.webhook_wake.notify_one();
        }
        Ok(())
    }

    /// Stores new messages; returns whether webhook deliveries were queued.
    async fn store(&self, inbox: &[InboundSms]) -> anyhow::Result<bool> {
        let keyword_apps = apps::with_keywords(&self.pool).await?;
        let default_app = match &self.config.default_app {
            Some(name) => apps::find_by_name(&self.pool, name)
                .await?
                .filter(|a| a.enabled),
            None => None,
        };

        let mut tx = self.pool.begin().await?;
        let mut queued = false;
        for sms in inbox {
            let from = self.policy.sender(&sms.from);
            let (app_id, routed_by) = self
                .route(
                    &mut tx,
                    &from,
                    &sms.text,
                    &keyword_apps,
                    default_app.as_ref(),
                )
                .await?;
            let inserted: Option<Message> = sqlx::query_as(&format!(
                "INSERT INTO messages (id, app_id, direction, status, peer, body, body_sha256,
                                       device_received_at, received_at, routed_by)
                 VALUES ($1, $2, 'inbound', 'received', $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (peer, device_received_at, body_sha256)
                    WHERE direction = 'inbound' DO NOTHING
                 RETURNING {MESSAGE_COLUMNS}"
            ))
            .bind(Uuid::now_v7())
            .bind(app_id)
            .bind(&from)
            .bind(&sms.text)
            .bind(sha256(&sms.text))
            .bind(sms.received_at)
            .bind(device_time_to_utc(self.timezone, sms.received_at))
            .bind(routed_by)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(message) = inserted else {
                continue; // already stored by a previous scan
            };
            tracing::info!(
                message_id = %message.id,
                from = %Masked(&from),
                routed_by,
                "inbound message stored"
            );
            metrics::counter!(m::INBOUND_MESSAGES, "routed_by" => routed_by).increment(1);
            queued |= webhooks::enqueue(&mut tx, EventType::Received, &message).await?;
        }
        tx.commit().await?;
        Ok(queued)
    }

    /// Keyword prefix, then reply correlation, then the default app.
    async fn route(
        &self,
        conn: &mut PgConnection,
        from: &str,
        text: &str,
        keyword_apps: &[App],
        default_app: Option<&App>,
    ) -> sqlx::Result<(Option<Uuid>, &'static str)> {
        if let Some(app) = keyword_app(text, keyword_apps) {
            return Ok((Some(app.id), "keyword"));
        }
        let replied: Option<Uuid> = sqlx::query_scalar(
            "SELECT m.app_id FROM messages m JOIN apps a ON a.id = m.app_id
             WHERE m.direction = 'outbound' AND m.peer = $1 AND a.enabled
               AND m.status IN ('sent', 'submitted', 'unknown', 'sending')
               AND m.created_at > now() - make_interval(hours => $2)
             ORDER BY m.created_at DESC
             LIMIT 1",
        )
        .bind(from)
        .bind(i32::try_from(self.config.reply_window_hours).unwrap_or(i32::MAX))
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(app_id) = replied {
            return Ok((Some(app_id), "reply"));
        }
        if let Some(app) = default_app {
            return Ok((Some(app.id), "default"));
        }
        Ok((None, "none"))
    }
}

/// The app whose keyword is the first word of `text` (case-insensitive).
pub fn keyword_app<'a>(text: &str, apps: &'a [App]) -> Option<&'a App> {
    let first = text.split_whitespace().next()?.to_uppercase();
    apps.iter().find(|a| a.inbound_keywords.contains(&first))
}

/// Converts the naive local time reported by the device to UTC.
pub fn device_time_to_utc(tz: Tz, local: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&local).earliest() {
        Some(t) => t.with_timezone(&Utc),
        // Inside a DST gap the device clock cannot legitimately show this
        // time: shift by one hour.
        None => tz
            .from_local_datetime(&(local + chrono::Duration::hours(1)))
            .earliest()
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|| local.and_utc()),
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn app(name: &str, keywords: &[&str]) -> App {
        App {
            id: Uuid::now_v7(),
            name: name.into(),
            key_prefix: String::new(),
            key_hash: Vec::new(),
            admin: false,
            enabled: true,
            webhook_url: None,
            webhook_secret: None,
            inbound_keywords: keywords.iter().map(|k| k.to_string()).collect(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn keyword_is_the_first_word_case_insensitive() {
        let apps = [app("agenda", &["RDV"]), app("stop", &["STOP"])];
        assert_eq!(
            keyword_app("  rdv demain 10h", &apps).unwrap().name,
            "agenda"
        );
        assert_eq!(keyword_app("Stop", &apps).unwrap().name, "stop");
        // Not a prefix match, and only the first word counts.
        assert!(keyword_app("RDVS demain", &apps).is_none());
        assert!(keyword_app("ok pour le rdv", &apps).is_none());
        assert!(keyword_app("", &apps).is_none());
    }

    #[test]
    fn device_time_converts_with_dst() {
        let tz: Tz = "Europe/Paris".parse().unwrap();
        let at = |d: u32, m: u32, h: u32, min: u32| {
            NaiveDate::from_ymd_opt(2026, m, d)
                .unwrap()
                .and_hms_opt(h, min, 0)
                .unwrap()
        };
        // Winter (UTC+1) and summer (UTC+2).
        assert_eq!(
            device_time_to_utc(tz, at(15, 1, 12, 0)),
            at(15, 1, 11, 0).and_utc()
        );
        assert_eq!(
            device_time_to_utc(tz, at(15, 7, 12, 0)),
            at(15, 7, 10, 0).and_utc()
        );
        // Ambiguous hour when clocks go back (2026-10-25 03:00 -> 02:00):
        // the earliest instant is chosen.
        assert_eq!(
            device_time_to_utc(tz, at(25, 10, 2, 30)),
            at(25, 10, 0, 30).and_utc()
        );
        // Nonexistent hour when clocks go forward (2026-03-29 02:00 -> 03:00).
        assert_eq!(
            device_time_to_utc(tz, at(29, 3, 2, 30)),
            at(29, 3, 1, 30).and_utc()
        );
    }
}
