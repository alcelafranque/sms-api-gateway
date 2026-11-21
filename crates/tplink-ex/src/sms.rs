//! SMS operations (`DEV2_LTE_SMS_*`).

use std::time::Duration;

use chrono::{DateTime, NaiveDateTime};
use serde_json::Value;
use tracing::{debug, warn};

use crate::act::{Act, as_list, int_attr, str_attr};
use crate::client::{Client, Session, run_session};
use crate::error::{Error, ErrorKind, Result};

/// Messages per inbox page (fixed by the firmware).
pub const INBOX_PAGE_SIZE: u32 = 8;
/// Upper bound on pages read, against a bogus `totalNumber`.
const MAX_PAGES: u32 = 64;

const SEND_OID: &str = "DEV2_LTE_SMS_SENDNEWMSG";
const BOX_OID: &str = "DEV2_LTE_SMS_RECVMSGBOX";
const ENTRY_OID: &str = "DEV2_LTE_SMS_RECVMSGENTRY";
pub(crate) const NET_STATUS_OID: &str = "DEV2_LTE_NET_STATUS";
pub(crate) const LTE_STACK: &str = "1,0,0,0,0,0";

/// Where an inbox entry lives: the page to select, then the object stack.
///
/// Only valid until the inbox changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryLocation {
    pub page: u32,
    pub stack: String,
}

/// A received message.
#[derive(Clone, PartialEq, Eq)]
pub struct InboxEntry {
    /// Device-side index (reused after deletion).
    pub index: u32,
    pub from: String,
    pub content: String,
    /// `receivedTime` as sent by the device.
    pub received_time: String,
    /// `receivedTime` parsed as device-local time, when recognized.
    pub received_at: Option<NaiveDateTime>,
    pub unread: bool,
    pub location: EntryLocation,
}

impl std::fmt::Debug for InboxEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Message bodies (OTP codes...) stay out of debug output.
        f.debug_struct("InboxEntry")
            .field("index", &self.index)
            .field("from", &self.from)
            .field("content_chars", &self.content.chars().count())
            .field("received_time", &self.received_time)
            .field("unread", &self.unread)
            .field("location", &self.location)
            .finish()
    }
}

/// What the device said about a submitted message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    /// The device reported the message as sent.
    Sent,
    /// The device reported a send failure.
    Failed,
    /// The device reported its modem busy: the message was not sent and can
    /// be submitted again later.
    Busy,
    /// The device accepted the message but gave no usable result (no result
    /// attribute, no change within the polling window, or polling failed).
    Submitted,
}

/// Result of [`Client::send_sms`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReport {
    pub status: SendStatus,
    /// Attribute polled for the result, e.g. `DEV2_LTE_SMS_SENDNEWMSG.sendResult`.
    pub source: Option<&'static str>,
    /// Result value read just before sending.
    pub baseline: Option<String>,
    /// Values read after sending, in order.
    pub observed: Vec<String>,
    /// Failure cause reported by the device, if any.
    pub cause: Option<String>,
}

impl SendReport {
    fn submitted(source: Option<&'static str>, baseline: Option<String>) -> Self {
        Self {
            status: SendStatus::Submitted,
            source,
            baseline,
            observed: Vec::new(),
            cause: None,
        }
    }
}

/// Where the outcome of the last send can be read.
///
/// - `DEV2_LTE_SMS_SENDNEWMSG.sendResult` (EX firmwares, verified on
///   NE200-Outdoor v1): the web UI reads it right after the `so` and polls
///   every 500 ms while it is 3 (sending); 1 = sent, 2 = modem busy,
///   0 = failed. The UI trusts the first value read, and so do we.
/// - `DEV2_LTE_NET_STATUS.smsSendResult` (fallback for MR-style firmwares,
///   unverified): 1 = sent, 2 = failed, 3 = sending. The value is not reset
///   by a new send, so it is only trusted when it can be attributed to our
///   send. The NE200 refuses this attribute (errorcode 9003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultSource {
    SendObject,
    NetStatus,
}

impl ResultSource {
    const ALL: [ResultSource; 2] = [ResultSource::SendObject, ResultSource::NetStatus];

