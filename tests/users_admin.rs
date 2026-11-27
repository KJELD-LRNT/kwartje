//! User management by the beheerder, and the forced change of a temporary
//! password.

mod common;

use axum::http::StatusCode;
use common::{TestApp, PASSWORD};
use kwartje::auth::totp;
use kwartje::auth::users::{self, Role};

/// Logged in as an admin (the harness user, promoted), with one company.
async fn admin_app() -> (TestApp, i64, i64) {
    let (app, company) = TestApp::with_company().await;
    let me = app.count("SELECT MIN(id) FROM users").await;
    users::set_role(&app.pool, me, Role::Admin, None)
        .await
        .unwrap();
    (app, me, company)
}

/// A second browser on the same app.
fn browser(app: &TestApp) -> TestApp {
    TestApp {
        router: app.router.clone(),
        pool: app.pool.clone(),
        cfg: app.cfg.clone(),
        dir: tempfile::tempdir().unwrap(),
        cookie: None,
        device: None,
        csrf: None,
        ip: "100.64.0.9".into(),
    }
}

const TEMP: &str = "tijdelijk-wachtwoord-1";

#[tokio::test]
async fn admin_creates_a_user_who_must_change_the_temporary_password() {
    let (mut app, _, _) = admin_app().await;
    let page = app.get("/gebruikers/nieuw").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Tijdelijk wachtwoord"));
    let r = app
        .post(
            "/gebruikers",
            &[
                ("name", "Bram"),
                ("email", "bram@example.test"),
                ("role", "employee"),
                ("password", TEMP),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let detail = app.get(r.location.as_deref().unwrap()).await;
    assert!(detail.body.contains(TEMP), "the password is shown once");
    assert!(app
        .get("/gebruikers")
        .await
        .body
        .contains("tijdelijk wachtwoord"));

    // Bram logs in with it, sets up MFA and is sent to change it
    let mut j = browser(&app);
    j.get("/inloggen").await;
    let r = j
        .post(
            "/inloggen",
            &[("email", "bram@example.test"), ("password", TEMP)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("/2fa/instellen"));
    let page = j.get("/2fa/instellen").await;
    let secret = common::extract_secret(&page.body).unwrap();
    let code = totp::code_at(&secret, totp::unix_now()).unwrap();
    j.post("/2fa/instellen", &[("code", &code)]).await;
    let r = j.get("/uren").await;
    assert_eq!(r.location.as_deref(), Some("/account/wachtwoord"));
    let page = j.get("/account/wachtwoord").await;
    assert!(page.body.contains("tijdelijk wachtwoord"));
    // mismatch and a wrong current password are refused
    let r = j
        .post(
            "/account/wachtwoord",
            &[
                ("current", TEMP),
                ("new", "mijn-eigen-zin-123"),
                ("repeat", "anders-123456"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = j
        .post(
            "/account/wachtwoord",
            &[
                ("current", "fout"),
                ("new", "mijn-eigen-zin-123"),
                ("repeat", "mijn-eigen-zin-123"),
            ],
        )
        .await;
    assert!(r.body.contains("Je huidige wachtwoord klopt niet."));
    let r = j
        .post(
            "/account/wachtwoord",
            &[
                ("current", TEMP),
                ("new", "mijn-eigen-zin-123"),
                ("repeat", "mijn-eigen-zin-123"),
            ],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("/"), "{}", r.body);
    assert_eq!(j.get("/uren").await.status, StatusCode::OK);
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event FROM security_events WHERE event IN ('user_created', 'password_changed') ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(events, ["user_created", "password_changed"]);
}

#[tokio::test]
async fn admin_creates_an_accountant_with_companies() {
    let (mut app, _, company) = admin_app().await;
    app.get("/gebruikers/nieuw").await;
    // a boekhouder without a company is refused
    let r = app
        .post(
            "/gebruikers",
            &[
                ("name", "Boek"),
                ("email", "boek@example.test"),
                ("role", "accountant"),
                ("password", TEMP),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Kies minstens één bedrijf"));
    let c = company.to_string();
    let r = app
        .post(
            "/gebruikers",
            &[
                ("name", "Boek"),
                ("email", "boek@example.test"),
                ("role", "accountant"),
                ("password", TEMP),
                ("bedrijf", &c),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = app.last_id("users").await;
    assert_eq!(
        users::company_ids(&app.pool, id).await.unwrap(),
        vec![company]
    );
    // duplicate e-mail
    app.get("/gebruikers/nieuw").await;
    let r = app
        .post(
            "/gebruikers",
            &[
                ("name", "Dubbel"),
                ("email", "BOEK@example.test"),
                ("role", "employee"),
                ("password", TEMP),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Er bestaat al een gebruiker"));
}

#[tokio::test]
async fn admin_edits_resets_and_archives() {
    let (mut app, me, _) = admin_app().await;
    let bram = app.create_user("Bram", "bram@example.test").await;
    let path = format!("/gebruikers/{bram}");
    app.get(&path).await;
    let r = app
        .post(
            &path,
            &[
                ("name", "Bram B"),
                ("email", "bram@example.test"),
                ("role", "admin"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let u = users::find_by_id(&app.pool, bram).await.unwrap().unwrap();
    assert_eq!((u.name.as_str(), u.role()), ("Bram B", Role::Admin));

    // password reset: temporary, must change, devices gone
    kwartje::auth::devices::create(&app.pool, bram, "Firefox op Linux", "1.2.3.4")
        .await
        .unwrap();
    app.get(&path).await;
    let r = app
        .post(&format!("{path}/wachtwoord"), &[("password", TEMP)])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let u = users::find_by_id(&app.pool, bram).await.unwrap().unwrap();
    assert!(u.must_change_password);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM trusted_devices WHERE revoked_at IS NULL")
            .await,
        0
    );
    // MFA reset
    sqlx::query(
        "UPDATE users SET totp_secret = 'X', totp_enabled_at = '2026-01-01T00:00:00Z' WHERE id = ?",
    )
    .bind(bram)
    .execute(&app.pool)
    .await
    .unwrap();
    app.get(&path).await;
    app.post(&format!("{path}/mfa-reset"), &[]).await;
    let u = users::find_by_id(&app.pool, bram).await.unwrap().unwrap();
    assert!(!u.has_totp());

    // archive: Bram can no longer log in; restore brings him back
    app.get(&path).await;
    app.post(&format!("{path}/archiveren"), &[]).await;
    assert!(users::find_by_id(&app.pool, bram).await.unwrap().is_none());
    let mut j = browser(&app);
    j.get("/inloggen").await;
    let r = j
        .post(
            "/inloggen",
            &[("email", "bram@example.test"), ("password", TEMP)],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    app.get(&path).await;
    app.post(&format!("{path}/herstellen"), &[]).await;
    assert!(users::find_by_id(&app.pool, bram).await.unwrap().is_some());

    // you can't archive or demote yourself
    let mine = format!("/gebruikers/{me}");
    app.get(&mine).await;
    app.post(&format!("{mine}/archiveren"), &[]).await;
    assert!(users::find_by_id(&app.pool, me).await.unwrap().is_some());
    let r = app
        .post(
            &mine,
            &[
                ("name", "Anna"),
                ("email", "anna@example.test"),
                ("role", "employee"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("eigen beheerdersrol"));

    let events: Vec<String> =
        sqlx::query_scalar("SELECT event FROM security_events WHERE user_id = ? ORDER BY id")
            .bind(bram)
            .fetch_all(&app.pool)
            .await
            .unwrap();
    for e in [
        "role_changed",
        "password_reset",
        "mfa_reset",
        "user_archived",
        "user_restored",
    ] {
        assert!(events.iter().any(|x| x == e), "{e} in {events:?}");
    }
}

#[tokio::test]
async fn own_password_change_keeps_this_device_trusted() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.get("/inloggen").await;
    app.post(
        "/inloggen",
        &[("email", "anna@example.test"), ("password", PASSWORD)],
    )
    .await;
    let page = app.get("/2fa/instellen").await;
    let secret = common::extract_secret(&page.body).unwrap();
    let code = totp::code_at(&secret, totp::unix_now()).unwrap();
    app.post("/2fa/instellen", &[("code", &code), ("trust", "1")])
        .await;
    let before = app.device.clone().unwrap();
    app.get("/account/wachtwoord").await;
    let r = app
        .post(
            "/account/wachtwoord",
            &[
                ("current", PASSWORD),
                ("new", "nieuw-wachtwoord-99"),
                ("repeat", "nieuw-wachtwoord-99"),
            ],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("/account"), "{}", r.body);
    let after = app.device.clone().unwrap();
    assert_ne!(before, after, "a fresh device token");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM trusted_devices WHERE revoked_at IS NULL")
            .await,
        1
    );
}
