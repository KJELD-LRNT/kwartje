//! Dashboard per company: company summary, hours this week/month, unbilled.
//! invoices, documents, BTW estimate, quotes and recurring drafts.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::routing::get;
use axum::Router;
use time::Duration;

use super::companies::{self, Company};
use super::{hours, unbilled};
use crate::format;
use crate::web::filters;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{AppResult, AppState};

pub struct Stats {
    pub week_minutes: i64,
    pub month_minutes: i64,
    pub month_business_km: i64,
    pub month_name: &'static str,
    pub unbilled_minutes: i64,
    pub unbilled_cents: i64,
    pub unbilled_clients: usize,
    pub invoices: super::invoices::list::Summary,
    pub quarter_docs: usize,
    pub quarter_missing: usize,
    pub quarter_label: String,
    pub year: i32,
    pub year_per_user: Vec<(String, i64)>,
    pub btw_balance_cents: i64,
    pub btw_quarter: u8,
    pub recurring_drafts: i64,
    pub open_quotes: usize,
}

/// Hours this year per user (everyone, this company).
async fn year_per_user(
    pool: &sqlx::SqlitePool,
    company_id: i64,
    from: &str,
    to: &str,
) -> AppResult<Vec<(String, i64)>> {
    Ok(sqlx::query!(
        r#"SELECT u.name AS "name!", COALESCE(SUM(t.minutes), 0) AS "m!: i64" FROM time_entries t
           JOIN users u ON u.id = t.user_id
           WHERE t.company_id = ? AND t.archived_at IS NULL AND t.date BETWEEN ? AND ?
           GROUP BY u.id ORDER BY u.name"#,
        company_id,
        from,
        to
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.name, r.m))
    .collect())
}

#[derive(Template, WebTemplate)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    l: Layout,
    company: Option<Company>,
    stats: Option<Stats>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/instellingen", get(settings))
}

/// The gear in the sidebar: settings start at the current company's details.
async fn settings(Page(l): Page) -> axum::response::Response {
    if l.company_id > 0 {
        crate::web::error::redirect(&format!("/bedrijven/{}", l.company_id))
    } else {
        crate::web::error::redirect("/bedrijven")
    }
}

async fn dashboard(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
) -> AppResult<DashboardPage> {
    if l.company_id == 0 {
        return Ok(DashboardPage {
            l,
            company: None,
            stats: None,
        });
    }
    let company = {
        let mut conn = st.pool.acquire().await?;
        companies::get(&mut conn, l.company_id).await?
    };
    let today = format::today();
    let monday = format::week_start(today);
    let first = today.replace_day(1).expect("day 1 exists");
    let next_month = if today.month() == time::Month::December {
        time::Date::from_calendar_date(today.year() + 1, time::Month::January, 1)
    } else {
        time::Date::from_calendar_date(today.year(), today.month().next(), 1)
    }
    .expect("valid date");
    let iso = format::iso_date;
    let groups = unbilled::groups(&st.pool, company.id, None, None, None).await?;
    let (qfrom, qto) =
        crate::documents::quarter_range(today.year(), crate::documents::quarter_of(today));
    let qdocs = super::documents::list(
        &st.pool,
        company.id,
        &super::documents::ListFilter {
            from: Some(iso(qfrom)),
            to: Some(iso(qto)),
            ..Default::default()
        },
    )
    .await?;
    let quarter = crate::documents::quarter_of(today);
    let btw = super::btw::report(&st.pool, company.id, today.year(), quarter).await?;
    let year_from =
        time::Date::from_calendar_date(today.year(), time::Month::January, 1).expect("valid date");
    let year_to = time::Date::from_calendar_date(today.year(), time::Month::December, 31)
        .expect("valid date");
    let stats = Stats {
        year: today.year(),
        year_per_user: year_per_user(&st.pool, company.id, &iso(year_from), &iso(year_to)).await?,
        btw_balance_cents: btw.summary.balance_cents(),
        btw_quarter: quarter,
        recurring_drafts: super::recurring::waiting_drafts(&st.pool, company.id).await?,
        open_quotes: super::quotes::open_count(&st.pool, company.id).await?,
        week_minutes: hours::minutes_between(
            &st.pool,
            company.id,
            Some(user.id),
            &iso(monday),
            &iso(monday + Duration::days(6)),
        )
        .await?,
        month_minutes: hours::minutes_between(
            &st.pool,
            company.id,
            None,
            &iso(first),
            &iso(next_month - Duration::days(1)),
        )
        .await?,
        month_business_km: super::trips::business_km_between(
            &st.pool,
            company.id,
            &iso(first),
            &iso(next_month - Duration::days(1)),
        )
        .await?,
        month_name: format::month_name(today.month()),
        unbilled_minutes: groups.iter().map(|g| g.minutes).sum(),
        unbilled_cents: groups.iter().map(|g| g.amount_cents).sum(),
        unbilled_clients: groups.len(),
        invoices: super::invoices::list::summary_for(&st.pool, company.id).await?,
        quarter_docs: qdocs.iter().filter(|d| !d.is_generated()).count(),
        quarter_missing: qdocs.iter().filter(|d| d.missing_amounts()).count(),
        quarter_label: format!("Q{} {}", crate::documents::quarter_of(today), today.year()),
    };
    Ok(DashboardPage {
        l,
        company: Some(company),
        stats: Some(stats),
    })
}
