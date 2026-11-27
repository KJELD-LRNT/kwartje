//! The page of an issued invoice.

use askama::Template;
use askama_web::WebTemplate;

use super::issue::{ClientSnapshot, CompanySnapshot};
use super::model::{self, Invoice, Line};
use crate::invoice::Status;
use crate::money;
use crate::web::filters;
use crate::web::layout::Layout;
use crate::web::{AppResult, AppState};

pub struct PaymentRow {
    pub id: i64,
    pub date: String,
    pub amount_cents: i64,
    pub method: String,
    pub note: String,
}

impl PaymentRow {
    pub fn method_label(&self) -> &'static str {
        super::payments::method_label(&self.method)
    }
}

pub struct ReminderRow {
    pub level: String,
    pub sent_at: String,
    pub sent_to: String,
    pub via_email: bool,
}

impl ReminderRow {
    pub fn label(&self) -> &'static str {
        match self.level.as_str() {
            "1" => "Eerste herinnering",
            "2" => "Tweede herinnering",
            _ => "Laatste aanmaning",
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/show.html")]
pub struct ShowPage {
    pub l: Layout,
    pub inv: Invoice,
    pub company: CompanySnapshot,
    pub client: ClientSnapshot,
    pub lines: Vec<Line>,
    pub totals: money::Totals,
    pub status: Status,
    pub paid_cents: i64,
    pub open_cents: i64,
    pub payments: Vec<PaymentRow>,
    pub reminders: Vec<ReminderRow>,
    pub next_reminder: Option<(&'static str, &'static str)>,
    pub credit_of: Option<(i64, String)>,
    pub credits: Vec<(i64, String)>,
    pub can_email: bool,
    pub today: String,
    pub methods: &'static [(&'static str, &'static str)],
}

impl ShowPage {
    pub fn status_class(&self) -> &'static str {
        self.status.css()
    }
}

pub async fn page(st: &AppState, l: Layout, company_id: i64, inv: Invoice) -> AppResult<ShowPage> {
    let mut conn = st.pool.acquire().await?;
    let lines = model::lines(&mut conn, inv.id).await?;
    let totals = model::totals_of(&mut conn, inv.id).await?;
    let status = model::status_of(&mut conn, &inv).await?;
    let paid_cents = model::paid_cents(&mut conn, inv.id).await?;
    let payments = sqlx::query!(
        r#"SELECT id AS "id!", date, amount_cents, method, note FROM payments
           WHERE invoice_id = ? AND archived_at IS NULL ORDER BY date, id"#,
        inv.id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| PaymentRow {
        id: r.id,
        date: r.date,
        amount_cents: r.amount_cents,
        method: r.method,
        note: r.note,
    })
    .collect();
    let reminders: Vec<ReminderRow> = sqlx::query!(
        r#"SELECT level, sent_at, sent_to, via_email AS "via_email: bool" FROM reminders
           WHERE invoice_id = ? ORDER BY id"#,
        inv.id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| ReminderRow {
        level: r.level,
        sent_at: r.sent_at,
        sent_to: r.sent_to,
        via_email: r.via_email,
    })
    .collect();
    let next_reminder = if status == Status::Vervallen && !inv.is_credit() {
        match reminders.len() {
            0 => Some(("1", "Eerste herinnering")),
            1 => Some(("2", "Tweede herinnering")),
            2 => Some(("final", "Laatste aanmaning")),
            _ => None,
        }
    } else {
        None
    };
    let credit_of = match inv.credit_for_invoice_id {
        Some(o) => model::get(&mut conn, company_id, o)
            .await?
            .number
            .map(|n| (o, n)),
        None => None,
    };
    let credits = sqlx::query!(
        r#"SELECT id AS "id!", COALESCE(number, 'concept') AS "n!: String" FROM invoices
           WHERE credit_for_invoice_id = ? AND company_id = ? ORDER BY id"#,
        inv.id,
        company_id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.id, r.n))
    .collect();
    drop(conn);
    let company: CompanySnapshot = inv
        .company_snapshot_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| anyhow::anyhow!("invoice {} has no company snapshot", inv.id))?;
    let client: ClientSnapshot = inv
        .client_snapshot_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| anyhow::anyhow!("invoice {} has no client snapshot", inv.id))?;
    Ok(ShowPage {
        l,
        company,
        client,
        lines,
        open_cents: inv.total_cents - paid_cents,
        totals,
        status,
        paid_cents,
        payments,
        reminders,
        next_reminder,
        credit_of,
        credits,
        can_email: st.mailer.is_enabled(),
        today: crate::format::iso_date(crate::format::today()),
        methods: super::payments::METHODS,
        inv,
    })
}
