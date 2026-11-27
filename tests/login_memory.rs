//! "Ingelogd blijven (7 dagen)" and "MFA 30 dagen onthouden" (trusted devices).

mod common;

use axum::http::{header, StatusCode};
use common::{Resp, TestApp, PASSWORD};
use kwartje::auth::{devices, totp, users};

fn set_cookie<'a>(r: &'a Resp, name: &str) -> Option<&'a str> {
    r.headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
}

fn max_age(cookie: &str) -> i64 {
    cookie
        .split(';')
        .find_map(|p| p.trim().strip_prefix("Max-Age="))
        .expect("Max-Age")
        .parse()
        .unwrap()
}

async fn password_step(app: &mut TestApp, email: &str, password: &str, remember: bool) -> Resp {
    app.get("/inloggen").await;
    let mut f = vec![("email", email), ("password", password)];
    if remember {
        f.push(("remember", "1"));
    }
    app.post("/inloggen", &f).await
}

/// First login: password, then TOTP enrolment. Returns the secret.
async fn enrol(app: &mut TestApp, email: &str, remember: bool, trust: bool) -> (String, Resp) {
    let r = password_step(app, email, PASSWORD, remember).await;
    assert_eq!(r.location.as_deref(), Some("/2fa/instellen"), "{}", r.body);
    let page = app.get("/2fa/instellen").await;
    let secret = common::extract_secret(&page.body).unwrap();
    let code = totp::code_at(&secret, totp::unix_now() - 30).unwrap();
    let mut f = vec![("code", code.as_str())];
    if trust {
        f.push(("trust", "1"));
    }
    let r = app.post("/2fa/instellen", &f).await;
    assert_eq!(r.location.as_deref(), Some("/"), "{}", r.body);
    (secret, r)
}

async fn logout(app: &mut TestApp) {
    app.get("/").await;
    let r = app.post("/uitloggen", &[]).await;
    assert_eq!(r.location.as_deref(), Some("/inloggen"));
}

#[tokio::test]
async fn remember_keeps_the_session_cookie_for_seven_days() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    let (_, r) = enrol(&mut app, "anna@example.test", true, false).await;
    let c = set_cookie(&r, "kwartje_sid").expect("session cookie");
    let age = max_age(c);
    assert!((7 * 86400 - 60..=7 * 86400).contains(&age), "{c}");
    assert!(set_cookie(&r, devices::COOKIE).is_none());

    // A later request that changes the session keeps the 7-day end.
    app.create_company("Alfa", "AA").await;
    let beta = app.create_company("Beta", "BB").await;
    app.get("/").await;
    let r = app
        .post("/bedrijf/kiezen", &[("company_id", &beta.to_string())])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let c = set_cookie(&r, "kwartje_sid").expect("session saved");
    assert!(max_age(c) > 6 * 86400, "{c}");
    let page = app.get("/account").await;
    assert!(
        page.body.contains("Je blijft ingelogd tot"),
        "{}",
        page.body
    );
}

#[tokio::test]
async fn without_remember_the_session_ends_after_inactivity() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    let (_, r) = enrol(&mut app, "anna@example.test", false, false).await;
    let c = set_cookie(&r, "kwartje_sid").expect("session cookie");
    assert_eq!(max_age(c), 12 * 3600, "{c}");
    assert!(app
        .get("/account")
        .await
        .body
        .contains("na 12 uur zonder activiteit"));
}

