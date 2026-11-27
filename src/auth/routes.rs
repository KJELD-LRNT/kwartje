//! Login flow: /inloggen (password) → /2fa or /2fa/instellen (TOTP) → /.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use serde_json::json;
use tower_sessions::Session;

use super::ratelimit::LOCKED_MESSAGE;
use super::session_keys::{
    COMPANY_ID, LOGIN_AT, PENDING, PENDING_REMEMBER, REMEMBER_UNTIL, TOTP_SETUP, USER_ID,
};
use super::{devices, password, totp, users};
use crate::security_log::{self, ClientInfo, Event};
use crate::web::error::redirect;
use crate::web::layout::{AnonPage, AuthUser, Layout};
use crate::web::{flash, AppResult, AppState};

/// The password step is valid for this long before TOTP must be entered.
const PENDING_TTL_SECS: u64 = 5 * 60;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/inloggen", get(login_page).post(login_submit))
        .route("/2fa", get(totp_page).post(totp_submit))
        .route("/2fa/instellen", get(setup_page).post(setup_submit))
        .route("/uitloggen", post(logout))
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/login.html")]
struct LoginPage {
    l: Layout,
    email: String,
    remember: bool,
    error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/totp.html")]
struct TotpPage {
    l: Layout,
    trust: bool,
    error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/totp_setup.html")]
struct SetupPage {
    l: Layout,
    qr_svg: String,
    secret: String,
    trust: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
    /// "Ingelogd blijven (7 dagen)"
    #[serde(default)]
    remember: Option<String>,
}

#[derive(Deserialize)]
struct CodeForm {
    code: String,
    /// "MFA 30 dagen onthouden op dit apparaat"
    #[serde(default)]
    trust: Option<String>,
}

fn keys(email: &str, ip: &str) -> Vec<String> {
    vec![
        format!("acct:{}", email.trim().to_lowercase()),
        format!("ip:{ip}"),
    ]
}

async fn login_page(AnonPage(l): AnonPage, session: Session) -> AppResult<Response> {
    if session.get::<i64>(USER_ID).await?.is_some() {
        return Ok(redirect("/"));
    }
    Ok(LoginPage {
        l,
        email: String::new(),
        remember: false,
        error: None,
    }
    .into_response())
}

async fn login_submit(
    State(st): State<AppState>,
    AnonPage(l): AnonPage,
    session: Session,
    client: ClientInfo,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    let ip = client.ip.clone();
    let k = keys(&form.email, &ip);
    let remember = form.remember.is_some();
    let fail = |l: Layout, msg: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            LoginPage {
                l,
                email: form.email.clone(),
                remember,
                error: Some(msg.to_string()),
            },
        )
            .into_response()
    };
    if st.limiter.is_locked(&k) {
        security_log::record(
            &st.pool,
            Event::LoginLocked,
            None,
            &form.email,
            &client,
            json!({}),
        )
        .await;
        return Ok(fail(l, LOCKED_MESSAGE));
    }
    let user = users::find_by_email(&st.pool, &form.email).await?;
    let ok = match &user {
        Some(u) => password::verify(&u.password_hash, &form.password),
        None => {
            password::burn_time(&form.password);
            false
        }
    };
    let known = user.as_ref().map(|u| u.id);
    let Some(user) = user.filter(|_| ok) else {
        st.limiter.record_failure(&k);
        tracing::warn!(ip = %ip, "failed login");
        let detail = if known.is_some() {
            json!({"method": "onjuist wachtwoord"})
        } else {
            json!({"method": "onbekend e-mailadres"})
        };
        security_log::record(
            &st.pool,
            Event::LoginFailedPassword,
            known,
            &form.email,
            &client,
            detail,
        )
        .await;
        return Ok(fail(l, "Onbekend e-mailadres of onjuist wachtwoord."));
    };

    // A trusted device skips the TOTP step (the password was just checked).
    if user.has_totp() {
        if let Some(token) = devices::token_from(&headers) {
            if let Some(used) = devices::use_token(&st.pool, user.id, &token, &ip).await? {
                if let Some(old) = &used.ip_changed_from {
                    security_log::record(
                        &st.pool,
                        Event::DeviceIpChanged,
                        Some(user.id),
                        &user.email,
                        &client,
                        json!({"old_ip": old}),
                    )
                    .await;
                }
                complete_login(
                    &st,
                    &session,
                    &user,
                    &k,
                    remember,
                    &client,
                    "met vertrouwd apparaat",
                )
                .await?;
                tracing::info!(
                    user_id = user.id,
                    device = used.id,
                    "login on trusted device"
                );
                return Ok(redirect("/"));
            }
        }
    }

