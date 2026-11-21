//! HTTP API (v1).

pub mod auth;
pub mod dev;
pub mod error;
pub mod health;
pub mod messages;
pub mod modem;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use tokio::sync::Notify;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa_swagger_ui::SwaggerUi;

use crate::config::{Config, ModemKind};
use crate::modem_ctl::ModemHandle;
use crate::phone::NumberPolicy;
use dev::DevModem;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub modem: Arc<ModemHandle>,
    pub modem_kind: ModemKind,
    pub policy: Arc<NumberPolicy>,
    /// Set with the mock backend only.
    pub dev: Option<Arc<DevModem>>,
    pub metrics: PrometheusHandle,
    pub outbound_wake: Arc<Notify>,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "sms-gateway", description = "Send and receive SMS through a cellular modem."),
    paths(
        messages::send,
        messages::get,
        messages::list,
        modem::status,
        modem::pause,
        modem::resume,
        health::healthz,
        health::readyz,
    ),
    modifiers(&BearerAuth),
    tags(
        (name = "messages", description = "Outbound and inbound SMS"),
        (name = "modem", description = "Device status and control"),
        (name = "health", description = "Probes"),
    ),
)]
pub struct ApiDoc;

struct BearerAuth;

impl utoipa::Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "api_key",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("Per-app API key (`sgw_...`)"))
                    .build(),
            ),
        );
    }
}

pub fn router(state: AppState, metrics_on_main: bool) -> Router {
    let mut v1 = Router::new()
        .route("/messages", post(messages::send).get(messages::list))
        .route("/messages/{id}", get(messages::get))
        .route("/modem", get(modem::status))
        .route("/modem/pause", post(modem::pause))
        .route("/modem/resume", post(modem::resume));
    if state.dev.is_some() {
        v1 = v1
            .route("/dev/inbound", post(dev::inject_inbound))
            .route("/dev/sent", get(dev::sent))
            .route("/dev/faults", post(dev::set_faults));
    }

    let mut app = Router::new()
        .nest("/v1", v1)
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz));
    if metrics_on_main {
        app = app.route("/metrics", get(health::metrics_admin));
    }
    if state.config.server.swagger_ui {
        app = app.merge(SwaggerUi::new("/docs").url("/openapi.json", ApiDoc::openapi()));
    } else {
        app = app.route(
            "/openapi.json",
            get(|| async { axum::Json(ApiDoc::openapi()) }),
        );
    }

    let server = &state.config.server;
    let trace = TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
        let request_id = req
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        // Path only: query strings are not logged.
        tracing::info_span!(
            "http",
            method = %req.method(),
            path = %req.uri().path(),
            request_id,
            app = tracing::field::Empty,
        )
    });

    // Each `layer` call wraps the previous ones (last = outermost).
    app.fallback(|| async { error::ApiError::not_found("no such route") })
        .layer(RequestBodyLimitLayer::new(server.body_limit_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(server.request_timeout_secs),
        ))
        .layer(axum::middleware::map_response(error::json_errors))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(trace)
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(state)
}

/// Dedicated, unauthenticated metrics listener.
pub fn metrics_router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(health::metrics_open))
        .with_state(state)
}
