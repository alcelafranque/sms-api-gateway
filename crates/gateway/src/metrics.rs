//! Prometheus metric names and recorder setup.

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

pub const OUTBOUND_QUEUE_DEPTH: &str = "sms_outbound_queue_depth";
pub const OUTBOUND_SENDS: &str = "sms_outbound_sends_total";
pub const INBOUND_MESSAGES: &str = "sms_inbound_messages_total";
pub const INBOUND_LAST_POLL: &str = "sms_inbound_last_successful_poll_timestamp_seconds";
pub const WEBHOOK_DELIVERIES: &str = "sms_webhook_deliveries_total";
pub const WEBHOOK_QUEUE_DEPTH: &str = "sms_webhook_queue_depth";
pub const MODEM_SIGNAL_LEVEL: &str = "sms_modem_signal_level";
pub const MODEM_RSRP: &str = "sms_modem_rsrp_dbm";
pub const MODEM_INBOX_TOTAL: &str = "sms_modem_inbox_total";
pub const MODEM_ERRORS: &str = "sms_modem_errors_total";
pub const MODEM_PAUSED: &str = "sms_modem_paused";
pub const RETENTION_PURGED: &str = "sms_retention_purged_total";

pub fn install() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    metrics::describe_gauge!(OUTBOUND_QUEUE_DEPTH, "Outbound messages waiting to be sent");
    metrics::describe_counter!(OUTBOUND_SENDS, "Outbound send attempts by outcome");
    metrics::describe_counter!(INBOUND_MESSAGES, "Inbound messages stored, by routing");
    metrics::describe_gauge!(
        INBOUND_LAST_POLL,
        "Unix time of the last successful inbox poll"
    );
    metrics::describe_counter!(WEBHOOK_DELIVERIES, "Webhook delivery attempts by outcome");
    metrics::describe_gauge!(WEBHOOK_QUEUE_DEPTH, "Webhook deliveries pending");
    metrics::describe_gauge!(MODEM_SIGNAL_LEVEL, "Signal level reported by the device");
    metrics::describe_gauge!(MODEM_RSRP, "RSRP reported by the device, in dBm");
    metrics::describe_gauge!(MODEM_INBOX_TOTAL, "Messages stored in the device inbox");
    metrics::describe_counter!(MODEM_ERRORS, "Device call errors by operation and kind");
    metrics::describe_gauge!(MODEM_PAUSED, "1 while the modem is paused by an operator");
    metrics::describe_counter!(RETENTION_PURGED, "Rows touched by the retention job");
    Ok(handle)
}
