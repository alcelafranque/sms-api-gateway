//! [`SmsModem`] backend for TP-Link EX-family CPEs (NE200-Outdoor...),
//! built on the `tplink-ex` crate.
//!
//! One device session is shared by every call and access is serialized
//! behind an async mutex: the device accepts a single administrator session
//! at a time. Login is lazy and renewed transparently by `tplink-ex`.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;
use tplink_ex::{Client, ClientConfig, ErrorKind, InboxEntry, SendStatus, Variant};
use tracing::{debug, warn};

use crate::{InboundSms, InboxRef, ModemError, ModemStatus, Result, SendOutcome, SmsModem};

/// Encryption scheme used by the device firmware for `/cgi_gdpr`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CipherVariant {
    /// Probe the device at login.
    #[default]
    Auto,
    /// AES-128-CBC session, raw RSA signature.
    Cbc,
    /// AES-GCM session, raw RSA signature.
    Gcm,
    /// AES-GCM session, RSA-OAEP signature.
    GcmOaep,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TplinkConfig {
    /// Base URL of the device web UI, e.g. `http://192.168.254.1`.
    pub url: String,
    #[serde(default = "default_username")]
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub variant: CipherVariant,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Accept self-signed certificates when `url` is HTTPS.
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

fn default_username() -> String {
    "user".into()
}

fn default_timeout_secs() -> u64 {
    30
}

impl fmt::Debug for TplinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TplinkConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("variant", &self.variant)
            .field("timeout_secs", &self.timeout_secs)
            .field("accept_invalid_certs", &self.accept_invalid_certs)
            .finish()
    }
}

/// TP-Link CPE modem. Owns a single device session and serializes every
/// call to it (the device allows only one logged-in admin at a time).
pub struct TplinkModem {
    client: Mutex<Client>,
}

impl TplinkModem {
    /// Builds the client. Does not contact the device: login is lazy.
    pub fn new(config: TplinkConfig) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        let mut cfg = ClientConfig::new(config.url, config.username, config.password);
        cfg.variant = match config.variant {
            CipherVariant::Auto => Variant::Auto,
            CipherVariant::Cbc => Variant::Cbc,
            CipherVariant::Gcm => Variant::Gcm,
            CipherVariant::GcmOaep => Variant::GcmOaep,
        };
        cfg.timeout = timeout;
        cfg.connect_timeout = timeout.min(Duration::from_secs(10));
        cfg.accept_invalid_certs = config.accept_invalid_certs;
        let client = Client::new(cfg).map_err(|e| {
            ModemError::Unavailable(format!("invalid TP-Link configuration: {}", e.detailed()))
        })?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }
}

/// Maps a driver error for an operation with side effects.
fn map_err(e: tplink_ex::Error) -> ModemError {
    let msg = e.detailed();
    match e.kind() {
        ErrorKind::NotExecuted => ModemError::Unavailable(msg),
        ErrorKind::OutcomeUnknown => ModemError::Unknown(msg),
        ErrorKind::Refused => ModemError::Rejected(msg),
        ErrorKind::Protocol => ModemError::Protocol(msg),
    }
}

/// Maps a driver error for a read-only operation: repeating a read is always
/// safe, so an unknown outcome is reported as plain unavailability.
fn map_read_err(e: tplink_ex::Error) -> ModemError {
    match e.kind() {
        ErrorKind::OutcomeUnknown => ModemError::Unavailable(e.detailed()),
        _ => map_err(e),
    }
}

