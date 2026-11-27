use std::os::unix::fs::MetadataExt;

use kwartje::auth::users;
use kwartje::backup::{self, BackupManifest, ServeLock};
use kwartje::config::Config;
use kwartje::db;
use time::macros::date;

async fn setup() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::for_dir(dir.path());
    let pool = db::connect(&cfg.db_path()).await.unwrap();
    db::migrate(&pool).await.unwrap();
    users::create(
        &pool,
        "Anna",
        "anna@example.test",
        "test-wachtwoord-123",
        None,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO companies (name, invoice_prefix, created_at, updated_at) VALUES ('Alfa', 'AA', 'x', 'x')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let docs = cfg.documents_dir().join("1/2026/receipt");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("1_bon.pdf"), b"%PDF-1.4 bon").unwrap();
    (dir, cfg)
}

async fn company_count(cfg: &Config) -> i64 {
    let pool = db::connect(&cfg.db_path()).await.unwrap();
    let n = sqlx::query_scalar("SELECT COUNT(*) FROM companies")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    n
}

#[tokio::test]
async fn backup_produces_a_complete_verified_folder() {
    let (_dir, cfg) = setup().await;
    // Keep a connection open, like a running server.
    let live = db::connect(&cfg.db_path()).await.unwrap();

    let r = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();
    assert_eq!(r.dir, cfg.backup_dir.join("2026-11-24"));
    for p in [
        "app.db",
        "manifest.json",
        "csv/companies.csv",
        "csv/users.csv",
        "csv/audit_log.csv",
        "documents/1/2026/receipt/1_bon.pdf",
    ] {
        assert!(r.dir.join(p).exists(), "{p} missing");
    }
    let m = BackupManifest::read(&r.dir).unwrap();
    assert_eq!(m.schema_version, db::binary_schema_version());
    assert_eq!(m.documents.files, 1);
    let rows = |name: &str| m.csv.iter().find(|c| c.name == name).unwrap().rows;
    assert_eq!(rows("companies"), 1);
    assert_eq!(rows("users"), 1);
    backup::verify(&r.dir).await.unwrap();

    // No temp folders are left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&cfg.backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".tmp") || n.starts_with(".old"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    live.close().await;
}

#[tokio::test]
async fn unchanged_documents_are_hard_linked_and_same_day_is_replaced() {
    let (_dir, cfg) = setup().await;
    let a = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();
    let b = backup::backup_on(&cfg, date!(2026 - 11 - 25))
        .await
        .unwrap();
    assert_eq!(b.manifest.documents.linked, 1);
    let rel = "documents/1/2026/receipt/1_bon.pdf";
    let ino_a = std::fs::metadata(a.dir.join(rel)).unwrap().ino();
    let ino_b = std::fs::metadata(b.dir.join(rel)).unwrap().ino();
    assert_eq!(ino_a, ino_b);

    // A second run on the same day replaces that day's folder.
    let again = backup::backup_on(&cfg, date!(2026 - 11 - 25))
        .await
        .unwrap();
    assert_eq!(again.dir, b.dir);
    assert_eq!(backup::list_backups(&cfg.backup_dir).unwrap().len(), 2);
}

#[tokio::test]
async fn verify_detects_tampering() {
    let (_dir, cfg) = setup().await;
    let r = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();
    std::fs::write(r.dir.join("csv/companies.csv"), "kapot").unwrap();
    let err = backup::verify(&r.dir).await.unwrap_err().to_string();
    assert!(err.contains("controlesom"), "{err}");
}

