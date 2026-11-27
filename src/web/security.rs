//! Security middleware: response headers and CSRF protection.

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use tower_sessions::Session;

use super::AppError;

pub const CSRF_KEY: &str = "csrf";
pub const CSRF_FIELD: &str = "csrf_token";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Largest urlencoded body the CSRF check will buffer.
const MAX_FORM_BODY: usize = 256 * 1024;

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; \
                   script-src 'self'; object-src 'none'; frame-ancestors 'none'; \
                   form-action 'self'; base-uri 'none'";

/// Add security headers to every response.
pub async fn headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let set = |h: &mut axum::http::HeaderMap, k: &'static str, v: &'static str| {
        if !h.contains_key(k) {
            h.insert(k, HeaderValue::from_static(v));
        }
    };
    set(h, "content-security-policy", CSP);
    set(h, "x-frame-options", "DENY");
    set(h, "x-content-type-options", "nosniff");
    set(h, "referrer-policy", "same-origin");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(
        h,
        "permissions-policy",
        "camera=(self), geolocation=(), microphone=()",
    );
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

/// The session's CSRF token, created on first use.
pub async fn csrf_token(session: &Session) -> Result<String, AppError> {
    if let Some(t) = session.get::<String>(CSRF_KEY).await? {
        return Ok(t);
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    session.insert(CSRF_KEY, &token).await?;
    Ok(token)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn origin_ok(req: &Request) -> bool {
    let h = req.headers();
    if h.get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "cross-site")
    {
        return false;
    }
    let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true; // no Origin header: rely on the token
    };
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .trim_end_matches('/');
    // Behind `tailscale serve` the Host header may be rewritten to the
    // upstream address; the original host is then in X-Forwarded-Host.
    [header::HOST.as_str(), "x-forwarded-host"]
        .iter()
        .filter_map(|k| h.get(*k).and_then(|v| v.to_str().ok()))
        .any(|host| origin_host.eq_ignore_ascii_case(host.trim()))
}

fn query_token(req: &Request) -> Option<String> {
    let q = req.uri().query()?;
    serde_urlencoded::from_str::<Vec<(String, String)>>(q)
        .ok()?
        .into_iter()
        .find(|(k, _)| k == CSRF_FIELD)
        .map(|(_, v)| v)
}

/// Reject state-changing requests without a valid CSRF token (form field
/// `csrf_token`, header `X-CSRF-Token`, or `?csrf_token=` for multipart
/// uploads) or with a foreign Origin.
pub async fn csrf(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    if !origin_ok(&req) {
        tracing::warn!("CSRF: origin mismatch on {}", req.uri().path());
        return AppError::Forbidden.into_response();
    }
    let Some(session) = req.extensions().get::<Session>().cloned() else {
        return AppError::Forbidden.into_response();
    };
    let expected = match session.get::<String>(CSRF_KEY).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!("CSRF: no token in session for {}", req.uri().path());
            return AppError::Forbidden.into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    };

    let mut provided = req
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| query_token(&req));

    let is_form = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));

    let req = if provided.is_none() && is_form {
        let (parts, body) = req.into_parts();
        let bytes = match to_bytes(body, MAX_FORM_BODY).await {
            Ok(b) => b,
            Err(_) => {
                return AppError::BadRequest("Het formulier is te groot.".into()).into_response()
            }
        };
        provided = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&bytes)
            .ok()
            .and_then(|pairs| {
                pairs
                    .into_iter()
                    .find(|(k, _)| k == CSRF_FIELD)
                    .map(|(_, v)| v)
            });
        Request::from_parts(parts, Body::from(bytes))
    } else {
        req
    };

    match provided {
        Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => next.run(req).await,
        _ => {
            tracing::warn!("CSRF: missing or wrong token on {}", req.uri().path());
            AppError::Forbidden.into_response()
        }
    }
}

/// Pages only a beheerder may open (404 for everyone else).
pub const ADMIN_ONLY: &[&str] = &["/gebruikers", "/beveiliging"];
/// Pages a boekhouder doesn't see: the log covers every company.
const HIDDEN_FOR_ACCOUNTANT: &[&str] = &["/logboek"];
/// What a read-only boekhouder may still post: signing out, switching
/// company, their own password and devices, and the login steps.
const ACCOUNTANT_MAY_POST: &[&str] = &[
    "/uitloggen",
    "/bedrijf/kiezen",
    "/account",
    "/inloggen",
    "/2fa",
];
/// Reachable while a temporary password still has to be changed.
const DURING_PASSWORD_CHANGE: &[&str] =
    &["/account/wachtwoord", "/uitloggen", "/static", "/gezond"];

fn under(path: &str, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// Role-based access, checked centrally for every request of a logged-in
/// user: admin-only pages, the read-only boekhouder (any write → 403) and
/// the forced change of a temporary password.
pub async fn access(
    axum::extract::State(st): axum::extract::State<super::AppState>,
    req: Request,
    next: Next,
) -> Response {
    use crate::auth::users::{self, Role};
    let Some(session) = req.extensions().get::<Session>().cloned() else {
        return next.run(req).await;
    };
    let id = match session.get::<i64>(crate::auth::session_keys::USER_ID).await {
        Ok(Some(id)) => id,
        _ => return next.run(req).await,
    };
    let user = match users::find_by_id(&st.pool, id).await {
        Ok(Some(u)) => u,
        // Archived or unknown: the AuthUser extractor ends the session.
        _ => return next.run(req).await,
    };
    let path = req.uri().path().to_string();
    let role = user.role();
    if under(&path, ADMIN_ONLY) && role != Role::Admin {
        return AppError::NotFound.into_response();
    }
    if role == Role::Accountant {
        if under(&path, HIDDEN_FOR_ACCOUNTANT) {
            return AppError::NotFound.into_response();
        }
        let read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
        if !read && !under(&path, ACCOUNTANT_MAY_POST) {
            tracing::warn!(user_id = id, path, "read-only account tried to write");
            let (mut parts, _) = req.into_parts();
            let client = <crate::security_log::ClientInfo as axum::extract::FromRequestParts<_>>::from_request_parts(&mut parts, &st)
                .await
                .unwrap_or_default();
            crate::security_log::record(
                &st.pool,
                crate::security_log::Event::WriteRefused,
                Some(id),
                &user.email,
                &client,
                serde_json::json!({"path": path}),
            )
            .await;
            return super::error::error_page(
                axum::http::StatusCode::FORBIDDEN,
                "Alleen lezen",
                "Je account heeft alleen leesrechten: gegevens wijzigen is niet mogelijk.".into(),
            );
        }
    }
    if user.must_change_password && !under(&path, DURING_PASSWORD_CHANGE) {
        return super::error::redirect("/account/wachtwoord");
    }
    next.run(req).await
}