/// Identity of a message, used to check that the entry found at delete time
/// is still the one that was read (indexes are reused after deletion).
fn fingerprint(entry: &InboxEntry) -> u64 {
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [
        entry.from.as_bytes(),
        entry.content.as_bytes(),
        entry.received_time.as_bytes(),
    ] {
        for b in part.iter().chain(std::iter::once(&0u8)) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Finds the message to delete in a fresh listing: preferably at the same
/// index, but only an entry that is still the same message (indexes are
/// reused after deletion, so a matching index alone proves nothing).
fn resolve(entries: &[InboxEntry], index: u32, wanted: u64) -> Option<usize> {
    let same = |e: &InboxEntry| fingerprint(e) == wanted;
    entries
        .iter()
        .position(|e| e.index == index && same(e))
        .or_else(|| entries.iter().position(same))
}

/// Parsed `InboxRef::locator`: `p=<page>;s=<stack>;f=<fingerprint hex>`.
struct Locator {
    page: u32,
    stack: String,
    fingerprint: u64,
}

impl Locator {
    fn encode(entry: &InboxEntry) -> String {
        format!(
            "p={};s={};f={:016x}",
            entry.location.page,
            entry.location.stack,
            fingerprint(entry)
        )
    }

    fn parse(s: &str) -> Option<Self> {
        let (mut page, mut stack, mut fp) = (None, None, None);
        for part in s.split(';') {
            match part.split_once('=')? {
                ("p", v) => page = v.parse().ok(),
                ("s", v) => stack = Some(v.to_owned()),
                ("f", v) => fp = u64::from_str_radix(v, 16).ok(),
                _ => {}
            }
        }
        Some(Self {
            page: page?,
            stack: stack?,
            fingerprint: fp?,
        })
    }
}

#[async_trait]
impl SmsModem for TplinkModem {
    async fn send(&self, to: &str, text: &str) -> Result<SendOutcome> {
        let mut client = self.client.lock().await;
        let report = client.send_sms(to, text).await.map_err(map_err)?;
        debug!(
            status = ?report.status,
            source = report.source,
            baseline = ?report.baseline,
            observed = ?report.observed,
            "send report"
        );
        match report.status {
            SendStatus::Sent => Ok(SendOutcome::Sent),
            SendStatus::Submitted => Ok(SendOutcome::Submitted),
            // Not sent: submitting again later is safe.
            SendStatus::Busy => Err(ModemError::Unavailable(
                "device modem busy, message not sent".into(),
            )),
            SendStatus::Failed => Err(ModemError::Rejected(format!(
                "device reported a send failure (cause {})",
                report.cause.as_deref().unwrap_or("unknown")
            ))),
        }
    }

    async fn unread_count(&self) -> Result<u32> {
        let mut client = self.client.lock().await;
        client.unread_count().await.map_err(map_read_err)
    }

    async fn inbox(&self) -> Result<Vec<InboundSms>> {
        let mut client = self.client.lock().await;
        let entries = client.inbox().await.map_err(map_read_err)?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                let received_at = entry.received_at.unwrap_or_else(|| {
                    warn!(
                        index = entry.index,
                        raw = %entry.received_time,
                        "unrecognized SMS reception time, using the current time"
                    );
                    chrono::Local::now().naive_local()
                });
                InboundSms {
                    inbox_ref: InboxRef {
                        index: entry.index,
                        locator: Locator::encode(&entry),
                    },
                    from: entry.from,
                    text: entry.content,
                    received_at,
                    unread: entry.unread,
                }
            })
            .collect())
    }

    async fn delete(&self, inbox_ref: &InboxRef) -> Result<()> {
        let wanted = Locator::parse(&inbox_ref.locator).ok_or_else(|| {
            ModemError::Protocol(format!(
                "invalid TP-Link inbox locator {:?}",
                inbox_ref.locator
            ))
        })?;
        let mut client = self.client.lock().await;
        let deleted = client
            .delete_sms_resolved(|entries| resolve(entries, inbox_ref.index, wanted.fingerprint))
            .await
            .map_err(map_err)?;
        match deleted {
            Some(entry) => {
                if entry.location.page != wanted.page || entry.location.stack != wanted.stack {
                    debug!(
                        index = inbox_ref.index,
                        new_index = entry.index,
                        "message had moved in the inbox before deletion"
                    );
                }
            }
            None => debug!(
                index = inbox_ref.index,
                "message already gone from the inbox"
            ),
        }
        Ok(())
    }

    async fn status(&self) -> Result<ModemStatus> {
        let mut client = self.client.lock().await;
        let lte = client.lte_status().await.map_err(map_read_err)?;
        let inbox_total = match client.inbox_total().await {
            Ok(n) => Some(n),
            Err(e) => {
                debug!(error = %e.detailed(), "cannot read inbox size");
                None
            }
        };
        Ok(ModemStatus {
            operator: lte.operator,
            signal_level: lte.signal_level,
            rsrp_dbm: lte.rsrp,
            rsrq_db: lte.rsrq,
            // Device reports tenths of dB (unverified on NE200).
            snr_db: lte.snr.map(|v| v as f32 / 10.0),
            unread_count: lte.unread_count.unwrap_or(0),
            inbox_total,
        })
    }

    async fn release(&self) -> Result<()> {
        let mut client = self.client.lock().await;
        client.logout().await.map_err(map_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tplink_ex::EntryLocation;

    fn entry(index: u32, from: &str, content: &str) -> InboxEntry {
        InboxEntry {
            index,
            from: from.into(),
            content: content.into(),
            received_time: "2024-11-15 22:23:59".into(),
            received_at: None,
            unread: true,
            location: EntryLocation {
                page: 1,
                stack: format!("{index},0,0,0,0,0"),
            },
        }
    }

    #[test]
    fn locator_round_trips() {
        let e = entry(7, "+33612345678", "hello");
        let loc = Locator::parse(&Locator::encode(&e)).unwrap();
        assert_eq!((loc.page, loc.stack.as_str()), (1, "7,0,0,0,0,0"));
        assert_eq!(loc.fingerprint, fingerprint(&e));
        assert!(Locator::parse("p=1;s=1,0,0,0,0,0").is_none());
        assert!(Locator::parse("junk").is_none());
    }

    #[test]
    fn delete_resolution_never_trusts_a_reused_index() {
        let wanted = entry(2, "+33600000002", "code 1234");
        let fp = fingerprint(&wanted);
        // Same index, same message.
        let fresh = [entry(3, "+33600000003", "other"), wanted.clone()];
        assert_eq!(resolve(&fresh, 2, fp), Some(1));
        // The message moved to another index.
        let moved = [
            entry(2, "+33600000009", "newer"),
            entry(5, "+33600000002", "code 1234"),
        ];
        assert_eq!(resolve(&moved, 2, fp), Some(1));
        // Index reused by a different message, original gone: delete nothing.
        let reused = [entry(2, "+33600000002", "code 9999")];
        assert_eq!(resolve(&reused, 2, fp), None);
    }

    #[test]
    fn read_errors_with_unknown_outcome_are_retryable() {
        let unknown = || tplink_ex::Error::OutcomeUnknown {
            reason: "timeout".into(),
            source: None,
        };
        assert!(matches!(map_err(unknown()), ModemError::Unknown(_)));
        assert!(map_read_err(unknown()).is_retryable());
        let refused = tplink_ex::Error::Refused {
            operation: "so".into(),
            oid: "DEV2_LTE_SMS_SENDNEWMSG".into(),
            code: "9800".into(),
        };
        assert!(matches!(map_err(refused), ModemError::Rejected(_)));
        let expired = tplink_ex::Error::SessionExpired("HTTP 403".into());
        assert!(map_err(expired).is_retryable());
    }

    #[test]
    fn new_does_not_contact_the_device() {
        let cfg = TplinkConfig {
            url: "http://192.0.2.1".into(),
            username: default_username(),
            password: "secret".into(),
            variant: CipherVariant::Auto,
            timeout_secs: default_timeout_secs(),
            accept_invalid_certs: false,
        };
        assert!(TplinkModem::new(cfg.clone()).is_ok());
        assert!(!format!("{cfg:?}").contains("secret"));
        let bad = TplinkConfig {
            url: "not a url".into(),
            ..cfg
        };
        assert!(TplinkModem::new(bad).is_err());
    }
}
