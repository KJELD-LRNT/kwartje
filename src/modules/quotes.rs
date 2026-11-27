//! Quotes (offertes): draft with manual lines → "Definitief maken" (own
//! number series PREFIX-OFF-YYYY-NNNN, frozen snapshots) → PDF, send,
//! accepted/rejected → "Omzetten naar factuur" (a draft invoice).

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use time::Duration;
use tower_sessions::Session;

use super::clients::{self, ClientOption};
use super::companies::{self, CurrentCompany, VAT_CHOICES};
use super::invoices::issue::{missing_data, ClientSnapshot, CompanySnapshot};
use super::invoices::send::greeting;
use crate::invoice::{self, DraftLine};
use crate::mail::Email;
use crate::pdf::INVOICE_TEMPLATE;
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format, money, validate};

pub const DEFAULT_VALID_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Quote {
    pub id: i64,
    pub company_id: i64,
    pub client_id: i64,
    pub number: Option<String>,
    pub issue_date: Option<String>,
    pub valid_until: String,
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
    pub accepted_at: Option<String>,
    pub rejected_at: Option<String>,
    pub invoice_id: Option<i64>,
    /// Stored PDF (since schema 13); older issued quotes render on request.
    pub pdf_document_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStatus {
    Concept,
    Definitief,
    Verzonden,
    Geaccepteerd,
    Afgewezen,
    Verlopen,
    Gefactureerd,
}

impl QuoteStatus {
    pub fn label(self) -> &'static str {
        match self {
            QuoteStatus::Concept => "concept",
            QuoteStatus::Definitief => "definitief",
            QuoteStatus::Verzonden => "verzonden",
            QuoteStatus::Geaccepteerd => "geaccepteerd",
            QuoteStatus::Afgewezen => "afgewezen",
            QuoteStatus::Verlopen => "verlopen",
            QuoteStatus::Gefactureerd => "gefactureerd",
        }
    }
    /// CSS class of the status badge.
    pub fn css(self) -> &'static str {
        match self {
            QuoteStatus::Concept => "draft",
            QuoteStatus::Definitief => "",
            QuoteStatus::Verzonden => "info",
            QuoteStatus::Geaccepteerd | QuoteStatus::Gefactureerd => "ok",
            QuoteStatus::Afgewezen | QuoteStatus::Verlopen => "bad",
        }
    }
    /// Waiting for the client's answer.
    pub fn is_open(self) -> bool {
        matches!(self, QuoteStatus::Definitief | QuoteStatus::Verzonden)
    }
}

