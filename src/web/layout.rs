//! Per-request page context for the base layout, and the extractors handlers
//! use: `AuthUser`, `Page` (logged-in page), `AnonPage` (login pages), `ClientIp`.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use tower_sessions::Session;

use super::flash::{self, Flash};
use super::security::csrf_token;
use super::{AppError, AppState};
use crate::auth::users::Role;
use crate::auth::{session_keys, users};
use crate::modules::companies::{self, CompanyOption};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where the source of this version can be found (AGPL section 13).
pub const SOURCE: &str = env!("CARGO_PKG_REPOSITORY");

/// Everything `base.html` needs.
#[derive(Debug, Clone)]
pub struct Layout {
    pub version: &'static str,
    pub csrf: String,
    pub flashes: Vec<Flash>,
    pub user_name: String,
    pub logged_in: bool,
    pub companies: Vec<CompanyOption>,
    /// Selected company id, 0 when there is none yet.
    pub company_id: i64,
    pub company_name: String,
    /// Current path, so the company switcher can return here.
    pub path: String,
    pub role: Role,
}

impl Layout {
    /// True on `prefix` itself or any page below it.
    pub fn under(&self, prefix: &str) -> bool {
        let p = self.path.as_str();
        p == prefix || p.starts_with(&format!("{prefix}/"))
    }

    /// The navigation section the current page belongs to, for highlighting.
    pub fn section(&self) -> &'static str {
        const SETTINGS: &[&str] = &[
            "/instellingen",
            "/bedrijven",
            "/taken",
            "/tarieven",
            "/voertuigen",
            "/routes",
            "/kilometervergoeding",
            "/export",
            "/logboek",
            "/gebruikers",
            "/beveiliging",
        ];
        let sections: &[(&str, &'static str)] = &[
            ("/uren", "uren"),
            ("/kilometers", "kilometers"),
            ("/ritten", "kilometers"),
            ("/klanten", "klanten"),
            ("/projecten", "klanten"),
            ("/te-factureren", "te-factureren"),
            ("/facturen", "facturen"),
            ("/offertes", "offertes"),
            ("/terugkerend", "terugkerend"),
            ("/documenten", "documenten"),
            ("/btw", "btw"),
        ];
        if self.path == "/" {
            return "overzicht";
        }
        if SETTINGS.iter().any(|p| self.under(p)) {
            return "instellingen";
        }
        sections
            .iter()
            .find(|(p, _)| self.under(p))
            .map(|(_, s)| *s)
            .unwrap_or("")
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
    pub fn source(&self) -> &'static str {
        SOURCE
    }
    /// `false` for a read-only boekhouder: hides add/edit/delete controls.
    /// The server refuses such requests anyway (`security::read_only`).
    pub fn can_edit(&self) -> bool {
        self.logged_in && self.role != Role::Accountant
    }

    /// First letter(s) of the user's name for the avatar.
    pub fn initials(&self) -> String {
        self.user_name
            .split_whitespace()
            .filter(|w| w.chars().next().is_some_and(char::is_alphabetic))
            .take(2)
            .filter_map(|w| w.chars().next())
            .flat_map(char::to_uppercase)
            .collect()
    }
}

/// The logged-in user (password and TOTP both verified).
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: Role,
    /// A temporary password set by a beheerder must be changed first.
    pub must_change_password: bool,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
    pub fn is_accountant(&self) -> bool {
        self.role == Role::Accountant
    }
}

async fn session_of(parts: &mut Parts, state: &AppState) -> Result<Session, AppError> {
    Session::from_request_parts(parts, state)
        .await
        .map_err(|(_, msg)| AppError::Internal(anyhow::anyhow!("session layer: {msg}")))
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        if let Some(u) = parts.extensions.get::<AuthUser>() {
            return Ok(u.clone());
        }
        let session = session_of(parts, state).await?;
        let Some(id) = session.get::<i64>(session_keys::USER_ID).await? else {
            return Err(AppError::LoginRequired);
        };
        let login_at = session.get::<u64>(session_keys::LOGIN_AT).await?;
        if crate::auth::login_expired(login_at, crate::auth::totp::unix_now()) {
            let client = crate::security_log::ClientInfo::from_request_parts(parts, state)
                .await
                .unwrap_or_default();
            crate::security_log::record(
                &state.pool,
                crate::security_log::Event::SessionExpired,
                Some(id),
                "",
                &client,
                serde_json::json!({}),
            )
            .await;
            session.flush().await?;
            flash::error(&session, "Je sessie is verlopen. Log opnieuw in.").await?;
            return Err(AppError::LoginRequired);
        }
        match users::find_by_id(&state.pool, id).await? {
            Some(u) if u.has_totp() => {
                let user = AuthUser {
                    id: u.id,
                    role: u.role(),
                    must_change_password: u.must_change_password,
                    name: u.name,
                    email: u.email,
                };
                parts.extensions.insert(user.clone());
                Ok(user)
            }
            _ => {
                // Account archived or 2FA reset: end the session.
                session.flush().await?;
                Err(AppError::LoginRequired)
            }
        }
    }
}

