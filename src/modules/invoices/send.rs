//! Sending an issued invoice: by e-mail with the PDF attached, or marked as
//! sent by hand when e-mail is not configured.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Form;
use serde::Deserialize;
use sqlx::SqliteConnection;
use tower_sessions::Session;

use super::issue::{ClientSnapshot, CompanySnapshot};
use super::model::{self, Invoice};
use crate::mail::Email;
use crate::modules::companies::CurrentCompany;
use crate::web::error::redirect;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format, validate};

pub fn snapshots(inv: &Invoice) -> AppResult<(CompanySnapshot, ClientSnapshot)> {
    let c = inv
        .company_snapshot_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| AppError::BadRequest("Deze factuur is nog een concept.".into()))?;
    let k = inv
        .client_snapshot_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| AppError::BadRequest("Deze factuur is nog een concept.".into()))?;
    Ok((c, k))
}

pub fn greeting(client: &ClientSnapshot) -> String {
    if client.contact_name.is_empty() {
        "Geachte heer/mevrouw,".into()
    } else {
        format!("Beste {},", client.contact_name)
    }
}

fn d(s: &Option<String>) -> String {
    s.as_deref()
        .and_then(format::parse_iso_date)
        .map(format::date)
        .unwrap_or_default()
}

/// Default subject and body (Dutch).
pub fn default_message(
    inv: &Invoice,
    company: &CompanySnapshot,
    client: &ClientSnapshot,
    sender: &str,
) -> (String, String) {
    let number = inv.number.clone().unwrap_or_default();
    if inv.is_credit() {
        return (
            format!("Creditfactuur {number} van {}", company.name),
            format!(
                "{}\n\nIn de bijlage vindt u creditfactuur {number} van {} over {}. Het bedrag van {} wordt met u verrekend of aan u terugbetaald.\n\nMet vriendelijke groet,\n\n{sender}\n{}",
                greeting(client),
                d(&inv.issue_date),
                format::amount(inv.total_cents.abs()),
                inv.period_label(),
                company.name
            ),
        );
    }
    (
        format!("Factuur {number} van {}", company.name),
        format!(
            "{}\n\nIn de bijlage vindt u factuur {number} van {} voor de periode {}.\n\nHet totaalbedrag is {}. Wilt u dit uiterlijk {} overmaken op IBAN {} ten name van {}, onder vermelding van factuurnummer {number}?\n\nMet vriendelijke groet,\n\n{sender}\n{}",
            greeting(client),
            d(&inv.issue_date),
            inv.period_label(),
            format::amount(inv.total_cents),
            d(&inv.due_date),
            company.iban,
            company.name,
            company.name
        ),
    )
}

/// The stored PDF of an invoice: (file name, bytes).
pub async fn pdf_bytes(
    conn: &mut SqliteConnection,
    data_dir: &std::path::Path,
    inv: &Invoice,
) -> AppResult<(String, Vec<u8>)> {
    let doc_id = inv
        .pdf_document_id
        .ok_or_else(|| AppError::BadRequest("Deze factuur heeft nog geen PDF.".into()))?;
    let row = sqlx::query!(
        "SELECT file_path, original_name FROM documents WHERE id = ? AND company_id = ?",
        doc_id,
        inv.company_id
    )
    .fetch_one(conn)
    .await?;
    let bytes = std::fs::read(data_dir.join(&row.file_path))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("PDF lezen: {e}")))?;
    Ok((row.original_name, bytes))
}

async fn mark_sent(st: &AppState, inv: &Invoice, to: &str, by: i64) -> AppResult<()> {
    let now = db::now();
    let mut tx = st.pool.begin().await?;
    sqlx::query!(
        "UPDATE invoices SET sent_at = COALESCE(sent_at, ?), sent_to = ?, updated_at = ?
         WHERE id = ? AND company_id = ? AND number IS NOT NULL",
        now,
        to,
        now,
        inv.id,
        inv.company_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        Some(by),
        "invoices",
        inv.id,
        audit::Action::Update,
        &serde_json::json!({ "sent_at": [inv.sent_at, now], "sent_to": [inv.sent_to, to] }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/send.html")]
pub struct SendPage {
    pub l: Layout,
    pub inv: Invoice,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub can_email: bool,
    pub error: Option<String>,
}

pub async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<SendPage> {
    let inv = {
        let mut conn = st.pool.acquire().await?;
        model::get(&mut conn, company.id, id).await?
    };
    let (c, k) = snapshots(&inv)?;
    let (subject, body) = default_message(&inv, &c, &k, &user.name);
    Ok(SendPage {
        l,
        to: k.email.clone(),
        subject,
        body,
        can_email: st.mailer.is_enabled(),
        error: None,
        inv,
    })
}

#[derive(Deserialize)]
pub struct SendForm {
    #[serde(default)]
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
}

pub async fn send(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
    Form(f): Form<SendForm>,
) -> AppResult<Response> {
    let (inv, pdf) = {
        let mut conn = st.pool.acquire().await?;
        let inv = model::get(&mut conn, company.id, id).await?;
        let pdf = pdf_bytes(&mut conn, &st.cfg.data_dir, &inv).await?;
        (inv, pdf)
    };
    let (c, _) = snapshots(&inv)?;
    let fail = |msg: String, inv: Invoice| SendPage {
        l,
        to: f.to.clone(),
        subject: f.subject.clone(),
        body: f.body.clone(),
        can_email: st.mailer.is_enabled(),
        error: Some(msg),
        inv,
    };
    let to = match validate::email(&f.to) {
        Ok(v) => v,
        Err(m) => return Ok(fail(m.into(), inv).into_response()),
    };
    if f.subject.trim().is_empty() || f.body.trim().is_empty() {
        return Ok(fail("Onderwerp en bericht zijn verplicht.".into(), inv).into_response());
    }
    let email = Email {
        to: to.clone(),
        reply_to: (!c.email.is_empty()).then(|| c.email.clone()),
        from_name: c.name.clone(),
        subject: f.subject.trim().to_string(),
        body: f.body.replace("\r\n", "\n"),
        attachment: Some(pdf),
    };
    if let Err(e) = st.mailer.send(email).await {
        tracing::warn!("sending invoice {}: {e:#}", inv.id);
        return Ok(fail(format!("Versturen is mislukt: {e}"), inv).into_response());
    }
    mark_sent(&st, &inv, &to, user.id).await?;
    flash::ok(&session, &format!("{} verstuurd naar {to}.", inv.title())).await?;
    Ok(redirect(&format!("/facturen/{id}")))
}

pub async fn mark_as_sent(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let inv = {
        let mut conn = st.pool.acquire().await?;
        model::get(&mut conn, company.id, id).await?
    };
    if inv.is_draft() {
        return Err(AppError::BadRequest(
            "Maak de factuur eerst definitief.".into(),
        ));
    }
    mark_sent(&st, &inv, "handmatig", user.id).await?;
    flash::ok(
        &session,
        &format!("{} gemarkeerd als verzonden.", inv.title()),
    )
    .await?;
    Ok(redirect(&format!("/facturen/{id}")))
}
