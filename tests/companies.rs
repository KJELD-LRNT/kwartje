mod common;

use axum::http::StatusCode;
use common::TestApp;

async fn logged_in() -> TestApp {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.login("anna@example.test").await;
    app
}

fn form<'a>(name: &'a str, prefix: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("name", name),
        ("kvk", "12345678"),
        ("btw_id", "NL123456789B01"),
        ("iban", "NL91ABNA0417164300"),
        ("street", "Hoofdstraat 1"),
        ("postal_code", "1234ab"),
        ("city", "Utrecht"),
        ("invoice_prefix", prefix),
        ("payment_term_days", "14"),
        ("default_vat_bp", "2100"),
        ("rounding_minutes", "15"),
    ]
}

async fn audit_actions(app: &TestApp, id: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'companies' AND row_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(&app.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn create_edit_archive_with_audit_trail() {
    let mut app = logged_in().await;
    app.get("/bedrijven/nieuw").await;
    let r = app.post("/bedrijven", &form("Voorbeeld B.V.", "vb")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id: i64 = sqlx::query_scalar("SELECT id FROM companies WHERE invoice_prefix = 'VB'")
        .fetch_one(&app.pool)
        .await
        .unwrap();

    let list = app.get("/bedrijven").await;
    assert!(list.body.contains("Bedrijf Voorbeeld B.V. aangemaakt."));
    assert!(list.body.contains("Voorbeeld B.V."));

    // Normalised values are shown on the edit page.
    let page = app.get(&format!("/bedrijven/{id}")).await;
    assert!(page.body.contains("NL91 ABNA 0417 1643 00"));
    assert!(page.body.contains("1234 AB"));

    let mut f = form("Voorbeeld Advies B.V.", "VB");
    f.retain(|(k, _)| *k != "payment_term_days");
    f.push(("payment_term_days", "30"));
    let r = app.post(&format!("/bedrijven/{id}"), &f).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    app.get(&format!("/bedrijven/{id}")).await;
    let r = app.post(&format!("/bedrijven/{id}/archiveren"), &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let archived: Option<String> =
        sqlx::query_scalar("SELECT archived_at FROM companies WHERE id = ?")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(archived.is_some(), "archived, not deleted");
    assert!(!app
        .get("/bedrijven")
        .await
        .body
        .contains("Voorbeeld Advies"));
    assert!(app
        .get("/bedrijven?archief=1")
        .await
        .body
        .contains("Voorbeeld Advies"));

    let r = app.post(&format!("/bedrijven/{id}/terugzetten"), &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);

    assert_eq!(
        audit_actions(&app, id).await,
        ["insert", "update", "archive", "unarchive"]
    );
    let diff: String = sqlx::query_scalar(
        "SELECT diff_json FROM audit_log WHERE table_name = 'companies' AND row_id = ? AND action = 'update'",
    )
    .bind(id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&diff).unwrap();
    assert_eq!(v["payment_term_days"], serde_json::json!([14, 30]));
    assert_eq!(
        v["name"],
        serde_json::json!(["Voorbeeld B.V.", "Voorbeeld Advies B.V."])
    );
    assert!(v.get("kvk").is_none());

    // The logbook shows it.
    let log = app.get("/logboek").await;
    assert!(log.body.contains("gearchiveerd"));
}

#[tokio::test]
async fn validation_errors_are_dutch_and_nothing_is_saved() {
    let mut app = logged_in().await;
    app.get("/bedrijven/nieuw").await;
    let mut f = form("", "VB");
    f.retain(|(k, _)| *k != "iban");
    f.push(("iban", "NL00ABNA0417164300"));
    let r = app.post("/bedrijven", &f).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Naam is verplicht."));
    assert!(r.body.contains("Dit IBAN is ongeldig"));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM companies")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn duplicate_prefix_is_refused() {
    let mut app = logged_in().await;
    app.get("/bedrijven/nieuw").await;
    app.post("/bedrijven", &form("A", "JB")).await;
    app.get("/bedrijven/nieuw").await;
    let r = app.post("/bedrijven", &form("B", "jb")).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Deze factuurprefix is al in gebruik."));
}

#[tokio::test]
async fn company_switcher_only_accepts_active_companies() {
    let mut app = logged_in().await;
    app.get("/bedrijven/nieuw").await;
    app.post("/bedrijven", &form("Alfa", "AA")).await;
    app.get("/bedrijven/nieuw").await;
    app.post("/bedrijven", &form("Beta", "BB")).await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM companies ORDER BY id")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    let (alfa, beta) = (ids[0], ids[1]);

    app.get("/").await;
    let r = app
        .post(
            "/bedrijf/kiezen",
            &[("company_id", &alfa.to_string()), ("back", "/bedrijven")],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("/bedrijven"));
    let home = app.get("/").await;
    assert!(home.body.contains("<h1>Alfa</h1>"), "{}", home.body);

    // Unknown company: refused, selection unchanged.
    let r = app
        .post("/bedrijf/kiezen", &[("company_id", "9999"), ("back", "/")])
        .await;
    assert_eq!(r.location.as_deref(), Some("/"));
    let home = app.get("/").await;
    assert!(home.body.contains("bestaat niet of is gearchiveerd"));
    assert!(home.body.contains("<h1>Alfa</h1>"));

    // Archived company: refused, and archiving the selected one falls back.
    app.post(&format!("/bedrijven/{beta}/archiveren"), &[])
        .await;
    app.get("/").await;
    app.post("/bedrijf/kiezen", &[("company_id", &beta.to_string())])
        .await;
    assert!(app.get("/").await.body.contains("<h1>Alfa</h1>"));
    app.post(&format!("/bedrijven/{alfa}/archiveren"), &[])
        .await;
    assert!(app.get("/").await.body.contains("Er is nog geen bedrijf"));

    // Open redirect via `back` is not possible.
    app.post(&format!("/bedrijven/{alfa}/terugzetten"), &[])
        .await;
    app.get("/").await;
    let r = app
        .post(
            "/bedrijf/kiezen",
            &[
                ("company_id", &alfa.to_string()),
                ("back", "//evil.example"),
            ],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn logo_upload_rejects_non_images() {
    let mut app = logged_in().await;
    app.get("/bedrijven/nieuw").await;
    app.post("/bedrijven", &form("Alfa", "AA")).await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM companies")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    app.get(&format!("/bedrijven/{id}")).await;
    let token = app.csrf.clone().unwrap();

    let upload = |bytes: &'static [u8]| {
        let boundary = "XBOUNDARYX";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"logo\"; filename=\"x\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        axum::http::Request::post(format!("/bedrijven/{id}/logo?csrf_token={token}"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(body))
            .unwrap()
    };

    let r = app.send(upload(b"%PDF-1.4 nope")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(app
        .get(&format!("/bedrijven/{id}"))
        .await
        .body
        .contains("Alleen PNG, JPG of SVG"));

    let r = app.send(upload(b"\x89PNG\r\n\x1a\nfake-png")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let path: String = sqlx::query_scalar("SELECT logo_path FROM companies WHERE id = ?")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(path.starts_with(&format!("documents/{id}/logo-")));
    assert!(app.cfg.data_dir.join(&path).exists());
    let r = app.get(&format!("/bedrijven/{id}/logo")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "image/png");
}

#[tokio::test]
async fn export_routes_require_login_and_return_csv_and_zip() {
    let mut app = TestApp::new().await;
    assert_eq!(
        app.get("/export/companies.csv").await.location.as_deref(),
        Some("/inloggen")
    );
    app.create_user("Anna", "anna@example.test").await;
    app.login("anna@example.test").await;
    app.get("/bedrijven/nieuw").await;
    app.post("/bedrijven", &form("Alfa", "AA")).await;

    let page = app.get("/export?from=01-01-2026").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/export/companies.csv?from=01-01-2026"));

    let r = app.get("/export/companies.csv").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "text/csv; charset=utf-8");
    assert!(r.headers["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment; filename=\"companies-"));
    assert!(r.body.starts_with('\u{feff}'));
    assert!(r.body.contains("Alfa"));

    let r = app.get("/export/users.csv").await;
    assert!(!r.body.contains("argon2"), "no password hashes in CSV");

    let r = app.get("/export/all.zip").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "application/zip");

    assert_eq!(
        app.get("/export/geheim.csv").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get("/export/companies.csv?from=gisteren").await.status,
        StatusCode::BAD_REQUEST
    );
}
