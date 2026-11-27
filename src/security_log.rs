//! Security log: sign-ins (successful and failed), sign-outs, MFA and
//! password changes, trusted devices and user management, with the IP, the
//! route the request came in by (Tailscale, internet via the reverse proxy,
//! or locally) and the browser.
//!
//! Rows are append-only (database trigger) and kept for [`RETENTION_DAYS`];
//! the hourly cleanup removes older ones. Writing an event never fails the
//! request: a problem is logged and the request goes on.

use std::net::IpAddr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::auth::devices::label_from_user_agent;
use crate::db;
use crate::web::layout::ClientIp;
use crate::web::AppState;

pub const RETENTION_DAYS: i64 = 365;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    LoginOk,
    LoginFailedPassword,
    LoginFailedCode,
    LoginLocked,
    Logout,
    SessionExpired,
    MfaEnrolled,
    MfaReset,
    PasswordChanged,
    PasswordReset,
    DeviceTrusted,
    DeviceRevoked,
    DeviceIpChanged,
    UserCreated,
    UserUpdated,
    RoleChanged,
    UserArchived,
    UserRestored,
    AccessChanged,
    WriteRefused,
}

impl Event {
    pub const ALL: [Event; 20] = [
        Event::LoginOk,
        Event::LoginFailedPassword,
        Event::LoginFailedCode,
        Event::LoginLocked,
        Event::Logout,
        Event::SessionExpired,
        Event::MfaEnrolled,
        Event::MfaReset,
        Event::PasswordChanged,
        Event::PasswordReset,
        Event::DeviceTrusted,
        Event::DeviceRevoked,
        Event::DeviceIpChanged,
        Event::UserCreated,
        Event::UserUpdated,
        Event::RoleChanged,
        Event::UserArchived,
        Event::UserRestored,
        Event::AccessChanged,
        Event::WriteRefused,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Event::LoginOk => "login_ok",
            Event::LoginFailedPassword => "login_failed_password",
            Event::LoginFailedCode => "login_failed_code",
            Event::LoginLocked => "login_locked",
            Event::Logout => "logout",
            Event::SessionExpired => "session_expired",
            Event::MfaEnrolled => "mfa_enrolled",
            Event::MfaReset => "mfa_reset",
            Event::PasswordChanged => "password_changed",
            Event::PasswordReset => "password_reset",
            Event::DeviceTrusted => "device_trusted",
            Event::DeviceRevoked => "device_revoked",
            Event::DeviceIpChanged => "device_ip_changed",
            Event::UserCreated => "user_created",
            Event::UserUpdated => "user_updated",
            Event::RoleChanged => "role_changed",
            Event::UserArchived => "user_archived",
            Event::UserRestored => "user_restored",
            Event::AccessChanged => "access_changed",
            Event::WriteRefused => "write_refused",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Event::LoginOk => "Ingelogd",
            Event::LoginFailedPassword => "Onjuist wachtwoord of onbekend e-mailadres",
            Event::LoginFailedCode => "Onjuiste verificatiecode",
            Event::LoginLocked => "Geblokkeerd na te veel pogingen",
            Event::Logout => "Uitgelogd",
            Event::SessionExpired => "Sessie verlopen",
            Event::MfaEnrolled => "Tweestapsverificatie ingesteld",
            Event::MfaReset => "Tweestapsverificatie gereset",
            Event::PasswordChanged => "Wachtwoord gewijzigd",
            Event::PasswordReset => "Wachtwoord gereset door beheerder",
            Event::DeviceTrusted => "Apparaat vertrouwd (MFA onthouden)",
            Event::DeviceRevoked => "Vertrouwd apparaat vergeten",
            Event::DeviceIpChanged => "Vertrouwd apparaat vanaf ander IP",
            Event::UserCreated => "Gebruiker aangemaakt",
            Event::UserUpdated => "Gebruiker gewijzigd",
            Event::RoleChanged => "Rol gewijzigd",
            Event::UserArchived => "Gebruiker gearchiveerd",
            Event::UserRestored => "Gebruiker hersteld",
            Event::AccessChanged => "Toegang boekhouder gewijzigd",
            Event::WriteRefused => "Wijziging geweigerd (alleen lezen)",
        }
    }

    /// Failed or suspicious: highlighted, and counted on the overview.
    pub fn is_warning(self) -> bool {
        matches!(
            self,
            Event::LoginFailedPassword
                | Event::LoginFailedCode
                | Event::LoginLocked
                | Event::DeviceIpChanged
                | Event::WriteRefused
        )
    }

    pub fn from_db(s: &str) -> Option<Event> {
        Event::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

/// Route a request came in by.
pub fn via(ip: &str) -> &'static str {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            if v4.is_loopback() {
                "lokaal"
            } else if o[0] == 100 && (64..=127).contains(&o[1]) {
                "Tailscale"
            } else {
                "internet"
            }
        }
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            if v6.is_loopback() {
                "lokaal"
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                "Tailscale"
            } else {
                "internet"
            }
        }
        Err(_) => "onbekend",
    }
}