#[tokio::test]
async fn trusted_device_skips_the_code_but_never_the_password() {
    let mut app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;
    let (_, r) = enrol(&mut app, "anna@example.test", false, true).await;
    let c = set_cookie(&r, devices::COOKIE).expect("device cookie");
    assert!(
        c.contains("HttpOnly") && c.contains("SameSite=Strict"),
        "{c}"
    );
    assert_eq!(max_age(c), 30 * 86400, "{c}");
    assert_eq!(
        app.count(&format!(
            "SELECT COUNT(*) FROM trusted_devices WHERE user_id = {id}"
        ))
        .await,
        1
    );

    logout(&mut app).await;
    assert!(app.device.is_some(), "logging out keeps the device trusted");

    // wrong password: still refused
    let r = password_step(&mut app, "anna@example.test", "fout-wachtwoord", false).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    // right password: straight in, no /2fa
    let r = password_step(&mut app, "anna@example.test", PASSWORD, false).await;
    assert_eq!(r.location.as_deref(), Some("/"), "{}", r.body);
    assert_eq!(app.get("/").await.status, StatusCode::OK);
    let used: Option<String> = sqlx::query_scalar("SELECT last_used_at FROM trusted_devices")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(used.is_some());

    // another browser without the cookie still needs the code
    logout(&mut app).await;
    let device = app.device.clone();
    app.new_client();
    let r = password_step(&mut app, "anna@example.test", PASSWORD, false).await;
    assert_eq!(r.location.as_deref(), Some("/2fa"));

    // the token is bound to its user
    let other = app.create_user("Bram", "bram@example.test").await;
    let token = device.unwrap().split_once('=').unwrap().1.to_string();
    assert_eq!(
        devices::use_token(&app.pool, other, &token, "100.64.0.1")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn account_page_lists_and_revokes_devices() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    enrol(&mut app, "anna@example.test", false, true).await;
    let page = app.get("/account").await;
    assert!(page.body.contains("(dit apparaat)"), "{}", page.body);
    let dev = app.last_id("trusted_devices").await;

    let r = app
        .post(&format!("/account/apparaten/{dev}/intrekken"), &[])
        .await;
    assert_eq!(r.location.as_deref(), Some("/account"));
    assert!(app.device.is_none(), "own cookie is cleared");
    assert!(app
        .get("/account")
        .await
        .body
        .contains("Geen vertrouwde apparaten"));
    // revoking twice, or someone else's device, is a 404
    assert_eq!(
        app.post(&format!("/account/apparaten/{dev}/intrekken"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'trusted_devices' ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["insert", "archive"]);
    let logged: String = sqlx::query_scalar(
        "SELECT diff_json FROM audit_log WHERE table_name = 'trusted_devices' LIMIT 1",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(!logged.contains("token"), "{logged}");
    assert!(app
        .get("/logboek")
        .await
        .body
        .contains("Vertrouwde apparaten"));
}

#[tokio::test]
async fn devices_of_another_user_cannot_be_revoked() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    enrol(&mut app, "anna@example.test", false, true).await;
    let dev = app.last_id("trusted_devices").await;
    logout(&mut app).await;
    app.new_client();
    app.create_user("Bram", "bram@example.test").await;
    enrol(&mut app, "bram@example.test", false, false).await;
    assert_eq!(
        app.post(&format!("/account/apparaten/{dev}/intrekken"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(!app.get("/account").await.body.contains("(dit apparaat)"));
    assert_eq!(
        app.count("SELECT COUNT(*) FROM trusted_devices WHERE revoked_at IS NULL")
            .await,
        1
    );
}

#[tokio::test]
async fn password_reset_and_expiry_end_the_trust() {
    let mut app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;
    enrol(&mut app, "anna@example.test", false, true).await;
    logout(&mut app).await;

    // expired: the code is asked again
    sqlx::query("UPDATE trusted_devices SET expires_at = '2020-01-01T00:00:00Z'")
        .execute(&app.pool)
        .await
        .unwrap();
    let r = password_step(&mut app, "anna@example.test", PASSWORD, false).await;
    assert_eq!(r.location.as_deref(), Some("/2fa"));
    sqlx::query("UPDATE trusted_devices SET expires_at = '2099-01-01T00:00:00Z'")
        .execute(&app.pool)
        .await
        .unwrap();
    app.new_client();
    app.device = None;

    // a new password revokes every device
    let new_pw = "nog-een-wachtwoord-456";
    users::set_password(&app.pool, id, new_pw, None)
        .await
        .unwrap();
    assert_eq!(
        app.count("SELECT COUNT(*) FROM trusted_devices WHERE revoked_at IS NULL")
            .await,
        0
    );
}

#[tokio::test]
async fn totp_reset_revokes_devices_and_all_can_be_forgotten() {
    let mut app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;
    let (secret, _) = enrol(&mut app, "anna@example.test", false, true).await;
    // second device of the same user
    devices::create(&app.pool, id, "Safari op iPhone", "100.64.0.2")
        .await
        .unwrap();
    let r = app.post("/account/apparaten/intrekken", &[]).await;
    assert_eq!(r.location.as_deref(), Some("/account"));
    assert!(app
        .get("/account")
        .await
        .body
        .contains("2 apparaten zijn vergeten."));

    // trust again, then reset TOTP from the CLI path
    logout(&mut app).await;
    let r = password_step(&mut app, "anna@example.test", PASSWORD, false).await;
    assert_eq!(r.location.as_deref(), Some("/2fa"));
    app.get("/2fa").await;
    let code = totp::code_at(&secret, totp::unix_now()).unwrap();
    let r = app.post("/2fa", &[("code", &code), ("trust", "1")]).await;
    assert_eq!(r.location.as_deref(), Some("/"));
    assert!(app.device.is_some());
    users::reset_totp(&app.pool, id, None).await.unwrap();
    assert_eq!(
        app.count("SELECT COUNT(*) FROM trusted_devices WHERE revoked_at IS NULL")
            .await,
        0
    );
}
