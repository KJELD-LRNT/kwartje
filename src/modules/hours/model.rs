//! Hour entries: the rows, queries and the insert/update/archive writes, plus totals used elsewhere (dashboard, clients).

use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};

use crate::auth::users;
use crate::web::{AppError, AppResult};
use crate::{audit, db, format};

use super::form::*;

pub const MAX_DESCRIPTION: usize = 2000;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Entry {
    pub id: i64,
    pub company_id: i64,
    pub user_id: i64,
    pub task_id: i64,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub tariff_id: Option<i64>,
    pub date: String,
    pub minutes: i64,
    pub description: String,
    pub price_cents: Option<i64>,
    pub vat_bp: Option<i64>,
    pub invoice_line_id: Option<i64>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

impl Entry {
    pub fn is_locked(&self) -> bool {
        self.invoice_line_id.is_some()
    }
}

/// An entry with names, for lists.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EntryRow {
    pub id: i64,
    pub date: String,
    pub user_id: i64,
    pub user_name: String,
    pub task_name: String,
    pub client_id: Option<i64>,
    pub client_name: Option<String>,
    pub project_name: Option<String>,
    pub tariff_name: Option<String>,
    pub minutes: i64,
    pub description: String,
    pub price_cents: Option<i64>,
    pub invoice_line_id: Option<i64>,
}

impl EntryRow {
    pub fn billable(&self) -> bool {
        self.price_cents.is_some()
    }

    pub fn tariff_label(&self) -> String {
        match (&self.tariff_name, self.price_cents) {
            (Some(n), Some(p)) => format!("{n} · {}/u", format::amount(p)),
            _ => "intern".into(),
        }
    }

    pub fn amount_cents(&self) -> i64 {
        self.price_cents
            .and_then(|p| crate::money::time_total_cents(self.minutes, p).ok())
            .unwrap_or(0)
    }
}

// ---------- queries ----------

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Entry> {
    sqlx::query_as!(
        Entry,
        r#"SELECT id AS "id!", company_id, user_id, task_id, client_id, project_id, tariff_id,
                  date, minutes, description, price_cents, vat_bp, invoice_line_id, archived_at,
                  created_at, updated_at, created_by
           FROM time_entries WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

/// Active entries of one user in a date range (inclusive), oldest first.
pub async fn rows_for_user(
    pool: &SqlitePool,
    company_id: i64,
    user_id: i64,
    from: &str,
    to: &str,
) -> AppResult<Vec<EntryRow>> {
    Ok(sqlx::query_as!(
        EntryRow,
        r#"SELECT e.id AS "id!", e.date, e.user_id, u.name AS user_name, k.name AS task_name,
                  e.client_id, c.name AS "client_name?", p.name AS "project_name?",
                  t.name AS "tariff_name?", e.minutes, e.description, e.price_cents,
                  e.invoice_line_id
           FROM time_entries e
           JOIN users u ON u.id = e.user_id
           JOIN tasks k ON k.id = e.task_id
           LEFT JOIN clients c ON c.id = e.client_id
           LEFT JOIN projects p ON p.id = e.project_id
           LEFT JOIN tariffs t ON t.id = e.tariff_id
           WHERE e.company_id = ? AND e.user_id = ? AND e.archived_at IS NULL
             AND e.date BETWEEN ? AND ?
           ORDER BY e.date, e.id"#,
        company_id,
        user_id,
        from,
        to
    )
    .fetch_all(pool)
    .await?)
}

pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    p: &Prepared,
    by: Option<i64>,
) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    let id = insert_in(&mut tx, company_id, p, by).await?;
    tx.commit().await?;
    Ok(id)
}