    // New session id at every privilege change.
    session.cycle_id().await?;
    session.remove_value(USER_ID).await?;
    session.insert(PENDING, (user.id, totp::unix_now())).await?;
    session.insert(PENDING_REMEMBER, remember).await?;
    if user.has_totp() {
        Ok(redirect("/2fa"))
    } else {
        Ok(redirect("/2fa/instellen"))
    }
}

/// User id from the password step, if still fresh.
async fn pending_user(session: &Session) -> AppResult<Option<i64>> {
    let Some((id, at)) = session.get::<(i64, u64)>(PENDING).await? else {
        return Ok(None);
    };
    if totp::unix_now().saturating_sub(at) > PENDING_TTL_SECS {
        session.remove_value(PENDING).await?;
        flash::error(session, "De inlogpoging is verlopen. Log opnieuw in.").await?;
        return Ok(None);
    }
    Ok(Some(id))
}

async fn complete_login(
    st: &AppState,
    session: &Session,
    user: &users::User,
    k: &[String],
    remember: bool,
    client: &ClientInfo,
    method: &str,
) -> AppResult<()> {
    let user_id = user.id;
    for key in k {
        st.limiter.reset(key);
    }
    let now = totp::unix_now();
    session.cycle_id().await?;
    session.remove_value(PENDING).await?;
    session.remove_value(PENDING_REMEMBER).await?;
    session.remove_value(TOTP_SETUP).await?;
    session.remove_value(COMPANY_ID).await?;
    session.insert(USER_ID, user_id).await?;
    session.insert(LOGIN_AT, now).await?;
    if remember {
        session
            .insert(REMEMBER_UNTIL, now + super::SESSION_MAX_SECS)
            .await?;
    } else {
        session.remove_value(REMEMBER_UNTIL).await?;
    }
    tracing::info!(user_id, remember, "login");
    users::touch_login(&st.pool, user_id).await?;
    security_log::record(
        &st.pool,
        Event::LoginOk,
        Some(user_id),
        &user.email,
        client,
        json!({"method": method}),
    )
    .await;
    Ok(())
}

/// After the TOTP step: the "Ingelogd blijven" choice from the password step.
async fn pending_remember(session: &Session) -> AppResult<bool> {
    Ok(session
        .get::<bool>(PENDING_REMEMBER)
        .await?
        .unwrap_or(false))
}

/// Redirect home, trusting this device first when the user asked for it.
async fn finish(
    st: &AppState,
    user: &users::User,
    trust: bool,
    client: &ClientInfo,
) -> AppResult<Response> {
    let user_id = user.id;
    let mut resp = redirect("/");
    if trust {
        let token = devices::create(&st.pool, user_id, &client.user_agent, &client.ip).await?;
        security_log::record(
            &st.pool,
            Event::DeviceTrusted,
            Some(user_id),
            &user.email,
            client,
            json!({"method": client.user_agent}),
        )
        .await;
        resp.headers_mut().append(
            header::SET_COOKIE,
            devices::set_cookie(&token, st.cfg.cookie_secure),
        );
        tracing::info!(user_id, "device trusted");
    }
    Ok(resp)
}

async fn totp_page(AnonPage(l): AnonPage, session: Session) -> AppResult<Response> {
    if pending_user(&session).await?.is_none() {
        return Ok(redirect("/inloggen"));
    }
    Ok(TotpPage {
        l,
        trust: false,
        error: None,
    }
    .into_response())
}

async fn totp_submit(
    State(st): State<AppState>,
    AnonPage(l): AnonPage,
    session: Session,
    client: ClientInfo,
    Form(form): Form<CodeForm>,
) -> AppResult<Response> {
    let ip = client.ip.clone();
    let Some(id) = pending_user(&session).await? else {
        return Ok(redirect("/inloggen"));
    };
    let Some(user) = users::find_by_id(&st.pool, id).await? else {
        return Ok(redirect("/inloggen"));
    };
    let Some(secret) = user.totp_secret.as_deref().filter(|_| user.has_totp()) else {
        return Ok(redirect("/2fa/instellen"));
    };
    let k = keys(&user.email, &ip);
    let trust = form.trust.is_some();
    let fail = |l, msg: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            TotpPage {
                l,
                trust,
                error: Some(msg.to_string()),
            },
        )
            .into_response()
    };
    if st.limiter.is_locked(&k) {
        security_log::record(
            &st.pool,
            Event::LoginLocked,
            Some(user.id),
            &user.email,
            &client,
            json!({}),
        )
        .await;
        return Ok(fail(l, LOCKED_MESSAGE));
    }
    match totp::verify(secret, &form.code, totp::unix_now(), user.totp_last_step) {
        Some(step) => {
            users::set_totp_step(&st.pool, user.id, step).await?;
            let remember = pending_remember(&session).await?;
            complete_login(&st, &session, &user, &k, remember, &client, "met code").await?;
            finish(&st, &user, trust, &client).await
        }
        None => {
            st.limiter.record_failure(&k);
            tracing::warn!(ip = %ip, user_id = user.id, "failed TOTP");
            security_log::record(
                &st.pool,
                Event::LoginFailedCode,
                Some(user.id),
                &user.email,
                &client,
                json!({}),
            )
            .await;
            Ok(fail(l, "Onjuiste of verlopen code."))
        }
    }
}

