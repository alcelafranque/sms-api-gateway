use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::AppState;
use super::auth::AuthApp;
use super::error::{ApiError, ErrorBody, Json, Path, Query};
use crate::messages::{
    self, Cursor, Direction, InsertOutcome, ListFilter, MessageView, NewOutbound, Order, Status,
};
use crate::phone::{Masked, NumberError};
use crate::sms_text::{Encoding, SmsSize};

const MAX_CLIENT_REF: usize = 128;
const MAX_IDEMPOTENCY_KEY: usize = 255;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    /// Recipient. E.164 (`+33612345678`) or a national number in the
    /// gateway default region (`0612345678`).
    #[schema(example = "+33612345678")]
    pub to: String,
    #[schema(example = "Your code is 123456")]
    pub text: String,
    /// Free-form reference echoed back in the message and webhooks.
    pub client_ref: Option<String>,
}

/// Queue an SMS.
///
/// The message is stored as `queued` and sent asynchronously; follow its
/// status with `GET /v1/messages/{id}` or the `message.*` webhooks. With an
/// `Idempotency-Key` header, repeating the same request returns the message
/// created by the first one (header `Idempotent-Replayed: true`).
#[utoipa::path(
    post,
    path = "/v1/messages",
    tag = "messages",
    request_body = SendRequest,
    params(
        ("Idempotency-Key" = Option<String>, Header,
         description = "Client-chosen key, unique per app, making retries safe"),
    ),
    responses(
        (status = 202, description = "Queued", body = MessageView),
        (status = 401, body = ErrorBody),
        (status = 409, description = "Idempotency key reused with another payload", body = ErrorBody),
        (status = 422, description = "Invalid recipient, or text empty or longer than `validation.max_segments` SMS segments", body = ErrorBody),
    ),
    security(("api_key" = [])),
)]
pub async fn send(
    State(state): State<AppState>,
    AuthApp(app): AuthApp,
    headers: HeaderMap,
    Json(req): Json<SendRequest>,
) -> Result<Response, ApiError> {
    let idempotency_key = match headers.get("idempotency-key") {
        None => None,
        Some(v) => {
            let key = v
                .to_str()
                .ok()
                .map(str::trim)
                .filter(|k| !k.is_empty() && k.len() <= MAX_IDEMPOTENCY_KEY)
                .ok_or_else(|| {
                    ApiError::bad_request(format!(
                        "Idempotency-Key must be 1 to {MAX_IDEMPOTENCY_KEY} visible ASCII characters"
                    ))
                })?;
            Some(key.to_owned())
        }
    };

    let to = state.policy.destination(&req.to).map_err(|e| match e {
        NumberError::Invalid => ApiError::validation("to: not a valid phone number"),
        NumberError::NotAllowed => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
            "to: destination not allowed by the gateway policy",
        ),
    })?;
    if req.text.trim().is_empty() {
        return Err(ApiError::validation("text: must not be empty"));
    }
    if req.text.contains('\0') {
        return Err(ApiError::validation(
            "text: must not contain NUL characters",
        ));
    }
    let size = SmsSize::of(&req.text);
    let max_segments = state.config.validation.max_segments;
    let max_units = size.encoding.max_units(max_segments);
    if size.units > max_units {
        let (unit, note) = match size.encoding {
            Encoding::Gsm7 => ("septets", "extension characters such as € count twice"),
            Encoding::Ucs2 => ("units", "most emoji count twice"),
        };
        return Err(ApiError::validation(format!(
            "text: {} {} {unit}, at most {max_units} allowed in {max_segments} SMS segment(s) ({note})",
            size.units,
            size.encoding.as_str(),
        )));
    }
    if let Some(client_ref) = &req.client_ref
        && (client_ref.chars().count() > MAX_CLIENT_REF || client_ref.contains('\0'))
    {
        return Err(ApiError::validation(format!(
            "client_ref: at most {MAX_CLIENT_REF} characters"
        )));
    }

    let outcome = messages::insert_outbound(
        &state.pool,
        NewOutbound {
            app_id: app.id,
            to: &to,
            text: &req.text,
            client_ref: req.client_ref.as_deref(),
            idempotency_key: idempotency_key.as_deref(),
        },
    )
    .await?;

    let (message, replayed) = match outcome {
        InsertOutcome::Created(message) => {
            tracing::info!(
                message_id = %message.id,
                app = %app.name,
                to = %Masked(&to),
                encoding = size.encoding.as_str(),
                segments = size.segments,
                "message queued"
            );
            state.outbound_wake.notify_one();
            (message, false)
        }
        InsertOutcome::Existing {
            message,
            same_request: true,
        } => (message, true),
        InsertOutcome::Existing { .. } => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "Idempotency-Key already used with a different request",
            ));
        }
    };

    let mut resp = (StatusCode::ACCEPTED, Json(MessageView::from(&message))).into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/v1/messages/{}", message.id)) {
        resp.headers_mut().insert(header::LOCATION, location);
    }
    if replayed {
        resp.headers_mut()
            .insert("idempotent-replayed", HeaderValue::from_static("true"));
    }
    Ok(resp)
}

