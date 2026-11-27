mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn vehicles_are_shared_and_plates_unique() {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/voertuigen/nieuw").await;
    let r = app
        .post(
            "/voertuigen",
            &[("name", "Golf"), ("plate", "ab123c"), ("kind", "private")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let list = app.get("/voertuigen").await;
    assert!(list.body.contains("AB-123-C") && list.body.contains("Privéauto"));

    app.get("/voertuigen/nieuw").await;
    let r = app
        .post(
            "/voertuigen",
            &[
                ("name", "Nog een"),
                ("plate", "AB-123-C"),
                ("kind", "private"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Er is al een voertuig met dit kenteken."));
    let r = app
        .post(
            "/voertuigen",
            &[("name", "X"), ("plate", "ABC"), ("kind", "x")],
        )
        .await;
    assert!(r.body.contains("Nederlands kenteken") && r.body.contains("Kies het soort auto."));

    // visible from another company too
    let b = app.create_company("Beta", "BB").await;
    app.select_company(b).await;
    assert!(app.get("/voertuigen").await.body.contains("AB-123-C"));

    let id = app.last_id("vehicles").await;
    app.get(&format!("/voertuigen/{id}")).await;
    app.post(&format!("/voertuigen/{id}/archiveren"), &[]).await;
    app.get("/voertuigen").await;
    assert!(!app.get("/voertuigen").await.body.contains("AB-123-C"));
    assert_eq!(
        app.count("SELECT COUNT(*) FROM audit_log WHERE table_name = 'vehicles'")
            .await,
        2
    );
}

#[tokio::test]
async fn mileage_rate_per_year_is_upserted() {
    let (mut app, _) = TestApp::with_company().await;
    app.get("/kilometervergoeding").await;
    let r = app
        .post(
            "/kilometervergoeding",
            &[("year", "2026"), ("rate", "0,23")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    app.post(
        "/kilometervergoeding",
        &[("year", "2026"), ("rate", "0,25")],
    )
    .await;
    let page = app.get("/kilometervergoeding").await;
    assert!(page.body.contains("€ 0,25"));
    assert_eq!(app.count("SELECT COUNT(*) FROM mileage_rates").await, 1);
    let r = app
        .post("/kilometervergoeding", &[("year", "1999"), ("rate", "abc")])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'mileage_rates' ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["insert", "update"]);
}

#[tokio::test]
async fn saved_routes_per_company() {
    let (mut app, a) = TestApp::with_company().await;
    app.get("/routes/nieuw").await;
    let r = app
        .post(
            "/routes",
            &[
                ("name", "Kantoor → stadion"),
                ("from_address", "Kantoorweg 1, Utrecht"),
                ("to_address", "Stadionlaan 2, Arnhem"),
                ("km", "64"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = app.last_id("saved_routes").await;
    assert!(app
        .get("/routes")
        .await
        .body
        .contains("Stadionlaan 2, Arnhem"));
    let r = app.post("/routes", &[("name", ""), ("km", "0")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Vul het vertrekadres in.") && r.body.contains("hele kilometers"));

    let b = app.create_company("Beta", "BB").await;
    app.select_company(b).await;
    assert!(!app.get("/routes").await.body.contains("Arnhem"));
    assert_eq!(
        app.get(&format!("/routes/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    app.select_company(a).await;
    app.get(&format!("/routes/{id}")).await;
    app.post(&format!("/routes/{id}/archiveren"), &[]).await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM saved_routes WHERE archived_at IS NOT NULL")
            .await,
        1
    );
}