/// Layout for a logged-in page.
pub struct Page(pub Layout);

impl FromRequestParts<AppState> for Page {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        let session = session_of(parts, state).await?;
        let csrf = csrf_token(&session).await?;
        let flashes = flash::take(&session).await?;
        let list = companies::visible(&state.pool, &user).await?;
        let current = companies::resolve_current(&session, &list).await?;
        Ok(Page(Layout {
            version: VERSION,
            csrf,
            flashes,
            user_name: user.name,
            logged_in: true,
            company_id: current.as_ref().map(|c| c.id).unwrap_or(0),
            company_name: current.map(|c| c.name).unwrap_or_default(),
            companies: list,
            path: parts.uri.path().to_string(),
            role: user.role,
        }))
    }
}

/// Layout for pages shown before login.
pub struct AnonPage(pub Layout);

impl FromRequestParts<AppState> for AnonPage {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let session = session_of(parts, state).await?;
        let csrf = csrf_token(&session).await?;
        let flashes = flash::take(&session).await?;
        Ok(AnonPage(Layout {
            version: VERSION,
            csrf,
            flashes,
            user_name: String::new(),
            logged_in: false,
            companies: Vec::new(),
            company_id: 0,
            company_name: String::new(),
            path: parts.uri.path().to_string(),
            role: Role::default(),
        }))
    }
}

/// Client IP for rate limiting.
pub struct ClientIp(pub String);

/// The client address: the direct peer, unless the peer is a trusted proxy
/// (loopback or listed in `trusted_proxies`). Then `X-Forwarded-For` is read
/// from right to left and the first address that is not itself a trusted
/// proxy is the client. Addresses to the left of it can be set by anyone and
/// are ignored.
pub fn client_ip(peer: Option<IpAddr>, forwarded_for: Option<&str>, trusted: &[IpAddr]) -> String {
    let is_trusted = |ip: &IpAddr| ip.is_loopback() || trusted.contains(ip);
    match peer {
        Some(p) if is_trusted(&p) => {
            let hops: Vec<IpAddr> = forwarded_for
                .unwrap_or("")
                .split(',')
                .filter_map(|h| h.trim().parse().ok())
                .collect();
            hops.iter()
                .rev()
                .find(|ip| !is_trusted(ip))
                .or(hops.first())
                .copied()
                .unwrap_or(p)
                .to_string()
        }
        Some(p) => p.to_string(),
        None => "onbekend".to_string(),
    }
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer: Option<IpAddr> = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        let forwarded = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        Ok(ClientIp(client_ip(
            peer,
            forwarded,
            &state.cfg.trusted_proxies,
        )))
    }
}

/// Only allow redirects to local paths (`/...`, not `//host`).
pub fn safe_local_path(p: &str) -> &str {
    if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') {
        p
    } else {
        "/"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn client_ip_trusts_only_known_proxies() {
        let none: &[IpAddr] = &[];
        // direct connection: the peer, whatever the header says
        assert_eq!(
            client_ip(Some(ip("100.64.0.7")), Some("1.2.3.4"), none),
            "100.64.0.7"
        );
        // tailscale serve on the same host
        assert_eq!(
            client_ip(Some(ip("127.0.0.1")), Some("100.64.0.7"), none),
            "100.64.0.7"
        );
        // a proxy on another host, not configured: its own address
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), Some("203.0.113.9"), none),
            "10.0.0.5"
        );
        // configured: the client it reports, spoofed entries on the left ignored
        let t = [ip("10.0.0.5")];
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), Some("6.6.6.6, 203.0.113.9"), &t),
            "203.0.113.9"
        );
        // two proxies in a row (SSO gate → reverse proxy)
        let t2 = [ip("10.0.0.5"), ip("10.0.0.6")];
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), Some("203.0.113.9, 10.0.0.6"), &t2),
            "203.0.113.9"
        );
        // header missing or garbage: the proxy itself
        assert_eq!(client_ip(Some(ip("10.0.0.5")), None, &t), "10.0.0.5");
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), Some("onzin"), &t),
            "10.0.0.5"
        );
        assert_eq!(client_ip(None, Some("1.2.3.4"), none), "onbekend");
    }
}
