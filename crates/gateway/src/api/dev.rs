//! Development helpers, mounted only with `modem.kind = "mock"`: inject
//! inbound SMS, inspect sent SMS and inject device faults.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use modem::{InboundSms, InboxRef, MockModem, ModemError, ModemStatus, SendOutcome, SmsModem};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::AdminApp;
use super::error::{ApiError, Json};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    Unavailable,
    Unknown,
    Rejected,
    Protocol,
    /// The device accepts the message without confirming it left.
    Submitted,
}

#[derive(Default)]
struct Faults {
    send: VecDeque<Fault>,
    offline: bool,
}

/// [`MockModem`] with scriptable failures.
#[derive(Default)]
pub struct DevModem {
    inner: MockModem,
    faults: Mutex<Faults>,
}

impl DevModem {
    /// Simulates the reception of a message.
    pub fn inject_inbound(&self, from: impl Into<String>, text: impl Into<String>) {
        self.inner.inject_inbound(from, text);
    }

    /// Messages that left the (mock) device.
    pub fn sent(&self) -> Vec<Sent> {
        self.inner
            .sent()
            .into_iter()
            .map(|s| Sent {
                to: s.to,
                text: s.text,
            })
            .collect()
    }

    /// Outcomes of the next send calls, in order (replaces the queue).
    pub fn set_send_faults(&self, faults: impl IntoIterator<Item = Fault>) {
        self.faults.lock().unwrap().send = faults.into_iter().collect();
    }

    /// While set, every device call fails with `Unavailable`.
    pub fn set_offline(&self, offline: bool) {
        self.faults.lock().unwrap().offline = offline;
    }

    fn check_offline(&self) -> modem::Result<()> {
        if self.faults.lock().unwrap().offline {
            return Err(ModemError::Unavailable("dev: modem offline".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl SmsModem for DevModem {
    async fn send(&self, to: &str, text: &str) -> modem::Result<SendOutcome> {
        self.check_offline()?;
        let fault = self.faults.lock().unwrap().send.pop_front();
        match fault {
            None => self.inner.send(to, text).await,
            Some(Fault::Submitted) => {
                self.inner.send(to, text).await?;
                Ok(SendOutcome::Submitted)
            }
            Some(Fault::Unavailable) => Err(ModemError::Unavailable("dev: injected".into())),
            Some(Fault::Unknown) => Err(ModemError::Unknown("dev: injected".into())),
            Some(Fault::Rejected) => Err(ModemError::Rejected("dev: injected".into())),
            Some(Fault::Protocol) => Err(ModemError::Protocol("dev: injected".into())),
        }
    }

    async fn unread_count(&self) -> modem::Result<u32> {
        self.check_offline()?;
        self.inner.unread_count().await
    }

    async fn inbox(&self) -> modem::Result<Vec<InboundSms>> {
        self.check_offline()?;
        self.inner.inbox().await
    }

    async fn delete(&self, inbox_ref: &InboxRef) -> modem::Result<()> {
        self.check_offline()?;
        self.inner.delete(inbox_ref).await
    }

    async fn status(&self) -> modem::Result<ModemStatus> {
        self.check_offline()?;
        self.inner.status().await
    }
}

fn dev_modem(state: &AppState) -> Result<&DevModem, ApiError> {
    state
        .dev
        .as_deref()
        .ok_or_else(|| ApiError::not_found("dev routes require modem.kind = \"mock\""))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectInbound {
    pub from: String,
    pub text: String,
}

pub async fn inject_inbound(
    State(state): State<AppState>,
    _admin: AdminApp,
    Json(req): Json<InjectInbound>,
) -> Result<StatusCode, ApiError> {
    dev_modem(&state)?.inject_inbound(req.from, req.text);
    Ok(StatusCode::ACCEPTED)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sent {
    pub to: String,
    pub text: String,
}

pub async fn sent(
    State(state): State<AppState>,
    _admin: AdminApp,
) -> Result<Json<Vec<Sent>>, ApiError> {
    Ok(Json(dev_modem(&state)?.sent()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFaults {
    /// Outcomes for the next send calls, in order (replaces the queue).
    #[serde(default)]
    pub send: Option<Vec<Fault>>,
    /// Every device call fails with `Unavailable` while set.
    #[serde(default)]
    pub offline: Option<bool>,
}

#[derive(Serialize)]
pub struct FaultState {
    pub send: Vec<Fault>,
    pub offline: bool,
}

pub async fn set_faults(
    State(state): State<AppState>,
    _admin: AdminApp,
    Json(req): Json<SetFaults>,
) -> Result<Json<FaultState>, ApiError> {
    let dev = dev_modem(&state)?;
    if let Some(send) = req.send {
        dev.set_send_faults(send);
    }
    if let Some(offline) = req.offline {
        dev.set_offline(offline);
    }
    let faults = dev.faults.lock().unwrap();
    Ok(Json(FaultState {
        send: faults.send.iter().copied().collect(),
        offline: faults.offline,
    }))
}
