//! Invoice rows, queries and shared operations (all scoped by company).

use serde::Serialize;
use sqlx::SqliteConnection;

use crate::invoice::{self, Status, StatusInput};
use crate::money;
use crate::web::{AppError, AppResult};
use crate::{audit, db, format};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: i64,
    pub company_id: i64,
    pub client_id: i64,
    pub kind: String,
    pub credit_for_invoice_id: Option<i64>,
    pub release_entries: bool,
    pub number: Option<String>,
    pub issue_date: Option<String>,
    pub due_date: Option<String>,
    pub period_from: String,
    pub period_to: String,
    pub line_style: String,
    pub hide_zero_lines: bool,
    pub include_hours_spec: bool,
    pub intro_text: String,
    pub footer_text: String,
    pub subtotal_cents: i64,
    pub vat_cents: i64,
    pub total_cents: i64,
    pub company_snapshot_json: Option<String>,
    pub client_snapshot_json: Option<String>,
    pub issued_at: Option<String>,
    pub sent_at: Option<String>,
    pub sent_to: Option<String>,
    pub pdf_document_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

impl Invoice {
    pub fn is_draft(&self) -> bool {
        self.number.is_none()
    }
    pub fn is_credit(&self) -> bool {
        self.kind == "credit"
    }
    pub fn title(&self) -> String {
        let what = if self.is_credit() {
            "Creditfactuur"
        } else {
            "Factuur"
        };
        match &self.number {
            Some(n) => format!("{what} {n}"),
            None => format!("{what} (concept)"),
        }
    }
    pub fn period_label(&self) -> String {
        let d = |s: &str| {
            format::parse_iso_date(s)
                .map(format::date)
                .unwrap_or_default()
        };
        if self.period_from == self.period_to {
            d(&self.period_from)
        } else {
            format!("{} t/m {}", d(&self.period_from), d(&self.period_to))
        }
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Line {
    pub id: i64,
    pub invoice_id: i64,
    pub position: i64,
    pub description: String,
    pub quantity_milli: i64,
    pub unit: String,
    pub unit_price_cents: i64,
    pub vat_bp: i64,
    pub line_total_cents: i64,
    pub source: String,
    pub is_zero: bool,
}

impl Line {
    pub fn quantity_label(&self) -> String {
        invoice::quantity_label(self.quantity_milli)
    }
}

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Invoice> {
    sqlx::query_as!(
        Invoice,
        r#"SELECT id AS "id!", company_id, client_id, kind, credit_for_invoice_id,
                  release_entries AS "release_entries: bool", number, issue_date, due_date,
                  period_from, period_to, line_style, hide_zero_lines AS "hide_zero_lines: bool",
                  include_hours_spec AS "include_hours_spec: bool", intro_text, footer_text,
                  subtotal_cents, vat_cents, total_cents, company_snapshot_json,
                  client_snapshot_json, issued_at, sent_at, sent_to, pdf_document_id, created_at,
                  updated_at, created_by
           FROM invoices WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn get_draft(
    conn: &mut SqliteConnection,
    company_id: i64,
    id: i64,
) -> AppResult<Invoice> {
    let inv = get(conn, company_id, id).await?;
    if !inv.is_draft() {
        return Err(AppError::BadRequest(
            "Deze factuur is definitief en kan niet meer worden gewijzigd. Maak een creditfactuur om te corrigeren.".into(),
        ));
    }
    Ok(inv)
}

pub async fn lines(conn: &mut SqliteConnection, invoice_id: i64) -> AppResult<Vec<Line>> {
    Ok(sqlx::query_as!(
        Line,
        r#"SELECT id AS "id!", invoice_id, position, description, quantity_milli, unit,
                  unit_price_cents, vat_bp, line_total_cents, source, is_zero AS "is_zero: bool"
           FROM invoice_lines WHERE invoice_id = ? ORDER BY position, id"#,
        invoice_id
    )
    .fetch_all(conn)
    .await?)
}

/// Recompute and store subtotal/VAT/total of a draft from its lines.
pub async fn recalc(conn: &mut SqliteConnection, invoice_id: i64) -> AppResult<money::Totals> {
    let ls = lines(conn, invoice_id).await?;
    let t = money::totals(
        &ls.iter()
            .map(|l| (l.line_total_cents, l.vat_bp))
            .collect::<Vec<_>>(),
    )
    .map_err(|e| AppError::Internal(e.into()))?;
    let now = db::now();
    sqlx::query!(
        "UPDATE invoices SET subtotal_cents = ?, vat_cents = ?, total_cents = ?, updated_at = ?
         WHERE id = ? AND number IS NULL",
        t.subtotal_cents,
        t.vat_cents,
        t.total_cents,
        now,
        invoice_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(t)
}

pub async fn totals_of(conn: &mut SqliteConnection, invoice_id: i64) -> AppResult<money::Totals> {
    let ls = lines(conn, invoice_id).await?;
    money::totals(
        &ls.iter()
            .map(|l| (l.line_total_cents, l.vat_bp))
            .collect::<Vec<_>>(),
    )
    .map_err(|e| AppError::Internal(e.into()))
}

pub async fn paid_cents(conn: &mut SqliteConnection, invoice_id: i64) -> AppResult<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(amount_cents), 0) AS "p!: i64" FROM payments
           WHERE invoice_id = ? AND archived_at IS NULL"#,
        invoice_id
    )
    .fetch_one(conn)
    .await?)
}

/// Is there an issued credit note for this invoice?
pub async fn is_credited(conn: &mut SqliteConnection, invoice_id: i64) -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM invoices
           WHERE credit_for_invoice_id = ? AND number IS NOT NULL"#,
        invoice_id
    )
    .fetch_one(conn)
    .await?;
    Ok(n > 0)
}