impl Quote {
    pub fn is_draft(&self) -> bool {
        self.number.is_none()
    }
    pub fn title(&self) -> String {
        match &self.number {
            Some(n) => format!("Offerte {n}"),
            None => "Offerte (concept)".into(),
        }
    }
    pub fn status_on(&self, today: &str) -> QuoteStatus {
        if self.is_draft() {
            QuoteStatus::Concept
        } else if self.invoice_id.is_some() {
            QuoteStatus::Gefactureerd
        } else if self.accepted_at.is_some() {
            QuoteStatus::Geaccepteerd
        } else if self.rejected_at.is_some() {
            QuoteStatus::Afgewezen
        } else if self.valid_until.as_str() < today {
            QuoteStatus::Verlopen
        } else if self.sent_at.is_some() {
            QuoteStatus::Verzonden
        } else {
            QuoteStatus::Definitief
        }
    }
    pub fn status(&self) -> QuoteStatus {
        self.status_on(&format::iso_date(format::today()))
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct QuoteLine {
    pub id: i64,
    pub quote_id: i64,
    pub position: i64,
    pub description: String,
    pub quantity_milli: i64,
    pub unit: String,
    pub unit_price_cents: i64,
    pub vat_bp: i64,
    pub line_total_cents: i64,
}

impl QuoteLine {
    pub fn quantity_label(&self) -> String {
        invoice::quantity_label(self.quantity_milli)
    }
}

// ---------- queries ----------

pub async fn get_quote(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Quote> {
    sqlx::query_as!(
        Quote,
        r#"SELECT id AS "id!", company_id, client_id, number, issue_date, valid_until, intro_text,
                  footer_text, subtotal_cents, vat_cents, total_cents, company_snapshot_json,
                  client_snapshot_json, issued_at, sent_at, sent_to, accepted_at, rejected_at,
                  invoice_id, pdf_document_id, created_at, updated_at, created_by
           FROM quotes WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

async fn get_draft(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Quote> {
    let q = get_quote(conn, company_id, id).await?;
    if !q.is_draft() {
        return Err(AppError::BadRequest(
            "Deze offerte is definitief en kan niet meer worden gewijzigd.".into(),
        ));
    }
    Ok(q)
}

pub async fn lines(conn: &mut SqliteConnection, quote_id: i64) -> AppResult<Vec<QuoteLine>> {
    Ok(sqlx::query_as!(
        QuoteLine,
        r#"SELECT id AS "id!", quote_id, position, description, quantity_milli, unit,
                  unit_price_cents, vat_bp, line_total_cents
           FROM quote_lines WHERE quote_id = ? ORDER BY position, id"#,
        quote_id
    )
    .fetch_all(conn)
    .await?)
}

fn totals(ls: &[QuoteLine]) -> AppResult<money::Totals> {
    money::totals(
        &ls.iter()
            .map(|l| (l.line_total_cents, l.vat_bp))
            .collect::<Vec<_>>(),
    )
    .map_err(|e| AppError::BadRequest(e.to_string()))
}

async fn recalc(conn: &mut SqliteConnection, id: i64) -> AppResult<()> {
    let t = totals(&lines(conn, id).await?)?;
    let now = db::now();
    sqlx::query!(
        "UPDATE quotes SET subtotal_cents = ?, vat_cents = ?, total_cents = ?, updated_at = ?
         WHERE id = ? AND number IS NULL",
        t.subtotal_cents,
        t.vat_cents,
        t.total_cents,
        now,
        id
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub struct QuoteRow {
    pub q: Quote,
    pub client_name: String,
}

/// All quotes of the company with their client's name (two queries).
pub async fn list(pool: &SqlitePool, company_id: i64) -> AppResult<Vec<QuoteRow>> {
    let quotes = sqlx::query_as!(
        Quote,
        r#"SELECT id AS "id!", company_id, client_id, number, issue_date, valid_until, intro_text,
                  footer_text, subtotal_cents, vat_cents, total_cents, company_snapshot_json,
                  client_snapshot_json, issued_at, sent_at, sent_to, accepted_at, rejected_at,
                  invoice_id, pdf_document_id, created_at, updated_at, created_by
           FROM quotes WHERE company_id = ?
           ORDER BY number IS NOT NULL, issue_date DESC, id DESC"#,
        company_id
    )
    .fetch_all(pool)
    .await?;
    let names = client_names(pool, company_id).await?;
    Ok(quotes
        .into_iter()
        .map(|q| QuoteRow {
            client_name: names.get(&q.client_id).cloned().unwrap_or_default(),
            q,
        })
        .collect())
}

/// id → name of every client of the company, archived ones included.
pub async fn client_names(
    pool: &SqlitePool,
    company_id: i64,
) -> AppResult<std::collections::HashMap<i64, String>> {
    Ok(sqlx::query!(
        r#"SELECT id AS "id!", name FROM clients WHERE company_id = ?"#,
        company_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.id, r.name))
    .collect())
}

// ---------- pages ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/offertes", get(list_page).post(create))
        .route("/offertes/nieuw", get(new_page))
        .route("/offertes/{id}", get(show).post(save))
        .route("/offertes/{id}/regels", post(add_line))
        .route(
            "/offertes/{id}/regels/{line}/verwijderen",
            post(remove_line),
        )
        .route("/offertes/{id}/verwijderen", post(remove_draft))
        .route("/offertes/{id}/definitief", post(issue))
        .route("/offertes/{id}/pdf", get(pdf))
        .route("/offertes/{id}/versturen", post(send))
        .route("/offertes/{id}/verzonden", post(mark_sent))
        .route("/offertes/{id}/geaccepteerd", post(accept))
        .route("/offertes/{id}/afgewezen", post(reject))
        .route("/offertes/{id}/factuur", post(to_invoice))
}

#[derive(Template, WebTemplate)]
#[template(path = "quotes/list.html")]
struct ListPage {
    l: Layout,
    rows: Vec<QuoteRow>,
    clients: Vec<ClientOption>,
}

async fn list_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
) -> AppResult<ListPage> {
    Ok(ListPage {
        l,
        rows: list(&st.pool, company.id).await?,
        clients: clients::options(&st.pool, company.id).await?,
    })
}

/// "Nieuwe offerte" lives on the list page; this keeps old links working.
async fn new_page(_user: AuthUser) -> Response {
    redirect("/offertes")
}

#[derive(Deserialize)]
struct CreateForm {
    #[serde(default)]
    client_id: String,
}

async fn create(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    Form(f): Form<CreateForm>,
) -> AppResult<Response> {
    let client_id: i64 = f
        .client_id
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("Kies een klant.".into()))?;
    let mut tx = st.pool.begin().await?;
    clients::get_active(&mut tx, company.id, client_id).await?;
    let now = db::now();
    let valid = format::iso_date(format::today() + Duration::days(DEFAULT_VALID_DAYS));
    let id = sqlx::query!(
        "INSERT INTO quotes (company_id, client_id, valid_until, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?)",
        company.id,
        client_id,
        valid,
        now,
        now,
        user.id
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get_quote(&mut tx, company.id, id).await?;
    audit::record_insert(&mut tx, Some(user.id), "quotes", id, &row).await?;
    tx.commit().await?;
    Ok(redirect(&format!("/offertes/{id}")))
}

#[derive(Template, WebTemplate)]
#[template(path = "quotes/show.html")]
struct ShowPage {
    l: Layout,
    q: Quote,
    client_name: String,
    client_email: String,
    lines: Vec<QuoteLine>,
    totals: money::Totals,
    status: QuoteStatus,
    vat_choices: Vec<(String, String)>,
    can_email: bool,
    subject: String,
    body: String,
    invoice_label: Option<String>,
}

async fn show(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<ShowPage> {
    let mut conn = st.pool.acquire().await?;
    let q = get_quote(&mut conn, company.id, id).await?;
    let client = clients::get(&mut conn, company.id, q.client_id).await?;
    let ls = lines(&mut conn, id).await?;
    let invoice_label = match q.invoice_id {
        Some(i) => Some(
            super::invoices::model::get(&mut conn, company.id, i)
                .await?
                .number
                .unwrap_or_else(|| "conceptfactuur".into()),
        ),
        None => None,
    };
    drop(conn);
    let (subject, body) = match snapshots(&q) {
        Ok((c, k)) => message(&q, &c, &k, &user.name),
        Err(_) => (String::new(), String::new()),
    };
    Ok(ShowPage {
        l,
        status: q.status(),
        totals: totals(&ls)?,
        lines: ls,
        client_name: client.name,
        client_email: client.email,
        vat_choices: VAT_CHOICES
            .iter()
            .map(|bp| (bp.to_string(), format::vat_pct(*bp)))
            .collect(),
        can_email: st.mailer.is_enabled(),
        subject,
        body,
        invoice_label,
        q,
    })
}

#[derive(Deserialize)]
struct SaveForm {
    #[serde(default)]
    valid_until: String,
    #[serde(default)]
    intro_text: String,
    #[serde(default)]
    footer_text: String,
}

async fn save(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<SaveForm>,
) -> AppResult<Response> {
    let Some(valid) = format::parse_date(&f.valid_until) else {
        flash::error(&session, "Vul een geldige datum in bij 'geldig tot'.").await?;
        return Ok(redirect(&format!("/offertes/{id}")));
    };
    let valid = format::iso_date(valid);
    let mut tx = st.pool.begin().await?;
    let old = get_draft(&mut tx, company.id, id).await?;
    let now = db::now();
    let (intro, footer) = (f.intro_text.trim(), f.footer_text.trim());
    sqlx::query!(
        "UPDATE quotes SET valid_until = ?, intro_text = ?, footer_text = ?, updated_at = ?
         WHERE id = ? AND company_id = ? AND number IS NULL",
        valid,
        intro,
        footer,
        now,
        id,
        company.id
    )
    .execute(&mut *tx)
    .await?;
    let new = get_quote(&mut tx, company.id, id).await?;
    audit::record_update(&mut tx, Some(user.id), "quotes", id, &old, &new).await?;
    tx.commit().await?;
    flash::ok(&session, "Offerte opgeslagen.").await?;
    Ok(redirect(&format!("/offertes/{id}")))
}

async fn add_line(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<invoice::ManualLineForm>,
) -> AppResult<Response> {
    let back = format!("/offertes/{id}");
    let line = match f.parse(VAT_CHOICES) {
        Ok(l) => l,
        Err(m) => {
            flash::error(&session, &m).await?;
            return Ok(redirect(&back));
        }
    };
    let mut tx = st.pool.begin().await?;
    get_draft(&mut tx, company.id, id).await?;
    let now = db::now();
    let pos: i64 = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(position), 0) + 1 AS "p!: i64" FROM quote_lines WHERE quote_id = ?"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let lid = sqlx::query!(
        "INSERT INTO quote_lines (quote_id, position, description, quantity_milli, unit, unit_price_cents,
                                  vat_bp, line_total_cents, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        pos,
        line.description,
        line.quantity_milli,
        line.unit,
        line.unit_price_cents,
        line.vat_bp,
        line.line_total_cents,
        now,
        now,
        user.id
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    audit::record_insert(&mut tx, Some(user.id), "quote_lines", lid, &serde_json::json!({"quote_id": id, "description": line.description, "line_total_cents": line.line_total_cents})).await?;
    recalc(&mut tx, id).await?;
    tx.commit().await?;
    Ok(redirect(&back))
}

async fn remove_line(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    Path((id, line)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    get_draft(&mut tx, company.id, id).await?;
    let n = sqlx::query!(
        "DELETE FROM quote_lines WHERE id = ? AND quote_id = ?",
        line,
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        Some(user.id),
        "quote_lines",
        line,
        audit::Action::Delete,
        &serde_json::json!({"quote_id": id}),
    )
    .await?;
    recalc(&mut tx, id).await?;
    tx.commit().await?;
    Ok(redirect(&format!("/offertes/{id}")))
}

async fn remove_draft(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    let q = get_draft(&mut tx, company.id, id).await?;
    sqlx::query!("DELETE FROM quote_lines WHERE quote_id = ?", id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM quotes WHERE id = ? AND number IS NULL", id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        Some(user.id),
        "quotes",
        id,
        audit::Action::Delete,
        &serde_json::to_value(&q).expect("row serialises"),
    )
    .await?;
    tx.commit().await?;
    flash::ok(&session, "Concept-offerte verwijderd.").await?;
    Ok(redirect("/offertes"))
}

/// What issuing a quote needs, read without writing.
struct Prepared {
    q: Quote,
    prefix: String,
    c: CompanySnapshot,
    k: ClientSnapshot,
    ls: Vec<QuoteLine>,
    t: money::Totals,
    assets: Vec<(String, Vec<u8>)>,
    logo: Option<String>,
}

async fn prepare(
    st: &AppState,
    conn: &mut SqliteConnection,
    company_id: i64,
    id: i64,
) -> AppResult<Prepared> {
    let q = get_draft(conn, company_id, id).await?;
    let c = companies::get(conn, company_id).await?;
    let k = clients::get(conn, company_id, q.client_id).await?;
    let ls = lines(conn, id).await?;
    let missing: Vec<String> = missing_data(&c, &k, ls.len())
        .into_iter()
        .map(|m| m.replace("factuurregel", "offerteregel"))
        .collect();
    if !missing.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Deze offerte kan nog niet definitief worden. Ontbreekt: {}.",
            missing.join(", ")
        )));
    }
    let t = totals(&ls)?;
    let mut assets = Vec::new();
    let mut logo = None;
    if let Some(rel) = &c.logo_path {
        if let Ok(b) = std::fs::read(st.cfg.data_dir.join(rel)) {
            let name = format!("logo.{}", rel.rsplit('.').next().unwrap_or("png"));
            assets.push((name.clone(), b));
            logo = Some(name);
        }
    }
    Ok(Prepared {
        prefix: format!("{}-OFF", k.invoice_prefix.clone().unwrap_or_default()),
        c: CompanySnapshot::of(&c),
        k: ClientSnapshot::of(&k),
        q,
        ls,
        t,
        assets,
        logo,
    })
}

/// The PDF data of `p` once it has `number` and today's date.
fn issued_data(p: &Prepared, number: &str, issue_date: &str) -> String {
    let q = Quote {
        number: Some(number.to_string()),
        issue_date: Some(issue_date.to_string()),
        ..p.q.clone()
    };
    pdf_data(&q, &p.c, &p.k, &p.ls, &p.t, p.logo.as_deref()).to_string()
}

async fn quote_number(
    conn: &mut SqliteConnection,
    company_id: i64,
    prefix: &str,
    year: i32,
    reserve: bool,
) -> AppResult<String> {
    let y = i64::from(year);
    let last: Option<i64> = sqlx::query_scalar!(
        "SELECT last_number FROM quote_sequences WHERE company_id = ? AND year = ?",
        company_id,
        y
    )
    .fetch_optional(&mut *conn)
    .await?;
    let n = last.unwrap_or(0) + 1;
    if reserve {
        let now = db::now();
        if last.is_some() {
            sqlx::query!(
                "UPDATE quote_sequences SET last_number = ?, updated_at = ? WHERE company_id = ? AND year = ?",
                n, now, company_id, y
            )
            .execute(&mut *conn)
            .await?;
        } else {
            sqlx::query!(
                "INSERT INTO quote_sequences (company_id, year, last_number, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
                company_id, y, n, now, now
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(invoice::number(prefix, year, n))
}

/// Make a draft quote final: number, frozen snapshots and a stored PDF, in
/// the same way as invoices (render outside the write lock, commit only if
/// nothing changed meanwhile, otherwise render again).
pub async fn issue_quote(st: &AppState, company_id: i64, id: i64, by: i64) -> AppResult<String> {
    let today = format::today();
    let issue_date = format::iso_date(today);
    for _ in 0..3 {
        let (rendered, pdf) = {
            let mut conn = st.pool.acquire().await?;
            let p = prepare(st, &mut conn, company_id, id).await?;
            let candidate =
                quote_number(&mut conn, company_id, &p.prefix, today.year(), false).await?;
            drop(conn);
            let data = issued_data(&p, &candidate, &issue_date);
            let pdf = crate::pdf::render_blocking(
                st.pdf.clone(),
                INVOICE_TEMPLATE,
                data.clone(),
                p.assets.clone(),
            )
            .await
            .map_err(|e| AppError::BadRequest(format!("De PDF kon niet worden gemaakt: {e}")))?;
            (data, pdf)
        };
        let mut tx = st.pool.begin_with("BEGIN IMMEDIATE").await?;
        let p = prepare(st, &mut tx, company_id, id).await?;
        let number = quote_number(&mut tx, company_id, &p.prefix, today.year(), true).await?;
        if issued_data(&p, &number, &issue_date) != rendered {
            tx.rollback().await?;
            continue;
        }
        // Store the PDF as a document of kind 'quote'.
        let now = db::now();
        let rel = format!(
            "documents/{company_id}/{}/quote/{id}_{number}.pdf",
            today.year()
        );
        let abs = st.cfg.data_dir.join(&rel);
        let dir = abs.parent().expect("has parent").to_path_buf();
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".quote-{id}.tmp"));
            std::fs::write(&tmp, &pdf)?;
            std::fs::rename(&tmp, &abs)
        };
        write().map_err(|e| AppError::Internal(anyhow::anyhow!("PDF opslaan: {e}")))?;
        let cleanup = |e: AppError| {
            let _ = std::fs::remove_file(&abs);
            e
        };
        let sha = crate::export::sha256_hex(&pdf);
        let size = pdf.len() as i64;
        let original = format!("{number}.pdf");
        let doc_id = sqlx::query!(
            "INSERT INTO documents (company_id, kind, file_path, sha256, original_name, mime, size,
                                    doc_date, amount_cents, vat_cents, client_id, created_at, updated_at, created_by)
             VALUES (?, 'quote', ?, ?, ?, 'application/pdf', ?, ?, ?, ?, ?, ?, ?, ?)",
            company_id,
            rel,
            sha,
            original,
            size,
            issue_date,
            p.t.total_cents,
            p.t.vat_cents,
            p.q.client_id,
            now,
            now,
            by
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| cleanup(e.into()))?
        .last_insert_rowid();
        audit::record_insert(
            &mut tx,
            Some(by),
            "documents",
            doc_id,
            &serde_json::json!({"kind": "quote", "file_path": rel, "sha256": sha, "quote_id": id}),
        )
        .await
        .map_err(|e| cleanup(AppError::Internal(e)))?;
        let cj = serde_json::to_string(&p.c).expect("snapshot serialises");
        let kj = serde_json::to_string(&p.k).expect("snapshot serialises");
        sqlx::query!(
            "UPDATE quotes SET number = ?, issue_date = ?, subtotal_cents = ?, vat_cents = ?, total_cents = ?,
                    company_snapshot_json = ?, client_snapshot_json = ?, issued_at = ?, pdf_document_id = ?,
                    updated_at = ?
             WHERE id = ? AND number IS NULL",
            number,
            issue_date,
            p.t.subtotal_cents,
            p.t.vat_cents,
            p.t.total_cents,
            cj,
            kj,
            now,
            doc_id,
            now,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| cleanup(e.into()))?;
        let new = get_quote(&mut tx, company_id, id).await.map_err(cleanup)?;
        audit::record_update(&mut tx, Some(by), "quotes", id, &p.q, &new)
            .await
            .map_err(|e| cleanup(AppError::Internal(e)))?;
        tx.commit().await.map_err(|e| cleanup(e.into()))?;
        return Ok(number);
    }
    Err(AppError::BadRequest(
        "Tijdens het maken van de PDF werd er ook iets anders gewijzigd. Probeer het opnieuw."
            .into(),
    ))
}

async fn issue(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let back = format!("/offertes/{id}");
    match issue_quote(&st, company.id, id, user.id).await {
        Ok(number) => flash::ok(&session, &format!("Offerte {number} is definitief.")).await?,
        Err(AppError::BadRequest(m)) => flash::error(&session, &m).await?,
        Err(e) => return Err(e),
    }
    Ok(redirect(&back))
}

fn snapshots(q: &Quote) -> AppResult<(CompanySnapshot, ClientSnapshot)> {
    let c = q
        .company_snapshot_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());
    let k = q
        .client_snapshot_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());
    match (c, k) {
        (Some(c), Some(k)) => Ok((c, k)),
        _ => Err(AppError::BadRequest(
            "Maak de offerte eerst definitief.".into(),
        )),
    }
}