    fn act(self) -> Act {
        match self {
            ResultSource::SendObject => Act::get(SEND_OID, &["sendResult"]),
            ResultSource::NetStatus => {
                Act::get(NET_STATUS_OID, &["smsSendResult", "smsSendCause"]).stack(LTE_STACK)
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            ResultSource::SendObject => "DEV2_LTE_SMS_SENDNEWMSG.sendResult",
            ResultSource::NetStatus => "DEV2_LTE_NET_STATUS.smsSendResult",
        }
    }

    /// Returns `(result, cause)`.
    fn extract(self, data: Option<Value>) -> Option<(String, Option<String>)> {
        let obj = as_list(data).into_iter().next()?;
        let (key, cause_key) = match self {
            ResultSource::SendObject => ("sendResult", "sendCause"),
            ResultSource::NetStatus => ("smsSendResult", "smsSendCause"),
        };
        let value = match obj.get(key)? {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return None,
        };
        let cause = str_attr(&obj, cause_key).map(str::to_owned);
        Some((value, cause))
    }
}

/// Per-session memory of the probed result source.
#[derive(Debug, Default)]
pub(crate) enum ResultSourceCache {
    #[default]
    Unknown,
    Unsupported,
    Known(ResultSource),
}

const RESULT_FAILED_EX: &str = "0";
const RESULT_SENT: &str = "1";
/// "Failed" on MR-style `smsSendResult`, "modem busy" on EX `sendResult`.
const RESULT_FAILED_OR_BUSY: &str = "2";
const RESULT_SENDING: &str = "3";

/// Escapes line breaks the way the device web UI does before a send: the
/// firmware expects `\n` as 0x12 and `\r` as 0x11.
fn encode_sms_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => '\u{12}',
            '\r' => '\u{11}',
            c => c,
        })
        .collect()
}

/// Reverses [`encode_sms_text`] on inbox contents, like the web UI does when
/// displaying them (`doEscapeChar`).
fn decode_sms_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{12}' => '\n',
            '\u{11}' => '\r',
            c => c,
        })
        .collect()
}

