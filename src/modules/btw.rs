//! `/btw`: quarterly BTW summary page and CSV.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use sqlx::SqlitePool;

use super::companies::CurrentCompany;
use super::invoices::model;
use crate::btw::{self, Summary};
use crate::documents::{quarter_of, quarter_range};
use crate::format;
use crate::web::filters;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{AppResult, AppState};

pub struct BoxRow {
    pub code: &'static str,
    pub label: &'static str,
    pub base_cents: i64,
    pub vat_cents: i64,
}

impl BoxRow {
    pub fn base_euros(&self) -> i64 {
        btw::euros_down(self.base_cents)
    }
    pub fn vat_euros(&self) -> i64 {
        btw::euros_down(self.vat_cents)
    }
}

pub struct Report {
    pub year: i32,
    pub quarter: u8,
    pub label: String,
    pub summary: Summary,
    pub rows: Vec<BoxRow>,
    pub invoice_count: usize,
    pub purchase_count: usize,
    pub missing_vat: usize,
    pub drafts: usize,
    pub odd_rates: Vec<String>,
}

impl Report {
    pub fn input_euros(&self) -> i64 {
        btw::euros_up(self.summary.input_vat_cents)
    }
}

pub async fn report(
    pool: &SqlitePool,
    company_id: i64,
    year: i32,
    quarter: u8,
) -> AppResult<Report> {
    let (from, to) = quarter_range(year, quarter);
    let (from, to) = (format::iso_date(from), format::iso_date(to));
    let ids: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, number FROM invoices WHERE company_id = ? AND number IS NOT NULL AND issue_date BETWEEN ? AND ? ORDER BY issue_date, number",
    )
    .bind(company_id)
    .bind(&from)
    .bind(&to)
    .fetch_all(pool)
    .await?;
    let mut summary = Summary::default();
    let mut odd_rates = Vec::new();
    {
        let mut conn = pool.acquire().await?;
        for (id, number) in &ids {
            let t = model::totals_of(&mut conn, *id).await?;
            for bp in t.per_rate.keys() {
                if btw::box_for(*bp) == "?" {
                    odd_rates.push(format!("{number} ({})", format::vat_pct(*bp)));
                }
            }
            summary.add_invoice(&t);
        }
    }
    let (purchase_count, input_vat, missing_vat): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(vat_cents), 0), COALESCE(SUM(vat_cents IS NULL), 0) FROM documents
         WHERE company_id = ? AND kind IN ('purchase_invoice', 'receipt') AND archived_at IS NULL
           AND doc_date BETWEEN ? AND ?",
    )
    .bind(company_id)
    .bind(&from)
    .bind(&to)
    .fetch_one(pool)
    .await?;
    summary.input_vat_cents = input_vat;
    let drafts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE company_id = ? AND number IS NULL AND period_to BETWEEN ? AND ?",
    )
    .bind(company_id)
    .bind(&from)
    .bind(&to)
    .fetch_one(pool)
    .await?;
    let mut rows: Vec<BoxRow> = ["1a", "1b", "1e"]
        .iter()
        .map(|code| BoxRow {
            code,
            label: btw::box_label(code),
            base_cents: 0,
            vat_cents: 0,
        })
        .collect();
    for (bp, r) in &summary.sales {
        let code = btw::box_for(*bp);
        match rows.iter_mut().find(|b| b.code == code) {
            Some(b) => {
                b.base_cents += r.base_cents;
                b.vat_cents += r.vat_cents;
            }
            None => rows.push(BoxRow {
                code: "?",
                label: btw::box_label("?"),
                base_cents: r.base_cents,
                vat_cents: r.vat_cents,
            }),
        }
    }
    Ok(Report {
        year,
        quarter,
        label: format!("Q{quarter} {year}"),
        summary,
        rows,
        invoice_count: ids.len(),
        purchase_count: purchase_count as usize,
        missing_vat: missing_vat as usize,
        drafts: drafts as usize,
        odd_rates,
    })
}

#[derive(Deserialize, Default)]
struct Q {
    #[serde(default)]
    jaar: String,
    #[serde(default)]
    kwartaal: String,
}

fn period(q: &Q) -> (i32, u8) {
    let today = format::today();
    let year = q
        .jaar
        .trim()
        .parse()
        .ok()
        .filter(|y| (2000..=2100).contains(y))
        .unwrap_or(today.year());
    let quarter = q
        .kwartaal
        .trim()
        .parse()
        .ok()
        .filter(|q| (1..=4).contains(q))
        .unwrap_or(quarter_of(today));
    (year, quarter)
}

#[derive(Template, WebTemplate)]
#[template(path = "btw.html")]
struct BtwPage {
    l: Layout,
    r: Report,
    prev: (i32, u8),
    next: (i32, u8),
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/btw", get(page))
        .route("/btw.csv", get(csv))
}

fn shift(year: i32, q: u8, by: i32) -> (i32, u8) {
    let idx = year * 4 + i32::from(q) - 1 + by;
    (idx.div_euclid(4), (idx.rem_euclid(4) + 1) as u8)
}

async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<Q>,
) -> AppResult<BtwPage> {
    let (year, quarter) = period(&q);
    Ok(BtwPage {
        l,
        r: report(&st.pool, company.id, year, quarter).await?,
        prev: shift(year, quarter, -1),
        next: shift(year, quarter, 1),
    })
}

async fn csv(
    State(st): State<AppState>,
    _u: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<Q>,
) -> AppResult<Response> {
    let (year, quarter) = period(&q);
    let r = report(&st.pool, company.id, year, quarter).await?;
    let mut out = String::from(
        "\u{feff}Rubriek;Omschrijving;Omzet (€);Btw (€);Omzet afgerond (€);Btw afgerond (€)\r\n",
    );
    for b in &r.rows {
        out.push_str(&format!(
            "{};{};{};{};{};{}\r\n",
            b.code,
            b.label,
            format::decimal(b.base_cents),
            format::decimal(b.vat_cents),
            b.base_euros(),
            b.vat_euros()
        ));
    }
    out.push_str(&format!(
        "5a;Verschuldigde omzetbelasting;;{};;{}\r\n",
        format::decimal(r.summary.sales_vat_cents()),
        r.rows.iter().map(|b| b.vat_euros()).sum::<i64>()
    ));
    out.push_str(&format!(
        "5b;Voorbelasting;;{};;{}\r\n",
        format::decimal(r.summary.input_vat_cents),
        r.input_euros()
    ));
    out.push_str(&format!(
        "5c;Saldo (positief = betalen);;{};;{}\r\n",
        format::decimal(r.summary.balance_cents()),
        r.summary.balance_euros()
    ));
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"btw-{}-Q{}.csv\"", year, quarter),
            ),
        ],
        out,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_shift() {
        assert_eq!(shift(2026, 1, -1), (2025, 4));
        assert_eq!(shift(2026, 4, 1), (2027, 1));
        assert_eq!(shift(2026, 2, 1), (2026, 3));
    }
}