fn d(s: &str) -> String {
    format::parse_iso_date(s)
        .map(format::date)
        .unwrap_or_default()
}

/// Data for the shared Typst template.
pub fn pdf_data(
    q: &Quote,
    c: &CompanySnapshot,
    k: &ClientSnapshot,
    ls: &[QuoteLine],
    t: &money::Totals,
    logo: Option<&str>,
) -> serde_json::Value {
    let addr = |s: &str, pc: &str, city: &str| -> Vec<String> {
        [s.to_string(), format!("{pc} {city}").trim().to_string()]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect()
    };
    let number = q.number.clone().unwrap_or_default();
    let meta = vec![
        vec!["Offertenummer".to_string(), number.clone()],
        vec![
            "Datum".to_string(),
            q.issue_date.as_deref().map(d).unwrap_or_default(),
        ],
        vec!["Geldig tot".to_string(), d(&q.valid_until)],
    ];
    serde_json::json!({
        "company": {"name": c.name, "address": addr(&c.street, &c.postal_code, &c.city), "kvk": c.kvk, "btw_id": c.btw_id, "iban": c.iban, "email": c.email, "phone": c.phone},
        "client": {"name": k.name, "contact": k.contact_name, "address": addr(&k.street, &k.postal_code, &k.city), "btw_id": k.btw_id},
        "invoice": {"title": format!("Offerte {number}"), "kind_label": "Offerte", "number": number, "intro": q.intro_text, "footer": q.footer_text},
        "meta": meta,
        "lines": ls.iter().map(|l| serde_json::json!({
            "description": l.description, "quantity": format!("{} {}", l.quantity_label(), l.unit),
            "price": format::amount(l.unit_price_cents), "vat": format::vat_pct(l.vat_bp), "total": format::amount(l.line_total_cents),
        })).collect::<Vec<_>>(),
        "vat_groups": t.per_rate.iter().map(|(bp, g)| serde_json::json!({"rate": format::vat_pct(*bp), "base": format::amount(g.base_cents), "vat": format::amount(g.vat_cents)})).collect::<Vec<_>>(),
        "subtotal": format::amount(t.subtotal_cents),
        "total": format::amount(t.total_cents),
        "closing": format!("Deze offerte is geldig tot en met {}. Gaat u akkoord? Laat het ons weten door te antwoorden op deze offerte (onder vermelding van {number}).", d(&q.valid_until)),
        "logo": logo,
        "spec": serde_json::Value::Null,
    })
}

