use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use gateway::api::{self, AppState};
use gateway::apps;
use gateway::config::ModemKind;
use gateway::modem_ctl::PauseState;
use gateway::phone::NumberPolicy;
use metrics_exporter_prometheus::PrometheusBuilder;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::{config, create_app, modem, wake};

fn router(pool: &PgPool) -> Router {
    let config = config();
    let (dev, handle) = modem();
    api::router(
        AppState {
            pool: pool.clone(),
            policy: Arc::new(NumberPolicy::new(&config.validation)),
            config: Arc::new(config),
            modem: handle,
            modem_kind: ModemKind::Mock,
            dev: Some(dev),
            metrics: PrometheusBuilder::new().build_recorder().handle(),
            outbound_wake: wake(),
        },
        true,
    )
}

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    key: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let req = match body {
        Some(body) => req
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (parts.status, parts.headers, json)
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn apps_only_see_their_own_messages(pool: PgPool) {
    let router = router(&pool);
    let (_, key_a) = create_app(&pool, "a", None, &[], false).await;
    let (_, key_b) = create_app(&pool, "b", None, &[], false).await;

    let send = json!({"to": "06 12 34 56 78", "text": "hello"});
    let (status, headers, msg) = call(
        &router,
        Method::POST,
        "/v1/messages",
        Some(&key_a),
        &[],
        Some(send),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        (msg["status"].as_str(), msg["to"].as_str()),
        (Some("queued"), Some("+33612345678"))
    );
    let id = msg["id"].as_str().unwrap();
    assert_eq!(headers["location"], format!("/v1/messages/{id}").as_str());
    let uri = format!("/v1/messages/{id}");

    let (status, _, body) = call(&router, Method::GET, &uri, Some(&key_a), &[], None).await;
    assert_eq!((status, body["id"].as_str()), (StatusCode::OK, Some(id)));
    let (status, _, body) = call(&router, Method::GET, &uri, Some(&key_b), &[], None).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );

    let list = |key: String| {
        let router = router.clone();
        async move {
            let (_, _, body) =
                call(&router, Method::GET, "/v1/messages", Some(&key), &[], None).await;
            body["items"].as_array().unwrap().len()
        }
    };
    assert_eq!(list(key_a.clone()).await, 1);
    assert_eq!(list(key_b.clone()).await, 0);

    // Missing, forged and disabled credentials.
    let (status, _, _) = call(&router, Method::GET, &uri, None, &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let forged = format!("{}x", &key_a[..key_a.len() - 1]);
    let (status, _, _) = call(&router, Method::GET, &uri, Some(&forged), &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    apps::set_enabled(&pool, "a", false).await.unwrap();
    let (status, _, _) = call(&router, Method::GET, &uri, Some(&key_a), &[], None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn text_length_is_limited_in_sms_segments(pool: PgPool) {
    let router = router(&pool);
    let (_, key) = create_app(&pool, "a", None, &[], false).await;
    let send = |text: String| {
        let router = router.clone();
        let key = key.clone();
        async move {
            let body = json!({"to": "+33612345678", "text": text});
            let (status, _, msg) = call(
                &router,
                Method::POST,
                "/v1/messages",
                Some(&key),
                &[],
                Some(body),
            )
            .await;
            (status, msg)
        }
    };
    // 382 × 2 septets: 764 of the 765 that fit in 5 segments.
    let (status, msg) = send("€".repeat(382)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        (msg["encoding"].as_str(), msg["segments"].as_u64()),
        (Some("gsm7"), Some(5))
    );
    let (status, msg) = send("€".repeat(383)).await;
    assert_eq!(
        (status, msg["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("validation_failed"))
    );
    // Emoji are two UCS-2 units: 168 of them exceed the 335 allowed.
    let (status, msg) = send("📡".repeat(167)).await;
    assert_eq!(
        (status, msg["encoding"].as_str(), msg["segments"].as_u64()),
        (StatusCode::ACCEPTED, Some("ucs2"), Some(5))
    );
    let (status, _) = send("📡".repeat(168)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn idempotency_key_replays_or_conflicts(pool: PgPool) {
    let router = router(&pool);
    let (_, key_a) = create_app(&pool, "a", None, &[], false).await;
    let (_, key_b) = create_app(&pool, "b", None, &[], false).await;
    let idem = [("idempotency-key", "order-1")];
    let body = json!({"to": "+33612345678", "text": "hello"});

    let (s1, _, first) = call(
        &router,
        Method::POST,
        "/v1/messages",
        Some(&key_a),
        &idem,
        Some(body.clone()),
    )
    .await;
    let (s2, h2, replay) = call(
        &router,
        Method::POST,
        "/v1/messages",
        Some(&key_a),
        &idem,
        Some(body.clone()),
    )
    .await;
    assert_eq!((s1, s2), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    assert_eq!(first["id"], replay["id"]);
    assert_eq!(h2["idempotent-replayed"], "true");

    // Keys are scoped per app.
    let (_, _, other) = call(
        &router,
        Method::POST,
        "/v1/messages",
        Some(&key_b),
        &idem,
        Some(body),
    )
    .await;
    assert_ne!(first["id"], other["id"]);

    let changed = json!({"to": "+33612345678", "text": "different"});
    let (status, _, err) = call(
        &router,
        Method::POST,
        "/v1/messages",
        Some(&key_a),
        &idem,
        Some(changed),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"]["code"], "idempotency_conflict");
}

#[sqlx::test(migrator = "gateway::db::MIGRATOR")]
async fn modem_control_requires_admin_and_persists(pool: PgPool) {
    let router = router(&pool);
    let (_, user) = create_app(&pool, "user", None, &[], false).await;
    let (_, admin) = create_app(&pool, "ops", None, &[], true).await;

    let (status, _, _) = call(
        &router,
        Method::POST,
        "/v1/modem/pause",
        Some(&user),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(&router, Method::GET, "/metrics", Some(&user), &[], None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, info) = call(
        &router,
        Method::POST,
        "/v1/modem/pause",
        Some(&admin),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (info["paused"].as_bool(), info["session"]["state"].as_str()),
        (Some(true), Some("paused"))
    );
    let stored: Option<PauseState> = gateway::db::get_setting(&pool, api::modem::PAUSE_SETTING)
        .await
        .unwrap();
    assert!(stored.unwrap().paused);

    let (status, _, info) = call(
        &router,
        Method::POST,
        "/v1/modem/resume",
        Some(&admin),
        &[],
        None,
    )
    .await;
    assert_eq!(
        (status, info["paused"].as_bool()),
        (StatusCode::OK, Some(false))
    );
}