#[tokio::test]
async fn restore_round_trip_keeps_previous_data_aside() {
    let (_dir, cfg) = setup().await;
    let r = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();

    // Change data after the backup.
    {
        let pool = db::connect(&cfg.db_path()).await.unwrap();
        sqlx::query("INSERT INTO companies (name, invoice_prefix, created_at, updated_at) VALUES ('Beta', 'BB', 'x', 'x')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }
    std::fs::write(cfg.documents_dir().join("nieuw.pdf"), b"%PDF nieuw").unwrap();
    assert_eq!(company_count(&cfg).await, 2);

    let res = backup::restore(&cfg, &r.dir, true).await.unwrap();
    assert_eq!(company_count(&cfg).await, 1);
    assert!(cfg
        .documents_dir()
        .join("1/2026/receipt/1_bon.pdf")
        .exists());
    assert!(!cfg.documents_dir().join("nieuw.pdf").exists());
    // Nothing was deleted: the previous state is kept aside.
    assert!(res.aside.join("app.db").exists());
    assert!(res.aside.join("documents/nieuw.pdf").exists());
}

#[tokio::test]
async fn restore_refuses_while_serve_runs() {
    let (_dir, cfg) = setup().await;
    let r = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();
    let _lock = ServeLock::acquire(&cfg.data_dir).unwrap();
    assert!(ServeLock::is_held(&cfg.data_dir).unwrap());
    let err = backup::restore(&cfg, &r.dir, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("draait nog"), "{err}");
}

#[tokio::test]
async fn backup_fails_without_database() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::for_dir(dir.path());
    assert!(backup::backup(&cfg).await.is_err());
}

async fn add_document(cfg: &Config, rel: &str, bytes: &[u8]) {
    let pool = db::connect(&cfg.db_path()).await.unwrap();
    std::fs::create_dir_all(cfg.data_dir.join(rel).parent().unwrap()).unwrap();
    std::fs::write(cfg.data_dir.join(rel), bytes).unwrap();
    sqlx::query("INSERT INTO documents (company_id, kind, file_path, sha256, original_name, mime, size, doc_date, created_at, updated_at) VALUES (1, 'receipt', ?, ?, 'bon.pdf', 'application/pdf', ?, '2026-11-01', 't', 't')")
        .bind(rel)
        .bind(kwartje::export::sha256_hex(bytes))
        .bind(bytes.len() as i64)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn documents_are_verified_by_cli_check_and_backup() {
    let (_dir, cfg) = setup().await;
    add_document(&cfg, "documents/1/2026/receipt/1_bon.pdf", b"%PDF-1.4 bon").await;
    let pool = db::connect(&cfg.db_path()).await.unwrap();
    let (n, problems) = kwartje::documents::verify_all(&pool, &cfg.data_dir)
        .await
        .unwrap();
    assert_eq!((n, problems.len()), (1, 0));
    let r = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();
    assert_eq!(r.manifest.documents.verified, 1);
    assert!(r.manifest.documents.problems.is_empty());

    // tamper with the file: check and backup both fail
    std::fs::write(
        cfg.documents_dir().join("1/2026/receipt/1_bon.pdf"),
        b"%PDF-1.4 vervalst",
    )
    .unwrap();
    let (_, problems) = kwartje::documents::verify_all(&pool, &cfg.data_dir)
        .await
        .unwrap();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].message().contains("controlesom klopt niet"));
    let err = backup::backup_on(&cfg, date!(2026 - 11 - 25))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ontbreken of zijn gewijzigd"), "{err}");
    assert!(
        !cfg.backup_dir.join("2026-11-25").exists(),
        "no backup folder for a failed run"
    );

    // missing file
    std::fs::remove_file(cfg.documents_dir().join("1/2026/receipt/1_bon.pdf")).unwrap();
    let (_, problems) = kwartje::documents::verify_all(&pool, &cfg.data_dir)
        .await
        .unwrap();
    assert!(problems[0].missing);
    pool.close().await;
}

#[tokio::test]
async fn restore_test_passes_on_a_good_backup_and_fails_on_a_bad_one() {
    let (_dir, cfg) = setup().await;
    add_document(&cfg, "documents/1/2026/receipt/1_bon.pdf", b"%PDF-1.4 bon").await;
    let b = backup::backup_on(&cfg, date!(2026 - 11 - 24))
        .await
        .unwrap();
    let r = kwartje::restore_test::run(&b.dir).await.unwrap();
    assert!(r.ok(), "{:?}", r.problems);
    assert!(r.lines.iter().any(|l| l.contains("aantallen kloppen")));
    assert!(r.lines.iter().any(|l| l.contains("inlogpagina bereikbaar")));
    // live data untouched
    assert!(cfg.db_path().exists());
    assert_eq!(kwartje::restore_test::latest(&cfg).unwrap(), b.dir);

    // tampered document in the backup
    std::fs::write(
        b.dir.join("documents/1/2026/receipt/1_bon.pdf"),
        b"%PDF kapot",
    )
    .unwrap();
    let r = kwartje::restore_test::run(&b.dir).await.unwrap();
    assert!(!r.ok());
    assert!(
        r.problems[0].contains("controle van de backup mislukt"),
        "{:?}",
        r.problems
    );
}

/// A database at an older schema version: only migrations up to `v` applied.
async fn db_at_version(path: &std::path::Path, v: i64) -> sqlx::SqlitePool {
    let pool = db::connect(path).await.unwrap();
    let mut m = sqlx::migrate::Migrator {
        migrations: db::MIGRATOR.migrations.clone(),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    m.migrations = std::borrow::Cow::Owned(
        db::MIGRATOR
            .iter()
            .filter(|x| x.version <= v)
            .cloned()
            .collect(),
    );
    m.run(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn pending_migrations_take_a_snapshot_first() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let latest = db::binary_schema_version();

    // a new database gets no snapshot
    let fresh = db::connect(&data.join("fresh.db")).await.unwrap();
    assert!(db::migrate_with_snapshot(&fresh, &data)
        .await
        .unwrap()
        .is_none());

    // an older one does, with its data, before migrating
    let pool = db_at_version(&data.join("app.db"), latest - 1).await;
    sqlx::query("INSERT INTO companies (name, invoice_prefix, created_at, updated_at) VALUES ('Alfa', 'AA', 't', 't')")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        db::pending_versions(&db::applied_versions(&pool).await.unwrap()),
        vec![latest]
    );
    let snap = db::migrate_with_snapshot(&pool, &data)
        .await
        .unwrap()
        .expect("snapshot");
    assert!(snap.starts_with(data.join("pre-migration")));
    assert!(snap
        .to_string_lossy()
        .ends_with(&format!("-v{}-naar-v{latest}.db", latest - 1)));
    assert_eq!(db::schema_version(&pool).await.unwrap(), latest);
    let copy = db::connect(&snap).await.unwrap();
    assert_eq!(db::schema_version(&copy).await.unwrap(), latest - 1);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM companies")
        .fetch_one(&copy)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // nothing pending: no new snapshot
    assert!(db::migrate_with_snapshot(&pool, &data)
        .await
        .unwrap()
        .is_none());
    let snapshots = std::fs::read_dir(data.join("pre-migration"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "db")
        })
        .count();
    assert_eq!(snapshots, 1);
}
