//! Uniform JSON error bodies: `{"error": {"code": "...", "message": "..."}}`.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Stable machine-readable code (`validation_failed`, `not_found`...).
    pub code: String,
    /// Human-readable description.
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            message,
        )
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid API key",
        )
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "internal error",
        )
    }

    /// Generic error for a bare status produced by a middleware.
    pub fn from_status(status: StatusCode) -> Self {
        let code = match status {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            StatusCode::REQUEST_TIMEOUT => "timeout",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            StatusCode::SERVICE_UNAVAILABLE => "unavailable",
            s if s.is_server_error() => "internal_error",
            _ => "bad_request",
        };
        let message = status.canonical_reason().unwrap_or("error").to_lowercase();
        Self::new(status, code, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut resp = (
            self.status,
            axum::Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code.into(),
                    message: self.message,
                },
            }),
        )
            .into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        resp
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "database error");
        Self::internal()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "internal error");
        Self::internal()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        let status = e.status();
        let code = match status {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            StatusCode::UNPROCESSABLE_ENTITY => "validation_failed",
            _ => "bad_request",
        };
        Self::new(status, code, e.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(e: QueryRejection) -> Self {
        Self::bad_request(e.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(e: PathRejection) -> Self {
        Self::bad_request(e.body_text())
    }
}

/// `axum::Json` with JSON rejections.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Query` with JSON rejections.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct Query<T>(pub T);

/// `axum::extract::Path` with JSON rejections.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct Path<T>(pub T);

/// Turns bare error responses (404 fallback, 405, timeouts, body limit...)
/// into JSON error bodies.
pub async fn json_errors(resp: Response) -> Response {
    let status = resp.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    let is_json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if is_json {
        return resp;
    }
    let (mut parts, _) = resp.into_parts();
    let mut new = ApiError::from_status(status).into_response();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_TYPE);
    for (name, value) in new.headers() {
        parts.headers.insert(name.clone(), value.clone());
    }
    std::mem::swap(new.headers_mut(), &mut parts.headers);
    new
}