async fn setup_page(
    State(st): State<AppState>,
    AnonPage(l): AnonPage,
    session: Session,
) -> AppResult<Response> {
    let Some(id) = pending_user(&session).await? else {
        return Ok(redirect("/inloggen"));
    };
    let Some(user) = users::find_by_id(&st.pool, id).await? else {
        return Ok(redirect("/inloggen"));
    };
    if user.has_totp() {
        return Ok(redirect("/2fa"));
    }
    let secret = match session.get::<String>(TOTP_SETUP).await? {
        Some(s) => s,
        None => {
            let s = totp::generate_secret();
            session.insert(TOTP_SETUP, &s).await?;
            s
        }
    };
    Ok(render_setup(l, &user.email, secret, false, None)?.into_response())
}

fn render_setup(
    l: Layout,
    email: &str,
    secret: String,
    trust: bool,
    error: Option<String>,
) -> AppResult<SetupPage> {
    let url = totp::otpauth_url(&secret, email)?;
    let svg = totp::qr_svg(&url)?;
    // Drop the XML prolog; the SVG is embedded in HTML.
    let qr_svg = match svg.find("<svg") {
        Some(i) => svg[i..].to_string(),
        None => svg,
    };
    Ok(SetupPage {
        l,
        qr_svg,
        secret,
        trust,
        error,
    })
}

async fn setup_submit(
    State(st): State<AppState>,
    AnonPage(l): AnonPage,
    session: Session,
    client: ClientInfo,
    Form(form): Form<CodeForm>,
) -> AppResult<Response> {
    let ip = client.ip.clone();
    let Some(id) = pending_user(&session).await? else {
        return Ok(redirect("/inloggen"));
    };
    let Some(user) = users::find_by_id(&st.pool, id).await? else {
        return Ok(redirect("/inloggen"));
    };
    if user.has_totp() {
        return Ok(redirect("/2fa"));
    }
    let Some(secret) = session.get::<String>(TOTP_SETUP).await? else {
        return Ok(redirect("/2fa/instellen"));
    };
    let k = keys(&user.email, &ip);
    let trust = form.trust.is_some();
    if st.limiter.is_locked(&k) {
        security_log::record(
            &st.pool,
            Event::LoginLocked,
            Some(user.id),
            &user.email,
            &client,
            json!({}),
        )
        .await;
        let page = render_setup(l, &user.email, secret, trust, Some(LOCKED_MESSAGE.into()))?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    match totp::verify(&secret, &form.code, totp::unix_now(), None) {
        Some(step) => {
            users::enable_totp(&st.pool, user.id, &secret, step).await?;
            security_log::record(
                &st.pool,
                Event::MfaEnrolled,
                Some(user.id),
                &user.email,
                &client,
                json!({}),
            )
            .await;
            let remember = pending_remember(&session).await?;
            complete_login(&st, &session, &user, &k, remember, &client, "met code").await?;
            flash::ok(&session, "Tweestapsverificatie is ingesteld.").await?;
            finish(&st, &user, trust, &client).await
        }
        None => {
            st.limiter.record_failure(&k);
            security_log::record(
                &st.pool,
                Event::LoginFailedCode,
                Some(user.id),
                &user.email,
                &client,
                json!({}),
            )
            .await;
            let page = render_setup(
                l,
                &user.email,
                secret,
                trust,
                Some(
                    "Onjuiste code. Controleer de tijd op je telefoon en probeer het opnieuw."
                        .into(),
                ),
            )?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

async fn logout(
    State(st): State<AppState>,
    session: Session,
    client: ClientInfo,
    user: Result<AuthUser, crate::web::AppError>,
) -> AppResult<Response> {
    if let Ok(u) = user {
        tracing::info!(user_id = u.id, "logout");
        security_log::record(
            &st.pool,
            Event::Logout,
            Some(u.id),
            &u.email,
            &client,
            json!({}),
        )
        .await;
    }
    session.flush().await?;
    Ok(redirect("/inloggen"))
}
