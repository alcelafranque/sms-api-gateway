//! Client applications and their API keys.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use uuid::Uuid;

const KEY_TAG: &str = "sgw_";
const PREFIX_LEN: usize = 12;

#[derive(Clone, sqlx::FromRow)]
pub struct App {
    pub id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub key_hash: Vec<u8>,
    pub admin: bool,
    pub enabled: bool,
    pub webhook_url: Option<String>,
    pub webhook_secret: Option<String>,
    pub inbound_keywords: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// A freshly generated API key, shown once.
pub struct NewKey {
    pub key: String,
    pub prefix: String,
    pub hash: Vec<u8>,
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// `sgw_<12 hex chars, public lookup id>_<256-bit secret, base64url>`.
pub fn generate_key() -> NewKey {
    let prefix = hex::encode(random_bytes::<{ PREFIX_LEN / 2 }>());
    let secret = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
    let key = format!("{KEY_TAG}{prefix}_{secret}");
    let hash = Sha256::digest(key.as_bytes()).to_vec();
    NewKey { key, prefix, hash }
}

pub fn generate_webhook_secret() -> String {
    format!("whsec_{}", hex::encode(random_bytes::<32>()))
}

/// Extracts the lookup prefix of a presented key.
pub fn key_prefix(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(KEY_TAG)?;
    let prefix = rest.get(..PREFIX_LEN)?;
    (rest.as_bytes().get(PREFIX_LEN) == Some(&b'_')).then_some(prefix)
}

pub fn key_matches(app: &App, key: &str) -> bool {
    let hash = Sha256::digest(key.as_bytes());
    hash.as_slice().ct_eq(&app.key_hash).into()
}

pub fn normalize_keywords(keywords: &[String]) -> Vec<String> {
    let mut out: Vec<String> = keywords
        .iter()
        .flat_map(|k| k.split(','))
        .map(|k| k.trim().to_uppercase())
        .filter(|k| !k.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

const APP_COLUMNS: &str = "id, name, key_prefix, key_hash, admin, enabled, webhook_url, \
                           webhook_secret, inbound_keywords, created_at";

pub async fn find_by_prefix(pool: &PgPool, prefix: &str) -> sqlx::Result<Option<App>> {
    sqlx::query_as(&format!(
        "SELECT {APP_COLUMNS} FROM apps WHERE key_prefix = $1"
    ))
    .bind(prefix)
    .fetch_optional(pool)
    .await
}

pub async fn find_by_name(pool: &PgPool, name: &str) -> sqlx::Result<Option<App>> {
    sqlx::query_as(&format!("SELECT {APP_COLUMNS} FROM apps WHERE name = $1"))
        .bind(name)
        .fetch_optional(pool)
        .await
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<App>> {
    sqlx::query_as(&format!("SELECT {APP_COLUMNS} FROM apps ORDER BY name"))
        .fetch_all(pool)
        .await
}

/// Enabled apps owning at least one inbound keyword.
pub async fn with_keywords(pool: &PgPool) -> sqlx::Result<Vec<App>> {
    sqlx::query_as(&format!(
        "SELECT {APP_COLUMNS} FROM apps WHERE enabled AND cardinality(inbound_keywords) > 0"
    ))
    .fetch_all(pool)
    .await
}

pub struct CreateApp {
    pub name: String,
    pub admin: bool,
    pub webhook_url: Option<String>,
    pub keywords: Vec<String>,
}

/// Creates an app; returns it with its API key and webhook secret in clear.
pub async fn create(pool: &PgPool, req: CreateApp) -> anyhow::Result<(App, String)> {
    let key = generate_key();
    let secret = req.webhook_url.as_ref().map(|_| generate_webhook_secret());
    let app = sqlx::query_as(&format!(
        "INSERT INTO apps (id, name, key_prefix, key_hash, admin, webhook_url, webhook_secret,
                           inbound_keywords)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING {APP_COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(&req.name)
    .bind(&key.prefix)
    .bind(&key.hash)
    .bind(req.admin)
    .bind(&req.webhook_url)
    .bind(&secret)
    .bind(normalize_keywords(&req.keywords))
    .fetch_one(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            anyhow::anyhow!("an app named {:?} already exists", req.name)
        }
        _ => e.into(),
    })?;
    Ok((app, key.key))
}

pub async fn set_enabled(pool: &PgPool, name: &str, enabled: bool) -> sqlx::Result<bool> {
    let res = sqlx::query("UPDATE apps SET enabled = $2, updated_at = now() WHERE name = $1")
        .bind(name)
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn rotate_key(pool: &PgPool, name: &str) -> sqlx::Result<Option<String>> {
    let key = generate_key();
    let res = sqlx::query(
        "UPDATE apps SET key_prefix = $2, key_hash = $3, updated_at = now() WHERE name = $1",
    )
    .bind(name)
    .bind(&key.prefix)
    .bind(&key.hash)
    .execute(pool)
    .await?;
    Ok((res.rows_affected() == 1).then_some(key.key))
}

#[derive(Default)]
pub struct UpdateApp {
    pub admin: Option<bool>,
    /// `Some(None)` removes the webhook.
    pub webhook_url: Option<Option<String>>,
    pub rotate_webhook_secret: bool,
    pub keywords: Option<Vec<String>>,
}

/// Applies `req`; returns the updated app and the new webhook secret when
/// one was generated.
pub async fn update(
    pool: &PgPool,
    name: &str,
    req: UpdateApp,
) -> anyhow::Result<Option<(App, Option<String>)>> {
    let mut tx = pool.begin().await?;
    let Some(app): Option<App> = sqlx::query_as(&format!(
        "SELECT {APP_COLUMNS} FROM apps WHERE name = $1 FOR UPDATE"
    ))
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let webhook_url = req.webhook_url.unwrap_or(app.webhook_url.clone());
    let mut new_secret = None;
    let webhook_secret = match &webhook_url {
        None => None,
        Some(_) if req.rotate_webhook_secret || app.webhook_secret.is_none() => {
            new_secret = Some(generate_webhook_secret());
            new_secret.clone()
        }
        Some(_) => app.webhook_secret.clone(),
    };
    let keywords = req
        .keywords
        .map(|k| normalize_keywords(&k))
        .unwrap_or(app.inbound_keywords.clone());
    let app = sqlx::query_as(&format!(
        "UPDATE apps SET admin = $2, webhook_url = $3, webhook_secret = $4,
                         inbound_keywords = $5, updated_at = now()
         WHERE id = $1 RETURNING {APP_COLUMNS}"
    ))
    .bind(app.id)
    .bind(req.admin.unwrap_or(app.admin))
    .bind(&webhook_url)
    .bind(&webhook_secret)
    .bind(&keywords)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some((app, new_secret)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(key: &NewKey) -> App {
        App {
            id: Uuid::now_v7(),
            name: "test".into(),
            key_prefix: key.prefix.clone(),
            key_hash: key.hash.clone(),
            admin: false,
            enabled: true,
            webhook_url: None,
            webhook_secret: None,
            inbound_keywords: Vec::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn generated_keys_are_found_by_prefix_and_verified_by_hash() {
        let key = generate_key();
        assert_eq!(key_prefix(&key.key), Some(key.prefix.as_str()));
        let app = app_with(&key);
        assert!(key_matches(&app, &key.key));
        // Same prefix, different secret.
        let forged = format!("{}x", &key.key[..key.key.len() - 1]);
        assert!(!key_matches(&app, &forged));
        assert!(!key_matches(&app, &generate_key().key));
    }

    #[test]
    fn malformed_keys_have_no_prefix() {
        for key in [
            "",
            "sgw_",
            "sgw_0123456789a",
            "sgw_0123456789abXsecret",
            "xyz_0123456789ab_s",
        ] {
            assert_eq!(key_prefix(key), None, "{key}");
        }
    }

    #[test]
    fn keywords_are_split_uppercased_and_deduplicated() {
        let keywords = normalize_keywords(&["rdv, Agenda".into(), "RDV".into(), " ".into()]);
        assert_eq!(keywords, ["AGENDA", "RDV"]);
    }
}
