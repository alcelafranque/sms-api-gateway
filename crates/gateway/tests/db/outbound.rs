use std::sync::Arc;

use gateway::api::dev::{DevModem, Fault};
use gateway::messages::Status;
use gateway::modem_ctl::ModemHandle;
use gateway::workers::outbound::{OutboundWorker, Step};
use sqlx::PgPool;

use crate::{config, create_app, deliveries, make_due, message, modem, queue, wake};

fn worker(pool: &PgPool, modem: &Arc<ModemHandle>) -> OutboundWorker {
    OutboundWorker {
        pool: pool.clone(),
        modem: modem.clone(),
        config: config().outbound,
        wake: wake(),
        webhook_wake: wake(),
    }
}

fn attempted(status: Status) -> Step {
    Step::Attempted {
        status,
        retryable: false,
    }
}

fn sent_texts(dev: &DevModem) -> Vec<String> {
    dev.sent().into_iter().map(|s| s.text).collect()
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn queued_message_is_sent_once_and_notified(pool: PgPool) {
    let (app, _) = create_app(&pool, "otp", Some("http://127.0.0.1:9/hook"), &[], false).await;
    let (dev, handle) = modem();
    let worker = worker(&pool, &handle);
    let msg = queue(&pool, app.id, "+33612345678", "hello").await;

    // Paused: the device is not touched and the message stays queued.
    handle.pause(None).await;
    assert_eq!(worker.step().await.unwrap(), Step::Paused);
    assert_eq!(message(&pool, msg.id).await.status, Status::Queued);
    handle.resume();

    assert_eq!(worker.step().await.unwrap(), attempted(Status::Sent));
    let sent = message(&pool, msg.id).await;
    assert_eq!((sent.status, sent.attempts), (Status::Sent, 1));
    assert!(sent.sent_at.is_some());
    assert_eq!(dev.sent()[0].to, "+33612345678");
    assert_eq!(
        deliveries(&pool).await,
        [("message.sent".to_string(), app.id, Some(msg.id))]
    );
    assert_eq!(worker.step().await.unwrap(), Step::Idle);
    assert_eq!(sent_texts(&dev), ["hello"]);
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn unavailable_is_retried_with_backoff_up_to_max_attempts(pool: PgPool) {
    let (app, _) = create_app(&pool, "otp", Some("http://127.0.0.1:9/hook"), &[], false).await;
    let (dev, handle) = modem();
    let worker = worker(&pool, &handle);

    // Two transient failures, then success.
    dev.set_send_faults([Fault::Unavailable, Fault::Unavailable]);
    let msg = queue(&pool, app.id, "+33612345678", "retried").await;
    let retry = Step::Attempted {
        status: Status::Queued,
        retryable: true,
    };
    assert_eq!(worker.step().await.unwrap(), retry);
    let queued = message(&pool, msg.id).await;
    assert_eq!((queued.status, queued.attempts), (Status::Queued, 1));
    assert!(queued.next_attempt_at.unwrap() > chrono::Utc::now());
    assert!(queued.last_error.unwrap().contains("unavailable"));
    // Not due yet.
    assert_eq!(worker.step().await.unwrap(), Step::Idle);
    make_due(&pool).await;
    assert_eq!(worker.step().await.unwrap(), retry);
    make_due(&pool).await;
    assert_eq!(worker.step().await.unwrap(), attempted(Status::Sent));
    let sent = message(&pool, msg.id).await;
    assert_eq!((sent.status, sent.attempts), (Status::Sent, 3));

    // Always unavailable: fails after max_attempts (3).
    dev.set_send_faults([Fault::Unavailable; 3]);
    let msg = queue(&pool, app.id, "+33612345678", "exhausted").await;
    for _ in 0..2 {
        assert_eq!(worker.step().await.unwrap(), retry);
        make_due(&pool).await;
    }
    let last = worker.step().await.unwrap();
    assert!(matches!(
        last,
        Step::Attempted {
            status: Status::Failed,
            ..
        }
    ));
    let failed = message(&pool, msg.id).await;
    assert_eq!((failed.status, failed.attempts), (Status::Failed, 3));
    let events: Vec<String> = deliveries(&pool).await.into_iter().map(|d| d.0).collect();
    assert_eq!(events, ["message.sent", "message.failed"]);
    assert_eq!(sent_texts(&dev), ["retried"]);
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn unknown_outcome_is_never_resent(pool: PgPool) {
    let (app, _) = create_app(&pool, "otp", Some("http://127.0.0.1:9/hook"), &[], false).await;
    let (dev, handle) = modem();
    let worker = worker(&pool, &handle);

    dev.set_send_faults([Fault::Unknown, Fault::Rejected]);
    let unknown = queue(&pool, app.id, "+33612345678", "maybe sent").await;
    let rejected = queue(&pool, app.id, "+33612345678", "refused").await;
    assert_eq!(worker.step().await.unwrap(), attempted(Status::Unknown));
    assert_eq!(worker.step().await.unwrap(), attempted(Status::Failed));
    make_due(&pool).await;
    assert_eq!(worker.step().await.unwrap(), Step::Idle);

    assert_eq!(message(&pool, unknown.id).await.status, Status::Unknown);
    assert_eq!(message(&pool, rejected.id).await.status, Status::Failed);
    let events: Vec<String> = deliveries(&pool).await.into_iter().map(|d| d.0).collect();
    assert_eq!(events, ["message.unknown", "message.failed"]);
    assert!(dev.sent().is_empty());
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn interrupted_send_becomes_unknown(pool: PgPool) {
    let (app, _) = create_app(&pool, "otp", None, &[], false).await;
    let (dev, handle) = modem();
    let worker = worker(&pool, &handle);
    let msg = queue(&pool, app.id, "+33612345678", "in flight").await;
    // A gateway died while this message was being sent.
    sqlx::query(
        "UPDATE messages SET status = 'sending', attempts = 1,
                             updated_at = now() - interval '1 hour'
         WHERE id = $1",
    )
    .bind(msg.id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(worker.reap_stale().await.unwrap(), 1);
    assert_eq!(message(&pool, msg.id).await.status, Status::Unknown);
    assert_eq!(worker.step().await.unwrap(), Step::Idle);
    assert!(dev.sent().is_empty());
}
