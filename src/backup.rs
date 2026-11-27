//! `kwartje backup` / `kwartje restore`.
//!
//! A backup is a folder `backup_dir/YYYY-MM-DD/` with:
//! - `app.db`: consistent copy via `VACUUM INTO` (safe while `serve` runs)
//! - `csv/`: full CSV export, made from that copy so both always agree
//! - `documents/`: the document store; unchanged files are hard-linked to the
//!   previous backup, so 42 retained backups don't cost 42× the disk
//! - `manifest.json`: versions, row counts and SHA-256 checksums
//!
//! It is built in a temporary folder and renamed into place at the end, so a
//! half-finished backup never looks like a real one. Afterwards old backups are
//! pruned to `keep_daily` dailies + one per month for `keep_monthly` months.

use std::collections::BTreeSet;
use std::fs::{self, File, TryLockError};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use time::Date;

use crate::config::Config;
use crate::export::{self, sha256_hex, Filter, TableEntry};
use crate::{db, format};

// ---------- locks ----------

/// Exclusive lock held by `serve` on `data_dir/.serve.lock`.
pub struct ServeLock {
    _file: File,
}

fn try_lock(path: &Path, what: &str) -> Result<File> {
    let file = File::create(path).with_context(|| format!("aanmaken van {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!("{what}"),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("vergrendelen van {}", path.display()))
        }
    }
}

impl ServeLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        let file = try_lock(
            &data_dir.join(".serve.lock"),
            &format!(
                "kwartje draait al op {} (of een herstel is bezig)",
                data_dir.display()
            ),
        )?;
        Ok(ServeLock { _file: file })
    }

    /// True when a running `serve` (or restore) holds the lock.
    pub fn is_held(data_dir: &Path) -> Result<bool> {
        let path = data_dir.join(".serve.lock");
        if !path.exists() {
            return Ok(false);
        }
        let file = File::open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                file.unlock()?;
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(e).context("controleren van serve-lock"),
        }
    }
}

// ---------- manifest ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSum {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocumentsSummary {
    pub files: u64,
    pub bytes: u64,
    /// Files hard-linked to the previous backup (unchanged).
    pub linked: u64,
    /// Documents in the database whose file was found with a matching SHA-256.
    #[serde(default)]
    pub verified: u64,
    /// Missing or changed files (the backup fails when this is not empty).
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CsvSummary {
    pub name: String,
    pub file: String,
    pub rows: usize,
    pub sha256: String,
}

