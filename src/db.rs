//! Database pool, migrations and small shared helpers.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

fn base_options(opts: SqliteConnectOptions) -> SqliteConnectOptions {
    opts.foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal)
}

/// Open (and create) the database file in WAL mode.
pub async fn connect(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("aanmaken van {}", parent.display()))?;
    }
    let opts = base_options(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal),
    );
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .with_context(|| format!("openen van database {}", path.display()))?;
    Ok(pool)
}

/// Private in-memory database (one connection, so every query sees the same DB).
pub async fn connect_memory() -> Result<SqlitePool> {
    let opts = base_options(SqliteConnectOptions::from_str("sqlite::memory:")?);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// Migrations whose file was edited after release in a comment only (names
/// removed before the code was published). A database that applied the old
/// text gets the new checksum, so sqlx accepts the edited file. The SQL
/// itself is unchanged.
const COMMENT_ONLY_EDITS: &[(i64, &str)] = &[
    (
        1,
        "03d43d8857469b24ab523fa93622dd1fd57e7eb6ec11280d651de3bc5a02ae2224340f2cc4f284aea4ca4934bcc2d886",
    ),
    (
        2,
        "2e8a59a1957861ce7894d8d8ab1951d341961b21872539306f866dbfcbeacd9664bf542b3b4a839f170d57b0625d7ff6",
    ),
];

async fn repair_checksums(pool: &SqlitePool) -> Result<()> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        return Ok(());
    }
    for (version, old) in COMMENT_ONLY_EDITS {
        let Some(m) = MIGRATOR.iter().find(|m| m.version == *version) else {
            continue;
        };
        let n = sqlx::query(
            "UPDATE _sqlx_migrations SET checksum = ? WHERE version = ? AND lower(hex(checksum)) = ?",
        )
        .bind(m.checksum.as_ref())
        .bind(version)
        .bind(old)
        .execute(pool)
        .await?
        .rows_affected();
        if n > 0 {
            tracing::info!(
                version,
                "checksum bijgewerkt na tekstwijziging in migratiecommentaar"
            );
        }
    }
    Ok(())
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    repair_checksums(pool).await?;
    MIGRATOR
        .run(pool)
        .await
        .context("uitvoeren van migraties")?;
    Ok(())
}

/// Pre-migration snapshots kept in `data_dir/pre-migration/`.
pub const PRE_MIGRATION_KEEP: usize = 3;

/// Versions of the migrations already applied (empty for a new database).
pub async fn applied_versions(pool: &SqlitePool) -> Result<Vec<i64>> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "SELECT version FROM _sqlx_migrations WHERE success = 1 ORDER BY version",
    )
    .fetch_all(pool)
    .await?)
}

/// Migrations this binary would apply to a database that has `applied`.
pub fn pending_versions(applied: &[i64]) -> Vec<i64> {
    MIGRATOR
        .iter()
        .map(|m| m.version)
        .filter(|v| !applied.contains(v))
        .collect()
}

/// Run pending migrations, but first write a consistent copy of the
/// database to `data_dir/pre-migration/` (only when there is existing data
/// and something to apply). Returns the snapshot path, if one was made.
/// Used by `serve` and the CLI; a failed migration rolls back on its own,
/// the snapshot covers mistakes that only show up later.
pub async fn migrate_with_snapshot(
    pool: &SqlitePool,
    data_dir: &Path,
) -> Result<Option<std::path::PathBuf>> {
    let applied = applied_versions(pool).await?;
    let pending = pending_versions(&applied);
    let mut snapshot = None;
    if !applied.is_empty() && !pending.is_empty() {
        let dir = data_dir.join("pre-migration");
        std::fs::create_dir_all(&dir).with_context(|| format!("aanmaken van {}", dir.display()))?;
        let from = applied.iter().max().copied().unwrap_or(0);
        let to = pending.iter().max().copied().unwrap_or(from);
        let stamp = OffsetDateTime::now_utc()
            .format(time::macros::format_description!(
                "[year][month][day]-[hour][minute][second]"
            ))
            .context("tijdstempel")?;
        let path = dir.join(format!("app-{stamp}-v{from}-naar-v{to}.db"));
        sqlx::query("VACUUM INTO ?")
            .bind(path.to_string_lossy().to_string())
            .execute(pool)
            .await
            .with_context(|| format!("kopie vóór migratie naar {}", path.display()))?;
        tracing::info!(path = %path.display(), from, to, "kopie gemaakt vóór databasemigratie");
        prune_snapshots(&dir, PRE_MIGRATION_KEEP)?;
        snapshot = Some(path);
    }
    migrate(pool).await?;
    Ok(snapshot)
}

