mod common;

use axum::http::StatusCode;
use common::TestApp;
use kwartje::tariff::{self, Context};

fn tariff_form<'a>(
    name: &'a str,
    price: &'a str,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut f = vec![
        ("name", name),
        ("unit", "hour"),
        ("price", price),
        ("vat_bp", "2100"),
        ("valid_from", "2026-01-01"),
    ];
    f.extend_from_slice(extra);
    f
}

#[tokio::test]
async fn defaults_cannot_overlap_at_the_same_level() {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/tarieven/nieuw").await;
    let r = app
        .post(
            "/tarieven",
            &tariff_form("Standaard", "95,00", &[("is_default", "1")]),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    app.get("/tarieven/nieuw").await;
    let r = app
        .post(
            "/tarieven",
            &tariff_form("Spoed", "140", &[("is_default", "1")]),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Er is al een standaardtarief (Standaard)"));

    // As a non-default it is fine, and a € 0 tariff is allowed.
    app.post("/tarieven", &tariff_form("Spoed", "140", &[]))
        .await;
    app.get("/tarieven/nieuw").await;
    let r = app
        .post("/tarieven", &tariff_form("Kennismaking", "0", &[]))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(app.count("SELECT COUNT(*) FROM tariffs").await, 3);
    let list = app.get("/tarieven").await;
    assert!(list.body.contains("€ 95,00") && list.body.contains("€ 0,00"));
    assert!(list.body.contains("standaard"));
}

#[tokio::test]
async fn project_sets_the_client_and_must_match_it() {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant A")]).await;
    let a = app.last_id("clients").await;
    app.post("/klanten", &[("name", "Klant B")]).await;
    let b = app.last_id("clients").await;
    app.get(&format!("/klanten/{a}")).await;
    app.post(&format!("/klanten/{a}/projecten"), &[("name", "Project A")])
        .await;
    let pa = app.last_id("projects").await;

    app.get("/tarieven/nieuw").await;
    let pa_s = pa.to_string();
    let r = app
        .post(
            "/tarieven",
            &tariff_form("Project", "120", &[("project_id", &pa_s)]),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let client_of: Option<i64> =
        sqlx::query_scalar("SELECT client_id FROM tariffs WHERE project_id = ?")
            .bind(pa)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(client_of, Some(a));

    app.get("/tarieven/nieuw").await;
    let b_s = b.to_string();
    let r = app
        .post(
            "/tarieven",
            &tariff_form("Fout", "120", &[("project_id", &pa_s), ("client_id", &b_s)]),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r
        .body
        .contains("Dit project hoort niet bij de gekozen klant."));

    // client page lists its tariffs
    assert!(app
        .get(&format!("/klanten/{a}"))
        .await
        .body
        .contains("Project"));
}

#[tokio::test]
async fn new_price_creates_a_version_and_old_dates_keep_the_old_price() {
    let (mut app, company) = TestApp::with_company().await;
    app.get("/tarieven/nieuw").await;
    app.post(
        "/tarieven",
        &tariff_form("Standaard", "95", &[("is_default", "1")]),
    )
    .await;
    let id = app.last_id("tariffs").await;

    app.get(&format!("/tarieven/{id}/nieuwe-prijs")).await;
    // a date before the current start is refused
    let r = app
        .post(
            &format!("/tarieven/{id}/nieuwe-prijs"),
            &[("price", "100"), ("from", "2025-06-01")],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = app
        .post(
            &format!("/tarieven/{id}/nieuwe-prijs"),
            &[("price", "100,00"), ("from", "01-01-2027")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let new_id = app.last_id("tariffs").await;
    assert_ne!(new_id, id);

    let (old_to, lineage): (Option<String>, i64) =
        sqlx::query_as("SELECT valid_to, lineage_id FROM tariffs WHERE id = ?")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(old_to.as_deref(), Some("2026-12-31"));
    let new_lineage: i64 = app
        .count(&format!(
            "SELECT lineage_id FROM tariffs WHERE id = {new_id}"
        ))
        .await;
    assert_eq!(new_lineage, lineage);

    let mut conn = app.pool.acquire().await.unwrap();
    let cands = tariff::hour_candidates(&mut conn, company).await.unwrap();
    let at = |date: &'static str| Context {
        task_billable: true,
        task_lineage: None,
        client_id: None,
        project_id: None,
        date,
    };
    assert_eq!(
        tariff::resolve(&at("2026-12-31"), &cands)
            .unwrap()
            .0
            .price_cents,
        9500
    );
    assert_eq!(
        tariff::resolve(&at("2027-01-01"), &cands)
            .unwrap()
            .0
            .price_cents,
        10_000
    );
    drop(conn);

    // a second price change must start from the newest version
    app.get(&format!("/tarieven/{id}/nieuwe-prijs")).await;
    let r = app
        .post(
            &format!("/tarieven/{id}/nieuwe-prijs"),
            &[("price", "110"), ("from", "2028-01-01")],
        )
        .await;
    assert!(r.body.contains("Er is al een nieuwere versie"));
    // audited: update of the old version + insert of the new one
    assert_eq!(
        app.count("SELECT COUNT(*) FROM audit_log WHERE table_name = 'tariffs'")
            .await,
        3
    );
}

#[tokio::test]
async fn other_companys_tariffs_are_invisible() {
    let (mut app, a) = TestApp::with_company().await;
    let b = app.create_company("Beta", "BB").await;
    app.select_company(b).await;
    app.get("/tarieven/nieuw").await;
    app.post("/tarieven", &tariff_form("Beta-tarief", "50", &[]))
        .await;
    let tb = app.last_id("tariffs").await;
    app.select_company(a).await;
    assert!(!app.get("/tarieven").await.body.contains("Beta-tarief"));
    assert_eq!(
        app.get(&format!("/tarieven/{tb}")).await.status,
        StatusCode::NOT_FOUND
    );
    app.get("/tarieven").await;
    assert_eq!(
        app.post(&format!("/tarieven/{tb}/archiveren"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