/// Where a request came from, for the log.
#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub ip: String,
    pub via: String,
    /// Short browser/system label ("Firefox op Windows").
    pub user_agent: String,
}

impl ClientInfo {
    pub fn new(ip: &str, user_agent: &str) -> Self {
        ClientInfo {
            ip: ip.to_string(),
            via: via(ip).to_string(),
            user_agent: label_from_user_agent(user_agent),
        }
    }
    /// An action from the command line on the server.
    pub fn cli() -> Self {
        ClientInfo {
            ip: String::new(),
            via: "cli".into(),
            user_agent: "Opdrachtregel".into(),
        }
    }
}

impl FromRequestParts<AppState> for ClientInfo {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ClientIp(ip) = ClientIp::from_request_parts(parts, state).await?;
        let ua = parts
            .headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        Ok(ClientInfo::new(&ip, ua))
    }
}

/// Write one event. Never fails the caller.
pub async fn record(
    pool: &SqlitePool,
    event: Event,
    user_id: Option<i64>,
    email: &str,
    client: &ClientInfo,
    detail: Value,
) {
    let at = db::now();
    let ev = event.as_str();
    let detail = detail.to_string();
    let email = email.trim().to_lowercase();
    let r = sqlx::query!(
        "INSERT INTO security_events (at, event, user_id, email, ip, via, user_agent, detail_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        at,
        ev,
        user_id,
        email,
        client.ip,
        client.via,
        client.user_agent,
        detail
    )
    .execute(pool)
    .await;
    if let Err(e) = r {
        tracing::error!(error = %e, event = ev, "security log write failed");
    }
}

/// Remove events older than the retention. Returns the number removed.
pub async fn cleanup(pool: &SqlitePool) -> anyhow::Result<u64> {
    let cutoff = db::timestamp(
        time::OffsetDateTime::now_utc()
            - time::Duration::days(RETENTION_DAYS)
            - time::Duration::hours(1),
    );
    Ok(
        sqlx::query!("DELETE FROM security_events WHERE at < ?", cutoff)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub id: i64,
    pub at: String,
    pub event: String,
    pub user_id: Option<i64>,
    pub user_name: Option<String>,
    pub email: String,
    pub ip: String,
    pub via: String,
    pub user_agent: String,
    pub detail_json: String,
}

impl Row {
    pub fn when(&self) -> String {
        crate::format::datetime(&self.at)
    }
    pub fn kind(&self) -> Option<Event> {
        Event::from_db(&self.event)
    }
    pub fn label(&self) -> String {
        self.kind()
            .map(|e| e.label().to_string())
            .unwrap_or_else(|| self.event.clone())
    }
    pub fn is_warning(&self) -> bool {
        self.kind().is_some_and(Event::is_warning)
    }
    pub fn who(&self) -> String {
        match (&self.user_name, self.email.is_empty()) {
            (Some(n), true) => n.clone(),
            (Some(n), false) => format!("{n} ({})", self.email),
            (None, false) => self.email.clone(),
            (None, true) => "—".into(),
        }
    }
    /// The detail as short Dutch text ("met vertrouwd apparaat", "IP 1.2.3.4 → 5.6.7.8").
    pub fn detail(&self) -> String {
        let v: Value = serde_json::from_str(&self.detail_json).unwrap_or(Value::Null);
        let Some(obj) = v.as_object() else {
            return String::new();
        };
        let mut parts = Vec::new();
        for (k, v) in obj {
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Null => continue,
                other => other.to_string(),
            };
            let key = match k.as_str() {
                "method" => "",
                "old_ip" => "vorig IP ",
                "by" => "door ",
                "target" => "gebruiker ",
                "role" => "rol ",
                "path" => "pagina ",
                _ => "",
            };
            parts.push(format!("{key}{text}"));
        }
        parts.join(" · ")
    }
}

