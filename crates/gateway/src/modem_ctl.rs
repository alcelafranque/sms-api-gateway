//! Single entry point to the device: serializes calls, tracks session
//! health, enforces the pause and wraps every call in a safety timeout.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use modem::{InboundSms, InboxRef, ModemError, ModemStatus, SendOutcome, SmsModem};
use serde::{Deserialize, Serialize};
use tokio::sync::{MutexGuard, watch};
use utoipa::ToSchema;

use crate::metrics as m;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// No device call made yet.
    Idle,
    /// Last device call succeeded.
    Ok,
    /// Last device call failed.
    Error,
    /// Paused by an operator: the gateway does not touch the device.
    Paused,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionInfo {
    pub state: SessionState,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PauseState {
    pub paused: bool,
    /// Automatic resume time; `None` = until resumed explicitly.
    pub until: Option<DateTime<Utc>>,
}

impl PauseState {
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.paused && self.until.is_none_or(|until| until > now)
    }
}

#[derive(Debug, Clone)]
pub struct CachedStatus {
    pub status: ModemStatus,
    pub updated_at: DateTime<Utc>,
}

#[derive(Default)]
struct Health {
    last_ok_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    last_error_at: Option<DateTime<Utc>>,
    consecutive_failures: u32,
    status: Option<CachedStatus>,
    last_poll: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
#[error("modem paused")]
pub struct Paused;

pub struct ModemHandle {
    modem: Arc<dyn SmsModem>,
    /// Held for the duration of every device interaction.
    op_lock: tokio::sync::Mutex<()>,
    health: Mutex<Health>,
    pause: watch::Sender<PauseState>,
    op_timeout: Duration,
}

impl ModemHandle {
    pub fn new(modem: Arc<dyn SmsModem>, pause: PauseState, op_timeout: Duration) -> Self {
        let (pause_tx, _) = watch::channel(pause);
        let handle = Self {
            modem,
            op_lock: tokio::sync::Mutex::new(()),
            health: Mutex::new(Health::default()),
            pause: pause_tx,
            op_timeout,
        };
        handle.publish_pause_metric();
        handle
    }

    pub fn is_paused(&self) -> bool {
        let state = self.pause.borrow().clone();
        if state.paused && !state.active(Utc::now()) {
            // Timed pause elapsed.
            self.pause.send_replace(PauseState::default());
            metrics::gauge!(m::MODEM_PAUSED).set(0.0);
            tracing::info!("pause expired, modem resumed");
            return false;
        }
        state.paused
    }

    pub fn pause_state(&self) -> PauseState {
        if self.is_paused() {
            self.pause.borrow().clone()
        } else {
            PauseState::default()
        }
    }

    /// Notified whenever the pause state changes.
    pub fn subscribe_pause(&self) -> watch::Receiver<PauseState> {
        self.pause.subscribe()
    }

    /// Waits for in-flight device calls, stops further ones and releases the
    /// device session.
    pub async fn pause(&self, until: Option<DateTime<Utc>>) {
        let _guard = self.op_lock.lock().await;
        self.pause.send_replace(PauseState {
            paused: true,
            until,
        });
        self.publish_pause_metric();
        match tokio::time::timeout(self.op_timeout, self.modem.release()).await {
            Ok(Ok(())) => tracing::info!(?until, "modem paused, session released"),
            Ok(Err(e)) => tracing::warn!(error = %e, "modem paused, session release failed"),
            Err(_) => tracing::warn!("modem paused, session release timed out"),
        }
    }

    pub fn resume(&self) {
        self.pause.send_replace(PauseState::default());
        self.publish_pause_metric();
        tracing::info!("modem resumed");
    }

    fn publish_pause_metric(&self) {
        metrics::gauge!(m::MODEM_PAUSED).set(if self.is_paused() { 1.0 } else { 0.0 });
    }

    /// Exclusive access to the device, refused while paused.
    pub async fn session(&self) -> Result<ModemSession<'_>, Paused> {
        if self.is_paused() {
            return Err(Paused);
        }
        let guard = self.op_lock.lock().await;
        // The pause may have been taken while we waited for the lock.
        if self.is_paused() {
            return Err(Paused);
        }
        Ok(ModemSession {
            handle: self,
            _guard: guard,
        })
    }

    /// Releases the session on shutdown.
    pub async fn shutdown(&self) {
        let _guard = self.op_lock.lock().await;
        if let Ok(Err(e)) =
            tokio::time::timeout(Duration::from_secs(10), self.modem.release()).await
        {
            tracing::warn!(error = %e, "modem session release failed");
        }
    }

