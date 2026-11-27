mod common;

use axum::http::StatusCode;
use common::{TestApp, PASSWORD};
use kwartje::auth::{ratelimit, totp, users};

#[tokio::test]
async fn pages_require_login() {
    let mut app = TestApp::new().await;
    for path in ["/", "/bedrijven", "/logboek"] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(r.location.as_deref(), Some("/inloggen"), "{path}");
    }
    let r = app.get("/inloggen").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Inloggen"));
    assert!(r.headers["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'self'"));
}

#[tokio::test]
async fn full_login_with_totp_enrolment_then_verification() {
    let mut app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;

    let secret = app.login("anna@example.test").await;
    let r = app.get("/").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Anna"));
    let u = users::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert!(u.has_totp());
    assert_eq!(u.totp_secret.as_deref(), Some(secret.as_str()));

    // Logout, then log in again: now the TOTP step asks for a code.
    app.get("/").await;
    let r = app.post("/uitloggen", &[]).await;
    assert_eq!(r.location.as_deref(), Some("/inloggen"));
    assert_eq!(app.get("/").await.location.as_deref(), Some("/inloggen"));
    app.new_client();
    app.login("anna@example.test").await;
    assert_eq!(app.get("/").await.status, StatusCode::OK);
}

#[tokio::test]
async fn password_alone_is_not_enough() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.get("/inloggen").await;
    let r = app
        .post(
            "/inloggen",
            &[("email", "anna@example.test"), ("password", PASSWORD)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("/2fa/instellen"));
    // Not logged in yet.
    assert_eq!(app.get("/").await.location.as_deref(), Some("/inloggen"));
}

#[tokio::test]
async fn wrong_password_and_unknown_user_look_the_same() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.get("/inloggen").await;
    let a = app
        .post(
            "/inloggen",
            &[
                ("email", "anna@example.test"),
                ("password", "fout-wachtwoord"),
            ],
        )
        .await;
    let b = app
        .post(
            "/inloggen",
            &[
                ("email", "niemand@example.test"),
                ("password", "fout-wachtwoord"),
            ],
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(a
        .body
        .contains("Onbekend e-mailadres of onjuist wachtwoord."));
    assert!(b
        .body
        .contains("Onbekend e-mailadres of onjuist wachtwoord."));
}

#[tokio::test]
async fn login_locks_after_repeated_failures() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.get("/inloggen").await;
    for _ in 0..ratelimit::MAX_FAILURES {
        let r = app
            .post(
                "/inloggen",
                &[
                    ("email", "anna@example.test"),
                    ("password", "fout-wachtwoord"),
                ],
            )
            .await;
        assert!(r.body.contains("onjuist wachtwoord"));
    }
    // Even the right password is refused now.
    let r = app
        .post(
            "/inloggen",
            &[("email", "anna@example.test"), ("password", PASSWORD)],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains(ratelimit::LOCKED_MESSAGE));

    // Another IP is locked out too, via the account key.
    app.new_client();
    app.ip = "100.64.0.2".into();
    app.get("/inloggen").await;
    let r = app
        .post(
            "/inloggen",
            &[("email", "anna@example.test"), ("password", PASSWORD)],
        )
        .await;
    assert!(r.body.contains(ratelimit::LOCKED_MESSAGE));
}

