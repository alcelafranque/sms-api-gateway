//! Async client for the `/cgi_gdpr` web API of TP-Link "EX"-family CPEs
//! (NE200-Outdoor, EX511, EX920...), the encrypted endpoint behind their web
//! UI, operating on `DEV2_*` data-model objects.
//!
//! The protocol is undocumented. Supported encryption variants: AES-128-CBC
//! or AES-128-GCM sessions, with raw or OAEP RSA signatures (see
//! [`Variant`]).
//!
//! # Attribution
//!
//! All protocol knowledge (endpoints, object names, message formats,
//! encryption scheme, return codes) comes from the Python library
//! [`tplinkrouterc6u`](https://github.com/AlexandrErohin/TP-Link-Archer-C6U)
//! (GPL-3.0), whose authors and contributors reverse-engineered these
//! devices. This crate is distributed under GPL-3.0-or-later as well.
//!
//! ```no_run
//! # async fn demo() -> Result<(), tplink_ex::Error> {
//! use tplink_ex::{Client, ClientConfig};
//!
//! let mut client = Client::new(ClientConfig::new("http://192.168.254.1", "user", "secret"))?;
//! let status = client.lte_status().await?;
//! println!("{:?} unread", status.unread_count);
//! for sms in client.inbox().await? {
//!     println!("{} {}: {}", sms.received_time, sms.from, sms.content);
//! }
//! client.logout().await?;
//! # Ok(()) }
//! ```
//!
//! # Execution guarantees
//!
//! Every [`Error`] has an [`ErrorKind`] telling whether the request may have
//! been executed by the device. The client re-logs in transparently once
//! when the device rejects the session *before* executing a request, and
//! never re-sends a non-idempotent act (SMS submission, delete) whose
//! outcome is unknown.
//!
//! # Logging
//!
//! Uses `tracing`. Nothing secret (password, session key, signatures) is
//! logged; SMS bodies and decrypted replies only appear at `trace` level.

mod act;
mod client;
pub mod crypto;
mod error;
mod sms;
mod status;

pub use act::{Act, Operation, ROOT_STACK};
pub use client::{Client, ClientConfig, Variant};
pub use error::{Error, ErrorKind, LoginError, Result};
pub use sms::{
    EntryLocation, INBOX_PAGE_SIZE, InboxEntry, SendReport, SendStatus, parse_device_time,
};
pub use status::LteStatus;
