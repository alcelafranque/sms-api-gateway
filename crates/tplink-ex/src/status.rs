//! Radio / LTE status.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use tracing::debug;

use crate::act::{Act, as_list, int_attr, str_attr};
use crate::client::{Client, Session, run_session};
use crate::error::{Error, ErrorKind, Result};
use crate::sms::{LTE_STACK, NET_STATUS_OID};

const PROF_STAT_OID: &str = "DEV2_LTE_PROF_STAT";
const LINK_CFG_OID: &str = "DEV2_LTE_LINK_CFG";
const SERVING_CELL_OID: &str = "DEV2_LTE_SERVING_CELL_INFO";
/// "Not available" filler used by some firmwares.
const INVALID_SENTINEL: i64 = 268_435_455;

/// Radio status. Every field is best-effort.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LteStatus {
    pub unread_count: Option<u32>,
    /// Signal bars as shown by the web UI.
    pub signal_level: Option<u8>,
    /// Device units; dBm on known firmwares.
    pub rsrp: Option<i32>,
    /// Device units; dB on known firmwares.
    pub rsrq: Option<i32>,
    /// Device units; apparently tenths of dB (`SSSINR` = 270 for 27 dB).
    pub snr: Option<i32>,
    /// Whether the radio values come from `DEV2_LTE_SERVING_CELL_INFO`
    /// (5G / per-RAT data) rather than `DEV2_LTE_NET_STATUS`.
    pub from_serving_cell: bool,
    pub operator: Option<String>,
    pub network_type: Option<i64>,
    pub connect_status: Option<i64>,
    pub sim_status: Option<i64>,
    /// Raw `data` of every object read, by OID (for diagnostics).
    pub raw: BTreeMap<String, Value>,
}

fn radio_value(obj: &Map<String, Value>, key: &str) -> Option<i32> {
    int_attr(obj, key)
        .filter(|v| *v != 0 && *v != INVALID_SENTINEL)
        .and_then(|v| i32::try_from(v).ok())
}

impl Session {
    /// Reads an object whose absence must not fail the caller; only session
    /// rejections and transport failures are propagated.
    async fn optional(&mut self, act: Act) -> Result<Option<Value>> {
        match self.act(&act).await {
            Ok(v) => Ok(v),
            Err(e) if matches!(e.kind(), ErrorKind::NotExecuted | ErrorKind::OutcomeUnknown) => {
                Err(e)
            }
            Err(e) => {
                debug!(oid = act.oid(), error = %e, "optional object unavailable");
                Ok(None)
            }
        }
    }

    pub(crate) async fn unread_count(&mut self) -> Result<u32> {
        let data = self
            .act(&Act::get(NET_STATUS_OID, &["smsUnreadCount"]).stack(LTE_STACK))
            .await?;
        let obj = as_list(data).into_iter().next().unwrap_or_default();
        int_attr(&obj, "smsUnreadCount")
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                Error::Protocol("smsUnreadCount missing from DEV2_LTE_NET_STATUS".into())
            })
    }

    pub(crate) async fn lte_status(&mut self) -> Result<LteStatus> {
        let mut status = LteStatus::default();

        let net = self
            .act(
                &Act::get(
                    NET_STATUS_OID,
                    &[
                        "smsUnreadCount",
                        "sigLevel",
                        "rfInfoRsrp",
                        "rfInfoRsrq",
                        "rfInfoSnr",
                    ],
                )
                .stack(LTE_STACK),
            )
            .await?;
        if let Some(v) = &net {
            status.raw.insert(NET_STATUS_OID.into(), v.clone());
        }
        let net = as_list(net).into_iter().next().unwrap_or_default();
        status.unread_count = int_attr(&net, "smsUnreadCount").and_then(|n| u32::try_from(n).ok());
        status.signal_level = int_attr(&net, "sigLevel").and_then(|n| u8::try_from(n).ok());
        status.rsrp = radio_value(&net, "rfInfoRsrp");
        if status.rsrp.is_some() {
            status.rsrq = radio_value(&net, "rfInfoRsrq");
            status.snr = int_attr(&net, "rfInfoSnr").and_then(|v| i32::try_from(v).ok());
        }

        let prof = self
            .optional(Act::get(PROF_STAT_OID, &["ispName"]).stack(LTE_STACK))
            .await?;
        if let Some(v) = &prof {
            status.raw.insert(PROF_STAT_OID.into(), v.clone());
        }
        let prof = as_list(prof).into_iter().next().unwrap_or_default();
        status.operator = str_attr(&prof, "ispName")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let link = self
            .optional(
                Act::get(LINK_CFG_OID, &["networkType", "connectStatus", "simStatus"])
                    .stack(LTE_STACK),
            )
            .await?;
        if let Some(v) = &link {
            status.raw.insert(LINK_CFG_OID.into(), v.clone());
        }
        let link = as_list(link).into_iter().next().unwrap_or_default();
        status.network_type = int_attr(&link, "networkType");
        status.connect_status = int_attr(&link, "connectStatus");
        status.sim_status = int_attr(&link, "simStatus");

        // Per-RAT values; the only meaningful ones on 5G links.
        let cells = self
            .optional(Act::get_list(
                SERVING_CELL_OID,
                &[
                    "networkType",
                    "cellConnectionStatus",
                    "signalStrength",
                    "SSRSRP",
                    "SSRSRQ",
                    "SSSINR",
                    "RSRP",
                    "RSRQ",
                    "SNR",
                ],
            ))
            .await?;
        if let Some(v) = &cells {
            status.raw.insert(SERVING_CELL_OID.into(), v.clone());
        }
        if let Some(nt) = status.network_type {
            let cells = as_list(cells);
            let active = cells
                .iter()
                .filter(|c| int_attr(c, "networkType") == Some(nt))
                .max_by_key(|c| str_attr(c, "cellConnectionStatus") == Some("1"));
            if let Some(cell) = active {
                let (rsrp, rsrq, snr) = if radio_value(cell, "SSRSRP").is_some() {
                    ("SSRSRP", "SSRSRQ", "SSSINR")
                } else {
                    ("RSRP", "RSRQ", "SNR")
                };
                if let Some(v) = radio_value(cell, rsrp) {
                    status.from_serving_cell = true;
                    status.rsrp = Some(v);
                    status.rsrq = radio_value(cell, rsrq);
                    status.snr = int_attr(cell, snr).and_then(|v| i32::try_from(v).ok());
                    if let Some(level) =
                        int_attr(cell, "signalStrength").and_then(|n| u8::try_from(n).ok())
                    {
                        status.signal_level = Some(level);
                    }
                }
            }
        }
        Ok(status)
    }
}

impl Client {
    /// Number of unread messages (`DEV2_LTE_NET_STATUS.smsUnreadCount`), the
    /// cheapest call to poll for incoming SMS.
    pub async fn unread_count(&mut self) -> Result<u32> {
        run_session!(self, s => s.unread_count().await)
    }

    /// Radio status, operator and unread count.
    pub async fn lte_status(&mut self) -> Result<LteStatus> {
        run_session!(self, s => s.lte_status().await)
    }
}
