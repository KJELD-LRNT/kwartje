//! `kwartje restore-test`: restore a backup into a temporary folder (never
//! the live data), verify it, compare row counts with the manifest and boot
//! the app against it. Run quarterly by a systemd timer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::backup::{self, BackupManifest};
use crate::config::Config;
use crate::{db, documents, web};

#[derive(Debug, Default)]
pub struct Report {
    pub backup: PathBuf,
    pub lines: Vec<String>,
    pub problems: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Tables whose row count differs from the manifest: `(table, manifest, actual)`.
pub fn count_mismatches(
    expected: &[(String, usize)],
    actual: &[(String, usize)],
) -> Vec<(String, usize, usize)> {
    expected
        .iter()
        .filter_map(|(t, n)| {
            let got = actual
                .iter()
                .find(|(a, _)| a == t)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            (got != *n).then(|| (t.clone(), *n, got))
        })
        .collect()
}

fn copy_tree(src: &Path, dst: &Path) -> Result<u64> {
    let mut n = 0;
    std::fs::create_dir_all(dst)?;
    if !src.exists() {
        return Ok(0);
    }
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let target = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            n += copy_tree(&e.path(), &target)?;
        } else if e.file_type()?.is_file() {
            std::fs::copy(e.path(), &target)?;
            n += 1;
        }
    }
    Ok(n)
}

/// The newest backup folder in `backup_dir`.
pub fn latest(cfg: &Config) -> Result<PathBuf> {
    backup::list_backups(&cfg.backup_dir)?
        .into_iter()
        .next_back()
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow::anyhow!("geen backups gevonden in {}", cfg.backup_dir.display()))
}

pub async fn run(dir: &Path) -> Result<Report> {
    let mut r = Report {
        backup: dir.to_path_buf(),
        ..Default::default()
    };

    // 1. Checksums, SQLite integrity, documents, schema version.
    let manifest: BackupManifest = match backup::verify(dir).await {
        Ok(m) => m,
        Err(e) => {
            r.problems
                .push(format!("controle van de backup mislukt: {e:#}"));
            return Ok(r);
        }
    };
    r.lines.push(format!(
        "Backup van {} (schemaversie {}) is intact.",
        manifest.backup_date, manifest.schema_version
    ));

    // 2. Restore into a temporary data folder.
    let tmp = tempfile::tempdir().context("tijdelijke map")?;
    let mut cfg = Config::for_dir(tmp.path());
    cfg.listen = "127.0.0.1:0".parse().expect("valid address");
    std::fs::create_dir_all(&cfg.data_dir)?;
    std::fs::copy(dir.join("app.db"), cfg.db_path()).context("app.db kopiëren")?;
    let files = copy_tree(&dir.join("documents"), &cfg.documents_dir())?;
    let pool = db::connect(&cfg.db_path()).await?;
    db::migrate(&pool).await?;
    r.lines.push(format!(
        "Teruggezet in een tijdelijke map ({files} documentbestanden)."
    ));

    // 3. Row counts against the manifest.
    let expected: Vec<(String, usize)> = manifest
        .csv
        .iter()
        .map(|c| (c.name.clone(), c.rows))
        .collect();
    let mut actual = Vec::new();
    for (t, _) in &expected {
        if crate::export::find(t).is_none() {
            continue;
        }
        // Safe: `t` was just checked against the static export registry.
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {t}")))
            .fetch_one(&pool)
            .await
            .unwrap_or(-1);
        actual.push((t.clone(), n.max(0) as usize));
    }
    let mismatches = count_mismatches(&expected, &actual);
    if mismatches.is_empty() {
        let rows: usize = expected.iter().map(|(_, n)| n).sum();
        r.lines.push(format!(
            "{} tabellen, {rows} rijen: aantallen kloppen met het manifest.",
            expected.len()
        ));
    }
    for (t, want, got) in mismatches {
        r.problems
            .push(format!("tabel {t}: {got} rijen, manifest zegt {want}"));
    }

    // 4. Documents against their checksums.
    let (checked, problems) = documents::verify_all(&pool, &cfg.data_dir).await?;
    if problems.is_empty() {
        r.lines
            .push(format!("{checked} documenten gecontroleerd: intact."));
    }
    r.problems.extend(problems.iter().map(|p| p.message()));

    // 5. Boot the app against the restored data.
    let app = web::app_with(
        Arc::new(cfg.clone()),
        pool.clone(),
        web::Services {
            pdf: Arc::new(crate::pdf::FakePdf { fail: false }),
            mailer: crate::mail::Mailer::Disabled,
        },
    )
    .await?;
    let resp = app
        .oneshot(Request::get("/inloggen").body(Body::empty())?)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.status() == StatusCode::OK {
        r.lines
            .push("De app start op de teruggezette gegevens (inlogpagina bereikbaar).".into());
    } else {
        r.problems
            .push(format!("de app geeft {} op de inlogpagina", resp.status()));
    }
    pool.close().await;
    Ok(r)
}

/// CLI entry: prints the report, errors when there are problems.
pub async fn cli(cfg: &Config, dir: Option<PathBuf>) -> Result<()> {
    let dir = match dir {
        Some(d) => d,
        None => latest(cfg)?,
    };
    let r = run(&dir).await?;
    println!("Hersteltest van {}", r.backup.display());
    for l in &r.lines {
        println!("  ✓ {l}");
    }
    for p in &r.problems {
        println!("  ✗ {p}");
    }
    if !r.ok() {
        bail!(
            "hersteltest mislukt: {} probleem/problemen",
            r.problems.len()
        );
    }
    println!("Hersteltest geslaagd.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatches() {
        let e = vec![("users".to_string(), 2), ("companies".to_string(), 1)];
        let a = vec![("users".to_string(), 2), ("companies".to_string(), 0)];
        assert_eq!(
            count_mismatches(&e, &a),
            vec![("companies".to_string(), 1, 0)]
        );
        assert!(count_mismatches(&e, &e).is_empty());
    }
}
