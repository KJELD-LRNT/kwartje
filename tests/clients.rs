mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn create_search_edit_archive_client_with_audit() {
    let (mut app, company) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    let r = app
        .post(
            "/klanten",
            &[
                ("name", "Bakkerij De Korst"),
                ("contact_name", "Els"),
                ("email", "els@dekorst.example"),
                ("postal_code", "3511ab"),
                ("city", "Utrecht"),
                ("notes", "Belt liever dan mailt."),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = app.last_id("clients").await;
    assert_eq!(r.location, Some(format!("/klanten/{id}")));
    let cid: i64 = app
        .count(&format!("SELECT company_id FROM clients WHERE id = {id}"))
        .await;
    assert_eq!(cid, company);

    let page = app.get(&format!("/klanten/{id}")).await;
    assert!(page.body.contains("Klant Bakkerij De Korst aangemaakt."));
    assert!(page.body.contains("3511 AB"));
    assert!(page.body.contains("Belt liever dan mailt."));

    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Garage Jansen"), ("city", "Zeist")])
        .await;
    app.get("/klanten").await; // consume the flash message
    let found = app.get("/klanten?q=korst").await;
    assert!(found.body.contains("Bakkerij De Korst"));
    assert!(!found.body.contains("Garage Jansen"));
    let by_city = app.get("/klanten?q=zeist").await;
    assert!(by_city.body.contains("Garage Jansen"));

    app.get(&format!("/klanten/{id}/bewerken")).await;
    let r = app
        .post(
            &format!("/klanten/{id}/bewerken"),
            &[("name", "Bakkerij De Korst B.V."), ("city", "Utrecht")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);

    app.get(&format!("/klanten/{id}")).await;
    app.post(&format!("/klanten/{id}/archiveren"), &[]).await;
    assert!(!app.get("/klanten").await.body.contains("De Korst"));
    assert!(app
        .get("/klanten?archief=1")
        .await
        .body
        .contains("De Korst"));
    // archived, not deleted
    assert_eq!(
        app.count(&format!(
            "SELECT COUNT(*) FROM clients WHERE id = {id} AND archived_at IS NOT NULL"
        ))
        .await,
        1
    );
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'clients' AND row_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["insert", "update", "archive"]);
}

#[tokio::test]
async fn validation_errors_in_dutch() {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    let r = app
        .post("/klanten", &[("name", ""), ("email", "geen-mail")])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Naam is verplicht."));
    assert!(r.body.contains("Dit e-mailadres is ongeldig."));
    assert_eq!(app.count("SELECT COUNT(*) FROM clients").await, 0);
}

#[tokio::test]
async fn projects_are_managed_on_the_client_page() {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant A")]).await;
    let client = app.last_id("clients").await;
    app.get(&format!("/klanten/{client}")).await;
    let r = app
        .post(
            &format!("/klanten/{client}/projecten"),
            &[("name", "Transfer zomer")],
        )
        .await;
    assert_eq!(r.location, Some(format!("/klanten/{client}")));
    let project = app.last_id("projects").await;
    let page = app.get(&format!("/klanten/{client}")).await;
    assert!(page.body.contains("Transfer zomer"));

    app.post(
        &format!("/projecten/{project}"),
        &[("name", "Transfer winter")],
    )
    .await;
    app.post(&format!("/projecten/{project}/archiveren"), &[])
        .await;
    let page = app.get(&format!("/klanten/{client}")).await;
    assert!(page.body.contains("Transfer winter"));
    assert!(page.body.contains("(gearchiveerd)"));

    // empty name: flash error, nothing created
    app.post(&format!("/klanten/{client}/projecten"), &[("name", " ")])
        .await;
    assert!(app
        .get(&format!("/klanten/{client}"))
        .await
        .body
        .contains("Projectnaam is verplicht."));
    assert_eq!(app.count("SELECT COUNT(*) FROM projects").await, 1);
}

#[tokio::test]
async fn another_companys_clients_are_invisible() {
    let (mut app, _a) = TestApp::with_company().await;
    let b = app.create_company("Beta B.V.", "BB").await;
    app.select_company(b).await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant van Beta")]).await;
    let beta_client = app.last_id("clients").await;

    // Back to company A: list, page, edit, archive and projects all refuse.
    let a: i64 = app.count("SELECT MIN(id) FROM companies").await;
    app.select_company(a).await;
    assert!(!app.get("/klanten").await.body.contains("Klant van Beta"));
    assert_eq!(
        app.get(&format!("/klanten/{beta_client}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&format!("/klanten/{beta_client}/bewerken"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    app.get("/klanten").await;
    assert_eq!(
        app.post(&format!("/klanten/{beta_client}/archiveren"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.post(
            &format!("/klanten/{beta_client}/projecten"),
            &[("name", "X")]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(app.count("SELECT COUNT(*) FROM projects").await, 0);
}

#[tokio::test]
async fn pages_without_a_company_redirect_to_create_one() {
    let mut app = TestApp::new().await;
    app.create_user("Anna", "anna@example.test").await;
    app.login("anna@example.test").await;
    assert_eq!(
        app.get("/klanten").await.location.as_deref(),
        Some("/bedrijven/nieuw")
    );
}

#[tokio::test]
async fn csv_downloads_neutralise_formulas_but_exports_stay_exact() {
    let (mut app, company) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "=HYPERLINK(\"http://x\")")])
        .await;
    let r = app
        .get(&format!("/export/clients.csv?company={company}"))
        .await;
    assert!(r.body.contains("'=HYPERLINK"), "{}", r.body);
    // backups and `kwartje export` keep the stored value
    let dir = tempfile::tempdir().unwrap();
    kwartje::export::export_to_dir(
        &app.pool,
        dir.path(),
        &kwartje::export::Filter {
            company_id: None,
            from: None,
            to: None,
        },
    )
    .await
    .unwrap();
    let exact = std::fs::read_to_string(dir.path().join("clients.csv")).unwrap();
    assert!(
        exact.contains("\"=HYPERLINK(\"\"http://x\"\")\""),
        "{exact}"
    );
    assert!(!exact.contains("'=HYPERLINK"));
}
