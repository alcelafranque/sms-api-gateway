//! SMS gateway: HTTP API, PostgreSQL storage, modem workers and webhooks.
//!
//! The `sms-gateway` binary is a thin CLI over this library.

pub mod api;
pub mod apps;
pub mod config;
pub mod db;
pub mod messages;
pub mod metrics;
pub mod modem_ctl;
pub mod phone;
pub mod server;
pub mod sms_text;
pub mod webhooks;
pub mod workers;