impl Session {
    /// Reads the current send result, probing the source on first use.
    ///
    /// Also serves as a session liveness check before a send: only a
    /// session rejection or a transport failure is returned as an error.
    async fn read_send_result(&mut self) -> Result<Option<(ResultSource, String)>> {
        match self.result_source {
            ResultSourceCache::Known(src) => match self.act(&src.act()).await {
                Ok(data) => Ok(src.extract(data).map(|(v, _)| (src, v))),
                Err(e)
                    if matches!(e.kind(), ErrorKind::NotExecuted | ErrorKind::OutcomeUnknown) =>
                {
                    Err(e)
                }
                Err(_) => Ok(None),
            },
            ResultSourceCache::Unsupported => {
                match self
                    .act(&Act::get(NET_STATUS_OID, &["smsUnreadCount"]).stack(LTE_STACK))
                    .await
                {
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::NotExecuted | ErrorKind::OutcomeUnknown
                        ) =>
                    {
                        Err(e)
                    }
                    _ => Ok(None),
                }
            }
            ResultSourceCache::Unknown => {
                for src in ResultSource::ALL {
                    match self.act(&src.act()).await {
                        Ok(data) => {
                            if let Some((v, _)) = src.extract(data) {
                                debug!(source = src.label(), "send result attribute available");
                                self.result_source = ResultSourceCache::Known(src);
                                return Ok(Some((src, v)));
                            }
                            debug!(source = src.label(), "send result attribute absent");
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::NotExecuted | ErrorKind::OutcomeUnknown
                            ) =>
                        {
                            return Err(e);
                        }
                        Err(e) => {
                            debug!(source = src.label(), error = %e, "send result attribute unsupported");
                        }
                    }
                }
                self.result_source = ResultSourceCache::Unsupported;
                Ok(None)
            }
        }
    }

    async fn submit_sms(&mut self, to: &str, text: &str) -> Result<Option<(ResultSource, String)>> {
        let baseline = self
            .read_send_result()
            .await
            .map_err(|e| e.before_effect("pre-send check failed, message not sent"))?;
        let act = Act::set(SEND_OID)
            .attr("index", "1")
            .attr("to", to)
            .attr("textContent", encode_sms_text(text));
        match self.act(&act).await {
            Ok(_) => Ok(baseline),
            // The device executed something and answered garbage.
            Err(Error::Protocol(m)) => {
                Err(Error::unknown(format!("unparseable reply to send: {m}")))
            }
            Err(e) => Err(e),
        }
    }

    async fn poll_send_result(
        &mut self,
        baseline: Option<(ResultSource, String)>,
        timeout: Duration,
        interval: Duration,
    ) -> SendReport {
        let source = match self.result_source {
            ResultSourceCache::Known(src) => src,
            _ => return SendReport::submitted(None, None),
        };
        let baseline = baseline.filter(|(s, _)| *s == source).map(|(_, v)| v);
        let mut report = SendReport::submitted(Some(source.label()), baseline.clone());
        if timeout.is_zero() {
            return report;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen_sending = false;
        let mut seen_change = false;
        let mut first = true;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return report;
            }
            // Like the web UI, read the result right away: the device often
            // has it before the reply to the send act.
            if !first {
                tokio::time::sleep(interval.min(deadline - now)).await;
            }
            first = false;
            let (value, cause) = match self.act(&source.act()).await {
                Ok(data) => match source.extract(data) {
                    Some(v) => v,
                    None => return report,
                },
                Err(e) => {
                    debug!(error = %e, "send result polling failed");
                    return report;
                }
            };
            report.observed.push(value.clone());
            if baseline.as_deref().is_some_and(|b| b != value) {
                seen_change = true;
            }
            match source {
                ResultSource::SendObject => match value.as_str() {
                    RESULT_SENDING => {}
                    RESULT_SENT => {
                        report.status = SendStatus::Sent;
                        return report;
                    }
                    RESULT_FAILED_OR_BUSY => {
                        report.status = SendStatus::Busy;
                        return report;
                    }
                    RESULT_FAILED_EX => {
                        report.status = SendStatus::Failed;
                        report.cause = cause.filter(|c| !c.is_empty());
                        return report;
                    }
                    // Unknown value: keep polling until the deadline.
                    _ => {}
                },
                ResultSource::NetStatus => {
                    // A terminal value only counts if it can be attributed to
                    // this send: the value moved since the pre-send read, or a
                    // "sending" state was observed first.
                    let attributable = seen_change || seen_sending;
                    match value.as_str() {
                        RESULT_SENDING => seen_sending = true,
                        RESULT_SENT if attributable => {
                            report.status = SendStatus::Sent;
                            return report;
                        }
                        RESULT_FAILED_OR_BUSY if attributable => {
                            report.status = SendStatus::Failed;
                            report.cause = cause.filter(|c| !c.is_empty());
                            return report;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Loads the inbox and returns its size (`totalNumber`).
    ///
    /// The firmware only fills the message box on a page 0 selection, as
    /// the web UI does when opening the inbox: before it, `totalNumber` reads
    /// 0 whatever the number of stored messages.
    pub(crate) async fn inbox_total(&mut self) -> Result<u32> {
        self.select_page(0).await?;
        let data = self.act(&Act::get(BOX_OID, &[])).await?;
        let obj = as_list(data).into_iter().next().unwrap_or_default();
        Ok(int_attr(&obj, "totalNumber")
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0))
    }

    /// Selects an inbox page (1-based; 0 reloads the box). The firmware
    /// ignores the attribute unless spelled `PageNumber`, like the web UI.
    async fn select_page(&mut self, page: u32) -> Result<()> {
        let act = Act::set(BOX_OID)
            .attr("PageNumber", page.to_string())
            .replay_safe(true);
        self.act(&act).await.map(|_| ())
    }

    async fn inbox_page(&mut self, page: u32) -> Result<Vec<InboxEntry>> {
        self.select_page(page).await?;
        let data = self
            .act(&Act::get_list(
                ENTRY_OID,
                &["index", "from", "content", "receivedTime", "unread"],
            ))
            .await?;
        let mut entries = Vec::new();
        for obj in as_list(data) {
            let index = int_attr(&obj, "index").and_then(|i| u32::try_from(i).ok());
            let stack = str_attr(&obj, "stack");
            let (Some(index), Some(stack)) = (index, stack) else {
                warn!(page, "inbox entry without index or stack, skipped");
                continue;
            };
            let received_time = str_attr(&obj, "receivedTime")
                .unwrap_or_default()
                .to_owned();
            entries.push(InboxEntry {
                index,
                from: str_attr(&obj, "from").unwrap_or_default().to_owned(),
                content: decode_sms_text(str_attr(&obj, "content").unwrap_or_default()),
                received_at: parse_device_time(&received_time),
                received_time,
                unread: str_attr(&obj, "unread") == Some("1"),
                location: EntryLocation {
                    page,
                    stack: stack.to_owned(),
                },
            });
        }
        Ok(entries)
    }

    pub(crate) async fn inbox(&mut self) -> Result<Vec<InboxEntry>> {
        let total = self.inbox_total().await?;
        let pages = total.div_ceil(INBOX_PAGE_SIZE).min(MAX_PAGES);
        let mut all: Vec<InboxEntry> = Vec::with_capacity(total as usize);
        for page in 1..=pages {
            for entry in self.inbox_page(page).await? {
                // Guard against firmwares ignoring the page selection.
                if !all
                    .iter()
                    .any(|e| e.index == entry.index && e.location.stack == entry.location.stack)
                {
                    all.push(entry);
                }
            }
        }
        if all.len() != total as usize {
            debug!(
                total,
                read = all.len(),
                "inbox size differs from totalNumber"
            );
        }
        Ok(all)
    }

    async fn mark_read(&mut self, location: &EntryLocation) -> Result<()> {
        self.select_page(location.page).await?;
        let act = Act::set(ENTRY_OID)
            .stack(location.stack.as_str())
            .attr("unread", "0")
            .replay_safe(true);
        self.act(&act).await.map(|_| ())
    }

    async fn delete_resolved(
        &mut self,
        select: &mut impl FnMut(&[InboxEntry]) -> Option<usize>,
    ) -> Result<Option<InboxEntry>> {
        let mut entries = self
            .inbox()
            .await
            .map_err(|e| e.before_effect("cannot list the inbox, nothing deleted"))?;
        let Some(i) = select(&entries).filter(|i| *i < entries.len()) else {
            return Ok(None);
        };
        let entry = entries.swap_remove(i);
        self.delete_at(&entry.location).await?;
        Ok(Some(entry))
    }

    async fn delete_at(&mut self, location: &EntryLocation) -> Result<()> {
        self.select_page(location.page)
            .await
            .map_err(|e| e.before_effect("cannot select inbox page, nothing deleted"))?;
        let act = Act::new(crate::Operation::Do, ENTRY_OID).stack(location.stack.as_str());
        self.act(&act).await.map(|_| ())
    }
}

impl Client {
    /// Sends an SMS.
    ///
    /// Sequence: a read of the send-result attribute (which also proves the
    /// session is alive, so that an expired session is detected and renewed
    /// *before* the message is submitted), the `so DEV2_LTE_SMS_SENDNEWMSG`
    /// act, then best-effort polling of the result for
    /// [`ClientConfig::send_result_timeout`](crate::ClientConfig).
    ///
    /// The submission is re-sent only when the device rejected the session
    /// before executing it. Once submitted, polling problems never turn into
    /// errors: the report says [`SendStatus::Submitted`].
    ///
    /// Line breaks are escaped like the web UI does (`\n` as 0x12, `\r` as
    /// 0x11). The text length is not checked: the NE200 web UI allows up to
    /// 765 GSM-7 septets or 335 UCS-2 characters (5 concatenated parts).
    pub async fn send_sms(&mut self, to: &str, text: &str) -> Result<SendReport> {
        let to = to.trim();
        if to.is_empty() || to.contains(['\r', '\n']) {
            return Err(Error::InvalidInput("invalid recipient number".into()));
        }
        if text.is_empty() {
            return Err(Error::InvalidInput("empty message".into()));
        }
        let baseline = run_session!(self, s => s.submit_sms(to, text).await)?;
        debug!(to = %mask_number(to), "SMS accepted by the device");
        let timeout = self.config().send_result_timeout;
        let interval = self
            .config()
            .send_result_interval
            .max(Duration::from_millis(100));
        let Some(session) = self.session.as_mut() else {
            return Ok(SendReport::submitted(None, None));
        };
        let report = session.poll_send_result(baseline, timeout, interval).await;
        debug!(status = ?report.status, observed = ?report.observed, "send result");
        Ok(report)
    }

    /// Number of messages in the inbox (`totalNumber`), after reloading the
    /// box (a page 0 selection).
    pub async fn inbox_total(&mut self) -> Result<u32> {
        run_session!(self, s => s.inbox_total().await)
    }

    /// Reads the whole inbox, page by page.
    pub async fn inbox(&mut self) -> Result<Vec<InboxEntry>> {
        run_session!(self, s => s.inbox().await)
    }

    /// Marks an entry as read.
    pub async fn mark_read(&mut self, location: &EntryLocation) -> Result<()> {
        run_session!(self, s => s.mark_read(location).await)
    }

    /// Deletes the entry at `location`, as is. Prefer
    /// [`Client::delete_sms_resolved`] when the location may be stale.
    pub async fn delete_sms(&mut self, location: &EntryLocation) -> Result<()> {
        run_session!(self, s => s.delete_at(location).await)
    }

    /// Lists the inbox, lets `select` pick the entry to delete (or none),
    /// and deletes it, all on the same session. Returns the deleted entry.
    ///
    /// If the session is rejected before the delete executes, the whole
    /// sequence (listing included) runs again after re-login, so the
    /// selection is always made on fresh data.
    pub async fn delete_sms_resolved(
        &mut self,
        mut select: impl FnMut(&[InboxEntry]) -> Option<usize>,
    ) -> Result<Option<InboxEntry>> {
        run_session!(self, s => s.delete_resolved(&mut select).await)
    }
}

/// Parses the device time format (`2024-11-15 22:23:59`, device-local time),
/// tolerating the other ISO 8601 shapes Python's `fromisoformat` accepts.
pub fn parse_device_time(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    const NAIVE: [&str; 6] = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M:%S",
    ];
    if let Some(t) = NAIVE
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
    {
        return Some(t);
    }
    // With an explicit offset: keep the wall-clock time.
    [
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
    ]
    .iter()
    .find_map(|f| DateTime::parse_from_str(s, f).ok())
    .map(|t| t.naive_local())
}

/// Keeps the last digits of a number for logs.
pub(crate) fn mask_number(n: &str) -> String {
    let chars: Vec<char> = n.chars().collect();
    let keep = chars.len().min(3);
    let mut out = "*".repeat(chars.len() - keep);
    out.extend(&chars[chars.len() - keep..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_times_parse() {
        let expected = chrono::NaiveDate::from_ymd_opt(2024, 11, 15)
            .unwrap()
            .and_hms_opt(22, 23, 59)
            .unwrap();
        for s in [
            "2024-11-15 22:23:59",
            "2024-11-15T22:23:59",
            " 2024-11-15 22:23:59.000 ",
            "2024/11/15 22:23:59",
            "2024-11-15T22:23:59+01:00",
        ] {
            assert_eq!(parse_device_time(s), Some(expected), "{s}");
        }
        assert_eq!(parse_device_time(""), None);
        assert_eq!(parse_device_time("15/11/2024"), None);
    }

    #[test]
    fn send_result_is_read_from_net_status_sample() {
        let data: Value = serde_json::from_str(concat!(
            r#"{"ussdSessionStatus":"0","ussdStatus":"0","smsUnreadCount":"0","smsSendCause":"0","#,
            r#""smsSendResult":"3","rfSwitch":"1","sigLevel":"0","stack":"1,0,0,0,0,0"}"#
        ))
        .unwrap();
        assert_eq!(
            ResultSource::NetStatus.extract(Some(data.clone())),
            Some(("3".to_owned(), Some("0".to_owned())))
        );
        assert_eq!(ResultSource::SendObject.extract(Some(data)), None);
        assert_eq!(ResultSource::NetStatus.extract(None), None);
    }

    #[test]
    fn numbers_are_masked_in_logs() {
        assert_eq!(mask_number("+33612345678"), "*********678");
        assert_eq!(mask_number("12"), "12");
    }
}
