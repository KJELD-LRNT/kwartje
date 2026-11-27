//! The security log: what is recorded, append-only storage, retention and
//! the beheerder's page.

mod common;

use axum::http::StatusCode;
use common::{TestApp, PASSWORD};
use kwartje::auth::totp;
use kwartje::auth::users::{self, Role};
use kwartje::security_log;

async fn events(app: &TestApp) -> Vec<(String, Option<i64>, String, String, String)> {
    sqlx::query_as("SELECT event, user_id, email, ip, via FROM security_events ORDER BY id")
        .fetch_all(&app.pool)
        .await
        .unwrap()
}

async fn password(
    app: &mut TestApp,
    email: &str,
    pw: &str,
    extra: &[(&str, &str)],
) -> common::Resp {
    app.get("/inloggen").await;
    let mut f = vec![("email", email), ("password", pw)];
    f.extend_from_slice(extra);
    app.post("/inloggen", &f).await
}

#[tokio::test]
async fn sign_ins_are_logged_with_address_and_route() {
    let mut app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;
    app.ip = "188.122.74.75".into();
    password(&mut app, "anna@example.test", "fout", &[]).await;
    password(&mut app, "niemand@example.test", "fout", &[]).await;
    // enrol MFA with a wrong code first, then the right one, trusting the device
    let r = password(&mut app, "anna@example.test", PASSWORD, &[]).await;
    assert_eq!(r.location.as_deref(), Some("/2fa/instellen"));
    let page = app.get("/2fa/instellen").await;
    let secret = common::extract_secret(&page.body).unwrap();
    app.post("/2fa/instellen", &[("code", "000000")]).await;
    let code = totp::code_at(&secret, totp::unix_now() - 30).unwrap();
    let r = app
        .post("/2fa/instellen", &[("code", &code), ("trust", "1")])
        .await;
    assert_eq!(r.location.as_deref(), Some("/"));
    app.get("/").await;
    app.post("/uitloggen", &[]).await;
    // the trusted device from another network: allowed, and logged
    app.ip = "100.70.1.2".into();
    let r = password(&mut app, "anna@example.test", PASSWORD, &[]).await;
    assert_eq!(r.location.as_deref(), Some("/"), "{}", r.body);

    let ev = events(&app).await;
    let names: Vec<&str> = ev.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "login_failed_password",
            "login_failed_password",
            "login_failed_code",
            "mfa_enrolled",
            "login_ok",
            "device_trusted",
            "logout",
            "device_ip_changed",
            "login_ok",
        ]
    );
    assert_eq!(ev[0].1, Some(id), "a known account is linked");
    assert_eq!(ev[1].1, None);
    assert_eq!(ev[1].2, "niemand@example.test");
    assert_eq!(
        (ev[0].3.as_str(), ev[0].4.as_str()),
        ("188.122.74.75", "internet")
    );
    assert_eq!(
        (ev[8].3.as_str(), ev[8].4.as_str()),
        ("100.70.1.2", "Tailscale")
    );
    let details: Vec<String> =
        sqlx::query_scalar("SELECT detail_json FROM security_events ORDER BY id")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert!(details[0].contains("onjuist wachtwoord"));
    assert!(details[1].contains("onbekend e-mailadres"));
    assert!(details[7].contains("188.122.74.75"), "{}", details[7]);
    assert!(details[8].contains("vertrouwd apparaat"));
    assert!(!details.iter().any(|d| d.contains(PASSWORD)));
    let last: Option<String> = sqlx::query_scalar("SELECT last_login_at FROM users")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(last.is_some());
}

#[tokio::test]
async fn a_lock_is_logged() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    for _ in 0..6 {
        password(&mut app, "anna@example.test", "fout", &[]).await;
    }
    let ev = events(&app).await;
    assert_eq!(ev.last().unwrap().0, "login_locked");
}

#[tokio::test]
async fn the_log_is_append_only_and_old_rows_are_cleaned_up() {
    let app = TestApp::new().await;
    let c = security_log::ClientInfo::cli();
    security_log::record(
        &app.pool,
        security_log::Event::Logout,
        None,
        "x@example.test",
        &c,
        serde_json::json!({}),
    )
    .await;
    assert!(sqlx::query("UPDATE security_events SET ip = 'x'")
        .execute(&app.pool)
        .await
        .is_err());
    assert!(sqlx::query("DELETE FROM security_events")
        .execute(&app.pool)
        .await
        .is_err());
    sqlx::query(
        "INSERT INTO security_events (at, event) VALUES ('2020-01-01T00:00:00Z', 'logout')",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    assert_eq!(security_log::cleanup(&app.pool).await.unwrap(), 1);
    assert_eq!(app.count("SELECT COUNT(*) FROM security_events").await, 1);
}

#[tokio::test]
async fn admin_sees_and_filters_the_log() {
    let (mut app, _) = TestApp::with_company().await;
    let me = app.count("SELECT MIN(id) FROM users").await;
    users::set_role(&app.pool, me, Role::Admin, None)
        .await
        .unwrap();
    app.create_user("Bram", "bram@example.test").await;
    let mut other = TestApp {
        router: app.router.clone(),
        pool: app.pool.clone(),
        cfg: app.cfg.clone(),
        dir: tempfile::tempdir().unwrap(),
        cookie: None,
        device: None,
        csrf: None,
        ip: "203.0.113.9".into(),
    };
    password(&mut other, "bram@example.test", "fout", &[]).await;

    let page = app.get("/beveiliging").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("203.0.113.9"), "{}", page.body);
    assert!(page.body.contains("Onjuist wachtwoord"));
    assert!(page.body.contains("Ingelogd"));
    let only = app.get("/beveiliging?mislukt=1").await;
    assert!(only.body.contains("203.0.113.9"));
    assert!(
        !only.body.contains("\"Gebeurtenis\">Ingelogd<"),
        "{}",
        only.body
    );
    let csv = app.get("/beveiliging.csv?mislukt=1").await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(csv.body.contains("203.0.113.9"));
    assert!(csv.body.starts_with('\u{feff}'));
    let bad = app.get("/beveiliging?van=gisteren").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_refused_write_of_an_accountant_is_logged() {
    let (mut app, company) = TestApp::with_company().await;
    let id = app.create_user("Boek", "boek@example.test").await;
    users::set_role(&app.pool, id, Role::Accountant, None)
        .await
        .unwrap();
    users::set_company_ids(&app.pool, id, &[company], None)
        .await
        .unwrap();
    app.new_client();
    app.login("boek@example.test").await;
    app.get("/klanten").await;
    let r = app.post("/klanten", &[("name", "x")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let ev = events(&app).await;
    assert_eq!(ev.last().unwrap().0, "write_refused");
}
