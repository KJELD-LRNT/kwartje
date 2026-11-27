//! Reminders (1st, 2nd, final notice) for overdue invoices, and credit notes.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Form;
use serde::Deserialize;
use time::Duration;
use tower_sessions::Session;

use super::model::{self, Invoice};
use super::send::{greeting, pdf_bytes, snapshots};
use crate::invoice::{self, DraftLine, Status};
use crate::mail::Email;
use crate::modules::companies::CurrentCompany;
use crate::web::error::redirect;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format, validate};

pub const LEVELS: &[(&str, &str)] = &[
    ("1", "Eerste herinnering"),
    ("2", "Tweede herinnering"),
    ("final", "Laatste aanmaning"),
];

fn d(s: &Option<String>) -> String {
    s.as_deref()
        .and_then(format::parse_iso_date)
        .map(format::date)
        .unwrap_or_default()
}

/// Dutch reminder text: (subject, body).
pub fn reminder_message(
    level: &str,
    inv: &Invoice,
    company: &super::issue::CompanySnapshot,
    client: &super::issue::ClientSnapshot,
    open_cents: i64,
    sender: &str,
) -> (String, String) {
    let number = inv.number.clone().unwrap_or_default();
    let pay_by = format::date(format::today() + Duration::days(7));
    let amount = format::amount(open_cents);
    let pay = format!(
        "Wilt u het openstaande bedrag van {amount} uiterlijk {pay_by} overmaken op IBAN {} ten name van {}, onder vermelding van factuurnummer {number}?",
        company.iban, company.name
    );
    let (subject, middle) = match level {
        "1" => (
            format!("Herinnering: factuur {number}"),
            format!(
                "Volgens onze administratie hebben wij de betaling van factuur {number} van {} (vervaldatum {}) nog niet ontvangen. Mogelijk is dit aan uw aandacht ontsnapt.\n\n{pay}\n\nHeeft u inmiddels betaald? Dan kunt u deze herinnering als niet verzonden beschouwen.",
                d(&inv.issue_date),
                d(&inv.due_date)
            ),
        ),
        "2" => (
            format!("Tweede herinnering: factuur {number}"),
            format!(
                "Ondanks onze eerdere herinnering hebben wij de betaling van factuur {number} van {} (vervaldatum {}) nog niet ontvangen.\n\n{pay}\n\nNeem gerust contact met ons op als er iets niet klopt aan de factuur.",
                d(&inv.issue_date),
                d(&inv.due_date)
            ),
        ),
        _ => (
            format!("Laatste aanmaning: factuur {number}"),
            format!(
                "Ondanks twee eerdere herinneringen staat factuur {number} van {} (vervaldatum {}) nog altijd open.\n\nWij verzoeken u dringend het bedrag van {amount} uiterlijk {pay_by} over te maken op IBAN {} ten name van {}, onder vermelding van factuurnummer {number}. Blijft betaling uit, dan zijn wij genoodzaakt verdere stappen te ondernemen; de kosten daarvan komen voor uw rekening.",
                d(&inv.issue_date),
                d(&inv.due_date),
                company.iban,
                company.name
            ),
        ),
    };
    (
        subject,
        format!(
            "{}\n\n{middle}\n\nMet vriendelijke groet,\n\n{sender}\n{}",
            greeting(client),
            company.name
        ),
    )
}

/// The reminder that may be sent now, if any.
async fn next_level(
    conn: &mut sqlx::SqliteConnection,
    inv: &Invoice,
) -> AppResult<Option<&'static str>> {
    if inv.is_credit() || model::status_of(conn, inv).await? != Status::Vervallen {
        return Ok(None);
    }
    let sent: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM reminders WHERE invoice_id = ?"#,
        inv.id
    )
    .fetch_one(conn)
    .await?;
    Ok(LEVELS.get(sent as usize).map(|(k, _)| *k))
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/reminder.html")]
pub struct ReminderPage {
    pub l: Layout,
    pub inv: Invoice,
    pub level: String,
    pub level_label: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub can_email: bool,
    pub error: Option<String>,
}