#[tokio::test]
async fn wrong_totp_code_is_rejected_and_code_cannot_be_replayed() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    let secret = app.login("anna@example.test").await;

    app.new_client();
    app.get("/inloggen").await;
    app.post(
        "/inloggen",
        &[("email", "anna@example.test"), ("password", PASSWORD)],
    )
    .await;
    app.get("/2fa").await;
    let r = app.post("/2fa", &[("code", "000000")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Onjuiste of verlopen code."));

    let code = totp::code_at(&secret, totp::unix_now()).unwrap();
    let r = app.post("/2fa", &[("code", &code)]).await;
    assert_eq!(r.location.as_deref(), Some("/"));

    // Same code again in a new session: replay refused.
    app.new_client();
    app.get("/inloggen").await;
    app.post(
        "/inloggen",
        &[("email", "anna@example.test"), ("password", PASSWORD)],
    )
    .await;
    app.get("/2fa").await;
    let r = app.post("/2fa", &[("code", &code)]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn post_without_csrf_token_is_forbidden() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.login("anna@example.test").await;
    app.get("/bedrijven/nieuw").await;
    let r = app
        .post_raw(
            "/bedrijven",
            &[
                ("name".into(), "X".into()),
                ("invoice_prefix".into(), "X".into()),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app
        .post_raw(
            "/bedrijven",
            &[
                ("name".into(), "X".into()),
                ("csrf_token".into(), "fout".into()),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Also before login.
    app.new_client();
    let r = app
        .post_raw(
            "/inloggen",
            &[
                ("email".into(), "anna@example.test".into()),
                ("password".into(), PASSWORD.into()),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn foreign_origin_is_forbidden() {
    let mut app = TestApp::new().await;
    app.get("/inloggen").await;
    let token = app.csrf.clone().unwrap();
    let body = serde_urlencoded::to_string([
        ("email", "a@example.test"),
        ("password", "x"),
        ("csrf_token", &token),
    ])
    .unwrap();
    let req = axum::http::Request::post("/inloggen")
        .header("host", "admin.example.ts.net")
        .header("origin", "https://evil.example")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body))
        .unwrap();
    assert_eq!(app.send(req).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn reset_totp_ends_existing_sessions() {
    let mut app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;
    app.login("anna@example.test").await;
    assert_eq!(app.get("/").await.status, StatusCode::OK);
    users::reset_totp(&app.pool, id, None).await.unwrap();
    assert_eq!(app.get("/").await.location.as_deref(), Some("/inloggen"));
}

#[tokio::test]
async fn user_creation_is_audited_without_secrets() {
    let app = TestApp::new().await;
    let id = app.create_user("Anna", "anna@example.test").await;
    let diff: String = sqlx::query_scalar(
        "SELECT diff_json FROM audit_log WHERE table_name = 'users' AND row_id = ? AND action = 'insert'",
    )
    .bind(id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(diff.contains("anna@example.test"));
    assert!(!diff.contains("argon2"));
}

#[tokio::test]
async fn origin_matching_forwarded_host_is_accepted() {
    let mut app = TestApp::new().await;
    app.get("/inloggen").await;
    let token = app.csrf.clone().unwrap();
    let body = serde_urlencoded::to_string([
        ("email", "a@example.test"),
        ("password", "x"),
        ("csrf_token", &token),
    ])
    .unwrap();
    let req = axum::http::Request::post("/inloggen")
        .header("host", "127.0.0.1:8080")
        .header("x-forwarded-host", "admin.example.ts.net")
        .header("origin", "https://admin.example.ts.net")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body))
        .unwrap();
    // Not 403: the request reached the login handler (wrong password → 422).
    assert_eq!(app.send(req).await.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn session_cookie_is_httponly_and_lax() {
    let mut app = TestApp::new().await;
    let r = app.get("/inloggen").await;
    let c = r
        .headers
        .get(axum::http::header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(c.starts_with("kwartje_sid="), "{c}");
    assert!(c.contains("HttpOnly"), "{c}");
    assert!(c.contains("SameSite=Lax"), "{c}");
}

#[tokio::test]
async fn hsts_only_over_https_and_quote_shortcut_needs_login() {
    use tower::ServiceExt;
    for secure in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = kwartje::config::Config::for_dir(tmp.path());
        cfg.cookie_secure = secure;
        let pool = kwartje::db::connect_memory().await.unwrap();
        kwartje::db::migrate(&pool).await.unwrap();
        let app =
            kwartje::web::app_with(std::sync::Arc::new(cfg), pool, common::fake_services(false))
                .await
                .unwrap();
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::get("/inloggen")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.headers().contains_key("strict-transport-security"),
            secure
        );
        let r = app
            .oneshot(
                axum::http::Request::get("/offertes/nieuw")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.headers().get("location").unwrap(), "/inloggen");
    }
}

/// A login POST with chosen headers; 422 means it passed CSRF and reached
/// the handler (wrong password), 403 means CSRF stopped it.
async fn login_post(
    app: &mut TestApp,
    token: Option<&str>,
    headers: &[(&str, &str)],
    token_in: &str,
) -> StatusCode {
    let mut fields = vec![("email", "a@example.test"), ("password", "x")];
    if let (Some(t), "form") = (token, token_in) {
        fields.push(("csrf_token", t));
    }
    let uri = match (token, token_in) {
        (Some(t), "query") => format!("/inloggen?csrf_token={t}"),
        _ => "/inloggen".to_string(),
    };
    let mut req = axum::http::Request::post(uri)
        .header("host", "kwartje.example.ts.net")
        .header("content-type", "application/x-www-form-urlencoded");
    if let (Some(t), "header") = (token, token_in) {
        req = req.header("x-csrf-token", t);
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = serde_urlencoded::to_string(&fields).unwrap();
    app.send(req.body(axum::body::Body::from(body)).unwrap())
        .await
        .status
}

#[tokio::test]
async fn csrf_edge_cases() {
    let mut app = TestApp::new().await;
    app.get("/inloggen").await;
    let token = app.csrf.clone().unwrap();
    let t = Some(token.as_str());
    let ok = StatusCode::UNPROCESSABLE_ENTITY;
    let no = StatusCode::FORBIDDEN;

    // the token in the form, a header (htmx) or the query (multipart) works
    assert_eq!(login_post(&mut app, t, &[], "form").await, ok);
    assert_eq!(login_post(&mut app, t, &[], "header").await, ok);
    assert_eq!(login_post(&mut app, t, &[], "query").await, ok);
    // same-origin browser headers are fine
    let same = [
        ("origin", "https://kwartje.example.ts.net"),
        ("sec-fetch-site", "same-origin"),
    ];
    assert_eq!(login_post(&mut app, t, &same, "form").await, ok);

    // missing or wrong token
    assert_eq!(login_post(&mut app, None, &[], "form").await, no);
    assert_eq!(login_post(&mut app, Some("0000"), &[], "form").await, no);
    // change the last character, whatever it is (a token can end in '0')
    let last = if token.ends_with('0') { '1' } else { '0' };
    let almost = format!("{}{last}", &token[..token.len() - 1]);
    assert_eq!(login_post(&mut app, Some(&almost), &[], "header").await, no);
    // a valid token from another site's page is still refused
    assert_eq!(
        login_post(&mut app, t, &[("origin", "https://evil.example")], "form").await,
        no
    );
    assert_eq!(
        login_post(&mut app, t, &[("sec-fetch-site", "cross-site")], "form").await,
        no
    );
    // a forwarded host only counts when it matches the Origin
    assert_eq!(
        login_post(
            &mut app,
            t,
            &[
                ("origin", "https://evil.example"),
                ("x-forwarded-host", "kwartje.example.ts.net")
            ],
            "form"
        )
        .await,
        no
    );
    // a token from another session is refused
    let mut other = TestApp::new().await;
    other.get("/inloggen").await;
    assert_eq!(login_post(&mut other, t, &[], "form").await, no);
}
