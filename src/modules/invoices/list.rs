//! Invoice overview, ageing per client, and summaries for the dashboard and
//! client page.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::format;
use crate::invoice::{self, Status, StatusInput};
use crate::modules::companies::CurrentCompany;
use crate::web::filters;
use crate::web::layout::{Layout, Page};
use crate::web::{AppResult, AppState};

pub struct Row {
    pub id: i64,
    pub number: Option<String>,
    pub kind: String,
    pub client_id: i64,
    pub client_name: String,
    pub issue_date: Option<String>,
    pub due_date: Option<String>,
    pub total_cents: i64,
    pub paid_cents: i64,
    pub status: Status,
}

impl Row {
    pub fn open_cents(&self) -> i64 {
        if self.status.is_open() {
            self.total_cents - self.paid_cents
        } else {
            0
        }
    }
    pub fn label(&self) -> String {
        match &self.number {
            Some(n) => n.clone(),
            None => "concept".into(),
        }
    }
    pub fn is_credit(&self) -> bool {
        self.kind == "credit"
    }
    pub fn status_class(&self) -> &'static str {
        self.status.css()
    }
    /// Days past the due date (0 when not yet due).
    pub fn days_overdue(&self, today: time::Date) -> i64 {
        self.due_date
            .as_deref()
            .and_then(format::parse_iso_date)
            .map(|d| (today - d).whole_days().max(0))
            .unwrap_or(0)
    }
}

/// All invoices of the company (optionally one client), newest first.
pub async fn rows(
    pool: &SqlitePool,
    company_id: i64,
    client_id: Option<i64>,
) -> AppResult<Vec<Row>> {
    let today = format::iso_date(format::today());
    let raw = sqlx::query!(
        r#"SELECT i.id AS "id!", i.number, i.kind, i.client_id, c.name AS client_name,
                  i.issue_date, i.due_date, i.total_cents, i.sent_at,
                  COALESCE((SELECT SUM(amount_cents) FROM payments p
                            WHERE p.invoice_id = i.id AND p.archived_at IS NULL), 0) AS "paid!: i64",
                  EXISTS (SELECT 1 FROM invoices cr WHERE cr.credit_for_invoice_id = i.id
                          AND cr.number IS NOT NULL) AS "credited!: bool"
           FROM invoices i JOIN clients c ON c.id = i.client_id
           WHERE i.company_id = ? AND (? IS NULL OR i.client_id = ?)
           ORDER BY i.number IS NOT NULL, i.issue_date DESC, i.number DESC, i.id DESC"#,
        company_id,
        client_id,
        client_id
    )
    .fetch_all(pool)
    .await?;
    Ok(raw
        .into_iter()
        .map(|r| {
            let status = invoice::status(&StatusInput {
                issued: r.number.is_some(),
                sent: r.sent_at.is_some(),
                total_cents: r.total_cents,
                paid_cents: r.paid,
                due_date: r.due_date.as_deref(),
                today: &today,
                credited: r.credited,
            });
            Row {
                id: r.id,
                number: r.number,
                kind: r.kind,
                client_id: r.client_id,
                client_name: r.client_name,
                issue_date: r.issue_date,
                due_date: r.due_date,
                total_cents: r.total_cents,
                paid_cents: r.paid,
                status,
            }
        })
        .collect())
}

pub struct Summary {
    pub open_cents: i64,
    pub open_count: usize,
    pub overdue_cents: i64,
    pub overdue_count: usize,
    pub drafts: usize,
}

pub fn summary(rows: &[Row]) -> Summary {
    let open: Vec<&Row> = rows.iter().filter(|r| r.status.is_open()).collect();
    let overdue: Vec<&Row> = open
        .iter()
        .copied()
        .filter(|r| r.status == Status::Vervallen)
        .collect();
    Summary {
        open_cents: open.iter().map(|r| r.open_cents()).sum(),
        open_count: open.len(),
        overdue_cents: overdue.iter().map(|r| r.open_cents()).sum(),
        overdue_count: overdue.len(),
        drafts: rows.iter().filter(|r| r.status == Status::Concept).count(),
    }
}