/// Get one message of the calling app.
#[utoipa::path(
    get,
    path = "/v1/messages/{id}",
    tag = "messages",
    params(("id" = Uuid, Path, description = "Message id")),
    responses(
        (status = 200, body = MessageView),
        (status = 401, body = ErrorBody),
        (status = 404, body = ErrorBody),
    ),
    security(("api_key" = [])),
)]
pub async fn get(
    State(state): State<AppState>,
    AuthApp(app): AuthApp,
    Path(id): Path<Uuid>,
) -> Result<Json<MessageView>, ApiError> {
    let message = messages::get_for_app(&state.pool, app.id, id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such message"))?;
    Ok(Json(MessageView::from(&message)))
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListParams {
    pub direction: Option<Direction>,
    pub status: Option<Status>,
    /// Only messages created at or after this time (RFC 3339).
    pub since: Option<DateTime<Utc>>,
    /// Only messages created before this time (RFC 3339).
    pub until: Option<DateTime<Utc>>,
    /// Sort by creation time; default `desc` (newest first).
    pub order: Option<Order>,
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<i64>,
    /// `next_cursor` of the previous page (same filters and order).
    pub cursor: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct MessagePage {
    pub items: Vec<MessageView>,
    /// Pass as `cursor` to get the next page; `null` on the last page.
    pub next_cursor: Option<String>,
}

/// List the calling app's messages (sent, and inbound routed to it).
#[utoipa::path(
    get,
    path = "/v1/messages",
    tag = "messages",
    params(ListParams),
    responses(
        (status = 200, body = MessagePage),
        (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody),
    ),
    security(("api_key" = [])),
)]
pub async fn list(
    State(state): State<AppState>,
    AuthApp(app): AuthApp,
    Query(params): Query<ListParams>,
) -> Result<Json<MessagePage>, ApiError> {
    let limit = params.limit.unwrap_or(50);
    if !(1..=500).contains(&limit) {
        return Err(ApiError::bad_request("limit must be between 1 and 500"));
    }
    let cursor = params
        .cursor
        .as_deref()
        .map(|c| Cursor::decode(c).ok_or_else(|| ApiError::bad_request("invalid cursor")))
        .transpose()?;
    let filter = ListFilter {
        direction: params.direction,
        status: params.status,
        since: params.since,
        until: params.until,
        order: params.order.unwrap_or_default(),
        cursor,
        limit,
    };
    let (rows, next_cursor) = messages::list_for_app(&state.pool, app.id, &filter).await?;
    Ok(Json(MessagePage {
        items: rows.iter().map(MessageView::from).collect(),
        next_cursor,
    }))
}
