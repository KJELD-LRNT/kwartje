//! "Mijn account": the current session and the trusted devices of the
//! logged-in user, with revoking.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_sessions::Session;

use crate::auth::devices::{self, Device};
use crate::auth::session_keys::{LOGIN_AT, REMEMBER_UNTIL};
use crate::security_log::{self, ClientInfo, Event};
use crate::web::error::redirect;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/account", get(page))
        .route(
            "/account/wachtwoord",
            get(password_page).post(password_save),
        )
        .route("/account/apparaten/{id}/intrekken", post(revoke))
        .route("/account/apparaten/intrekken", post(revoke_all))
}

#[derive(Template, WebTemplate)]
#[template(path = "account.html")]
struct AccountPage {
    l: Layout,
    email: String,
    logged_in_at: String,
    /// `Some(end)` when "Ingelogd blijven" was ticked.
    remember_until: Option<String>,
    devices: Vec<Device>,
    /// Id of the device this browser is trusted as, if any.
    current: Option<i64>,
}

impl AccountPage {
    fn is_current(&self, id: &i64) -> bool {
        self.current == Some(*id)
    }
}

fn unix_to_text(t: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(t as i64)
        .map(|t| crate::format::datetime(&crate::db::timestamp(t)))
        .unwrap_or_default()
}

async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    headers: HeaderMap,
) -> AppResult<AccountPage> {
    let devices = devices::active(&st.pool, user.id).await?;
    let token = devices::token_from(&headers);
    let current = devices
        .iter()
        .find(|d| devices::is_token_of(d, token.as_deref()))
        .map(|d| d.id);
    Ok(AccountPage {
        l,
        email: user.email,
        logged_in_at: session
            .get::<u64>(LOGIN_AT)
            .await?
            .map(unix_to_text)
            .unwrap_or_default(),
        remember_until: session.get::<u64>(REMEMBER_UNTIL).await?.map(unix_to_text),
        devices,
        current,
    })
}

/// Clear the device cookie too when this browser's own trust was revoked.
async fn back(st: &AppState, user_id: i64, headers: &HeaderMap) -> AppResult<Response> {
    let mut resp = redirect("/account");
    if let Some(t) = devices::token_from(headers) {
        let still = devices::active(&st.pool, user_id)
            .await?
            .iter()
            .any(|d| devices::is_token_of(d, Some(&t)));
        if !still {
            resp.headers_mut().append(
                header::SET_COOKIE,
                devices::clear_cookie(st.cfg.cookie_secure),
            );
        }
    }
    Ok(resp)
}

async fn revoke(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    if !devices::revoke(&st.pool, user.id, id).await? {
        return Err(AppError::NotFound);
    }
    security_log::record(
        &st.pool,
        Event::DeviceRevoked,
        Some(user.id),
        &user.email,
        &client,
        json!({}),
    )
    .await;
    flash::ok(&session, "Het apparaat is vergeten.").await?;
    back(&st, user.id, &headers).await
}

async fn revoke_all(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    headers: HeaderMap,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    let n = devices::revoke_all(&mut tx, user.id, Some(user.id)).await?;
    tx.commit().await?;
    if n > 0 {
        security_log::record(
            &st.pool,
            Event::DeviceRevoked,
            Some(user.id),
            &user.email,
            &client,
            json!({"method": format!("{n} apparaten")}),
        )
        .await;
    }
    let msg = match n {
        0 => "Er waren geen vertrouwde apparaten.".to_string(),
        1 => "1 apparaat is vergeten.".to_string(),
        n => format!("{n} apparaten zijn vergeten."),
    };
    flash::ok(&session, &msg).await?;
    Ok(back(&st, user.id, &headers).await?.into_response())
}

// ---------- own password ----------

#[derive(Template, WebTemplate)]
#[template(path = "account_password.html")]
struct PasswordPage {
    l: Layout,
    forced: bool,
    errors: crate::web::form::Errors,
}

impl PasswordPage {
    fn err(&self, field: &str) -> &str {
        crate::web::form::err(&self.errors, field)
    }
}

#[derive(serde::Deserialize)]
struct PasswordForm {
    #[serde(default)]
    current: String,
    #[serde(default)]
    new: String,
    #[serde(default)]
    repeat: String,
}

async fn password_page(Page(l): Page, user: AuthUser) -> PasswordPage {
    PasswordPage {
        l,
        forced: user.must_change_password,
        errors: Default::default(),
    }
}

async fn password_save(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    headers: HeaderMap,
    axum::Form(f): axum::Form<PasswordForm>,
) -> AppResult<Response> {
    let mut errors = crate::web::form::Errors::new();
    let stored = crate::auth::users::find_by_id(&st.pool, user.id)
        .await?
        .ok_or(AppError::LoginRequired)?;
    if !crate::auth::password::verify(&stored.password_hash, &f.current) {
        errors.insert("current", "Je huidige wachtwoord klopt niet.".into());
    }
    if let Some(msg) = crate::auth::password::check_strength(&f.new) {
        errors.insert("new", msg);
    } else if f.new == f.current {
        errors.insert("new", "Kies een ander wachtwoord dan het huidige.".into());
    }
    if f.repeat != f.new {
        errors.insert(
            "repeat",
            "De twee nieuwe wachtwoorden zijn niet gelijk.".into(),
        );
    }
    if !errors.is_empty() {
        let page = PasswordPage {
            l,
            forced: user.must_change_password,
            errors,
        };
        return Ok((axum::http::StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    // A new password forgets every trusted device; this one, if it was
    // trusted, is trusted again so the user is not asked for a code here.
    let token = devices::token_from(&headers);
    let this_trusted = devices::active(&st.pool, user.id)
        .await?
        .iter()
        .any(|d| devices::is_token_of(d, token.as_deref()));
    crate::auth::users::change_own_password(&st.pool, user.id, &f.new).await?;
    security_log::record(
        &st.pool,
        Event::PasswordChanged,
        Some(user.id),
        &user.email,
        &client,
        json!({}),
    )
    .await;
    flash::ok(&session, "Je wachtwoord is gewijzigd.").await?;
    let mut resp = redirect(if user.must_change_password {
        "/"
    } else {
        "/account"
    });
    if this_trusted {
        let t = devices::create(&st.pool, user.id, &client.user_agent, &client.ip).await?;
        resp.headers_mut().append(
            header::SET_COOKIE,
            devices::set_cookie(&t, st.cfg.cookie_secure),
        );
    } else if token.is_some() {
        resp.headers_mut().append(
            header::SET_COOKIE,
            devices::clear_cookie(st.cfg.cookie_secure),
        );
    }
    Ok(resp)
}
