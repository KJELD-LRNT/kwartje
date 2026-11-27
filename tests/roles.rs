//! Roles: beheerder (books on behalf of medewerkers), medewerker, and the
//! read-only boekhouder with access per company.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use kwartje::auth::users::{self, Role};

#[tokio::test]
async fn admin_books_for_employees_and_is_not_listed_itself() {
    let (mut app, _company) = TestApp::with_company().await;
    let admin = app.count("SELECT MIN(id) FROM users").await;
    users::set_role(&app.pool, admin, Role::Admin, None)
        .await
        .unwrap();
    let bram = app.create_user("Bram", "bram@example.test").await;
    app.get("/taken/nieuw").await;
    app.post("/taken", &[("name", "Scouting")]).await;
    let task = app.last_id("tasks").await.to_string();

    // the picker shows Bram, preselected, and not the admin
    let page = app.get("/uren").await;
    assert!(page.body.contains("Bram"), "{}", page.body);
    assert!(
        page.body
            .contains(&format!("<option value=\"{bram}\" selected")),
        "{}",
        page.body
    );
    let picker = page
        .body
        .split("name=\"user_id\"")
        .nth(1)
        .and_then(|rest| rest.split("</select>").next())
        .expect("user picker");
    assert!(!picker.contains(&format!("value=\"{admin}\"")), "{picker}");

    let (a, j) = (admin.to_string(), bram.to_string());
    let log = |who: &str| {
        vec![
            ("date", "2026-11-23"),
            ("task_id", task.as_str()),
            ("duration", "2"),
            ("tariff", "auto"),
            ("user_id", who),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<Vec<_>>()
    };
    let refused = log(&a);
    let r = app.post("/uren", &to_pairs(&refused)).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Kies een medewerker."));
    let ok = log(&j);
    let r = app.post("/uren", &to_pairs(&ok)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let (owner, by): (i64, Option<i64>) =
        sqlx::query_as("SELECT user_id, created_by FROM time_entries")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!((owner, by), (bram, Some(admin)));
}

#[tokio::test]
async fn the_last_admin_cannot_be_demoted() {
    let app = TestApp::new().await;
    let a = app.create_user("Anna", "anna@example.test").await;
    let b = app.create_user("Bram", "bram@example.test").await;
    users::set_role(&app.pool, a, Role::Admin, None)
        .await
        .unwrap();
    let err = users::set_role(&app.pool, a, Role::Employee, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("laatste beheerder"), "{err}");
    users::set_role(&app.pool, b, Role::Admin, None)
        .await
        .unwrap();
    users::set_role(&app.pool, a, Role::Employee, None)
        .await
        .unwrap();
    assert_eq!(Role::parse("Boekhouder"), Some(Role::Accountant));
    assert_eq!(Role::parse("beheerder"), Some(Role::Admin));
    assert_eq!(Role::parse("admin"), Some(Role::Admin));
    assert_eq!(Role::parse("baas"), None);
}

#[tokio::test]
async fn admin_pages_are_hidden_from_employees() {
    let (mut app, _) = TestApp::with_company().await;
    for p in ["/gebruikers", "/beveiliging"] {
        assert_eq!(app.get(p).await.status, StatusCode::NOT_FOUND, "{p}");
    }
}

struct Books {
    app: TestApp,
    mine: i64,
    other: i64,
}

/// An accountant logged in, with access to company "Alfa" only.
async fn accountant() -> Books {
    let (mut app, mine) = TestApp::with_company().await;
    let other = app.create_company("Beta", "BB").await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant van Alfa")]).await;
    app.select_company(other).await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant van Beta")]).await;
    let id = app
        .create_user("Boek Houder", "boekhouder@example.test")
        .await;
    users::set_role(&app.pool, id, Role::Accountant, None)
        .await
        .unwrap();
    users::set_company_ids(&app.pool, id, &[mine], None)
        .await
        .unwrap();
    app.new_client();
    app.login("boekhouder@example.test").await;
    Books { app, mine, other }
}

#[tokio::test]
async fn accountant_reads_only_its_companies() {
    let mut b = accountant().await;
    let home = b.app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("alleen leesrechten"));
    let clients = b.app.get("/klanten").await;
    assert!(clients.body.contains("Klant van Alfa"));
    assert!(!clients.body.contains("Klant van Beta"));
    // the switcher only offers Alfa, and switching to Beta is refused
    let r = b
        .app
        .post("/bedrijf/kiezen", &[("company_id", &b.other.to_string())])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(!b.app.get("/klanten").await.body.contains("Klant van Beta"));
    assert_eq!(
        b.app.get(&format!("/bedrijven/{}", b.other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        b.app.get(&format!("/bedrijven/{}", b.mine)).await.status,
        StatusCode::OK
    );
    let list = b.app.get("/bedrijven").await;
    assert!(!list.body.contains("Beta<"), "{}", list.body);
    for p in ["/logboek", "/gebruikers", "/beveiliging"] {
        assert_eq!(b.app.get(p).await.status, StatusCode::NOT_FOUND, "{p}");
    }
}

#[tokio::test]
async fn accountant_cannot_write_anything() {
    let mut b = accountant().await;
    b.app.get("/klanten/nieuw").await;
    let r = b.app.post("/klanten", &[("name", "Stiekem")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("alleen leesrechten"));
    let r = b.app.post("/taken", &[("name", "Stiekem")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        b.app
            .count("SELECT COUNT(*) FROM clients WHERE name = 'Stiekem'")
            .await,
        0
    );
    // own account and signing out still work
    b.app.get("/account").await;
    let r = b.app.post("/account/apparaten/intrekken", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    b.app.get("/").await;
    let r = b.app.post("/uitloggen", &[]).await;
    assert_eq!(r.location.as_deref(), Some("/inloggen"));
}

#[tokio::test]
async fn accountant_exports_only_its_company() {
    let mut b = accountant().await;
    let page = b.app.get("/export").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/export/clients.csv"));
    assert!(!page.body.contains("/export/users.csv"));
    assert!(!page.body.contains("/export/audit_log.csv"));
    let csv = b.app.get("/export/clients.csv").await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(csv.body.contains("Klant van Alfa"));
    assert!(!csv.body.contains("Klant van Beta"));
    assert_eq!(
        b.app
            .get(&format!("/export/clients.csv?company={}", b.other))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        b.app.get("/export/users.csv").await.status,
        StatusCode::NOT_FOUND
    );
    let zip = b.app.get("/export/all.zip").await;
    assert_eq!(zip.status, StatusCode::OK);
}

fn to_pairs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}
