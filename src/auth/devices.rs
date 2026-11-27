//! Trusted devices ("MFA 30 dagen onthouden").
//!
//! After a successful TOTP step the user may trust the device: the browser
//! gets a random token in a cookie, the database keeps only its SHA-256 hash
//! tied to that user. At the next login on that device the **password is
//! still required**; only the TOTP code is skipped while the token matches an
//! active, unexpired row of the same user.
//!
//! Trust ends after [`TRUST_DAYS`], when the user revokes it on "Mijn account",
//! and for all devices of a user when the password or TOTP is reset.

use anyhow::Result;
use axum::http::{header, HeaderMap, HeaderValue};
use rand::RngCore;
use serde_json::json;
use sqlx::{SqliteConnection, SqlitePool};
use time::OffsetDateTime;
use tower_sessions::cookie::{Cookie, SameSite};

use crate::audit::{self, Action};
use crate::db;
use crate::export::sha256_hex;

/// How long a device stays trusted.
pub const TRUST_DAYS: i64 = 30;
/// Name of the device cookie.
pub const COOKIE: &str = "kwartje_apparaat";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Device {
    pub id: i64,
    pub label: String,
    pub token_hash: String,
    pub created_at: String,
    pub expires_at: String,
    pub last_used_at: Option<String>,
    pub last_ip: Option<String>,
}

impl Device {
    pub fn created(&self) -> String {
        crate::format::datetime(&self.created_at)
    }
    pub fn expires(&self) -> String {
        crate::format::datetime(&self.expires_at)
    }
    /// "188.122.74.75 (internet)", or empty when never used.
    pub fn last_place(&self) -> String {
        match &self.last_ip {
            Some(ip) if !ip.is_empty() => {
                format!("{ip} ({})", crate::security_log::via(ip))
            }
            _ => String::new(),
        }
    }
    pub fn last_used(&self) -> String {
        self.last_used_at
            .as_deref()
            .map(crate::format::datetime)
            .unwrap_or_else(|| "nog niet".into())
    }
}

fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hash(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// The device token from the request's cookies, if any.
pub fn token_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(Cookie::split_parse)
        .filter_map(|c| c.ok())
        .find(|c| c.name() == COOKIE)
        .map(|c| c.value().to_string())
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `Set-Cookie` value for a new device token.
pub fn set_cookie(token: &str, secure: bool) -> HeaderValue {
    let c = Cookie::build((COOKIE, token.to_string()))
        .path("/")
        .http_only(true)
        .secure(secure)
        // The cookie is only read on the login POST, which is same-site.
        .same_site(SameSite::Strict)
        .max_age(time::Duration::days(TRUST_DAYS))
        .build();
    HeaderValue::from_str(&c.to_string()).expect("cookie is ASCII")
}

/// `Set-Cookie` value that removes the device cookie.
pub fn clear_cookie(secure: bool) -> HeaderValue {
    let c = Cookie::build((COOKIE, ""))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Strict)
        .max_age(time::Duration::ZERO)
        .build();
    HeaderValue::from_str(&c.to_string()).expect("cookie is ASCII")
}

/// Short Dutch description of a browser from its User-Agent ("Firefox op Windows").
pub fn label_from_user_agent(ua: &str) -> String {
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "Onbekende browser"
    };
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "onbekend systeem"
    };
    format!("{browser} op {os}")
}

