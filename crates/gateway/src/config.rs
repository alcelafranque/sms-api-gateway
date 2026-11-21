//! Configuration: a TOML file merged with `SMS_GATEWAY__*` environment
//! variables (`__` separates nesting levels, e.g.
//! `SMS_GATEWAY__DATABASE__URL`, `SMS_GATEWAY__MODEM__TPLINK__PASSWORD`).

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, bail};
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

pub const ENV_PREFIX: &str = "SMS_GATEWAY__";

/// String settings read verbatim from the environment: figment would
/// otherwise parse values such as a numeric password into numbers.
const RAW_STRING_KEYS: &[&str] = &[
    "database.url",
    "modem.timezone",
    "modem.tplink.url",
    "modem.tplink.username",
    "modem.tplink.password",
    "inbound.default_app",
    "log.filter",
    "validation.default_region",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub log: LogConfig,
    pub database: DatabaseConfig,
    /// Required by `serve` only.
    #[serde(default)]
    pub modem: Option<ModemConfig>,
    #[serde(default)]
    pub outbound: OutboundConfig,
    #[serde(default)]
    pub inbound: InboundConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
    #[serde(default)]
    pub webhooks: WebhookConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Upper bound for the processing time of one API request.
    pub request_timeout_secs: u64,
    pub body_limit_bytes: usize,
    /// Serve Swagger UI at `/docs` (the OpenAPI document is always served at
    /// `/openapi.json`).
    pub swagger_ui: bool,
    /// Grace period for in-flight work on shutdown.
    pub shutdown_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().unwrap(),
            request_timeout_secs: 30,
            body_limit_bytes: 64 * 1024,
            swagger_ui: true,
            shutdown_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Dedicated, unauthenticated listener for `/metrics`. When unset,
    /// `/metrics` is served on the main listener to admin apps only.
    pub listen: Option<SocketAddr>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub format: LogFormat,
    /// `tracing_subscriber::EnvFilter` directives; `RUST_LOG` wins if set.
    pub filter: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Text,
            filter: "info,sqlx=warn,tower_http=info".into(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Apply pending migrations when `serve` starts.
    #[serde(default = "default_true")]
    pub migrate_on_start: bool,
}

impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseConfig")
            .field("url", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("migrate_on_start", &self.migrate_on_start)
            .finish()
    }
}

fn default_max_connections() -> u32 {
    10
}