async fn render(st: &AppState, q: &Quote) -> AppResult<(String, Vec<u8>)> {
    let name = format!("{}.pdf", q.number.clone().unwrap_or_default());
    if let Some(doc) = q.pdf_document_id {
        let path: String = {
            let mut conn = st.pool.acquire().await?;
            sqlx::query_scalar!(
                "SELECT file_path FROM documents WHERE id = ? AND company_id = ?",
                doc,
                q.company_id
            )
            .fetch_one(&mut *conn)
            .await?
        };
        let bytes = tokio::fs::read(st.cfg.data_dir.join(&path))
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("PDF {path} lezen: {e}")))?;
        return Ok((name, bytes));
    }
    // Quotes issued before schema 13 have no stored PDF.
    let (c, k) = snapshots(q)?;
    let ls = {
        let mut conn = st.pool.acquire().await?;
        lines(&mut conn, q.id).await?
    };
    let t = totals(&ls)?;
    let mut assets = Vec::new();
    let mut logo = None;
    if let Some(rel) = &c.logo_path {
        if let Ok(b) = std::fs::read(st.cfg.data_dir.join(rel)) {
            let name = format!("logo.{}", rel.rsplit('.').next().unwrap_or("png"));
            assets.push((name.clone(), b));
            logo = Some(name);
        }
    }
    let data = pdf_data(q, &c, &k, &ls, &t, logo.as_deref());
    let pdf =
        crate::pdf::render_blocking(st.pdf.clone(), INVOICE_TEMPLATE, data.to_string(), assets)
            .await
            .map_err(|e| AppError::BadRequest(format!("De PDF kon niet worden gemaakt: {e}")))?;
    Ok((format!("{}.pdf", q.number.clone().unwrap_or_default()), pdf))
}