impl From<&TableEntry> for CsvSummary {
    fn from(t: &TableEntry) -> Self {
        CsvSummary {
            name: t.name.clone(),
            file: format!("csv/{}", t.file),
            rows: t.rows,
            sha256: t.sha256.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub app: String,
    pub app_version: String,
    pub schema_version: i64,
    pub created_at: String,
    pub backup_date: String,
    pub app_db: FileSum,
    pub csv: Vec<CsvSummary>,
    pub documents: DocumentsSummary,
}

impl BackupManifest {
    pub fn read(dir: &Path) -> Result<Self> {
        let p = dir.join("manifest.json");
        let text = fs::read_to_string(&p).with_context(|| format!("lezen van {}", p.display()))?;
        serde_json::from_str(&text).with_context(|| format!("ongeldige {}", p.display()))
    }
}

// ---------- helpers ----------

fn sha256_file(path: &Path) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let mut f = File::open(path).with_context(|| format!("openen van {}", path.display()))?;
    let mut h = Sha256::new();
    let bytes = std::io::copy(&mut f, &mut h)?;
    Ok((
        bytes,
        h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

async fn open_copy(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite:")?
        .filename(path)
        .read_only(true)
        .journal_mode(SqliteJournalMode::Delete);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("openen van {}", path.display()))
}

async fn integrity_check(pool: &SqlitePool) -> Result<()> {
    let r: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await?;
    if r != "ok" {
        bail!("integriteitscontrole mislukt: {r}");
    }
    Ok(())
}

fn sync_dir(dir: &Path) {
    // Best effort: make the rename durable.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Backup folders (`YYYY-MM-DD`) in `backup_dir`, oldest first.
pub fn list_backups(backup_dir: &Path) -> Result<Vec<(Date, PathBuf)>> {
    let mut out = Vec::new();
    if !backup_dir.exists() {
        return Ok(out);
    }
    for e in fs::read_dir(backup_dir)? {
        let e = e?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(d) = format::parse_iso_date(&name).filter(|d| format::iso_date(*d) == name) {
            out.push((d, e.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Copy the document tree, hard-linking files unchanged since `prev`.
fn copy_documents(src: &Path, dst: &Path, prev: Option<&Path>) -> Result<DocumentsSummary> {
    let mut sum = DocumentsSummary::default();
    fs::create_dir_all(dst)?;
    if !src.exists() {
        return Ok(sum);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in fs::read_dir(src.join(&rel))? {
            let e = e?;
            let name = e.file_name();
            let rel_child = rel.join(&name);
            let ft = e.file_type()?;
            if ft.is_dir() {
                fs::create_dir_all(dst.join(&rel_child))?;
                stack.push(rel_child);
                continue;
            }
            if !ft.is_file() || name.to_string_lossy().starts_with(".upload-") {
                continue; // symlinks and in-progress uploads
            }
            let meta = e.metadata()?;
            let target = dst.join(&rel_child);
            let linked = prev.map(|p| p.join(&rel_child)).is_some_and(|p| {
                fs::metadata(&p).is_ok_and(|pm| {
                    pm.len() == meta.len()
                        && pm.modified().ok() == meta.modified().ok()
                        && fs::hard_link(&p, &target).is_ok()
                })
            });
            if linked {
                sum.linked += 1;
            } else {
                fs::copy(e.path(), &target)
                    .with_context(|| format!("kopiëren van {}", e.path().display()))?;
                if let Ok(m) = meta.modified() {
                    File::options().write(true).open(&target)?.set_modified(m)?;
                }
            }
            sum.files += 1;
            sum.bytes += meta.len();
        }
    }
    Ok(sum)
}

// ---------- prune ----------

/// Which backup dates to delete: keep the newest `keep_daily`, plus the newest
/// backup of each of the newest `keep_monthly` months.
pub fn select_prune(dates: &[Date], keep_daily: usize, keep_monthly: usize) -> Vec<Date> {
    let mut sorted: Vec<Date> = dates.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted.dedup();
    let mut keep: BTreeSet<Date> = sorted.iter().take(keep_daily).copied().collect();
    let mut months_seen = BTreeSet::new();
    for d in &sorted {
        if months_seen.len() >= keep_monthly {
            break;
        }
        if months_seen.insert((d.year(), d.month() as u8)) {
            keep.insert(*d);
        }
    }
    let mut out: Vec<Date> = sorted.into_iter().filter(|d| !keep.contains(d)).collect();
    out.sort();
    out
}

pub fn prune(cfg: &Config) -> Result<Vec<PathBuf>> {
    let backups = list_backups(&cfg.backup_dir)?;
    let dates: Vec<Date> = backups.iter().map(|(d, _)| *d).collect();
    let remove = select_prune(&dates, cfg.backup.keep_daily, cfg.backup.keep_monthly);
    let mut removed = Vec::new();
    for (d, p) in backups {
        if remove.contains(&d) {
            fs::remove_dir_all(&p).with_context(|| format!("verwijderen van {}", p.display()))?;
            removed.push(p);
        }
    }
    Ok(removed)
}

// ---------- backup ----------

#[derive(Debug)]
pub struct BackupResult {
    pub dir: PathBuf,
    pub manifest: BackupManifest,
    pub pruned: Vec<PathBuf>,
}

pub async fn backup(cfg: &Config) -> Result<BackupResult> {
    backup_on(cfg, format::today()).await
}

/// Backup into the folder for `date` (replacing an earlier backup of that day).
pub async fn backup_on(cfg: &Config, date: Date) -> Result<BackupResult> {
    let db_path = cfg.db_path();
    if !db_path.exists() {
        bail!("database {} bestaat niet", db_path.display());
    }
    fs::create_dir_all(&cfg.backup_dir)
        .with_context(|| format!("aanmaken van {}", cfg.backup_dir.display()))?;
    let _lock = try_lock(
        &cfg.backup_dir.join(".lock"),
        "er draait al een andere backup",
    )?;

    let name = format::iso_date(date);
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let tmp = cfg.backup_dir.join(format!(".tmp-{name}-{stamp}"));
    fs::create_dir_all(&tmp)?;
    let manifest = match build(cfg, &tmp, &name).await {
        Ok(m) => m,
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };

    let final_dir = cfg.backup_dir.join(&name);
    if final_dir.exists() {
        let old = cfg.backup_dir.join(format!(".old-{name}-{stamp}"));
        fs::rename(&final_dir, &old)?;
        fs::rename(&tmp, &final_dir)?;
        fs::remove_dir_all(&old)?;
    } else {
        fs::rename(&tmp, &final_dir)?;
    }
    sync_dir(&cfg.backup_dir);
    let pruned = prune(cfg)?;
    Ok(BackupResult {
        dir: final_dir,
        manifest,
        pruned,
    })
}

async fn build(cfg: &Config, tmp: &Path, name: &str) -> Result<BackupManifest> {
    // 1. Consistent copy of the live database.
    let copy = tmp.join("app.db");
    {
        let pool = db::connect(&cfg.db_path()).await?;
        let target = copy.to_string_lossy().to_string();
        sqlx::query("VACUUM INTO ?")
            .bind(&target)
            .execute(&pool)
            .await
            .context("VACUUM INTO mislukt")?;
        pool.close().await;
    }

    // 2. Check it and export CSV from the copy.
    let pool = open_copy(&copy).await?;
    integrity_check(&pool).await?;
    let schema_version = db::schema_version(&pool).await?;
    let csv_manifest = export::export_to_dir(&pool, &tmp.join("csv"), &Filter::default()).await?;
    pool.close().await;
    // export_to_dir writes its own manifest; the backup manifest supersedes it.
    let _ = fs::remove_file(tmp.join("csv/manifest.json"));
    File::open(&copy)?.sync_all()?;
    let (bytes, sha) = sha256_file(&copy)?;

    // 3. Documents, hard-linked to the newest existing backup where unchanged.
    let prev = list_backups(&cfg.backup_dir)?
        .into_iter()
        .map(|(_, p)| p.join("documents"))
        .next_back();
    let mut documents = copy_documents(
        &cfg.documents_dir(),
        &tmp.join("documents"),
        prev.as_deref(),
    )?;

    // Every document in the copied database must be in the backup, intact.
    let pool = open_copy(&copy).await?;
    let (checked, problems) = crate::documents::verify_all(&pool, tmp).await?;
    pool.close().await;
    documents.problems = problems.iter().map(|p| p.message()).collect();
    documents.verified = checked - problems.len() as u64;
    if !problems.is_empty() {
        bail!(
            "{} document(en) ontbreken of zijn gewijzigd:\n{}",
            problems.len(),
            documents.problems.join("\n")
        );
    }

    // 4. Manifest.
    let manifest = BackupManifest {
        app: "kwartje".into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        schema_version,
        created_at: db::now(),
        backup_date: name.to_string(),
        app_db: FileSum {
            file: "app.db".into(),
            bytes,
            sha256: sha,
        },
        csv: csv_manifest.tables.iter().map(CsvSummary::from).collect(),
        documents,
    };
    let mut f = File::create(tmp.join("manifest.json"))?;
    f.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    f.sync_all()?;
    sync_dir(tmp);
    Ok(manifest)
}

// ---------- verify / restore ----------

/// Check a backup folder: manifest, checksums, SQLite integrity, schema version.
pub async fn verify(dir: &Path) -> Result<BackupManifest> {
    let m = BackupManifest::read(dir)?;
    if m.app != "kwartje" {
        bail!("{} is geen kwartje-backup", dir.display());
    }
    if m.schema_version > db::binary_schema_version() {
        bail!(
            "backup heeft schemaversie {}, deze versie van kwartje kent maximaal {}; installeer eerst een nieuwere versie",
            m.schema_version,
            db::binary_schema_version()
        );
    }
    let (_, sha) = sha256_file(&dir.join("app.db"))?;
    if sha != m.app_db.sha256 {
        bail!("controlesom van app.db klopt niet; de backup is beschadigd");
    }
    for c in &m.csv {
        let bytes = fs::read(dir.join(&c.file)).with_context(|| format!("lezen van {}", c.file))?;
        if sha256_hex(&bytes) != c.sha256 {
            bail!("controlesom van {} klopt niet", c.file);
        }
    }
    let pool = open_copy(&dir.join("app.db")).await?;
    integrity_check(&pool).await?;
    let (_, problems) = crate::documents::verify_all(&pool, dir).await?;
    pool.close().await;
    if !problems.is_empty() {
        bail!(
            "de backup is onvolledig: {}",
            problems
                .iter()
                .map(|p| p.message())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(m)
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("herstel"))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    if !src.exists() {
        return Ok(());
    }
    for e in fs::read_dir(src)? {
        let e = e?;
        let target = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else if e.file_type()?.is_file() {
            fs::copy(e.path(), &target)?;
            if let Ok(m) = e.metadata()?.modified() {
                File::options().write(true).open(&target)?.set_modified(m)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct RestoreResult {
    pub manifest: BackupManifest,
    /// Where the previous `app.db` and `documents/` were moved to.
    pub aside: PathBuf,
}

/// Replace `app.db` and `documents/` with those from a backup. The current
/// data is moved to `data_dir/pre-restore-<timestamp>/`, never deleted.
pub async fn restore(cfg: &Config, dir: &Path, assume_yes: bool) -> Result<RestoreResult> {
    let manifest = verify(dir).await?;
    fs::create_dir_all(&cfg.data_dir)?;
    if ServeLock::is_held(&cfg.data_dir)? {
        bail!("kwartje draait nog; stop eerst de service (systemctl stop kwartje)");
    }
    if !assume_yes {
        println!(
            "Backup van {} (schemaversie {}, {} documenten).",
            manifest.backup_date, manifest.schema_version, manifest.documents.files
        );
        println!(
            "De huidige gegevens in {} worden opzij gezet en vervangen.",
            cfg.data_dir.display()
        );
        if !confirm("Typ 'herstel' om door te gaan:")? {
            bail!("herstel afgebroken");
        }
    }
    // Keep serve from starting while we swap files.
    let _lock = ServeLock::acquire(&cfg.data_dir)?;

    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let aside = cfg.data_dir.join(format!("pre-restore-{stamp}"));
    fs::create_dir_all(&aside)?;
    for name in ["app.db", "app.db-wal", "app.db-shm", "documents"] {
        let p = cfg.data_dir.join(name);
        if p.exists() {
            fs::rename(&p, aside.join(name))
                .with_context(|| format!("verplaatsen van {}", p.display()))?;
        }
    }
    fs::copy(dir.join("app.db"), cfg.db_path()).context("kopiëren van app.db")?;
    copy_tree(&dir.join("documents"), &cfg.documents_dir()).context("kopiëren van documents/")?;

    // Bring the restored database up to this binary's schema and back to WAL.
    let pool = db::connect(&cfg.db_path()).await?;
    db::migrate(&pool).await?;
    integrity_check(&pool).await?;
    pool.close().await;
    Ok(RestoreResult { manifest, aside })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn prune_keeps_dailies_and_one_per_month() {
        // A backup every day from 01-01-2025 to 31-12-2026.
        let mut dates = Vec::new();
        let mut d = date!(2025 - 01 - 01);
        while d <= date!(2026 - 12 - 31) {
            dates.push(d);
            d = d.next_day().unwrap();
        }
        let remove = select_prune(&dates, 30, 12);
        let keep: Vec<Date> = dates
            .iter()
            .copied()
            .filter(|d| !remove.contains(d))
            .collect();
        // 30 dailies (02-12 .. 31-12) + the newest of the 11 months before December.
        assert_eq!(keep.len(), 30 + 11);
        assert!(keep.contains(&date!(2026 - 12 - 31)));
        assert!(keep.contains(&date!(2026 - 12 - 02)));
        assert!(!keep.contains(&date!(2026 - 12 - 01)));
        assert!(keep.contains(&date!(2026 - 11 - 30)));
        assert!(keep.contains(&date!(2026 - 01 - 31)));
        assert!(!keep.contains(&date!(2025 - 12 - 31)));
    }

    #[test]
    fn prune_with_few_backups_removes_nothing() {
        let dates = [date!(2026 - 11 - 24), date!(2026 - 11 - 25)];
        assert!(select_prune(&dates, 30, 12).is_empty());
        assert_eq!(select_prune(&dates, 1, 0), vec![date!(2026 - 11 - 24)]);
    }
}
