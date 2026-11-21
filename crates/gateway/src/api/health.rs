use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AdminApp;
use super::error::Json;
use crate::modem_ctl::SessionState;

#[derive(Serialize, ToSchema)]
pub struct Health {
    pub status: &'static str,
}

/// Liveness: the process is up.
#[utoipa::path(get, path = "/healthz", tag = "health", responses((status = 200, body = Health)))]
pub async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

#[derive(Serialize, ToSchema)]
pub struct Readiness {
    /// `ok` or `unavailable`.
    pub status: &'static str,
    /// `ok` or `error`.
    pub database: &'static str,
    /// Session state of the device (`ok`, `paused`, `error`, `idle`).
    pub modem: SessionState,
}

/// Readiness: database reachable and last device call successful (or
/// device deliberately paused).
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "health",
    responses((status = 200, body = Readiness), (status = 503, body = Readiness)),
)]
pub async fn readyz(State(state): State<AppState>) -> Response {
    let database = match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => "ok",
        Err(e) => {
            tracing::warn!(error = %e, "readiness: database check failed");
            "error"
        }
    };
    let modem = state.modem.session_info().state;
    let ready = database == "ok" && matches!(modem, SessionState::Ok | SessionState::Paused);
    let body = Readiness {
        status: if ready { "ok" } else { "unavailable" },
        database,
        modem,
    };
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}

pub fn render_metrics(state: &AppState) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
        .into_response()
}

/// Prometheus metrics, on the main listener (admin only) when no dedicated
/// metrics listener is configured.
pub async fn metrics_admin(State(state): State<AppState>, _admin: AdminApp) -> Response {
    render_metrics(&state)
}

pub async fn metrics_open(State(state): State<AppState>) -> Response {
    render_metrics(&state)
}