/// Trust a device for `user_id`. Returns the token for the cookie.
pub async fn create(pool: &SqlitePool, user_id: i64, label: &str, ip: &str) -> Result<String> {
    let token = new_token();
    let token_hash = hash(&token);
    let now = OffsetDateTime::now_utc();
    let created_at = db::timestamp(now);
    let expires_at = db::timestamp(now + time::Duration::days(TRUST_DAYS));
    let mut tx = pool.begin().await?;
    let id = sqlx::query!(
        "INSERT INTO trusted_devices (user_id, token_hash, label, created_at, expires_at, last_ip)
         VALUES (?, ?, ?, ?, ?, ?)",
        user_id,
        token_hash,
        label,
        created_at,
        expires_at,
        ip
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    audit::record(
        &mut tx,
        Some(user_id),
        "trusted_devices",
        id,
        Action::Insert,
        &json!({
            "id": id, "user_id": user_id, "label": label,
            "created_at": created_at, "expires_at": expires_at,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(token)
}

/// A trusted device that was just used to skip the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Used {
    pub id: i64,
    /// The previous address when it differs from this one (logged; the
    /// device stays trusted: there is deliberately no IP binding).
    pub ip_changed_from: Option<String>,
}

/// The active device of `user_id` matching `token`, if any; marks it used
/// from `ip`.
pub async fn use_token(
    pool: &SqlitePool,
    user_id: i64,
    token: &str,
    ip: &str,
) -> Result<Option<Used>> {
    let token_hash = hash(token);
    let now = db::now();
    let row = sqlx::query!(
        r#"SELECT id AS "id!", last_ip FROM trusted_devices
           WHERE token_hash = ? AND user_id = ? AND revoked_at IS NULL AND expires_at > ?"#,
        token_hash,
        user_id,
        now
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    // Not audited: it changes at every login, like users.totp_last_step.
    sqlx::query!(
        "UPDATE trusted_devices SET last_used_at = ?, last_ip = ? WHERE id = ?",
        now,
        ip,
        row.id
    )
    .execute(pool)
    .await?;
    let ip_changed_from = row.last_ip.filter(|old| !old.is_empty() && old != ip);
    Ok(Some(Used {
        id: row.id,
        ip_changed_from,
    }))
}

/// Active (not revoked, not expired) devices of a user, newest first.
pub async fn active(pool: &SqlitePool, user_id: i64) -> Result<Vec<Device>> {
    let now = db::now();
    Ok(sqlx::query_as!(
        Device,
        r#"SELECT id AS "id!", label, token_hash, created_at, expires_at, last_used_at, last_ip
           FROM trusted_devices
           WHERE user_id = ? AND revoked_at IS NULL AND expires_at > ?
           ORDER BY created_at DESC, id DESC"#,
        user_id,
        now
    )
    .fetch_all(pool)
    .await?)
}

/// `true` when `token` belongs to device `d`.
pub fn is_token_of(d: &Device, token: Option<&str>) -> bool {
    token.is_some_and(|t| hash(t) == d.token_hash)
}

/// Revoke one device of `user_id`. Returns `false` when it is not theirs or
/// already inactive.
pub async fn revoke(pool: &SqlitePool, user_id: i64, id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let n = revoke_where(&mut tx, user_id, Some(id), Some(user_id)).await?;
    tx.commit().await?;
    Ok(n > 0)
}

/// Revoke every active device of `user_id` (password or TOTP reset, or
/// "Alle apparaten vergeten"). `by` is who did it (`None` = CLI).
pub async fn revoke_all(
    conn: &mut SqliteConnection,
    user_id: i64,
    by: Option<i64>,
) -> Result<usize> {
    revoke_where(conn, user_id, None, by).await
}

async fn revoke_where(
    conn: &mut SqliteConnection,
    user_id: i64,
    only: Option<i64>,
    by: Option<i64>,
) -> Result<usize> {
    let now = db::now();
    let ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM trusted_devices
           WHERE user_id = ? AND revoked_at IS NULL AND (? IS NULL OR id = ?)"#,
        user_id,
        only,
        only
    )
    .fetch_all(&mut *conn)
    .await?;
    for id in &ids {
        sqlx::query!(
            "UPDATE trusted_devices SET revoked_at = ? WHERE id = ?",
            now,
            id
        )
        .execute(&mut *conn)
        .await?;
        audit::record(
            conn,
            by,
            "trusted_devices",
            *id,
            Action::Archive,
            &json!({"revoked_at": [null, now]}),
        )
        .await?;
    }
    Ok(ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_common_browsers() {
        let firefox =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0";
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        let android = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";
        assert_eq!(label_from_user_agent(firefox), "Firefox op Windows");
        assert_eq!(label_from_user_agent(edge), "Edge op Windows");
        assert_eq!(label_from_user_agent(iphone), "Safari op iPhone");
        assert_eq!(label_from_user_agent(android), "Chrome op Android");
        assert_eq!(
            label_from_user_agent(""),
            "Onbekende browser op onbekend systeem"
        );
    }

    #[test]
    fn reads_only_wellformed_tokens() {
        let t = new_token();
        assert_eq!(t.len(), 64);
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("kwartje_sid=abc; {COOKIE}={t}")).unwrap(),
        );
        assert_eq!(token_from(&h).as_deref(), Some(t.as_str()));
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("kwartje_apparaat=../../etc"),
        );
        assert_eq!(token_from(&h), None);
    }
}