/// Several entries (the same hours on several days) in one transaction:
/// all are saved, or none.
pub async fn insert_all(
    pool: &SqlitePool,
    company_id: i64,
    entries: &[Prepared],
    by: Option<i64>,
) -> AppResult<Vec<i64>> {
    let mut tx = pool.begin().await?;
    let mut ids = Vec::with_capacity(entries.len());
    for p in entries {
        ids.push(insert_in(&mut tx, company_id, p, by).await?);
    }
    tx.commit().await?;
    Ok(ids)
}

async fn insert_in(
    tx: &mut SqliteConnection,
    company_id: i64,
    p: &Prepared,
    by: Option<i64>,
) -> AppResult<i64> {
    let now = db::now();
    let (tariff_id, price, vat) = match p.tariff {
        Some((t, pr, v)) => (Some(t), Some(pr), Some(v)),
        None => (None, None, None),
    };
    let id = sqlx::query!(
        "INSERT INTO time_entries (company_id, user_id, task_id, client_id, project_id, tariff_id,
                                   date, minutes, description, price_cents, vat_bp,
                                   created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        p.user_id,
        p.task_id,
        p.client_id,
        p.project_id,
        tariff_id,
        p.date,
        p.minutes,
        p.description,
        price,
        vat,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut *tx, company_id, id).await?;
    audit::record_insert(&mut *tx, by, "time_entries", id, &row).await?;
    Ok(id)
}

pub(super) const LOCKED: &str = "Deze uren zijn gefactureerd en kunnen niet meer worden gewijzigd.";

pub async fn update(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    p: &Prepared,
    by: i64,
) -> AppResult<()> {
    let now = db::now();
    let (tariff_id, price, vat) = match p.tariff {
        Some((t, pr, v)) => (Some(t), Some(pr), Some(v)),
        None => (None, None, None),
    };
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.is_locked() {
        return Err(AppError::BadRequest(LOCKED.into()));
    }
    sqlx::query!(
        "UPDATE time_entries SET user_id = ?, task_id = ?, client_id = ?, project_id = ?,
                tariff_id = ?, date = ?, minutes = ?, description = ?, price_cents = ?, vat_bp = ?,
                updated_at = ?
         WHERE id = ? AND company_id = ? AND invoice_line_id IS NULL",
        p.user_id,
        p.task_id,
        p.client_id,
        p.project_id,
        tariff_id,
        p.date,
        p.minutes,
        p.description,
        price,
        vat,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "time_entries", id, &old, &new).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn archive(pool: &SqlitePool, company_id: i64, id: i64, by: i64) -> AppResult<Entry> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.is_locked() {
        return Err(AppError::BadRequest(LOCKED.into()));
    }
    if old.archived_at.is_none() {
        sqlx::query!(
            "UPDATE time_entries SET archived_at = ?, updated_at = ?
             WHERE id = ? AND company_id = ?",
            now,
            now,
            id,
            company_id
        )
        .execute(&mut *tx)
        .await?;
        audit::record_archive(&mut tx, Some(by), "time_entries", id, None, Some(&now)).await?;
    }
    tx.commit().await?;
    Ok(old)
}

// ---------- year overview, summaries ----------

pub struct MonthTotals {
    pub label: &'static str,
    pub minutes: i64,
    pub billable_minutes: i64,
}

pub struct UserYear {
    pub user_id: i64,
    pub name: String,
    pub months: Vec<MonthTotals>,
    pub minutes: i64,
    pub billable_minutes: i64,
}

pub(super) const MONTHS: [&str; 12] = [
    "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
];