pub async fn status_of(conn: &mut SqliteConnection, inv: &Invoice) -> AppResult<Status> {
    let paid = paid_cents(conn, inv.id).await?;
    let credited = is_credited(conn, inv.id).await?;
    let today = format::iso_date(format::today());
    Ok(invoice::status(&StatusInput {
        issued: !inv.is_draft(),
        sent: inv.sent_at.is_some(),
        total_cents: inv.total_cents,
        paid_cents: paid,
        due_date: inv.due_date.as_deref(),
        today: &today,
        credited,
    }))
}

/// Insert draft lines (and link their entries) at the end of the invoice.
pub async fn insert_lines(
    tx: &mut SqliteConnection,
    company_id: i64,
    invoice_id: i64,
    lines: &[invoice::DraftLine],
    by: Option<i64>,
) -> AppResult<()> {
    let now = db::now();
    let mut pos: i64 = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(position), 0) AS "p!: i64" FROM invoice_lines WHERE invoice_id = ?"#,
        invoice_id
    )
    .fetch_one(&mut *tx)
    .await?;
    for l in lines {
        pos += 1;
        let line_id = sqlx::query!(
            "INSERT INTO invoice_lines (invoice_id, position, description, quantity_milli, unit,
                    unit_price_cents, vat_bp, line_total_cents, source, is_zero, created_at,
                    updated_at, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            invoice_id,
            pos,
            l.description,
            l.quantity_milli,
            l.unit,
            l.unit_price_cents,
            l.vat_bp,
            l.line_total_cents,
            l.source,
            l.is_zero,
            now,
            now,
            by
        )
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        audit::record_insert(
            tx,
            by,
            "invoice_lines",
            line_id,
            &serde_json::json!({
                "invoice_id": invoice_id, "description": l.description,
                "quantity_milli": l.quantity_milli, "unit": l.unit,
                "unit_price_cents": l.unit_price_cents, "vat_bp": l.vat_bp,
                "line_total_cents": l.line_total_cents, "source": l.source,
            }),
        )
        .await?;
        for e in &l.hour_entries {
            link_entry(tx, "time_entries", company_id, *e, Some(line_id), by).await?;
        }
        for e in &l.km_entries {
            link_entry(tx, "mileage_entries", company_id, *e, Some(line_id), by).await?;
        }
    }
    Ok(())
}

