mod common;

use axum::http::StatusCode;
use common::TestApp;

async fn setup() -> (TestApp, i64, i64, i64) {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant A")]).await;
    let a = app.last_id("clients").await;
    app.post("/klanten", &[("name", "Klant B")]).await;
    let b = app.last_id("clients").await;
    app.get("/tarieven/nieuw").await;
    let a_s = a.to_string();
    app.post(
        "/tarieven",
        &[
            ("name", "Tarief A"),
            ("unit", "hour"),
            ("price", "110"),
            ("vat_bp", "2100"),
            ("valid_from", "2026-01-01"),
            ("client_id", &a_s),
        ],
    )
    .await;
    let ta = app.last_id("tariffs").await;
    (app, a, b, ta)
}

#[tokio::test]
async fn create_edit_archive_task() {
    let (mut app, a, _b, ta) = setup().await;
    app.get("/taken/nieuw").await;
    let (a_s, ta_s) = (a.to_string(), ta.to_string());
    let r = app
        .post(
            "/taken",
            &[
                ("name", "Scouting"),
                ("client_id", &a_s),
                ("tariff_id", &ta_s),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = app.last_id("tasks").await;
    let list = app.get("/taken").await;
    assert!(list.body.contains("Scouting") && list.body.contains("Tarief A (€ 110,00)"));

    // Internal: the tariff is dropped even if one was chosen.
    app.get("/taken/nieuw").await;
    app.post(
        "/taken",
        &[
            ("name", "Administratie"),
            ("internal", "1"),
            ("tariff_id", &ta_s),
        ],
    )
    .await;
    let admin = app.last_id("tasks").await;
    let (billable, tariff): (bool, Option<i64>) =
        sqlx::query_as("SELECT billable, tariff_id FROM tasks WHERE id = ?")
            .bind(admin)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(!billable && tariff.is_none());
    assert!(app.get("/taken").await.body.contains("Niet declarabel"));

    app.get(&format!("/taken/{id}")).await;
    app.post(
        &format!("/taken/{id}"),
        &[("name", "Scouting jeugd"), ("client_id", &a_s)],
    )
    .await;
    app.post(&format!("/taken/{id}/archiveren"), &[]).await;
    assert!(!app.get("/taken").await.body.contains(">Scouting jeugd</a>"));
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'tasks' AND row_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["insert", "update", "archive"]);
}

#[tokio::test]
async fn tariff_of_another_client_is_refused() {
    let (mut app, _a, b, ta) = setup().await;
    app.get("/taken/nieuw").await;
    let (b_s, ta_s) = (b.to_string(), ta.to_string());
    let r = app
        .post(
            "/taken",
            &[("name", "X"), ("client_id", &b_s), ("tariff_id", &ta_s)],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("hoort bij een andere klant"));
    // without a client the client tariff does not fit either
    let r = app
        .post("/taken", &[("name", "X"), ("tariff_id", &ta_s)])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(app.count("SELECT COUNT(*) FROM tasks").await, 0);
}

#[tokio::test]
async fn other_companys_client_cannot_be_used() {
    let (mut app, a) = TestApp::with_company().await;
    let b = app.create_company("Beta", "BB").await;
    app.select_company(b).await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant van Beta")]).await;
    let beta_client = app.last_id("clients").await;
    app.select_company(a).await;
    app.get("/taken/nieuw").await;
    let r = app
        .post(
            "/taken",
            &[("name", "X"), ("client_id", &beta_client.to_string())],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("bestaat niet"));
    assert_eq!(app.count("SELECT COUNT(*) FROM tasks").await, 0);
}