async fn load(st: &AppState, company_id: i64, id: i64) -> AppResult<(Invoice, &'static str, i64)> {
    let mut conn = st.pool.acquire().await?;
    let inv = model::get(&mut conn, company_id, id).await?;
    let level = next_level(&mut conn, &inv).await?.ok_or_else(|| {
        AppError::BadRequest("Voor deze factuur kan nu geen herinnering worden verstuurd (alleen bij een vervallen, verzonden factuur).".into())
    })?;
    let open = inv.total_cents - model::paid_cents(&mut conn, inv.id).await?;
    Ok((inv, level, open))
}

fn label(level: &str) -> &'static str {
    LEVELS
        .iter()
        .find(|(k, _)| *k == level)
        .map(|(_, l)| *l)
        .unwrap_or("Herinnering")
}

pub async fn reminder_page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<ReminderPage> {
    let (inv, level, open) = load(&st, company.id, id).await?;
    let (c, k) = snapshots(&inv)?;
    let (subject, body) = reminder_message(level, &inv, &c, &k, open, &user.name);
    Ok(ReminderPage {
        l,
        to: k.email.clone(),
        level: level.into(),
        level_label: label(level).into(),
        subject,
        body,
        can_email: st.mailer.is_enabled(),
        error: None,
        inv,
    })
}

#[derive(Deserialize)]
pub struct ReminderForm {
    #[serde(default)]
    level: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    /// "email" or "manual"
    #[serde(default)]
    how: String,
}