    pub fn session_info(&self) -> SessionInfo {
        let health = self.health.lock().unwrap();
        let state = if self.is_paused() {
            SessionState::Paused
        } else if health.consecutive_failures > 0 {
            SessionState::Error
        } else if health.last_ok_at.is_some() {
            SessionState::Ok
        } else {
            SessionState::Idle
        };
        SessionInfo {
            state,
            last_ok_at: health.last_ok_at,
            last_error: health.last_error.clone(),
            last_error_at: health.last_error_at,
            consecutive_failures: health.consecutive_failures,
        }
    }

    /// Last successful inbox poll.
    pub fn last_poll(&self) -> Option<DateTime<Utc>> {
        self.health.lock().unwrap().last_poll
    }

    pub fn set_last_poll(&self, at: DateTime<Utc>) {
        self.health.lock().unwrap().last_poll = Some(at);
        metrics::gauge!(m::INBOUND_LAST_POLL).set(at.timestamp() as f64);
    }

    pub fn cached_status(&self) -> Option<CachedStatus> {
        self.health.lock().unwrap().status.clone()
    }

    fn record<T>(&self, op: &'static str, result: &Result<T, ModemError>) {
        let mut health = self.health.lock().unwrap();
        match result {
            Ok(_) => {
                health.last_ok_at = Some(Utc::now());
                health.consecutive_failures = 0;
            }
            Err(e) => {
                health.last_error = Some(e.to_string());
                health.last_error_at = Some(Utc::now());
                health.consecutive_failures += 1;
                metrics::counter!(m::MODEM_ERRORS, "op" => op, "kind" => error_kind(e))
                    .increment(1);
            }
        }
    }
}

pub fn error_kind(e: &ModemError) -> &'static str {
    match e {
        ModemError::Unavailable(_) => "unavailable",
        ModemError::Unknown(_) => "unknown",
        ModemError::Rejected(_) => "rejected",
        ModemError::Protocol(_) => "protocol",
    }
}

/// Exclusive, non-paused access to the device.
pub struct ModemSession<'a> {
    handle: &'a ModemHandle,
    _guard: MutexGuard<'a, ()>,
}

impl ModemSession<'_> {
    async fn run<T>(
        &self,
        op: &'static str,
        on_timeout: fn(String) -> ModemError,
        fut: impl Future<Output = modem::Result<T>>,
    ) -> modem::Result<T> {
        let timeout = self.handle.op_timeout;
        let result = match tokio::time::timeout(timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(on_timeout(format!("{op} timed out after {timeout:?}"))),
        };
        self.handle.record(op, &result);
        result
    }

    pub async fn send(&self, to: &str, text: &str) -> modem::Result<SendOutcome> {
        // A send cut short may still have left the device.
        self.run(
            "send",
            ModemError::Unknown,
            self.handle.modem.send(to, text),
        )
        .await
    }

    pub async fn unread_count(&self) -> modem::Result<u32> {
        self.run(
            "unread_count",
            ModemError::Unavailable,
            self.handle.modem.unread_count(),
        )
        .await
    }

    pub async fn inbox(&self) -> modem::Result<Vec<InboundSms>> {
        self.run("inbox", ModemError::Unavailable, self.handle.modem.inbox())
            .await
    }

    pub async fn delete(&self, inbox_ref: &InboxRef) -> modem::Result<()> {
        self.run(
            "delete",
            ModemError::Unavailable,
            self.handle.modem.delete(inbox_ref),
        )
        .await
    }

    pub async fn status(&self) -> modem::Result<ModemStatus> {
        let result = self
            .run(
                "status",
                ModemError::Unavailable,
                self.handle.modem.status(),
            )
            .await;
        if let Ok(status) = &result {
            self.handle.health.lock().unwrap().status = Some(CachedStatus {
                status: status.clone(),
                updated_at: Utc::now(),
            });
            if let Some(level) = status.signal_level {
                metrics::gauge!(m::MODEM_SIGNAL_LEVEL).set(f64::from(level));
            }
            if let Some(rsrp) = status.rsrp_dbm {
                metrics::gauge!(m::MODEM_RSRP).set(f64::from(rsrp));
            }
            if let Some(total) = status.inbox_total {
                metrics::gauge!(m::MODEM_INBOX_TOTAL).set(f64::from(total));
            }
        }
        result
    }
}
