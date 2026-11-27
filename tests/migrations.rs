//! Migrations on a database with data. `fixtures/schema-v10.db` was filled
//! through the real app at schema version 10 (seed data, an issued invoice
//! with its PDF document and a payment, a converted quote, a recurring
//! draft). Every later migration runs against a copy of it here, and the
//! current code must still read all of it. See `fixtures/README.md`.

use kwartje::{db, export};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/schema-v10.db");

async fn table_counts(pool: &sqlx::SqlitePool) -> Vec<(String, i64)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table'
           AND name NOT LIKE 'sqlite_%' AND name NOT IN ('_sqlx_migrations', 'tower_sessions')
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut out = Vec::new();
    for t in tables {
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM \"{t}\"")))
                .fetch_one(pool)
                .await
                .unwrap();
        out.push((t, n));
    }
    out
}

#[tokio::test]
async fn all_migrations_keep_the_data_of_a_filled_database() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let path = data.join("app.db");
    std::fs::copy(FIXTURE, &path).unwrap();
    let pool = db::connect(&path).await.unwrap();
    assert_eq!(db::schema_version(&pool).await.unwrap(), 10);
    let before = table_counts(&pool).await;
    assert!(before.iter().any(|(t, n)| t == "time_entries" && *n > 0));

    let snap = db::migrate_with_snapshot(&pool, &data).await.unwrap();
    assert!(snap.is_some(), "an older database gets a snapshot first");
    assert_eq!(
        db::schema_version(&pool).await.unwrap(),
        db::binary_schema_version()
    );

    // no table lost rows
    let after = table_counts(&pool).await;
    for (t, n) in &before {
        let m = after.iter().find(|(a, _)| a == t).map(|(_, m)| *m);
        assert_eq!(m, Some(*n), "table {t}");
    }
    // every reference still resolves and the file is sound
    let fk: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(fk.is_empty(), "{fk:?}");
    let ok: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ok, "ok");

    // the issued invoice is intact, linked to its PDF and still immutable
    let (number, doc_kind): (String, String) = sqlx::query_as(
        "SELECT i.number, d.kind FROM invoices i JOIN documents d ON d.id = i.pdf_document_id
         WHERE i.number IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (number.as_str(), doc_kind.as_str()),
        ("VA-2026-0001", "sales_invoice")
    );
    assert!(
        sqlx::query("UPDATE invoices SET intro_text = 'x' WHERE number IS NOT NULL")
            .execute(&pool)
            .await
            .is_err()
    );

    // the rebuilt trip table: readings and km intact, triggers back in place
    let bad: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mileage_entries
         WHERE odometer_start IS NULL OR km != odometer_end - odometer_start",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bad, 0);
    assert!(sqlx::query("DELETE FROM mileage_entries")
        .execute(&pool)
        .await
        .is_err());
    let modes: Vec<(String, String)> =
        sqlx::query_as("SELECT DISTINCT kind, odometer_mode FROM vehicles ORDER BY kind")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(modes
        .iter()
        .all(|(k, m)| (k == "private" && m == "km") || (k == "business" && m == "odometer")));
    assert!(
        sqlx::query("UPDATE vehicles SET odometer_mode = 'km' WHERE kind = 'business'")
            .execute(&pool)
            .await
            .map(|r| r.rows_affected() == 0)
            .unwrap_or(true)
    );

    // today's code reads all of it: full export, invoice list, documents
    let out = tmp.path().join("export");
    let m = export::export_to_dir(
        &pool,
        &out,
        &export::Filter {
            company_id: None,
            from: None,
            to: None,
        },
    )
    .await
    .unwrap();
    for e in &m.tables {
        if let Some((_, n)) = before.iter().find(|(t, _)| *t == e.name) {
            assert_eq!(e.rows as i64, *n, "export of {}", e.name);
        }
    }
    let company: i64 =
        sqlx::query_scalar("SELECT company_id FROM invoices WHERE number IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    let rows = kwartje::modules::invoices::list::rows(&pool, company, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    let docs = kwartje::modules::documents::list(&pool, company, &Default::default())
        .await
        .unwrap();
    assert!(!docs.is_empty());
}
