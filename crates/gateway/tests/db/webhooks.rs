use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use gateway::config::WebhookConfig;
use gateway::webhooks::{self, Dispatcher, EventType};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{create_app, make_due, queue, wake};

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    /// Number of upcoming requests answered with HTTP 500.
    failures: Arc<Mutex<usize>>,
}

async fn receive(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
    r.hits.lock().unwrap().push((headers, body));
    let mut failures = r.failures.lock().unwrap();
    if *failures > 0 {
        *failures -= 1;
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::NO_CONTENT
    }
}

async fn spawn_receiver() -> (Receiver, String) {
    let receiver = Receiver::default();
    let app = Router::new()
        .route("/hook", post(receive))
        .with_state(receiver.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (receiver, url)
}

/// `(status, attempts)` of a delivery and its number of logged attempts.
async fn delivery(pool: &PgPool, id: Uuid) -> (String, i32, i64) {
    sqlx::query_as(
        "SELECT status, attempts,
                (SELECT count(*) FROM webhook_attempts WHERE delivery_id = d.id)
         FROM webhook_deliveries d WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn enqueue(pool: &PgPool, app_id: Uuid) -> Uuid {
    let message = queue(pool, app_id, "+33612345678", "hello").await;
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        webhooks::enqueue(&mut conn, EventType::Sent, &message)
            .await
            .unwrap()
    );
    sqlx::query_scalar("SELECT id FROM webhook_deliveries WHERE message_id = $1")
        .bind(message.id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn dispatcher(pool: &PgPool, max_age_secs: u64) -> Arc<Dispatcher> {
    let config = WebhookConfig {
        retry_schedule_secs: vec![60],
        max_age_secs,
        ..WebhookConfig::default()
    };
    Arc::new(Dispatcher::new(pool.clone(), config, wake()).unwrap())
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn deliveries_are_signed_and_retried(pool: PgPool) {
    let (receiver, url) = spawn_receiver().await;
    let (app, _) = create_app(&pool, "otp", Some(&url), &[], false).await;
    let secret = app.webhook_secret.clone().unwrap();
    let id = enqueue(&pool, app.id).await;
    let dispatcher = dispatcher(&pool, 3600);

    *receiver.failures.lock().unwrap() = 1;
    assert_eq!(dispatcher.dispatch_once().await.unwrap(), 1);
    assert_eq!(delivery(&pool, id).await, ("pending".into(), 1, 1));
    // Next attempt scheduled per retry_schedule_secs, not before.
    assert_eq!(dispatcher.dispatch_once().await.unwrap(), 0);
    make_due(&pool).await;
    assert_eq!(dispatcher.dispatch_once().await.unwrap(), 1);
    assert_eq!(delivery(&pool, id).await, ("delivered".into(), 2, 2));

    let hits = receiver.hits.lock().unwrap();
    assert_eq!(hits.len(), 2);
    for (headers, body) in hits.iter() {
        let header = |name: &str| headers[name].to_str().unwrap().to_string();
        // Same event id on every attempt, for receiver-side dedup.
        assert_eq!(header("x-webhook-id"), id.to_string());
        assert_eq!(header("x-event-type"), "message.sent");
        let timestamp: i64 = header("x-timestamp").parse().unwrap();
        assert_eq!(
            header("x-signature"),
            webhooks::sign(&secret, timestamp, body)
        );
        let event: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(event["id"], id.to_string());
        assert_eq!(event["data"]["text"], "hello");
    }
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn deliveries_are_abandoned_after_max_age(pool: PgPool) {
    let (receiver, url) = spawn_receiver().await;
    let (app, _) = create_app(&pool, "otp", Some(&url), &[], false).await;
    let id = enqueue(&pool, app.id).await;

    *receiver.failures.lock().unwrap() = usize::MAX;
    assert_eq!(dispatcher(&pool, 0).dispatch_once().await.unwrap(), 1);
    assert_eq!(delivery(&pool, id).await, ("failed".into(), 1, 1));
}
