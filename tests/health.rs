mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn health_needs_a_recent_backup() {
    let mut app = TestApp::new().await;
    let r = app.get("/gezond").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.body.contains("geen backup gevonden"));

    let dir = app.cfg.backup_dir.join("2026-11-25");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = |created: &str| {
        serde_json::json!({
            "app": "kwartje", "app_version": "0", "schema_version": 1, "created_at": created,
            "backup_date": "2026-11-25",
            "app_db": {"file": "app.db", "bytes": 0, "sha256": ""},
            "csv": [], "documents": {"files": 0, "bytes": 0, "linked": 0}
        })
        .to_string()
    };
    std::fs::write(dir.join("manifest.json"), manifest(&kwartje::db::now())).unwrap();
    let r = app.get("/gezond").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.starts_with("ok\n") && r.body.contains("laatste backup:"));

    std::fs::write(dir.join("manifest.json"), manifest("2020-01-01T02:00:00Z")).unwrap();
    assert_eq!(
        app.get("/gezond").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
