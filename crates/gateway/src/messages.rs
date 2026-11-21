//! Message storage and API representation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, QueryBuilder};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::sms_text::{Encoding, SmsSize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outbound,
    Inbound,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Outbound => "outbound",
            Direction::Inbound => "inbound",
        }
    }
}

impl TryFrom<String> for Direction {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "outbound" => Ok(Direction::Outbound),
            "inbound" => Ok(Direction::Inbound),
            _ => Err(format!("unknown direction {value:?}")),
        }
    }
}

/// Message status.
///
/// Outbound: `queued` → `sending` → `sent` (device confirmed the message
/// left) | `submitted` (device accepted it, no confirmation available) |
/// `failed` (refused, or retries exhausted) | `unknown` (outcome cannot be
/// known; the gateway never re-sends such a message). Inbound: `received`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Sending,
    Sent,
    Submitted,
    Failed,
    Unknown,
    Received,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Sending => "sending",
            Status::Sent => "sent",
            Status::Submitted => "submitted",
            Status::Failed => "failed",
            Status::Unknown => "unknown",
            Status::Received => "received",
        }
    }
}

impl TryFrom<String> for Status {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(match value.as_str() {
            "queued" => Status::Queued,
            "sending" => Status::Sending,
            "sent" => Status::Sent,
            "submitted" => Status::Submitted,
            "failed" => Status::Failed,
            "unknown" => Status::Unknown,
            "received" => Status::Received,
            _ => return Err(format!("unknown status {value:?}")),
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Message {
    pub id: Uuid,
    pub app_id: Option<Uuid>,
    #[sqlx(try_from = "String")]
    pub direction: Direction,
    #[sqlx(try_from = "String")]
    pub status: Status,
    pub peer: String,
    pub body: Option<String>,
    pub client_ref: Option<String>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub received_at: Option<DateTime<Utc>>,
    pub routed_by: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub body_purged_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const MESSAGE_COLUMNS: &str = "id, app_id, direction, status, peer, body, client_ref, \
    attempts, last_error, next_attempt_at, received_at, routed_by, sent_at, body_purged_at, \
    created_at, updated_at";

/// A message as exposed by the API and in webhook payloads.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MessageView {
    pub id: Uuid,
    pub direction: Direction,
    pub status: Status,
    /// Recipient (outbound only), E.164.
    pub to: Option<String>,
    /// Sender (inbound only), E.164 when the device reports a number.
    pub from: Option<String>,
    /// Message text; `null` once purged by the retention policy.
    pub text: Option<String>,
    /// When the text was purged by the retention policy.
    pub text_purged_at: Option<DateTime<Utc>>,
    /// SMS encoding of the text (outbound, while the text is kept).
    pub encoding: Option<Encoding>,
    /// SMS segments the text is sent in (outbound, while the text is kept).
    pub segments: Option<u32>,
    pub client_ref: Option<String>,
    /// Send attempts made so far (outbound).
    pub attempts: i32,
    /// Last error (outbound).
    pub error: Option<String>,
    /// Next attempt time while a retry is scheduled (outbound).
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// When the device sent (or accepted) the message (outbound).
    pub sent_at: Option<DateTime<Utc>>,
    /// Reception time reported by the device, converted to UTC (inbound).
    pub received_at: Option<DateTime<Utc>>,
    /// Inbound routing rule that selected the app: `keyword`, `reply`,
    /// `default` or `none`.
    pub routed_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&Message> for MessageView {
    fn from(m: &Message) -> Self {
        let (to, from) = match m.direction {
            Direction::Outbound => (Some(m.peer.clone()), None),
            Direction::Inbound => (None, Some(m.peer.clone())),
        };
        let size = match m.direction {
            Direction::Outbound => m.body.as_deref().map(SmsSize::of),
            Direction::Inbound => None,
        };
        MessageView {
            id: m.id,
            direction: m.direction,
            status: m.status,
            to,
            from,
            text: m.body.clone(),
            text_purged_at: m.body_purged_at,
            encoding: size.map(|s| s.encoding),
            segments: size.map(|s| s.segments),
            client_ref: m.client_ref.clone(),
            attempts: m.attempts,
            error: m.last_error.clone(),
            next_attempt_at: (m.status == Status::Queued)
                .then_some(m.next_attempt_at)
                .flatten(),
            sent_at: m.sent_at,
            received_at: m.received_at,
            routed_by: m.routed_by.clone(),
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

pub fn sha256(data: &str) -> Vec<u8> {
    Sha256::digest(data.as_bytes()).to_vec()
}

pub struct NewOutbound<'a> {
    pub app_id: Uuid,
    pub to: &'a str,
    pub text: &'a str,
    pub client_ref: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
}

pub enum InsertOutcome {
    Created(Message),
    /// A message already exists for this idempotency key; `same_request`
    /// tells whether it was created with the same payload.
    Existing {
        message: Message,
        same_request: bool,
    },
}

pub async fn insert_outbound(pool: &PgPool, new: NewOutbound<'_>) -> sqlx::Result<InsertOutcome> {
    let request_hash = sha256(&serde_json::json!([new.to, new.text, new.client_ref]).to_string());
    let created: Option<Message> = sqlx::query_as(&format!(
        "INSERT INTO messages (id, app_id, direction, status, peer, body, body_sha256, client_ref,
                               idempotency_key, request_hash, next_attempt_at)
         VALUES ($1, $2, 'outbound', 'queued', $3, $4, $5, $6, $7, $8, now())
         ON CONFLICT (app_id, idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING
         RETURNING {MESSAGE_COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(new.app_id)
    .bind(new.to)
    .bind(new.text)
    .bind(sha256(new.text))
    .bind(new.client_ref)
    .bind(new.idempotency_key)
    .bind(&request_hash)
    .fetch_optional(pool)
    .await?;
    if let Some(message) = created {
        return Ok(InsertOutcome::Created(message));
    }
    #[derive(sqlx::FromRow)]
    struct Existing {
        #[sqlx(flatten)]
        message: Message,
        request_hash: Option<Vec<u8>>,
    }
    let existing: Existing = sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS}, request_hash FROM messages
         WHERE app_id = $1 AND idempotency_key = $2"
    ))
    .bind(new.app_id)
    .bind(new.idempotency_key)
    .fetch_one(pool)
    .await?;
    Ok(InsertOutcome::Existing {
        same_request: existing.request_hash.as_deref() == Some(request_hash.as_slice()),
        message: existing.message,
    })
}

pub async fn get_for_app(pool: &PgPool, app_id: Uuid, id: Uuid) -> sqlx::Result<Option<Message>> {
    sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = $1 AND app_id = $2"
    ))
    .bind(id)
    .bind(app_id)
    .fetch_optional(pool)
    .await
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    Asc,
    #[default]
    Desc,
}

/// Keyset pagination position: `(created_at, id)` of the last item returned.
#[derive(Debug, Serialize, Deserialize)]
pub struct Cursor {
    t: DateTime<Utc>,
    id: Uuid,
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("serializable"))
    }

    pub fn decode(s: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

pub struct ListFilter {
    pub direction: Option<Direction>,
    pub status: Option<Status>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub order: Order,
    pub cursor: Option<Cursor>,
    pub limit: i64,
}

/// Lists the app's messages; returns the page and the next cursor.
pub async fn list_for_app(
    pool: &PgPool,
    app_id: Uuid,
    filter: &ListFilter,
) -> sqlx::Result<(Vec<Message>, Option<String>)> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE app_id = "
    ));
    qb.push_bind(app_id);
    if let Some(direction) = filter.direction {
        qb.push(" AND direction = ").push_bind(direction.as_str());
    }
    if let Some(status) = filter.status {
        qb.push(" AND status = ").push_bind(status.as_str());
    }
    if let Some(since) = filter.since {
        qb.push(" AND created_at >= ").push_bind(since);
    }
    if let Some(until) = filter.until {
        qb.push(" AND created_at < ").push_bind(until);
    }
    let (cmp, dir) = match filter.order {
        Order::Asc => (">", "ASC"),
        Order::Desc => ("<", "DESC"),
    };
    if let Some(cursor) = &filter.cursor {
        qb.push(format!(" AND (created_at, id) {cmp} ("))
            .push_bind(cursor.t)
            .push(", ")
            .push_bind(cursor.id)
            .push(")");
    }
    qb.push(format!(" ORDER BY created_at {dir}, id {dir} LIMIT "))
        .push_bind(filter.limit + 1);
    let mut rows: Vec<Message> = qb.build_query_as().fetch_all(pool).await?;
    let next = if rows.len() as i64 > filter.limit {
        rows.truncate(filter.limit as usize);
        rows.last().map(|m| {
            Cursor {
                t: m.created_at,
                id: m.id,
            }
            .encode()
        })
    } else {
        None
    };
    Ok((rows, next))
}

pub struct QueueDepth {
    pub queued: i64,
    pub sending: i64,
    pub webhooks_pending: i64,
}

pub async fn queue_depth(pool: &PgPool) -> sqlx::Result<QueueDepth> {
    let (queued, sending, webhooks_pending): (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM messages WHERE status = 'queued'),
            (SELECT count(*) FROM messages WHERE status = 'sending'),
            (SELECT count(*) FROM webhook_deliveries WHERE status = 'pending')",
    )
    .fetch_one(pool)
    .await?;
    Ok(QueueDepth {
        queued,
        sending,
        webhooks_pending,
    })
}