pub async fn year_totals(
    pool: &SqlitePool,
    company_id: i64,
    year: i32,
) -> AppResult<Vec<UserYear>> {
    let from = format!("{year:04}-01-01");
    let to = format!("{year:04}-12-31");
    let rows = sqlx::query!(
        r#"SELECT e.user_id, CAST(substr(e.date, 6, 2) AS INTEGER) AS "month!: i64",
                  SUM(e.minutes) AS "minutes!: i64",
                  SUM(CASE WHEN e.tariff_id IS NOT NULL THEN e.minutes ELSE 0 END) AS "billable!: i64"
           FROM time_entries e
           WHERE e.company_id = ? AND e.archived_at IS NULL AND e.date BETWEEN ? AND ?
           GROUP BY e.user_id, substr(e.date, 6, 2)"#,
        company_id,
        from,
        to
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for u in users::workers(pool).await? {
        let mut y = UserYear {
            user_id: u.id,
            name: u.name,
            months: MONTHS
                .iter()
                .map(|m| MonthTotals {
                    label: m,
                    minutes: 0,
                    billable_minutes: 0,
                })
                .collect(),
            minutes: 0,
            billable_minutes: 0,
        };
        for r in rows.iter().filter(|r| r.user_id == u.id) {
            if let Some(m) = y.months.get_mut((r.month - 1).clamp(0, 11) as usize) {
                m.minutes += r.minutes;
                m.billable_minutes += r.billable;
            }
            y.minutes += r.minutes;
            y.billable_minutes += r.billable;
        }
        out.push(y);
    }
    Ok(out)
}

/// Last entries for a client (client page).
pub async fn recent_for_client(
    pool: &SqlitePool,
    company_id: i64,
    client_id: i64,
    limit: i64,
) -> AppResult<Vec<EntryRow>> {
    Ok(sqlx::query_as!(
        EntryRow,
        r#"SELECT e.id AS "id!", e.date, e.user_id, u.name AS user_name, k.name AS task_name,
                  e.client_id, c.name AS "client_name?", p.name AS "project_name?",
                  t.name AS "tariff_name?", e.minutes, e.description, e.price_cents,
                  e.invoice_line_id
           FROM time_entries e
           JOIN users u ON u.id = e.user_id
           JOIN tasks k ON k.id = e.task_id
           LEFT JOIN clients c ON c.id = e.client_id
           LEFT JOIN projects p ON p.id = e.project_id
           LEFT JOIN tariffs t ON t.id = e.tariff_id
           WHERE e.company_id = ? AND e.client_id = ? AND e.archived_at IS NULL
           ORDER BY e.date DESC, e.id DESC
           LIMIT ?"#,
        company_id,
        client_id,
        limit
    )
    .fetch_all(pool)
    .await?)
}

pub struct ClientSummary {
    pub year: i32,
    pub minutes_this_year: i64,
    pub unbilled_minutes: i64,
    pub unbilled_cents: i64,
}

pub async fn client_summary(
    pool: &SqlitePool,
    company_id: i64,
    client_id: i64,
) -> AppResult<ClientSummary> {
    let year = format::today().year();
    let (from, to) = (format!("{year:04}-01-01"), format!("{year:04}-12-31"));
    let minutes: i64 = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(minutes), 0) AS "m!: i64" FROM time_entries
           WHERE company_id = ? AND client_id = ? AND archived_at IS NULL AND date BETWEEN ? AND ?"#,
        company_id,
        client_id,
        from,
        to
    )
    .fetch_one(pool)
    .await?;
    let groups =
        crate::modules::unbilled::groups(pool, company_id, Some(client_id), None, None).await?;
    Ok(ClientSummary {
        year,
        minutes_this_year: minutes,
        unbilled_minutes: groups.iter().map(|g| g.minutes).sum(),
        unbilled_cents: groups.iter().map(|g| g.amount_cents).sum(),
    })
}

/// Total minutes in a date range for the company (optionally one user).
pub async fn minutes_between(
    pool: &SqlitePool,
    company_id: i64,
    user_id: Option<i64>,
    from: &str,
    to: &str,
) -> AppResult<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(minutes), 0) AS "m!: i64" FROM time_entries
           WHERE company_id = ? AND archived_at IS NULL AND date BETWEEN ? AND ?
             AND (? IS NULL OR user_id = ?)"#,
        company_id,
        from,
        to,
        user_id,
        user_id
    )
    .fetch_one(pool)
    .await?)
}
