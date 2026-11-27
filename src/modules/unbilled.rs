//! Te factureren: billable hours (with a tariff) that are not invoiced yet,
//! per client, grouped by task and tariff. Company rounding is applied only
//! when the invoice is built (phase 4); amounts here use raw minutes.

use std::collections::BTreeMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use sqlx::SqlitePool;

use super::companies::CurrentCompany;
use crate::format;
use crate::money;
use crate::web::filters;
use crate::web::layout::{Layout, Page};
use crate::web::{AppError, AppResult, AppState};

#[derive(Debug, Clone)]
pub struct Line {
    pub task_name: String,
    pub tariff_name: String,
    pub price_cents: i64,
    pub minutes: i64,
    pub entries: i64,
    pub amount_cents: i64,
}

/// Billable km per tariff.
#[derive(Debug, Clone)]
pub struct KmLine {
    pub tariff_name: String,
    pub price_cents: i64,
    pub km: i64,
    pub trips: i64,
    pub amount_cents: i64,
}

#[derive(Debug, Clone)]
pub struct ClientGroup {
    pub client_id: i64,
    pub client_name: String,
    pub lines: Vec<Line>,
    pub km_lines: Vec<KmLine>,
    pub km: i64,
    pub minutes: i64,
    pub amount_cents: i64,
    pub first_date: String,
    pub last_date: String,
}

impl ClientGroup {
    pub fn period(&self) -> String {
        let d = |s: &str| {
            format::parse_iso_date(s)
                .map(format::date)
                .unwrap_or_default()
        };
        format!("{} t/m {}", d(&self.first_date), d(&self.last_date))
    }
}

struct Raw {
    client_id: i64,
    client_name: String,
    task_name: String,
    tariff_name: String,
    price_cents: i64,
    minutes: i64,
    date: String,
}