fn default_connect_timeout() -> u64 {
    10
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModemKind {
    Mock,
    Tplink,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModemConfig {
    pub kind: ModemKind,
    /// IANA time zone of the device clock, used to interpret the naive
    /// reception time it reports for inbound SMS.
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// How often radio status (signal, operator...) is refreshed.
    #[serde(default = "default_status_interval")]
    pub status_interval_secs: u64,
    /// Safety net around every device call. A send exceeding it is recorded
    /// as `unknown` (it may have left the device).
    #[serde(default = "default_op_timeout")]
    pub op_timeout_secs: u64,
    /// Required when `kind = "tplink"`.
    pub tplink: Option<modem::tplink::TplinkConfig>,
}

fn default_timezone() -> String {
    "Europe/Paris".into()
}

fn default_status_interval() -> u64 {
    60
}

fn default_op_timeout() -> u64 {
    120
}

impl ModemConfig {
    pub fn timezone(&self) -> chrono_tz::Tz {
        self.timezone.parse().expect("validated at load")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboundConfig {
    /// Minimum delay between two send attempts (operator-friendly pacing).
    pub min_interval_ms: u64,
    /// Total attempts for a message failing with a retryable error.
    pub max_attempts: i32,
    pub retry_base_secs: u64,
    pub retry_max_secs: u64,
    /// A message left in `sending` for longer than this (crash during a
    /// send) is moved to `unknown`, never re-sent.
    pub sending_stale_secs: u64,
    /// Queue polling interval when idle.
    pub idle_poll_ms: u64,
}

impl Default for OutboundConfig {
    fn default() -> Self {
        Self {
            min_interval_ms: 3000,
            max_attempts: 5,
            retry_base_secs: 10,
            retry_max_secs: 600,
            sending_stale_secs: 600,
            idle_poll_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InboundConfig {
    /// Interval between two `unread_count` checks.
    pub poll_interval_secs: u64,
    /// Full inbox scan even when the device reports no unread message.
    pub full_scan_interval_secs: u64,
    /// Delete messages from the device once stored.
    pub delete_after_ingest: bool,
    /// Reply correlation window: an inbound SMS goes to the app that last
    /// sent to the same number within this window.
    pub reply_window_hours: u64,
    /// App receiving inbound SMS that match no keyword nor reply
    /// correlation. Unset: such messages are only stored.
    pub default_app: Option<String>,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 15,
            full_scan_interval_secs: 600,
            delete_after_ingest: true,
            reply_window_hours: 72,
            default_app: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ValidationConfig {
    /// ISO 3166 region used to parse numbers without a `+` prefix.
    pub default_region: String,
    /// Maximum SMS segments per message. A single SMS holds 160 GSM-7
    /// septets or 70 UCS-2 units, each segment of a concatenated one 153 / 67.
    pub max_segments: u32,
    /// If non-empty, recipients must start with one of these E.164 prefixes.
    pub allowed_prefixes: Vec<String>,
    /// Recipients starting with one of these E.164 prefixes are refused
    /// (takes precedence over `allowed_prefixes`).
    pub denied_prefixes: Vec<String>,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            default_region: "FR".into(),
            max_segments: 5,
            allowed_prefixes: Vec::new(),
            denied_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
    /// Delays between attempts; the last one repeats until `max_age_secs`.
    pub retry_schedule_secs: Vec<u64>,
    /// A delivery still failing this long after its creation is abandoned.
    pub max_age_secs: u64,
    /// Deliveries attempted in parallel.
    pub concurrency: usize,
    pub poll_interval_ms: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 10,
            connect_timeout_secs: 5,
            retry_schedule_secs: vec![10, 60, 600, 3600],
            max_age_secs: 24 * 3600,
            concurrency: 8,
            poll_interval_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub interval_secs: u64,
    /// Erase message bodies older than this (0 = keep).
    pub message_body_days: u32,
    /// Delete messages older than this (0 = keep).
    pub message_days: u32,
    /// Delete finished webhook deliveries (and their payloads) older than
    /// this (0 = keep).
    pub webhook_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval_secs: 3600,
            message_body_days: 30,
            message_days: 365,
            webhook_days: 30,
        }
    }
}

impl Config {
    /// Loads `path` (optional when absent and not explicitly requested),
    /// then environment overrides.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut figment = Figment::new();
        match path {
            Some(path) => {
                if !path.exists() {
                    bail!("config file {} not found", path.display());
                }
                figment = figment.merge(Toml::file(path));
            }
            None => {
                if Path::new("config.toml").exists() {
                    figment = figment.merge(Toml::file("config.toml"));
                }
            }
        }
        figment = figment.merge(Env::prefixed(ENV_PREFIX).split("__"));
        for key in RAW_STRING_KEYS {
            let var = format!("{ENV_PREFIX}{}", key.replace('.', "__").to_uppercase());
            if let Ok(value) = std::env::var(&var) {
                figment = figment.merge(Serialized::default(key, value));
            }
        }
        let config: Config = figment.extract().context("invalid configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Some(modem) = &self.modem {
            if modem.kind == ModemKind::Tplink && modem.tplink.is_none() {
                bail!("modem.kind = \"tplink\" requires a [modem.tplink] section");
            }
            modem
                .timezone
                .parse::<chrono_tz::Tz>()
                .map_err(|e| anyhow::anyhow!("modem.timezone: {e}"))?;
            if self.outbound.sending_stale_secs <= modem.op_timeout_secs {
                bail!("outbound.sending_stale_secs must exceed modem.op_timeout_secs");
            }
        }
        self.validation
            .default_region
            .parse::<phonenumber::country::Id>()
            .map_err(|_| {
                anyhow::anyhow!(
                    "validation.default_region: unknown region {:?}",
                    self.validation.default_region
                )
            })?;
        for prefix in self
            .validation
            .allowed_prefixes
            .iter()
            .chain(&self.validation.denied_prefixes)
        {
            if !prefix.starts_with('+') || !prefix[1..].chars().all(|c| c.is_ascii_digit()) {
                bail!("validation prefixes must look like \"+33\", got {prefix:?}");
            }
        }
        if !(1..=255).contains(&self.validation.max_segments) {
            bail!("validation.max_segments must be between 1 and 255");
        }
        if self.webhooks.retry_schedule_secs.is_empty() {
            bail!("webhooks.retry_schedule_secs must not be empty");
        }
        if self.outbound.max_attempts < 1 {
            bail!("outbound.max_attempts must be >= 1");
        }
        if self.webhooks.concurrency == 0 {
            bail!("webhooks.concurrency must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
// `Jail` closures must return `figment::Error`, which is large.
#[allow(clippy::result_large_err)]
mod tests {
    use figment::Jail;

    use super::*;

    const BASE: &str = r#"
        [database]
        url = "postgres://localhost/sms"
        [modem]
        kind = "tplink"
        [modem.tplink]
        url = "http://192.168.254.1"
        password = "from-file"
    "#;

    #[test]
    fn env_overrides_file_and_keeps_secrets_as_strings() {
        Jail::expect_with(|jail| {
            jail.create_file("config.toml", BASE)?;
            // Would be parsed as an integer by the generic env provider.
            jail.set_env("SMS_GATEWAY__MODEM__TPLINK__PASSWORD", "12345");
            jail.set_env("SMS_GATEWAY__DATABASE__URL", "postgres://u:p@db/sms");
            jail.set_env("SMS_GATEWAY__OUTBOUND__MAX_ATTEMPTS", "7");
            jail.set_env("SMS_GATEWAY__INBOUND__DELETE_AFTER_INGEST", "false");
            let config = Config::load(None).map_err(|e| format!("{e:#}"))?;
            let modem = config.modem.unwrap();
            assert_eq!(modem.tplink.unwrap().password, "12345");
            assert_eq!(config.database.url, "postgres://u:p@db/sms");
            assert_eq!(config.outbound.max_attempts, 7);
            assert!(!config.inbound.delete_after_ingest);
            Ok(())
        });
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                "[database]\nurl = \"x\"\n[modem]\nkind = \"tplink\"\n",
            )?;
            assert!(Config::load(None).is_err(), "tplink without [modem.tplink]");
            jail.create_file("config.toml", BASE)?;
            jail.set_env("SMS_GATEWAY__SERVER__LSITEN", "0.0.0.0:1");
            assert!(Config::load(None).is_err(), "unknown key");
            Ok(())
        });
    }
}
