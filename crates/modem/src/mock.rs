use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Local;

use crate::{InboundSms, InboxRef, ModemStatus, Result, SendOutcome, SmsModem};

/// A message recorded by [`MockModem::send`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentSms {
    pub to: String,
    pub text: String,
}

#[derive(Default)]
struct State {
    next_index: u32,
    inbox: Vec<InboundSms>,
    sent: Vec<SentSms>,
}

/// In-memory modem for local development without hardware.
#[derive(Default)]
pub struct MockModem {
    state: Mutex<State>,
}

impl MockModem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulates the reception of a message.
    pub fn inject_inbound(&self, from: impl Into<String>, text: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.next_index += 1;
        let index = state.next_index;
        state.inbox.push(InboundSms {
            inbox_ref: InboxRef {
                index,
                locator: index.to_string(),
            },
            from: from.into(),
            text: text.into(),
            received_at: Local::now().naive_local(),
            unread: true,
        });
    }

    /// Messages sent so far.
    pub fn sent(&self) -> Vec<SentSms> {
        self.state.lock().unwrap().sent.clone()
    }
}

#[async_trait]
impl SmsModem for MockModem {
    async fn send(&self, to: &str, text: &str) -> Result<SendOutcome> {
        self.state.lock().unwrap().sent.push(SentSms {
            to: to.to_owned(),
            text: text.to_owned(),
        });
        Ok(SendOutcome::Sent)
    }

    async fn unread_count(&self) -> Result<u32> {
        let state = self.state.lock().unwrap();
        Ok(state.inbox.iter().filter(|m| m.unread).count() as u32)
    }

    async fn inbox(&self) -> Result<Vec<InboundSms>> {
        Ok(self.state.lock().unwrap().inbox.clone())
    }

    async fn delete(&self, inbox_ref: &InboxRef) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .inbox
            .retain(|m| m.inbox_ref != *inbox_ref);
        Ok(())
    }

    async fn status(&self) -> Result<ModemStatus> {
        let state = self.state.lock().unwrap();
        Ok(ModemStatus {
            operator: Some("Mock".into()),
            signal_level: Some(4),
            unread_count: state.inbox.iter().filter(|m| m.unread).count() as u32,
            inbox_total: Some(state.inbox.len() as u32),
            ..Default::default()
        })
    }
}