/// Unbilled groups for the company, optionally for one client and a period.
pub async fn groups(
    pool: &SqlitePool,
    company_id: i64,
    client_id: Option<i64>,
    from: Option<&str>,
    to: Option<&str>,
) -> AppResult<Vec<ClientGroup>> {
    let rows = sqlx::query_as!(
        Raw,
        r#"SELECT e.client_id AS "client_id!", c.name AS client_name, k.name AS task_name,
                  t.name AS tariff_name, e.price_cents AS "price_cents!", e.minutes, e.date
           FROM time_entries e
           JOIN clients c ON c.id = e.client_id
           JOIN tasks k ON k.id = e.task_id
           JOIN tariffs t ON t.id = e.tariff_id
           WHERE e.company_id = ? AND e.tariff_id IS NOT NULL AND e.invoice_line_id IS NULL
             AND e.archived_at IS NULL
             AND (? IS NULL OR e.client_id = ?)
             AND (? IS NULL OR e.date >= ?)
             AND (? IS NULL OR e.date <= ?)
           ORDER BY c.name COLLATE NOCASE, k.name COLLATE NOCASE, t.name, e.date"#,
        company_id,
        client_id,
        client_id,
        from,
        from,
        to,
        to
    )
    .fetch_all(pool)
    .await?;

    let mut by_client: BTreeMap<(String, i64), ClientGroup> = BTreeMap::new();
    for r in rows {
        let amount = money::time_total_cents(r.minutes, r.price_cents)
            .map_err(|e| AppError::Internal(e.into()))?;
        let g = by_client
            .entry((r.client_name.to_lowercase(), r.client_id))
            .or_insert_with(|| ClientGroup {
                client_id: r.client_id,
                client_name: r.client_name.clone(),
                lines: Vec::new(),
                km_lines: Vec::new(),
                km: 0,
                minutes: 0,
                amount_cents: 0,
                first_date: r.date.clone(),
                last_date: r.date.clone(),
            });
        g.minutes += r.minutes;
        g.amount_cents += amount;
        if r.date < g.first_date {
            g.first_date = r.date.clone();
        }
        if r.date > g.last_date {
            g.last_date = r.date.clone();
        }
        match g.lines.iter_mut().find(|l| {
            l.task_name == r.task_name
                && l.tariff_name == r.tariff_name
                && l.price_cents == r.price_cents
        }) {
            Some(l) => {
                l.minutes += r.minutes;
                l.entries += 1;
                l.amount_cents += amount;
            }
            None => g.lines.push(Line {
                task_name: r.task_name,
                tariff_name: r.tariff_name,
                price_cents: r.price_cents,
                minutes: r.minutes,
                entries: 1,
                amount_cents: amount,
            }),
        }
    }
    // Billable km (only trips with an explicitly chosen km tariff).
    let trips = sqlx::query!(
        r#"SELECT m.client_id AS "client_id!", c.name AS client_name, t.name AS tariff_name,
                  m.price_cents AS "price_cents!", m.km, m.date
           FROM mileage_entries m
           JOIN clients c ON c.id = m.client_id
           JOIN tariffs t ON t.id = m.tariff_id
           WHERE m.company_id = ? AND m.tariff_id IS NOT NULL AND m.invoice_line_id IS NULL
             AND m.archived_at IS NULL
             AND (? IS NULL OR m.client_id = ?)
             AND (? IS NULL OR m.date >= ?)
             AND (? IS NULL OR m.date <= ?)
           ORDER BY c.name COLLATE NOCASE, t.name, m.date"#,
        company_id,
        client_id,
        client_id,
        from,
        from,
        to,
        to
    )
    .fetch_all(pool)
    .await?;
    for r in trips {
        let amount =
            r.km.checked_mul(r.price_cents)
                .ok_or_else(|| AppError::Internal(anyhow::anyhow!("km amount overflow")))?;
        let g = by_client
            .entry((r.client_name.to_lowercase(), r.client_id))
            .or_insert_with(|| ClientGroup {
                client_id: r.client_id,
                client_name: r.client_name.clone(),
                lines: Vec::new(),
                km_lines: Vec::new(),
                km: 0,
                minutes: 0,
                amount_cents: 0,
                first_date: r.date.clone(),
                last_date: r.date.clone(),
            });
        g.km += r.km;
        g.amount_cents += amount;
        if r.date < g.first_date {
            g.first_date = r.date.clone();
        }
        if r.date > g.last_date {
            g.last_date = r.date.clone();
        }
        match g
            .km_lines
            .iter_mut()
            .find(|l| l.tariff_name == r.tariff_name && l.price_cents == r.price_cents)
        {
            Some(l) => {
                l.km += r.km;
                l.trips += 1;
                l.amount_cents += amount;
            }
            None => g.km_lines.push(KmLine {
                tariff_name: r.tariff_name,
                price_cents: r.price_cents,
                km: r.km,
                trips: 1,
                amount_cents: amount,
            }),
        }
    }
    Ok(by_client.into_values().collect())
}

pub fn router() -> Router<AppState> {
    Router::new().route("/te-factureren", get(page))
}

#[derive(Template, WebTemplate)]
#[template(path = "hours/unbilled.html")]
struct UnbilledPage {
    l: Layout,
    groups: Vec<ClientGroup>,
    from: String,
    to: String,
    total_minutes: i64,
    total_km: i64,
    total_cents: i64,
    rounding_minutes: i64,
}

#[derive(Deserialize)]
struct Q {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<Q>,
) -> AppResult<UnbilledPage> {
    let parse = |s: &str| -> AppResult<Option<String>> {
        if s.trim().is_empty() {
            return Ok(None);
        }
        format::parse_date(s)
            .map(|d| Some(format::iso_date(d)))
            .ok_or_else(|| AppError::BadRequest("Ongeldige datum: gebruik dd-mm-jjjj.".into()))
    };
    let (from, to) = (parse(&q.from)?, parse(&q.to)?);
    let groups = groups(&st.pool, company.id, None, from.as_deref(), to.as_deref()).await?;
    Ok(UnbilledPage {
        l,
        total_minutes: groups.iter().map(|g| g.minutes).sum(),
        total_km: groups.iter().map(|g| g.km).sum(),
        total_cents: groups.iter().map(|g| g.amount_cents).sum(),
        groups,
        from: from.unwrap_or_default(),
        to: to.unwrap_or_default(),
        rounding_minutes: company.rounding_minutes,
    })
}
