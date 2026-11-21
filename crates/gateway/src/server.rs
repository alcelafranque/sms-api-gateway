//! `serve`: wires the API, the workers and graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use modem::SmsModem;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::api::dev::DevModem;
use crate::api::{self, AppState};
use crate::config::{Config, ModemKind};
use crate::modem_ctl::{ModemHandle, PauseState};
use crate::phone::NumberPolicy;
use crate::webhooks::Dispatcher;
use crate::workers::inbound::InboundPoller;
use crate::workers::outbound::OutboundWorker;
use crate::{db, metrics, workers};

pub async fn serve(config: Config) -> anyhow::Result<()> {
    let prometheus = metrics::install()?;
    let pool = db::connect(&config.database).await?;
    if config.database.migrate_on_start {
        db::migrate(&pool).await?;
    }

    let modem_config = config
        .modem
        .clone()
        .context("missing [modem] configuration section")?;
    let (modem, dev): (Arc<dyn SmsModem>, Option<Arc<DevModem>>) = match modem_config.kind {
        ModemKind::Mock => {
            tracing::warn!("using the mock modem: no SMS will actually be sent");
            let dev = Arc::new(DevModem::default());
            (dev.clone(), Some(dev))
        }
        ModemKind::Tplink => {
            let tplink = modem_config.tplink.clone().expect("validated at load");
            tracing::info!(url = %tplink.url, "using the TP-Link modem");
            let modem = modem::tplink::TplinkModem::new(tplink)
                .context("cannot initialize the TP-Link modem")?;
            (Arc::new(modem), None)
        }
    };

    let pause: PauseState = db::get_setting(&pool, api::modem::PAUSE_SETTING)
        .await?
        .unwrap_or_default();
    if pause.active(chrono::Utc::now()) {
        tracing::warn!(until = ?pause.until, "modem paused (persisted state)");
    }
    let modem = Arc::new(ModemHandle::new(
        modem,
        pause,
        Duration::from_secs(modem_config.op_timeout_secs),
    ));

    let config = Arc::new(config);
    let policy = Arc::new(NumberPolicy::new(&config.validation));
    let outbound_wake = Arc::new(Notify::new());
    let webhook_wake = Arc::new(Notify::new());
    let shutdown = CancellationToken::new();
    let mut tasks = JoinSet::new();

    tasks.spawn(
        OutboundWorker {
            pool: pool.clone(),
            modem: modem.clone(),
            config: config.outbound.clone(),
            wake: outbound_wake.clone(),
            webhook_wake: webhook_wake.clone(),
        }
        .run(shutdown.clone()),
    );
    tasks.spawn(
        InboundPoller {
            pool: pool.clone(),
            modem: modem.clone(),
            config: config.inbound.clone(),
            timezone: modem_config.timezone(),
            policy: policy.clone(),
            webhook_wake: webhook_wake.clone(),
        }
        .run(shutdown.clone()),
    );
    tasks.spawn(
        Dispatcher::new(pool.clone(), config.webhooks.clone(), webhook_wake.clone())?
            .run(shutdown.clone()),
    );
    tasks.spawn(workers::status_refresher(
        modem.clone(),
        Duration::from_secs(modem_config.status_interval_secs.max(5)),
        shutdown.clone(),
    ));
    tasks.spawn(workers::stats(
        pool.clone(),
        prometheus.clone(),
        shutdown.clone(),
    ));
    tasks.spawn(workers::retention(
        pool.clone(),
        config.retention.clone(),
        shutdown.clone(),
    ));

    let state = AppState {
        pool: pool.clone(),
        config: config.clone(),
        modem: modem.clone(),
        modem_kind: modem_config.kind,
        policy,
        dev,
        metrics: prometheus,
        outbound_wake,
    };

    let mut servers = JoinSet::new();
    if let Some(addr) = config.metrics.listen {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        tracing::info!(%addr, "metrics listening");
        let router = api::metrics_router(state.clone());
        let token = shutdown.clone();
        servers.spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
        });
    }
    let addr = config.server.listen;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    tracing::info!(%addr, "API listening");
    let router = api::router(state, config.metrics.listen.is_none());
    let token = shutdown.clone();
    servers.spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(token.cancelled_owned())
            .await
    });

    tokio::select! {
        _ = shutdown_signal() => tracing::info!("shutdown requested"),
        res = servers.join_next() => {
            tracing::error!(?res, "HTTP server stopped unexpectedly");
        }
    }
    shutdown.cancel();

    let grace = Duration::from_secs(config.server.shutdown_timeout_secs);
    let drain = async {
        while servers.join_next().await.is_some() {}
        while tasks.join_next().await.is_some() {}
    };
    if tokio::time::timeout(grace, drain).await.is_err() {
        tracing::warn!("shutdown grace period elapsed, aborting remaining tasks");
        servers.abort_all();
        tasks.abort_all();
    }
    modem.shutdown().await;
    pool.close().await;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
