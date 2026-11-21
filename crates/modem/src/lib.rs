//! Backend-agnostic abstraction over a device able to send and receive SMS.
//!
//! The gateway only talks to [`SmsModem`]; concrete backends (TP-Link CPE,
//! in-memory mock, future AT/ModemManager modems) live behind it.
//!
//! Implementations must be safe to share between tasks and must serialize
//! access to the underlying device themselves when the device does not
//! tolerate concurrent sessions.

mod mock;
#[cfg(feature = "tplink")]
pub mod tplink;

pub use mock::MockModem;

use async_trait::async_trait;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// Opaque handle to a message stored in the device inbox.
///
/// Only meaningful for the backend that produced it, and only until the
/// inbox changes (indexes are reused after deletion on most devices).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxRef {
    /// Device-side message index.
    pub index: u32,
    /// Backend-specific locator (e.g. TP-Link page number and object stack).
    pub locator: String,
}

/// A message read from the device inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundSms {
    pub inbox_ref: InboxRef,
    /// Sender as reported by the device (usually E.164, may be alphanumeric).
    pub from: String,
    pub text: String,
    /// Reception time as reported by the device, in the device's local time.
    pub received_at: NaiveDateTime,
    pub unread: bool,
}

/// Result of a successful send call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendOutcome {
    /// The device confirmed the message left it.
    Sent,
    /// The device accepted the request but gives no delivery confirmation.
    Submitted,
}

/// Radio and inbox status of the device. Every field is best-effort.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModemStatus {
    pub operator: Option<String>,
    /// Signal level as shown by the device UI (typically 0..=4 or 0..=5).
    pub signal_level: Option<u8>,
    pub rsrp_dbm: Option<i32>,
    pub rsrq_db: Option<i32>,
    pub snr_db: Option<f32>,
    pub unread_count: u32,
    /// Number of messages currently stored in the inbox, if known.
    pub inbox_total: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModemError {
    /// The request never reached the device (connection, login, session).
    /// Retrying is safe: nothing was executed.
    #[error("modem unavailable: {0}")]
    Unavailable(String),
    /// The request may or may not have been executed (e.g. timeout after the
    /// request was sent). Callers must not blindly retry a send.
    #[error("modem outcome unknown: {0}")]
    Unknown(String),
    /// The device refused the request (invalid number, message too long,
    /// SIM error...). Retrying will not help.
    #[error("modem rejected request: {0}")]
    Rejected(String),
    /// The device answered something we could not understand (firmware
    /// change, unsupported variant...).
    #[error("modem protocol error: {0}")]
    Protocol(String),
}

impl ModemError {
    /// Whether retrying the same operation is known to be safe.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ModemError::Unavailable(_))
    }
}

pub type Result<T, E = ModemError> = std::result::Result<T, E>;

#[async_trait]
pub trait SmsModem: Send + Sync {
    /// Sends `text` to `to` (already normalized, E.164).
    async fn send(&self, to: &str, text: &str) -> Result<SendOutcome>;

    /// Cheap check of the number of unread inbound messages.
    async fn unread_count(&self) -> Result<u32>;

    /// Reads every message currently stored in the inbox.
    async fn inbox(&self) -> Result<Vec<InboundSms>>;

    /// Removes a message from the device inbox.
    async fn delete(&self, inbox_ref: &InboxRef) -> Result<()>;

    /// Radio / inbox status.
    async fn status(&self) -> Result<ModemStatus>;

    /// Releases the device session, if any (e.g. so a human can use the
    /// device web UI). The next call re-opens it transparently.
    async fn release(&self) -> Result<()> {
        Ok(())
    }
}