async fn pdf(
    State(st): State<AppState>,
    _u: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let q = {
        let mut conn = st.pool.acquire().await?;
        get_quote(&mut conn, company.id, id).await?
    };
    let (name, bytes) = render(&st, &q).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

fn message(q: &Quote, c: &CompanySnapshot, k: &ClientSnapshot, sender: &str) -> (String, String) {
    let n = q.number.clone().unwrap_or_default();
    (
        format!("Offerte {n} van {}", c.name),
        format!(
            "{}\n\nIn de bijlage vindt u onze offerte {n}, geldig tot en met {}. Het totaalbedrag is {} (incl. btw).\n\nGaat u akkoord, of heeft u vragen? Dan horen wij graag van u.\n\nMet vriendelijke groet,\n\n{sender}\n{}",
            greeting(k),
            d(&q.valid_until),
            format::amount(q.total_cents),
            c.name
        ),
    )
}

async fn set_sent(st: &AppState, q: &Quote, to: &str, by: i64) -> AppResult<()> {
    let now = db::now();
    let mut tx = st.pool.begin().await?;
    sqlx::query!("UPDATE quotes SET sent_at = COALESCE(sent_at, ?), sent_to = ?, updated_at = ? WHERE id = ? AND number IS NOT NULL", now, to, now, q.id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        Some(by),
        "quotes",
        q.id,
        audit::Action::Update,
        &serde_json::json!({"sent_at": [q.sent_at, now], "sent_to": [q.sent_to, to]}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize)]
struct SendForm {
    #[serde(default)]
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
}

async fn send(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<SendForm>,
) -> AppResult<Response> {
    let back = format!("/offertes/{id}");
    let q = {
        let mut conn = st.pool.acquire().await?;
        get_quote(&mut conn, company.id, id).await?
    };
    let (c, _) = snapshots(&q)?;
    let to = match validate::email(&f.to) {
        Ok(t) => t,
        Err(m) => {
            flash::error(&session, m).await?;
            return Ok(redirect(&back));
        }
    };
    let pdf = render(&st, &q).await?;
    let email = Email {
        to: to.clone(),
        reply_to: (!c.email.is_empty()).then(|| c.email.clone()),
        from_name: c.name.clone(),
        subject: f.subject.trim().to_string(),
        body: f.body.replace("\r\n", "\n"),
        attachment: Some(pdf),
    };
    if let Err(e) = st.mailer.send(email).await {
        flash::error(&session, &format!("Versturen is mislukt: {e}")).await?;
        return Ok(redirect(&back));
    }
    set_sent(&st, &q, &to, user.id).await?;
    flash::ok(&session, &format!("{} verstuurd naar {to}.", q.title())).await?;
    Ok(redirect(&back))
}

async fn mark_sent(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let q = {
        let mut conn = st.pool.acquire().await?;
        get_quote(&mut conn, company.id, id).await?
    };
    if q.is_draft() {
        return Err(AppError::BadRequest(
            "Maak de offerte eerst definitief.".into(),
        ));
    }
    set_sent(&st, &q, "handmatig", user.id).await?;
    flash::ok(&session, "Offerte gemarkeerd als verzonden.").await?;
    Ok(redirect(&format!("/offertes/{id}")))
}

async fn decide(
    st: &AppState,
    company_id: i64,
    id: i64,
    accepted: bool,
    by: i64,
) -> AppResult<Quote> {
    let now = db::now();
    let mut tx = st.pool.begin().await?;
    let q = get_quote(&mut tx, company_id, id).await?;
    if q.is_draft() || q.invoice_id.is_some() {
        return Err(AppError::BadRequest(
            "Dit kan alleen bij een definitieve offerte die nog niet is gefactureerd.".into(),
        ));
    }
    let (acc, rej) = if accepted {
        (Some(now.clone()), None)
    } else {
        (None, Some(now.clone()))
    };
    sqlx::query!(
        "UPDATE quotes SET accepted_at = ?, rejected_at = ?, updated_at = ? WHERE id = ?",
        acc,
        rej,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(&mut tx, Some(by), "quotes", id, audit::Action::Update, &serde_json::json!({"accepted_at": [q.accepted_at, acc], "rejected_at": [q.rejected_at, rej]})).await?;
    tx.commit().await?;
    Ok(q)
}

async fn accept(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    decide(&st, company.id, id, true, user.id).await?;
    flash::ok(&session, "Offerte gemarkeerd als geaccepteerd.").await?;
    Ok(redirect(&format!("/offertes/{id}")))
}

async fn reject(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    decide(&st, company.id, id, false, user.id).await?;
    flash::ok(&session, "Offerte gemarkeerd als afgewezen.").await?;
    Ok(redirect(&format!("/offertes/{id}")))
}

/// Turn an issued quote into a draft invoice with the same lines.
pub async fn convert(pool: &SqlitePool, company_id: i64, id: i64, by: i64) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    let q = get_quote(&mut tx, company_id, id).await?;
    if q.is_draft() {
        return Err(AppError::BadRequest(
            "Maak de offerte eerst definitief.".into(),
        ));
    }
    if q.invoice_id.is_some() {
        return Err(AppError::BadRequest(
            "Deze offerte is al omgezet naar een factuur.".into(),
        ));
    }
    if q.rejected_at.is_some() {
        return Err(AppError::BadRequest(
            "Een afgewezen offerte kan niet worden gefactureerd.".into(),
        ));
    }
    let ls = lines(&mut tx, id).await?;
    let drafts: Vec<DraftLine> = ls
        .iter()
        .map(|l| DraftLine {
            description: l.description.clone(),
            quantity_milli: l.quantity_milli,
            unit: invoice::unit(&l.unit),
            unit_price_cents: l.unit_price_cents,
            vat_bp: l.vat_bp,
            line_total_cents: l.line_total_cents,
            source: "manual",
            is_zero: l.unit_price_cents == 0,
            hour_entries: Vec::new(),
            km_entries: Vec::new(),
        })
        .collect();
    let now = db::now();
    let today = format::iso_date(format::today());
    let intro = format!("Volgens offerte {}.", q.number.clone().unwrap_or_default());
    let inv = sqlx::query!(
        "INSERT INTO invoices (company_id, client_id, period_from, period_to, intro_text, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        q.client_id,
        today,
        today,
        intro,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = super::invoices::model::get(&mut tx, company_id, inv).await?;
    audit::record_insert(&mut tx, Some(by), "invoices", inv, &row).await?;
    super::invoices::model::insert_lines(&mut tx, company_id, inv, &drafts, Some(by)).await?;
    super::invoices::model::recalc(&mut tx, inv).await?;
    let accepted = q.accepted_at.clone().unwrap_or(now.clone());
    sqlx::query!(
        "UPDATE quotes SET invoice_id = ?, accepted_at = ?, updated_at = ? WHERE id = ?",
        inv,
        accepted,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        Some(by),
        "quotes",
        id,
        audit::Action::Update,
        &serde_json::json!({"invoice_id": [serde_json::Value::Null, inv]}),
    )
    .await?;
    tx.commit().await?;
    Ok(inv)
}

async fn to_invoice(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    match convert(&st.pool, company.id, id, user.id).await {
        Ok(inv) => {
            flash::ok(
                &session,
                "Conceptfactuur gemaakt van de offerte. Controleer hem en maak hem definitief.",
            )
            .await?;
            Ok(redirect(&format!("/facturen/{inv}")))
        }
        Err(AppError::BadRequest(m)) => {
            flash::error(&session, &m).await?;
            Ok(redirect(&format!("/offertes/{id}")))
        }
        Err(e) => Err(e),
    }
}

/// Count of issued quotes awaiting an answer (dashboard).
pub async fn open_count(pool: &SqlitePool, company_id: i64) -> AppResult<usize> {
    Ok(list(pool, company_id)
        .await?
        .iter()
        .filter(|r| r.q.status().is_open())
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Quote {
        Quote {
            id: 1,
            company_id: 1,
            client_id: 1,
            number: Some("AA-OFF-2026-0001".into()),
            issue_date: Some("2026-11-01".into()),
            valid_until: "2026-12-01".into(),
            intro_text: "".into(),
            footer_text: "".into(),
            subtotal_cents: 0,
            vat_cents: 0,
            total_cents: 0,
            company_snapshot_json: None,
            client_snapshot_json: None,
            issued_at: None,
            sent_at: None,
            sent_to: None,
            accepted_at: None,
            rejected_at: None,
            invoice_id: None,
            pdf_document_id: None,
            created_at: "".into(),
            updated_at: "".into(),
            created_by: None,
        }
    }

    #[test]
    fn statuses() {
        assert_eq!(q().status_on("2026-11-10"), QuoteStatus::Definitief);
        assert_eq!(
            Quote {
                sent_at: Some("x".into()),
                ..q()
            }
            .status_on("2026-11-10"),
            QuoteStatus::Verzonden
        );
        assert_eq!(
            Quote {
                sent_at: Some("x".into()),
                ..q()
            }
            .status_on("2026-12-02"),
            QuoteStatus::Verlopen
        );
        assert_eq!(
            Quote {
                accepted_at: Some("x".into()),
                ..q()
            }
            .status_on("2026-12-02"),
            QuoteStatus::Geaccepteerd
        );
        assert_eq!(
            Quote {
                rejected_at: Some("x".into()),
                ..q()
            }
            .status_on("2026-11-10"),
            QuoteStatus::Afgewezen
        );
        assert_eq!(
            Quote {
                invoice_id: Some(3),
                ..q()
            }
            .status_on("2026-11-10"),
            QuoteStatus::Gefactureerd
        );
        assert_eq!(
            Quote {
                number: None,
                issue_date: None,
                ..q()
            }
            .status_on("2026-11-10"),
            QuoteStatus::Concept
        );
        assert!(QuoteStatus::Verzonden.is_open() && !QuoteStatus::Verlopen.is_open());
    }
}
