//! `sms-gateway`: HTTP SMS gateway in front of a cellular modem.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use gateway::config::{self, Config, LogFormat};
use gateway::{apps, db, server};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "sms-gateway", version, about)]
struct Cli {
    /// Configuration file (default: ./config.toml when present). Every key
    /// can be overridden with SMS_GATEWAY__<SECTION>__<KEY> variables.
    #[arg(short, long, env = "SMS_GATEWAY_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the API server and background workers.
    Serve,
    /// Apply pending database migrations.
    Migrate,
    /// Manage client applications.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
}

#[derive(Subcommand)]
enum AppCommand {
    /// Create an app and print its API key (shown only once).
    Create {
        name: String,
        /// Allow modem control (pause/resume), metrics and dev routes.
        #[arg(long)]
        admin: bool,
        /// URL receiving the app's webhooks; a signing secret is generated.
        #[arg(long)]
        webhook_url: Option<String>,
        /// Inbound keyword routed to this app (repeatable, or comma-separated).
        #[arg(long = "keyword")]
        keywords: Vec<String>,
    },
    /// List apps.
    List,
    /// Change an app's settings.
    Update {
        name: String,
        #[arg(long)]
        admin: Option<bool>,
        #[arg(long, conflicts_with = "no_webhook")]
        webhook_url: Option<String>,
        /// Remove the webhook.
        #[arg(long)]
        no_webhook: bool,
        /// Generate a new webhook signing secret.
        #[arg(long)]
        rotate_webhook_secret: bool,
        /// Replace the inbound keywords (comma-separated; empty to clear).
        #[arg(long)]
        keywords: Option<String>,
    },
    /// Disable an app: its API key stops working, no webhook is sent to it.
    Disable { name: String },
    /// Re-enable a disabled app.
    Enable { name: String },
    /// Replace an app's API key and print the new one (shown only once).
    RotateKey { name: String },
}

fn init_logging(config: &config::LogConfig) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.filter));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match config.format {
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_span_list(false)
            .init(),
        LogFormat::Text => builder.init(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    init_logging(&config.log);

    match cli.command {
        Command::Serve => server::serve(config).await,
        Command::Migrate => {
            let pool = db::connect(&config.database).await?;
            db::migrate(&pool).await?;
            println!("database up to date");
            Ok(())
        }
        Command::App { command } => {
            let pool = db::connect(&config.database).await?;
            app_command(&pool, command).await
        }
    }
}

fn validate_webhook_url(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL {url:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("webhook URL must be http(s)");
    }
    Ok(())
}

async fn app_command(pool: &sqlx::PgPool, command: AppCommand) -> anyhow::Result<()> {
    match command {
        AppCommand::Create {
            name,
            admin,
            webhook_url,
            keywords,
        } => {
            if let Some(url) = &webhook_url {
                validate_webhook_url(url)?;
            }
            let (app, key) = apps::create(
                pool,
                apps::CreateApp {
                    name,
                    admin,
                    webhook_url,
                    keywords,
                },
            )
            .await?;
            println!("app:            {}", app.name);
            println!("id:             {}", app.id);
            println!("admin:          {}", app.admin);
            println!("api key:        {key}");
            if let Some(secret) = &app.webhook_secret {
                println!("webhook secret: {secret}");
            }
            println!("\nStore these secrets now: the API key cannot be shown again.");
        }
        AppCommand::List => {
            println!(
                "{:<24} {:<7} {:<5} {:<14} {:<20} {:<20} WEBHOOK",
                "NAME", "ENABLED", "ADMIN", "KEY PREFIX", "KEYWORDS", "CREATED"
            );
            for app in apps::list(pool).await? {
                println!(
                    "{:<24} {:<7} {:<5} {:<14} {:<20} {:<20} {}",
                    app.name,
                    app.enabled,
                    app.admin,
                    app.key_prefix,
                    app.inbound_keywords.join(","),
                    app.created_at.format("%Y-%m-%d %H:%M:%S"),
                    app.webhook_url.as_deref().unwrap_or("-"),
                );
            }
        }
        AppCommand::Update {
            name,
            admin,
            webhook_url,
            no_webhook,
            rotate_webhook_secret,
            keywords,
        } => {
            if let Some(url) = &webhook_url {
                validate_webhook_url(url)?;
            }
            let req = apps::UpdateApp {
                admin,
                webhook_url: if no_webhook {
                    Some(None)
                } else {
                    webhook_url.map(Some)
                },
                rotate_webhook_secret,
                keywords: keywords.map(|k| vec![k]),
            };
            let Some((app, secret)) = apps::update(pool, &name, req).await? else {
                bail!("no app named {name:?}");
            };
            println!(
                "app {} updated (admin: {}, keywords: [{}], webhook: {})",
                app.name,
                app.admin,
                app.inbound_keywords.join(","),
                app.webhook_url.as_deref().unwrap_or("-"),
            );
            if let Some(secret) = secret {
                println!("new webhook secret: {secret}");
            }
        }
        AppCommand::Disable { name } => {
            if !apps::set_enabled(pool, &name, false).await? {
                bail!("no app named {name:?}");
            }
            println!("app {name} disabled");
        }
        AppCommand::Enable { name } => {
            if !apps::set_enabled(pool, &name, true).await? {
                bail!("no app named {name:?}");
            }
            println!("app {name} enabled");
        }
        AppCommand::RotateKey { name } => {
            let Some(key) = apps::rotate_key(pool, &name).await? else {
                bail!("no app named {name:?}");
            };
            println!("new api key: {key}");
            println!("\nThe previous key no longer works. This key cannot be shown again.");
        }
    }
    Ok(())
}
