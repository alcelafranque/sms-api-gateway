use std::time::Duration;

use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::AppState;
use super::auth::{AdminApp, AuthApp};
use super::error::{ApiError, ErrorBody, Json, Query};
use crate::config::ModemKind;
use crate::db;
use crate::messages;
use crate::modem_ctl::{PauseState, SessionInfo};

pub const PAUSE_SETTING: &str = "modem_pause";

#[derive(Serialize, ToSchema)]
pub struct ModemInfo {
    #[schema(value_type = String, example = "tplink")]
    pub backend: ModemKind,
    pub paused: bool,
    /// Automatic resume time of the current pause, if any.
    pub paused_until: Option<DateTime<Utc>>,
    pub session: SessionInfo,
    /// Last radio status read from the device (`null` until the first read).
    pub status: Option<RadioStatus>,
    /// Last successful inbox poll.
    pub last_poll_at: Option<DateTime<Utc>>,
    pub queue: QueueInfo,
}

#[derive(Serialize, ToSchema)]
pub struct RadioStatus {
    pub operator: Option<String>,
    pub signal_level: Option<u8>,
    pub rsrp_dbm: Option<i32>,
    pub rsrq_db: Option<i32>,
    pub snr_db: Option<f32>,
    pub unread_count: u32,
    /// Messages stored in the device inbox.
    pub inbox_total: Option<u32>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct QueueInfo {
    /// Outbound messages waiting to be sent (including scheduled retries).
    pub queued: i64,
    pub sending: i64,
    pub webhooks_pending: i64,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct StatusParams {
    /// Read the radio status from the device now instead of returning the
    /// cached one (ignored while paused).
    pub refresh: Option<bool>,
}

async fn modem_info(state: &AppState) -> Result<ModemInfo, ApiError> {
    let depth = messages::queue_depth(&state.pool).await?;
    let pause = state.modem.pause_state();
    Ok(ModemInfo {
        backend: state.modem_kind,
        paused: pause.paused,
        paused_until: pause.until,
        session: state.modem.session_info(),
        status: state.modem.cached_status().map(|c| RadioStatus {
            operator: c.status.operator,
            signal_level: c.status.signal_level,
            rsrp_dbm: c.status.rsrp_dbm,
            rsrq_db: c.status.rsrq_db,
            snr_db: c.status.snr_db,
            unread_count: c.status.unread_count,
            inbox_total: c.status.inbox_total,
            updated_at: c.updated_at,
        }),
        last_poll_at: state.modem.last_poll(),
        queue: QueueInfo {
            queued: depth.queued,
            sending: depth.sending,
            webhooks_pending: depth.webhooks_pending,
        },
    })
}

/// Device status, session state and queue depth.
#[utoipa::path(
    get,
    path = "/v1/modem",
    tag = "modem",
    params(StatusParams),
    responses((status = 200, body = ModemInfo), (status = 401, body = ErrorBody)),
    security(("api_key" = [])),
)]
pub async fn status(
    State(state): State<AppState>,
    _app: AuthApp,
    Query(params): Query<StatusParams>,
) -> Result<Json<ModemInfo>, ApiError> {
    if params.refresh == Some(true) {
        // Do not queue behind a long device operation: fall back to the
        // cached status if the device is busy.
        if let Ok(Ok(session)) =
            tokio::time::timeout(Duration::from_secs(5), state.modem.session()).await
        {
            let _ = session.status().await;
        }
    }
    Ok(Json(modem_info(&state).await?))
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct PauseParams {
    /// Resume automatically after this many seconds (default: stay paused
    /// until `POST /v1/modem/resume`).
    pub duration_secs: Option<u64>,
}

/// Pause all device access (admin).
///
/// Waits for the in-flight device call, stops sending and polling, and
/// releases the device session so the device web UI can be used. Queued
/// messages stay queued. The pause survives restarts.
#[utoipa::path(
    post,
    path = "/v1/modem/pause",
    tag = "modem",
    params(PauseParams),
    responses(
        (status = 200, body = ModemInfo),
        (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody),
    ),
    security(("api_key" = [])),
)]
pub async fn pause(
    State(state): State<AppState>,
    AdminApp(app): AdminApp,
    Query(params): Query<PauseParams>,
) -> Result<Json<ModemInfo>, ApiError> {
    let until = params
        .duration_secs
        .map(|s| {
            i64::try_from(s)
                .ok()
                .and_then(chrono::Duration::try_seconds)
                .map(|d| Utc::now() + d)
                .ok_or_else(|| ApiError::bad_request("duration_secs out of range"))
        })
        .transpose()?;
    let pause = PauseState {
        paused: true,
        until,
    };
    db::set_setting(&state.pool, PAUSE_SETTING, &pause).await?;
    tracing::info!(app = %app.name, ?until, "pause requested");
    state.modem.pause(until).await;
    Ok(Json(modem_info(&state).await?))
}

/// Resume device access (admin).
#[utoipa::path(
    post,
    path = "/v1/modem/resume",
    tag = "modem",
    responses(
        (status = 200, body = ModemInfo),
        (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody),
    ),
    security(("api_key" = [])),
)]
pub async fn resume(
    State(state): State<AppState>,
    AdminApp(app): AdminApp,
) -> Result<Json<ModemInfo>, ApiError> {
    db::set_setting(&state.pool, PAUSE_SETTING, &PauseState::default()).await?;
    tracing::info!(app = %app.name, "resume requested");
    state.modem.resume();
    Ok(Json(modem_info(&state).await?))
}