pub async fn reminder_send(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
    Form(f): Form<ReminderForm>,
) -> AppResult<Response> {
    let (inv, level, _) = load(&st, company.id, id).await?;
    if f.level != level {
        return Err(AppError::BadRequest(
            "Deze herinnering is al verstuurd.".into(),
        ));
    }
    let via_email = f.how == "email";
    let mut to = String::from("handmatig");
    if via_email {
        let (c, _) = snapshots(&inv)?;
        let fail = |msg: String, inv: Invoice| ReminderPage {
            l,
            level: level.into(),
            level_label: label(level).into(),
            to: f.to.clone(),
            subject: f.subject.clone(),
            body: f.body.clone(),
            can_email: st.mailer.is_enabled(),
            error: Some(msg),
            inv,
        };
        to = match validate::email(&f.to) {
            Ok(v) => v,
            Err(m) => return Ok(fail(m.into(), inv).into_response()),
        };
        let pdf = {
            let mut conn = st.pool.acquire().await?;
            pdf_bytes(&mut conn, &st.cfg.data_dir, &inv).await?
        };
        let email = Email {
            to: to.clone(),
            reply_to: (!c.email.is_empty()).then(|| c.email.clone()),
            from_name: c.name.clone(),
            subject: f.subject.trim().to_string(),
            body: f.body.replace("\r\n", "\n"),
            attachment: Some(pdf),
        };
        if let Err(e) = st.mailer.send(email).await {
            return Ok(fail(format!("Versturen is mislukt: {e}"), inv).into_response());
        }
    }
    let now = db::now();
    let mut tx = st.pool.begin().await?;
    let rid = sqlx::query!(
        "INSERT INTO reminders (invoice_id, level, sent_at, sent_to, via_email, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        level,
        now,
        to,
        via_email,
        now,
        now,
        user.id
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    audit::record_insert(
        &mut tx,
        Some(user.id),
        "reminders",
        rid,
        &serde_json::json!({"invoice_id": id, "level": level, "sent_to": to, "via_email": via_email}),
    )
    .await?;
    tx.commit().await?;
    flash::ok(&session, &format!("{} vastgelegd.", label(level))).await?;
    Ok(redirect(&format!("/facturen/{id}")))
}

// ---------- credit notes ----------

#[derive(Deserialize)]
pub struct CreditForm {
    #[serde(default)]
    release_entries: Option<String>,
}

/// Create a draft credit note with the original's lines negated.
pub async fn create_credit(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<CreditForm>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    let orig = model::get(&mut tx, company.id, id).await?;
    if orig.is_draft() || orig.is_credit() {
        return Err(AppError::BadRequest(
            "Alleen een definitieve factuur kan worden gecrediteerd.".into(),
        ));
    }
    let existing: Option<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM invoices WHERE credit_for_invoice_id = ? AND company_id = ? LIMIT 1"#,
        id,
        company.id
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(e) = existing {
        drop(tx);
        flash::error(&session, "Er is al een creditfactuur voor deze factuur.").await?;
        return Ok(redirect(&format!("/facturen/{e}")));
    }
    let lines: Vec<DraftLine> = model::lines(&mut tx, id)
        .await?
        .into_iter()
        .map(|l| DraftLine {
            description: l.description,
            quantity_milli: l.quantity_milli,
            unit: invoice::unit(&l.unit),
            unit_price_cents: l.unit_price_cents,
            vat_bp: l.vat_bp,
            line_total_cents: l.line_total_cents,
            source: "manual",
            is_zero: l.is_zero,
            hour_entries: Vec::new(),
            km_entries: Vec::new(),
        })
        .collect();
    let negated = invoice::negate(&lines).map_err(|e| AppError::BadRequest(e.to_string()))?;
    let release = f.release_entries.is_some();
    let intro = format!(
        "Creditering van factuur {}.",
        orig.number.clone().unwrap_or_default()
    );
    let now = db::now();
    let cid = sqlx::query!(
        "INSERT INTO invoices (company_id, client_id, kind, credit_for_invoice_id, release_entries,
                               period_from, period_to, line_style, hide_zero_lines, include_hours_spec,
                               intro_text, created_at, updated_at, created_by)
         VALUES (?, ?, 'credit', ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?)",
        company.id,
        orig.client_id,
        id,
        release,
        orig.period_from,
        orig.period_to,
        orig.line_style,
        orig.hide_zero_lines,
        intro,
        now,
        now,
        user.id
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = model::get(&mut tx, company.id, cid).await?;
    audit::record_insert(&mut tx, Some(user.id), "invoices", cid, &row).await?;
    model::insert_lines(&mut tx, company.id, cid, &negated, Some(user.id)).await?;
    model::recalc(&mut tx, cid).await?;
    tx.commit().await?;
    flash::ok(
        &session,
        "Concept-creditfactuur gemaakt. Pas zo nodig de regels aan en maak hem definitief.",
    )
    .await?;
    Ok(redirect(&format!("/facturen/{cid}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::invoices::issue::{ClientSnapshot, CompanySnapshot};

    #[test]
    fn reminder_texts_are_dutch_and_escalate() {
        let inv = Invoice {
            id: 1,
            company_id: 1,
            client_id: 1,
            kind: "invoice".into(),
            credit_for_invoice_id: None,
            release_entries: false,
            number: Some("VA-2026-0007".into()),
            issue_date: Some("2026-10-01".into()),
            due_date: Some("2026-10-15".into()),
            period_from: "2026-09-01".into(),
            period_to: "2026-09-30".into(),
            line_style: "task".into(),
            hide_zero_lines: false,
            include_hours_spec: false,
            intro_text: "".into(),
            footer_text: "".into(),
            subtotal_cents: 10_000,
            vat_cents: 2100,
            total_cents: 12_100,
            company_snapshot_json: None,
            client_snapshot_json: None,
            issued_at: None,
            sent_at: None,
            sent_to: None,
            pdf_document_id: None,
            created_at: "".into(),
            updated_at: "".into(),
            created_by: None,
        };
        let c = CompanySnapshot {
            name: "Voorbeeld B.V.".into(),
            street: "".into(),
            postal_code: "".into(),
            city: "".into(),
            kvk: "".into(),
            btw_id: "".into(),
            iban: "NL91 ABNA 0417 1643 00".into(),
            email: "".into(),
            phone: "".into(),
            logo_path: None,
        };
        let k = ClientSnapshot {
            name: "Klant".into(),
            contact_name: "Els".into(),
            street: "".into(),
            postal_code: "".into(),
            city: "".into(),
            kvk: "".into(),
            btw_id: "".into(),
            email: "".into(),
        };
        let (s1, b1) = reminder_message("1", &inv, &c, &k, 12_100, "Bram");
        assert_eq!(s1, "Herinnering: factuur VA-2026-0007");
        assert!(b1.starts_with("Beste Els,"));
        assert!(
            b1.contains("€ 121,00")
                && b1.contains("15-10-2026")
                && b1.contains("NL91 ABNA 0417 1643 00")
        );
        let (s2, _) = reminder_message("2", &inv, &c, &k, 12_100, "Bram");
        assert!(s2.starts_with("Tweede herinnering"));
        let (s3, b3) = reminder_message("final", &inv, &c, &k, 5000, "Bram");
        assert!(s3.starts_with("Laatste aanmaning"));
        assert!(b3.contains("€ 50,00") && b3.contains("verdere stappen"));
    }
}
