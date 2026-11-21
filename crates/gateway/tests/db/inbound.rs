use std::sync::Arc;

use gateway::api::dev::DevModem;
use gateway::config::InboundConfig;
use gateway::modem_ctl::ModemHandle;
use gateway::phone::NumberPolicy;
use gateway::workers::inbound::{InboundPoller, PollState};
use modem::SmsModem;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{config, create_app, deliveries, modem, queue, wake};

fn poller(pool: &PgPool, modem: &Arc<ModemHandle>, inbound: InboundConfig) -> InboundPoller {
    InboundPoller {
        pool: pool.clone(),
        modem: modem.clone(),
        config: inbound,
        timezone: chrono_tz::Europe::Paris,
        policy: Arc::new(NumberPolicy::new(&config().validation)),
        webhook_wake: wake(),
    }
}

/// `(sender, text, app_id, routed_by)` of stored inbound messages.
async fn inbound(pool: &PgPool) -> Vec<(String, String, Option<Uuid>, String)> {
    sqlx::query_as(
        "SELECT peer, body, app_id, routed_by FROM messages
         WHERE direction = 'inbound' ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn device_inbox(dev: &DevModem) -> usize {
    dev.inbox().await.unwrap().len()
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn routes_by_keyword_then_reply_then_default(pool: PgPool) {
    let hook = Some("http://127.0.0.1:9/hook");
    let (agenda, _) = create_app(&pool, "agenda", hook, &["RDV"], false).await;
    let (otp, _) = create_app(&pool, "otp", hook, &[], false).await;
    let (catchall, _) = create_app(&pool, "catchall", hook, &[], false).await;
    let replied = queue(&pool, otp.id, "+33612345678", "your code").await;
    sqlx::query("UPDATE messages SET status = 'sent' WHERE id = $1")
        .bind(replied.id)
        .execute(&pool)
        .await
        .unwrap();

    let (dev, handle) = modem();
    let poller = poller(
        &pool,
        &handle,
        InboundConfig {
            default_app: Some("catchall".into()),
            ..InboundConfig::default()
        },
    );
    // The keyword wins over the reply correlation.
    dev.inject_inbound("+33612345678", "rdv demain 10h");
    // National format: normalized before correlation.
    dev.inject_inbound("0612345678", "merci");
    dev.inject_inbound("+33700000000", "bonjour");
    assert!(poller.poll_once(&mut PollState::default()).await.unwrap());

    let s = |v: &str| v.to_string();
    assert_eq!(
        inbound(&pool).await,
        [
            (
                s("+33612345678"),
                s("rdv demain 10h"),
                Some(agenda.id),
                s("keyword")
            ),
            (s("+33612345678"), s("merci"), Some(otp.id), s("reply")),
            (
                s("+33700000000"),
                s("bonjour"),
                Some(catchall.id),
                s("default")
            ),
        ]
    );
    assert_eq!(device_inbox(&dev).await, 0, "ingested messages are deleted");
    let received: Vec<Uuid> = deliveries(&pool)
        .await
        .into_iter()
        .filter(|d| d.0 == "message.received")
        .map(|d| d.1)
        .collect();
    assert_eq!(received, [agenda.id, otp.id, catchall.id]);

    // Without a default app, unmatched messages are only stored.
    let store_only = self::poller(&pool, &handle, InboundConfig::default());
    dev.inject_inbound("+33700000001", "anyone?");
    store_only
        .poll_once(&mut PollState::default())
        .await
        .unwrap();
    let last = inbound(&pool).await.pop().unwrap();
    assert_eq!((last.2, last.3.as_str()), (None, "none"));
    assert_eq!(deliveries(&pool).await.len(), 3);
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn messages_left_on_the_device_are_stored_once(pool: PgPool) {
    let (dev, handle) = modem();
    let poller = poller(
        &pool,
        &handle,
        InboundConfig {
            delete_after_ingest: false,
            ..InboundConfig::default()
        },
    );
    dev.inject_inbound("+33700000000", "first");
    dev.inject_inbound("+33700000000", "second");
    // Each fresh state forces a full scan, as after a restart (or a crash
    // between storage and deletion).
    for _ in 0..3 {
        poller.poll_once(&mut PollState::default()).await.unwrap();
    }
    assert_eq!(inbound(&pool).await.len(), 2);
    assert_eq!(device_inbox(&dev).await, 2);

    // A paused modem is not polled.
    handle.pause(None).await;
    dev.inject_inbound("+33700000000", "third");
    assert!(!poller.poll_once(&mut PollState::default()).await.unwrap());
    assert_eq!(inbound(&pool).await.len(), 2);
}