/// Set or clear `invoice_line_id` on a time or mileage entry (audited).
/// Linking only succeeds when the entry is still free.
pub async fn link_entry(
    tx: &mut SqliteConnection,
    table: &'static str,
    company_id: i64,
    entry_id: i64,
    line_id: Option<i64>,
    by: Option<i64>,
) -> AppResult<()> {
    let now = db::now();
    let changed = match (table, line_id) {
        ("time_entries", Some(l)) => sqlx::query!(
            "UPDATE time_entries SET invoice_line_id = ?, updated_at = ?
             WHERE id = ? AND company_id = ? AND invoice_line_id IS NULL AND archived_at IS NULL
               AND tariff_id IS NOT NULL",
            l,
            now,
            entry_id,
            company_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        ("time_entries", None) => sqlx::query!(
            "UPDATE time_entries SET invoice_line_id = NULL, updated_at = ?
             WHERE id = ? AND company_id = ?",
            now,
            entry_id,
            company_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        ("mileage_entries", Some(l)) => sqlx::query!(
            "UPDATE mileage_entries SET invoice_line_id = ?, updated_at = ?
             WHERE id = ? AND company_id = ? AND invoice_line_id IS NULL AND archived_at IS NULL
               AND tariff_id IS NOT NULL",
            l,
            now,
            entry_id,
            company_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        ("mileage_entries", None) => sqlx::query!(
            "UPDATE mileage_entries SET invoice_line_id = NULL, updated_at = ?
             WHERE id = ? AND company_id = ?",
            now,
            entry_id,
            company_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        _ => unreachable!("only time and mileage entries are linked"),
    };
    if changed != 1 {
        return Err(AppError::BadRequest(
            "Een deel van de uren of kilometers is intussen al gefactureerd of gewijzigd. Begin opnieuw vanuit Te factureren.".into(),
        ));
    }
    audit::record(
        tx,
        by,
        table,
        entry_id,
        audit::Action::Update,
        &serde_json::json!({ "invoice_line_id": [serde_json::Value::Null, line_id] }),
    )
    .await?;
    Ok(())
}

/// Entries linked to the lines of an invoice: (time entry ids, mileage entry ids).
pub async fn linked_entries(
    conn: &mut SqliteConnection,
    company_id: i64,
    invoice_id: i64,
) -> AppResult<(Vec<i64>, Vec<i64>)> {
    let hours: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT e.id AS "id!" FROM time_entries e JOIN invoice_lines l ON l.id = e.invoice_line_id
           WHERE l.invoice_id = ? AND e.company_id = ?"#,
        invoice_id,
        company_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let km: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT m.id AS "id!" FROM mileage_entries m JOIN invoice_lines l ON l.id = m.invoice_line_id
           WHERE l.invoice_id = ? AND m.company_id = ?"#,
        invoice_id,
        company_id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok((hours, km))
}

/// Entries linked to one line.
pub async fn entries_of_line(
    conn: &mut SqliteConnection,
    line_id: i64,
) -> AppResult<(Vec<i64>, Vec<i64>)> {
    let hours: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM time_entries WHERE invoice_line_id = ?"#,
        line_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let km: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM mileage_entries WHERE invoice_line_id = ?"#,
        line_id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok((hours, km))
}

/// Unbilled hours of a client as invoice items.
pub async fn unbilled_hours(
    conn: &mut SqliteConnection,
    company_id: i64,
    client_id: i64,
    from: Option<&str>,
    to: Option<&str>,
) -> AppResult<Vec<invoice::HourItem>> {
    let rows = sqlx::query!(
        r#"SELECT e.id AS "id!", e.date, e.task_id, k.name AS task_name, e.project_id,
                  p.name AS "project_name?", e.tariff_id AS "tariff_id!", t.name AS tariff_name,
                  e.price_cents AS "price_cents!", e.vat_bp AS "vat_bp!", e.minutes, e.description
           FROM time_entries e
           JOIN tasks k ON k.id = e.task_id
           JOIN tariffs t ON t.id = e.tariff_id
           LEFT JOIN projects p ON p.id = e.project_id
           WHERE e.company_id = ? AND e.client_id = ? AND e.tariff_id IS NOT NULL
             AND e.invoice_line_id IS NULL AND e.archived_at IS NULL
             AND (? IS NULL OR e.date >= ?) AND (? IS NULL OR e.date <= ?)
           ORDER BY e.date, e.id"#,
        company_id,
        client_id,
        from,
        from,
        to,
        to
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| invoice::HourItem {
            entry_id: r.id,
            date: r.date,
            task_id: r.task_id,
            task_name: r.task_name,
            project_id: r.project_id,
            project_name: r.project_name,
            tariff_id: r.tariff_id,
            tariff_name: r.tariff_name,
            price_cents: r.price_cents,
            vat_bp: r.vat_bp,
            minutes: r.minutes,
            description: r.description,
        })
        .collect())
}

/// Unbilled billable trips of a client as invoice items.
pub async fn unbilled_km(
    conn: &mut SqliteConnection,
    company_id: i64,
    client_id: i64,
    from: Option<&str>,
    to: Option<&str>,
) -> AppResult<Vec<invoice::KmItem>> {
    let rows = sqlx::query!(
        r#"SELECT m.id AS "id!", m.date, m.tariff_id AS "tariff_id!", t.name AS tariff_name,
                  m.price_cents AS "price_cents!", m.vat_bp AS "vat_bp!", m.km
           FROM mileage_entries m JOIN tariffs t ON t.id = m.tariff_id
           WHERE m.company_id = ? AND m.client_id = ? AND m.tariff_id IS NOT NULL
             AND m.invoice_line_id IS NULL AND m.archived_at IS NULL
             AND (? IS NULL OR m.date >= ?) AND (? IS NULL OR m.date <= ?)
           ORDER BY m.date, m.id"#,
        company_id,
        client_id,
        from,
        from,
        to,
        to
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| invoice::KmItem {
            entry_id: r.id,
            date: r.date,
            tariff_id: r.tariff_id,
            tariff_name: r.tariff_name,
            price_cents: r.price_cents,
            vat_bp: r.vat_bp,
            km: r.km,
        })
        .collect())
}

/// Hours behind an invoice, for the hours specification (page 2).
#[derive(Debug, Clone, Serialize)]
pub struct SpecRow {
    pub date: String,
    pub task: String,
    pub description: String,
    pub minutes: i64,
    pub user: String,
}

pub async fn hours_spec(
    conn: &mut SqliteConnection,
    company_id: i64,
    invoice_id: i64,
) -> AppResult<Vec<SpecRow>> {
    let rows = sqlx::query!(
        r#"SELECT e.date, k.name AS task, e.description, e.minutes, u.name AS user
           FROM time_entries e
           JOIN invoice_lines l ON l.id = e.invoice_line_id
           JOIN tasks k ON k.id = e.task_id
           JOIN users u ON u.id = e.user_id
           WHERE l.invoice_id = ? AND e.company_id = ?
           ORDER BY e.date, e.id"#,
        invoice_id,
        company_id
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SpecRow {
            date: r.date,
            task: r.task,
            description: r.description,
            minutes: r.minutes,
            user: r.user,
        })
        .collect())
}