/// Keep the newest `keep` snapshots (names sort by time).
fn prune_snapshots(dir: &Path, keep: usize) -> Result<()> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "db"))
        .collect();
    files.sort();
    let n = files.len().saturating_sub(keep);
    for old in files.into_iter().take(n) {
        std::fs::remove_file(&old).with_context(|| format!("opruimen van {}", old.display()))?;
    }
    Ok(())
}

/// Highest migration version this binary knows about.
pub fn binary_schema_version() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Highest migration version applied to this database.
pub async fn schema_version(pool: &SqlitePool) -> Result<i64> {
    let v: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(pool)
            .await?;
    Ok(v.unwrap_or(0))
}

/// Current UTC timestamp as stored in the database (`2026-11-25T10:00:00Z`).
pub fn now() -> String {
    timestamp(OffsetDateTime::now_utc())
}

pub fn timestamp(t: OffsetDateTime) -> String {
    t.replace_nanosecond(0)
        .expect("0 ns is valid")
        .format(&Rfc3339)
        .expect("RFC 3339 formatting cannot fail for UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_on_memory_db() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        assert_eq!(
            schema_version(&pool).await.unwrap(),
            binary_schema_version()
        );
    }

    #[tokio::test]
    async fn audit_log_is_append_only() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO audit_log (table_name, row_id, action, at, diff_json) VALUES ('x', 1, 'insert', 'now', '{}')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(sqlx::query("UPDATE audit_log SET row_id = 2")
            .execute(&pool)
            .await
            .is_err());
        assert!(sqlx::query("DELETE FROM audit_log")
            .execute(&pool)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rows_of_two_companies_cannot_be_linked() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        let x = |sql: &'static str| {
            let pool = pool.clone();
            async move { sqlx::query(sql).execute(&pool).await }
        };
        x("INSERT INTO companies (id, name, invoice_prefix, created_at, updated_at) VALUES (1, 'A', 'A', 't', 't'), (2, 'B', 'B', 't', 't')").await.unwrap();
        x("INSERT INTO clients (id, company_id, name, created_at, updated_at) VALUES (10, 1, 'Klant A', 't', 't'), (20, 2, 'Klant B', 't', 't')").await.unwrap();
        x("INSERT INTO projects (id, company_id, client_id, name, created_at, updated_at) VALUES (100, 1, 10, 'P', 't', 't')").await.unwrap();
        // project of company 1 pointing at a client of company 2
        assert!(x("INSERT INTO projects (company_id, client_id, name, created_at, updated_at) VALUES (1, 20, 'X', 't', 't')").await.is_err());
        // task in company 2 using company 1's project
        assert!(x("INSERT INTO tasks (company_id, name, client_id, project_id, created_at, updated_at) VALUES (2, 'T', 20, 100, 't', 't')").await.is_err());
        // project whose client does not match
        x("INSERT INTO clients (id, company_id, name, created_at, updated_at) VALUES (11, 1, 'Klant A2', 't', 't')").await.unwrap();
        assert!(x("INSERT INTO tasks (company_id, name, client_id, project_id, created_at, updated_at) VALUES (1, 'T', 11, 100, 't', 't')").await.is_err());
        // consistent row is fine; lineage defaults to own id
        x("INSERT INTO tasks (company_id, name, client_id, project_id, created_at, updated_at) VALUES (1, 'T', 10, 100, 't', 't')").await.unwrap();
        x("INSERT INTO tariffs (id, company_id, name, unit, price_cents, vat_bp, valid_from, created_at, updated_at) VALUES (7, 1, 'Std', 'hour', 9500, 2100, '2026-01-01', 't', 't')").await.unwrap();
        let lineage: i64 = sqlx::query_scalar("SELECT lineage_id FROM tariffs WHERE id = 7")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(lineage, 7);
    }

    #[tokio::test]
    async fn audit_log_allows_delete_action_and_stays_append_only() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO audit_log (table_name, row_id, action, at, diff_json) VALUES ('invoices', 1, 'delete', 'now', '{}')")
            .execute(&pool).await.unwrap();
        assert!(sqlx::query("DELETE FROM audit_log")
            .execute(&pool)
            .await
            .is_err());
        assert!(sqlx::query("UPDATE audit_log SET row_id = 2")
            .execute(&pool)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn issued_invoices_are_immutable_in_the_database() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        let x = |sql: &'static str| {
            let pool = pool.clone();
            async move { sqlx::query(sql).execute(&pool).await }
        };
        x("INSERT INTO companies (id, name, invoice_prefix, created_at, updated_at) VALUES (1, 'A', 'A', 't', 't')").await.unwrap();
        x("INSERT INTO clients (id, company_id, name, created_at, updated_at) VALUES (1, 1, 'K', 't', 't')").await.unwrap();
        x("INSERT INTO invoices (id, company_id, client_id, period_from, period_to, created_at, updated_at) VALUES (1, 1, 1, '2026-11-01', '2026-11-30', 't', 't')").await.unwrap();
        x("INSERT INTO invoice_lines (invoice_id, position, description, quantity_milli, unit, unit_price_cents, vat_bp, line_total_cents, source, created_at, updated_at) VALUES (1, 1, 'X', 1000, 'uur', 100, 2100, 100, 'manual', 't', 't')").await.unwrap();
        // drafts can change
        x("UPDATE invoices SET intro_text = 'hallo' WHERE id = 1")
            .await
            .unwrap();
        x("UPDATE invoices SET number = 'A-2026-0001', issue_date = '2026-11-30' WHERE id = 1")
            .await
            .unwrap();
        // issued: content and lines frozen, sending allowed
        assert!(x("UPDATE invoices SET intro_text = 'anders' WHERE id = 1")
            .await
            .is_err());
        assert!(x("UPDATE invoices SET number = 'A-2026-0002' WHERE id = 1")
            .await
            .is_err());
        assert!(x("UPDATE invoice_lines SET description = 'Y'")
            .await
            .is_err());
        assert!(x("DELETE FROM invoice_lines").await.is_err());
        assert!(x("DELETE FROM invoices WHERE id = 1").await.is_err());
        assert!(x("INSERT INTO invoice_lines (invoice_id, position, description, quantity_milli, unit, unit_price_cents, vat_bp, line_total_cents, source, created_at, updated_at) VALUES (1, 2, 'Z', 1000, 'uur', 100, 2100, 100, 'manual', 't', 't')").await.is_err());
        x("UPDATE invoices SET sent_at = 'nu', sent_to = 'a@b.nl', updated_at = 'nu' WHERE id = 1").await.unwrap();
    }

    #[tokio::test]
    async fn business_rows_cannot_be_deleted_and_invoiced_entries_are_locked() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        let x = |sql: &'static str| {
            let pool = pool.clone();
            async move { sqlx::query(sql).execute(&pool).await }
        };
        x("INSERT INTO users (id, name, email, password_hash, created_at, updated_at) VALUES (1, 'K', 'k@x', 'h', 't', 't')").await.unwrap();
        x("INSERT INTO companies (id, name, invoice_prefix, created_at, updated_at) VALUES (1, 'A', 'A', 't', 't')").await.unwrap();
        x("INSERT INTO clients (id, company_id, name, created_at, updated_at) VALUES (1, 1, 'K', 't', 't')").await.unwrap();
        x("INSERT INTO tasks (id, company_id, name, created_at, updated_at) VALUES (1, 1, 'T', 't', 't')").await.unwrap();
        x("INSERT INTO tariffs (id, company_id, name, unit, price_cents, vat_bp, valid_from, created_at, updated_at) VALUES (1, 1, 'S', 'hour', 9500, 2100, '2026-01-01', 't', 't')").await.unwrap();
        x("INSERT INTO time_entries (id, company_id, user_id, task_id, client_id, tariff_id, date, minutes, price_cents, vat_bp, created_at, updated_at) VALUES (1, 1, 1, 1, 1, 1, '2026-11-02', 60, 9500, 2100, 't', 't')").await.unwrap();
        x("INSERT INTO invoices (id, company_id, client_id, period_from, period_to, created_at, updated_at) VALUES (1, 1, 1, '2026-11-01', '2026-11-30', 't', 't')").await.unwrap();
        x("INSERT INTO invoice_lines (id, invoice_id, position, description, quantity_milli, unit, unit_price_cents, vat_bp, line_total_cents, source, created_at, updated_at) VALUES (1, 1, 1, 'X', 1000, 'uur', 9500, 2100, 9500, 'hours', 't', 't'), (2, 1, 2, 'Y', 1000, 'uur', 9500, 2100, 9500, 'hours', 't', 't')").await.unwrap();

        // no hard deletes of business rows
        for sql in [
            "DELETE FROM time_entries",
            "DELETE FROM clients",
            "DELETE FROM tasks",
            "DELETE FROM tariffs",
            "DELETE FROM companies",
            "DELETE FROM users",
        ] {
            assert!(x(sql).await.is_err(), "{sql}");
        }
        // an unbilled entry can change and be linked
        x("UPDATE time_entries SET minutes = 90 WHERE id = 1")
            .await
            .unwrap();
        x("UPDATE time_entries SET invoice_line_id = 1 WHERE id = 1")
            .await
            .unwrap();
        // once invoiced: no edits, no archiving, no moving to another line
        assert!(x("UPDATE time_entries SET minutes = 30 WHERE id = 1")
            .await
            .is_err());
        assert!(x("UPDATE time_entries SET price_cents = 1 WHERE id = 1")
            .await
            .is_err());
        assert!(x("UPDATE time_entries SET archived_at = 'nu' WHERE id = 1")
            .await
            .is_err());
        assert!(
            x("UPDATE time_entries SET invoice_line_id = 2 WHERE id = 1")
                .await
                .is_err()
        );
        // but it can be released (draft deleted, credit note) with updated_at
        x("UPDATE time_entries SET invoice_line_id = NULL, updated_at = 'later' WHERE id = 1")
            .await
            .unwrap();
        x("UPDATE time_entries SET minutes = 30 WHERE id = 1")
            .await
            .unwrap();
        // drafts stay deletable
        x("DELETE FROM invoice_lines WHERE invoice_id = 1")
            .await
            .unwrap();
        x("DELETE FROM invoices WHERE id = 1").await.unwrap();
    }

    #[tokio::test]
    async fn stored_document_files_are_immutable() {
        let pool = connect_memory().await.unwrap();
        migrate(&pool).await.unwrap();
        let x = |sql: &'static str| {
            let pool = pool.clone();
            async move { sqlx::query(sql).execute(&pool).await }
        };
        x("INSERT INTO companies (id, name, invoice_prefix, created_at, updated_at) VALUES (1, 'A', 'A', 't', 't'), (2, 'B', 'B', 't', 't')").await.unwrap();
        x("INSERT INTO documents (id, company_id, kind, file_path, sha256, original_name, mime, size, doc_date, created_at, updated_at) VALUES (1, 1, 'receipt', 'pending/abc', 'abc', 'bon.pdf', 'application/pdf', 10, '2026-11-01', 't', 't'), (2, 1, 'sales_invoice', 'documents/1/2026/sales_invoice/2_A-2026-0001.pdf', 'def', 'A-2026-0001.pdf', 'application/pdf', 20, '2026-11-01', 't', 't')").await.unwrap();
        // the one step from the temporary path is allowed, then the path is fixed
        x("UPDATE documents SET file_path = 'documents/1/2026/receipt/1_bon.pdf' WHERE id = 1")
            .await
            .unwrap();
        assert!(
            x("UPDATE documents SET file_path = 'elders.pdf' WHERE id = 1")
                .await
                .is_err()
        );
        for sql in [
            "UPDATE documents SET sha256 = 'x' WHERE id = 1",
            "UPDATE documents SET size = 1 WHERE id = 1",
            "UPDATE documents SET mime = 'image/png' WHERE id = 1",
            "UPDATE documents SET company_id = 2 WHERE id = 1",
            "UPDATE documents SET kind = 'sales_invoice' WHERE id = 1",
            "UPDATE documents SET kind = 'other' WHERE id = 2",
            "DELETE FROM documents WHERE id = 1",
        ] {
            assert!(x(sql).await.is_err(), "{sql}");
        }
        // metadata stays editable
        x("UPDATE documents SET kind = 'contract', supplier = 'KPN', amount_cents = 100, archived_at = 'nu' WHERE id = 1").await.unwrap();
    }

    #[test]
    fn prune_keeps_the_newest_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        for n in [
            "app-20260101-000000-v1-naar-v2.db",
            "app-20260201-000000-v2-naar-v3.db",
            "app-20260301-000000-v3-naar-v4.db",
            "app-20260401-000000-v4-naar-v5.db",
            "notitie.txt",
        ] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        prune_snapshots(dir.path(), 3).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "app-20260201-000000-v2-naar-v3.db",
                "app-20260301-000000-v3-naar-v4.db",
                "app-20260401-000000-v4-naar-v5.db",
                "notitie.txt"
            ]
        );
    }

    #[tokio::test]
    async fn bundled_sqlite_is_recent() {
        let pool = connect_memory().await.unwrap();
        let v: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        eprintln!("SQLite {v}");
        let parts: Vec<u32> = v.split('.').map(|p| p.parse().unwrap()).collect();
        // libsqlite3-sys 0.30 bundled 3.46.0 (RUSTSEC-2026-0459); require newer.
        assert!((parts[0], parts[1]) > (3, 46), "{v}");
    }

    #[test]
    fn timestamp_format() {
        let t = time::macros::datetime!(2026-11-25 10:00:00.123 UTC);
        assert_eq!(timestamp(t), "2026-11-25T10:00:00Z");
    }
}