pub async fn summary_for(pool: &SqlitePool, company_id: i64) -> AppResult<Summary> {
    Ok(summary(&rows(pool, company_id, None).await?))
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/list.html")]
pub struct ListPage {
    pub l: Layout,
    pub rows: Vec<Row>,
    pub filter: String,
    pub summary: Summary,
    pub paid_this_year: i64,
    pub year: i32,
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    status: String,
}

pub async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<ListQuery>,
) -> AppResult<ListPage> {
    let all = rows(&st.pool, company.id, None).await?;
    let summary = summary(&all);
    let filter = q.status.trim().to_string();
    let rows: Vec<Row> = all
        .into_iter()
        .filter(|r| match filter.as_str() {
            "open" => r.status.is_open(),
            "vervallen" => r.status == Status::Vervallen,
            "concept" => r.status == Status::Concept,
            "betaald" => r.status == Status::Betaald,
            _ => true,
        })
        .collect();
    let year = format::today().year();
    let (from, to) = (format!("{year:04}-01-01"), format!("{year:04}-12-31"));
    let paid_this_year: i64 = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(p.amount_cents), 0) AS "s!: i64" FROM payments p
           JOIN invoices i ON i.id = p.invoice_id
           WHERE i.company_id = ? AND p.archived_at IS NULL AND p.date BETWEEN ? AND ?"#,
        company.id,
        from,
        to
    )
    .fetch_one(&st.pool)
    .await?;
    Ok(ListPage {
        l,
        rows,
        filter,
        summary,
        paid_this_year,
        year,
    })
}

pub struct AgeingRow {
    pub client_id: i64,
    pub client_name: String,
    pub buckets: [i64; 5],
    pub total: i64,
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/ageing.html")]
pub struct AgeingPage {
    pub l: Layout,
    pub rows: Vec<AgeingRow>,
    pub totals: [i64; 5],
    pub labels: [&'static str; 5],
}

/// Bucket: not yet due, 1–30, 31–60, 61–90, more than 90 days past due.
pub fn bucket(days_overdue: i64, is_due: bool) -> usize {
    match (is_due, days_overdue) {
        (false, _) | (true, 0) => 0,
        (_, 1..=30) => 1,
        (_, 31..=60) => 2,
        (_, 61..=90) => 3,
        _ => 4,
    }
}

pub async fn ageing(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
) -> AppResult<AgeingPage> {
    let today = format::today();
    let today_s = format::iso_date(today);
    let mut out: Vec<AgeingRow> = Vec::new();
    let mut totals = [0i64; 5];
    for r in rows(&st.pool, company.id, None)
        .await?
        .into_iter()
        .filter(|r| r.status.is_open())
    {
        let is_due = r.due_date.as_deref().is_some_and(|d| d < today_s.as_str());
        let b = bucket(r.days_overdue(today), is_due);
        let open = r.open_cents();
        totals[b] += open;
        match out.iter_mut().find(|a| a.client_id == r.client_id) {
            Some(a) => {
                a.buckets[b] += open;
                a.total += open;
            }
            None => {
                let mut buckets = [0i64; 5];
                buckets[b] = open;
                out.push(AgeingRow {
                    client_id: r.client_id,
                    client_name: r.client_name.clone(),
                    buckets,
                    total: open,
                });
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.total));
    Ok(AgeingPage {
        l,
        rows: out,
        totals,
        labels: [
            "Nog niet vervallen",
            "1–30 dagen",
            "31–60 dagen",
            "61–90 dagen",
            "> 90 dagen",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets() {
        assert_eq!(bucket(0, false), 0);
        assert_eq!(bucket(5, false), 0);
        assert_eq!(bucket(1, true), 1);
        assert_eq!(bucket(30, true), 1);
        assert_eq!(bucket(31, true), 2);
        assert_eq!(bucket(90, true), 3);
        assert_eq!(bucket(91, true), 4);
    }
}
