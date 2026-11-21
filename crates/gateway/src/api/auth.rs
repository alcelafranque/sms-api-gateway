//! Bearer API key authentication.

use axum::extract::FromRequestParts;
use axum::http::header;
use axum::http::request::Parts;

use super::AppState;
use super::error::ApiError;
use crate::apps::{self, App};

/// The calling app, authenticated by `Authorization: Bearer <api key>`.
pub struct AuthApp(pub App);

/// An authenticated app holding the `admin` flag.
pub struct AdminApp(pub App);

impl FromRequestParts<AppState> for AuthApp {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or_else(ApiError::unauthorized)?;
        let prefix = apps::key_prefix(key).ok_or_else(ApiError::unauthorized)?;
        let app = apps::find_by_prefix(&state.pool, prefix)
            .await?
            .filter(|app| apps::key_matches(app, key))
            .ok_or_else(ApiError::unauthorized)?;
        if !app.enabled {
            return Err(ApiError::forbidden("app disabled"));
        }
        tracing::Span::current().record("app", app.name.as_str());
        Ok(AuthApp(app))
    }
}

impl FromRequestParts<AppState> for AdminApp {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let AuthApp(app) = AuthApp::from_request_parts(parts, state).await?;
        if !app.admin {
            return Err(ApiError::forbidden("admin privileges required"));
        }
        Ok(AdminApp(app))
    }
}