/// Filters of the log page.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub user_id: Option<i64>,
    pub event: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub only_warnings: bool,
}

/// Newest first, at most `limit` rows.
pub async fn query(pool: &SqlitePool, f: &Filter, limit: i64) -> anyhow::Result<Vec<Row>> {
    let warnings: Vec<&str> = Event::ALL
        .into_iter()
        .filter(|e| e.is_warning())
        .map(Event::as_str)
        .collect();
    let warn_json = serde_json::to_string(&warnings)?;
    let from = f.from.clone().map(|d| format!("{d}T00:00:00Z"));
    let to = f.to.clone().map(|d| format!("{d}T23:59:59Z"));
    let only = f.only_warnings;
    Ok(sqlx::query_as!(
        Row,
        r#"SELECT s.id AS "id!", s.at, s.event, s.user_id, u.name AS "user_name?", s.email, s.ip,
                  s.via, s.user_agent, s.detail_json
           FROM security_events s LEFT JOIN users u ON u.id = s.user_id
           WHERE (? IS NULL OR s.user_id = ?)
             AND (? IS NULL OR s.event = ?)
             AND (? IS NULL OR s.at >= ?)
             AND (? IS NULL OR s.at <= ?)
             AND (NOT ? OR s.event IN (SELECT value FROM json_each(?)))
           ORDER BY s.id DESC LIMIT ?"#,
        f.user_id,
        f.user_id,
        f.event,
        f.event,
        from,
        from,
        to,
        to,
        only,
        warn_json,
        limit
    )
    .fetch_all(pool)
    .await?)
}

/// Overview numbers for the last 24 hours.
pub struct Summary {
    pub failed_24h: i64,
    pub ips_24h: i64,
    pub logins_24h: i64,
}

pub async fn summary(pool: &SqlitePool) -> anyhow::Result<Summary> {
    let since = db::timestamp(time::OffsetDateTime::now_utc() - time::Duration::hours(24));
    let r = sqlx::query!(
        r#"SELECT
             COALESCE(SUM(event IN ('login_failed_password', 'login_failed_code', 'login_locked')), 0) AS "failed!: i64",
             COUNT(DISTINCT CASE WHEN ip != '' THEN ip END) AS "ips!: i64",
             COALESCE(SUM(event = 'login_ok'), 0) AS "logins!: i64"
           FROM security_events WHERE at >= ?"#,
        since
    )
    .fetch_one(pool)
    .await?;
    Ok(Summary {
        failed_24h: r.failed,
        ips_24h: r.ips,
        logins_24h: r.logins,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_from_the_address() {
        assert_eq!(via("100.64.0.1"), "Tailscale");
        assert_eq!(via("100.127.255.254"), "Tailscale");
        assert_eq!(via("100.128.0.1"), "internet");
        assert_eq!(via("fd7a:115c:a1e0::1"), "Tailscale");
        assert_eq!(via("188.122.74.75"), "internet");
        assert_eq!(via("2a02:a46f::1"), "internet");
        assert_eq!(via("127.0.0.1"), "lokaal");
        assert_eq!(via("::1"), "lokaal");
        assert_eq!(via("onbekend"), "onbekend");
    }

    #[test]
    fn every_event_round_trips() {
        for e in Event::ALL {
            assert_eq!(Event::from_db(e.as_str()), Some(e));
            assert!(!e.label().is_empty());
        }
    }
}
