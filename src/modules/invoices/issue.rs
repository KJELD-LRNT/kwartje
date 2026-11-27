//! "Definitief maken": number, snapshots and PDF in one transaction. If any
//! step fails nothing is stored and no number is used.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Form;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use time::Duration;
use tower_sessions::Session;

use super::model::{self, Invoice, Line};
use crate::invoice;
use crate::modules::clients::{self, Client};
use crate::modules::companies::{self, Company, CurrentCompany};
use crate::pdf::{PdfRenderer, INVOICE_TEMPLATE};
use crate::web::error::redirect;
use crate::web::layout::AuthUser;
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format, money};

/// Frozen at issuing and stored as JSON on the invoice/quote. Fields added
/// later must keep old JSON readable: the struct-level `serde(default)` fills
/// missing ones (see the compatibility test below).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompanySnapshot {
    pub name: String,
    pub street: String,
    pub postal_code: String,
    pub city: String,
    pub kvk: String,
    pub btw_id: String,
    pub iban: String,
    pub email: String,
    pub phone: String,
    pub logo_path: Option<String>,
}

impl CompanySnapshot {
    pub fn of(c: &Company) -> Self {
        CompanySnapshot {
            name: c.name.clone(),
            street: c.street.clone(),
            postal_code: c.postal_code.clone(),
            city: c.city.clone(),
            kvk: c.kvk.clone(),
            btw_id: c.btw_id.clone(),
            iban: c.iban.clone(),
            email: c.email.clone(),
            phone: c.phone.clone(),
            logo_path: c.logo_path.clone(),
        }
    }
}

/// Frozen at issuing and stored as JSON on the invoice/quote. Fields added
/// later must keep old JSON readable: the struct-level `serde(default)` fills
/// missing ones (see the compatibility test below).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSnapshot {
    pub name: String,
    pub contact_name: String,
    pub street: String,
    pub postal_code: String,
    pub city: String,
    pub kvk: String,
    pub btw_id: String,
    pub email: String,
}

impl ClientSnapshot {
    pub fn of(c: &Client) -> Self {
        ClientSnapshot {
            name: c.name.clone(),
            contact_name: c.contact_name.clone(),
            street: c.street.clone(),
            postal_code: c.postal_code.clone(),
            city: c.city.clone(),
            kvk: c.kvk.clone(),
            btw_id: c.btw_id.clone(),
            email: c.email.clone(),
        }
    }
}

/// Dutch list of what is missing before an invoice may be issued.
pub fn missing_data(c: &Company, k: &Client, line_count: usize) -> Vec<String> {
    let mut m = Vec::new();
    let company_fields = [
        (
            c.street.is_empty() || c.postal_code.is_empty() || c.city.is_empty(),
            "adres van het bedrijf",
        ),
        (c.kvk.is_empty(), "KvK-nummer van het bedrijf"),
        (c.btw_id.is_empty(), "btw-id van het bedrijf"),
        (c.iban.is_empty(), "IBAN van het bedrijf"),
    ];
    for (missing, what) in company_fields {
        if missing {
            m.push(what.to_string());
        }
    }
    if k.street.is_empty() || k.postal_code.is_empty() || k.city.is_empty() {
        m.push(format!("adres van klant {}", k.name));
    }
    if k.invoice_prefix.is_none() {
        m.push(format!(
            "factuurprefix van klant {} (bij de klantgegevens)",
            k.name
        ));
    }
    if line_count == 0 {
        m.push("minstens één factuurregel".into());
    }
    m
}

fn d(s: &str) -> String {
    format::parse_iso_date(s)
        .map(format::date)
        .unwrap_or_default()
}

/// Everything the Typst template needs, formatted the Dutch way.
#[allow(clippy::too_many_arguments)]
pub fn pdf_data(
    inv: &Invoice,
    number: &str,
    issue_date: &str,
    due_date: &str,
    company: &CompanySnapshot,
    client: &ClientSnapshot,
    lines: &[Line],
    totals: &money::Totals,
    spec: Option<&[model::SpecRow]>,
    credit_for: Option<&str>,
    has_logo: Option<&str>,
    rounding_minutes: i64,
) -> serde_json::Value {
    let address = |street: &str, pc: &str, city: &str| -> Vec<String> {
        let mut v = Vec::new();
        if !street.is_empty() {
            v.push(street.to_string());
        }
        let town = format!("{pc} {city}").trim().to_string();
        if !town.is_empty() {
            v.push(town);
        }
        v
    };
    let visible: Vec<&Line> = lines
        .iter()
        .filter(|l| !(inv.hide_zero_lines && l.is_zero))
        .collect();
    let payment_text = if inv.is_credit() {
        format!(
            "Het bedrag van {} wordt met u verrekend of aan u terugbetaald.",
            format::amount(totals.total_cents.abs())
        )
    } else {
        format!(
            "Gelieve het totaalbedrag van {} uiterlijk {} over te maken op IBAN {} ten name van {}, onder vermelding van factuurnummer {}.",
            format::amount(totals.total_cents),
            d(due_date),
            company.iban,
            company.name,
            number
        )
    };
    let mut meta = vec![
        vec!["Factuurnummer".to_string(), number.to_string()],
        vec!["Factuurdatum".to_string(), d(issue_date)],
        vec!["Vervaldatum".to_string(), d(due_date)],
        vec!["Periode".to_string(), inv.period_label()],
    ];
    if let Some(c) = credit_for {
        meta.push(vec!["Creditnota voor".to_string(), c.to_string()]);
    }
    serde_json::json!({
        "company": {
            "name": company.name,
            "address": address(&company.street, &company.postal_code, &company.city),
            "kvk": company.kvk, "btw_id": company.btw_id, "iban": company.iban,
            "email": company.email, "phone": company.phone,
        },
        "client": {
            "name": client.name, "contact": client.contact_name,
            "address": address(&client.street, &client.postal_code, &client.city),
            "btw_id": client.btw_id,
        },
        "invoice": {
            "title": format!("{} {number}", if inv.is_credit() { "Creditfactuur" } else { "Factuur" }),
            "kind_label": if inv.is_credit() { "Creditfactuur" } else { "Factuur" },
            "number": number,
            "issue_date": d(issue_date),
            "due_date": d(due_date),
            "period": inv.period_label(),
            "intro": inv.intro_text,
            "footer": inv.footer_text,
            "credit_for": credit_for,
        },
        "lines": visible.iter().map(|l| serde_json::json!({
            "description": l.description,
            "quantity": format!("{} {}", l.quantity_label(), l.unit),
            "price": format::amount(l.unit_price_cents),
            "vat": format::vat_pct(l.vat_bp),
            "total": format::amount(l.line_total_cents),
        })).collect::<Vec<_>>(),
        "vat_groups": totals.per_rate.iter().map(|(bp, g)| serde_json::json!({
            "rate": format::vat_pct(*bp),
            "base": format::amount(g.base_cents),
            "vat": format::amount(g.vat_cents),
        })).collect::<Vec<_>>(),
        "meta": meta,
        "closing": payment_text,
        "subtotal": format::amount(totals.subtotal_cents),
        "total": format::amount(totals.total_cents),
        "payment_text": payment_text,
        "logo": has_logo,
        "spec": spec.map(|rows| rows.iter().map(|r| serde_json::json!({
            "date": d(&r.date), "task": r.task, "description": r.description,
            "duration": format::duration(r.minutes),
            "billed": format::duration(money::round_up_minutes(r.minutes, rounding_minutes)),
        })).collect::<Vec<_>>()),
        "spec_total": spec.map(|rows| format::duration(rows.iter().map(|r| r.minutes).sum())),
        "spec_billed_total": spec.map(|rows| format::duration(rows.iter().map(|r| money::round_up_minutes(r.minutes, rounding_minutes)).sum())),
    })
}

/// The number `next_number` would give now, without reserving it.
async fn peek_number(
    conn: &mut SqliteConnection,
    company_id: i64,
    prefix: &str,
    year: i32,
) -> AppResult<String> {
    let y = i64::from(year);
    let last: Option<i64> = sqlx::query_scalar!(
        "SELECT last_number FROM invoice_sequences WHERE company_id = ? AND year = ?",
        company_id,
        y
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(invoice::number(prefix, year, last.unwrap_or(0) + 1))
}

/// Next number for the company and year (inside the transaction).
async fn next_number(
    tx: &mut SqliteConnection,
    company_id: i64,
    prefix: &str,
    year: i32,
) -> AppResult<String> {
    let y = i64::from(year);
    let now = db::now();
    let last: Option<i64> = sqlx::query_scalar!(
        "SELECT last_number FROM invoice_sequences WHERE company_id = ? AND year = ?",
        company_id,
        y
    )
    .fetch_optional(&mut *tx)
    .await?;
    let n = last.unwrap_or(0) + 1;
    match last {
        Some(_) => {
            sqlx::query!(
                "UPDATE invoice_sequences SET last_number = ?, updated_at = ? WHERE company_id = ? AND year = ?",
                n, now, company_id, y
            )
            .execute(&mut *tx)
            .await?;
        }
        None => {
            sqlx::query!(
                "INSERT INTO invoice_sequences (company_id, year, last_number, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?)",
                company_id, y, n, now, now
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(invoice::number(prefix, year, n))
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub struct IssueRequest<'a> {
    pub company_id: i64,
    pub invoice_id: i64,
    pub issue_date: time::Date,
    pub data_dir: &'a std::path::Path,
    pub by: i64,
}

/// Everything issuing needs, read in one go (no writes).
struct Prepared {
    inv: model::Invoice,
    company_snap: CompanySnapshot,
    client_snap: ClientSnapshot,
    prefix: String,
    rounding_minutes: i64,
    lines: Vec<model::Line>,
    totals: money::Totals,
    spec: Option<Vec<model::SpecRow>>,
    credit_for: Option<String>,
    issue_s: String,
    due_s: String,
    assets: Vec<(String, Vec<u8>)>,
    logo_name: Option<String>,
}

async fn prepare(conn: &mut SqliteConnection, r: &IssueRequest<'_>) -> AppResult<Prepared> {
    let inv = model::get_draft(conn, r.company_id, r.invoice_id).await?;
    let company = companies::get(conn, r.company_id).await?;
    let client = clients::get(conn, r.company_id, inv.client_id).await?;
    let lines = model::lines(conn, inv.id).await?;
    let missing = missing_data(&company, &client, lines.len());
    if !missing.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Deze factuur kan nog niet definitief worden. Ontbreekt: {}.",
            missing.join(", ")
        )));
    }
    let issue_s = format::iso_date(r.issue_date);
    let last_date: Option<String> = sqlx::query_scalar!(
        r#"SELECT MAX(issue_date) AS "d: String" FROM invoices WHERE company_id = ? AND number IS NOT NULL"#,
        r.company_id
    )
    .fetch_one(&mut *conn)
    .await?;
    if let Some(last) = last_date.filter(|l| *l > issue_s) {
        return Err(AppError::BadRequest(format!(
            "De factuurdatum mag niet vóór die van de vorige factuur ({}) liggen; factuurnummers lopen op in de tijd.",
            d(&last)
        )));
    }
    let due_s = format::iso_date(r.issue_date + Duration::days(company.payment_term_days));
    let totals = model::totals_of(conn, inv.id).await?;
    let spec = if inv.include_hours_spec {
        Some(model::hours_spec(conn, r.company_id, inv.id).await?)
    } else {
        None
    };
    let credit_for = match inv.credit_for_invoice_id {
        Some(o) => model::get(conn, r.company_id, o).await?.number,
        None => None,
    };
    // Logo as an asset for Typst.
    let mut assets = Vec::new();
    let mut logo_name = None;
    if let Some(rel) = &company.logo_path {
        if let Ok(bytes) = std::fs::read(r.data_dir.join(rel)) {
            let ext = rel.rsplit('.').next().unwrap_or("png");
            let name = format!("logo.{ext}");
            assets.push((name.clone(), bytes));
            logo_name = Some(name);
        }
    }
    Ok(Prepared {
        company_snap: CompanySnapshot::of(&company),
        client_snap: ClientSnapshot::of(&client),
        // Numbers carry the client's prefix, in the company's one series.
        prefix: client.invoice_prefix.clone().unwrap_or_default(),
        rounding_minutes: company.rounding_minutes,
        inv,
        lines,
        totals,
        spec,
        credit_for,
        issue_s,
        due_s,
        assets,
        logo_name,
    })
}

fn data_for(p: &Prepared, number: &str) -> String {
    pdf_data(
        &p.inv,
        number,
        &p.issue_s,
        &p.due_s,
        &p.company_snap,
        &p.client_snap,
        &p.lines,
        &p.totals,
        p.spec.as_deref(),
        p.credit_for.as_deref(),
        p.logo_name.as_deref(),
        p.rounding_minutes,
    )
    .to_string()
}

/// How often issuing re-renders when something changed while Typst ran.
const ISSUE_ATTEMPTS: usize = 3;

/// Issue a draft. Returns the number.
///
/// The PDF is rendered *outside* the write transaction (Typst can take a
/// moment and SQLite has one writer): first everything is read and the PDF
/// rendered for the number that would come next; then, in one transaction,
/// the number is reserved and the PDF data rebuilt. Only when that data is
/// identical to what was rendered is the PDF stored and the invoice issued;
/// otherwise (another invoice was issued, the draft was edited) it starts
/// over. So the stored PDF always matches the committed invoice exactly.
pub async fn issue(
    pool: &SqlitePool,
    renderer: std::sync::Arc<dyn PdfRenderer>,
    r: &IssueRequest<'_>,
) -> AppResult<String> {
    for _ in 0..ISSUE_ATTEMPTS {
        let (rendered_data, pdf) = {
            let mut conn = pool.acquire().await?;
            let p = prepare(&mut conn, r).await?;
            let candidate =
                peek_number(&mut conn, r.company_id, &p.prefix, r.issue_date.year()).await?;
            drop(conn);
            let data = data_for(&p, &candidate);
            let pdf = crate::pdf::render_blocking(
                renderer.clone(),
                INVOICE_TEMPLATE,
                data.clone(),
                p.assets.clone(),
            )
            .await
            .map_err(|e| AppError::BadRequest(format!("De PDF kon niet worden gemaakt: {e}")))?;
            (data, pdf)
        };
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let p = prepare(&mut tx, r).await?;
        let number = next_number(&mut tx, r.company_id, &p.prefix, r.issue_date.year()).await?;
        if data_for(&p, &number) != rendered_data {
            tx.rollback().await?;
            continue;
        }
        let Prepared {
            inv,
            totals,
            issue_s,
            due_s,
            company_snap,
            client_snap,
            ..
        } = p;
        // Store the PDF as a document.
        let now = db::now();
        let rel = format!(
            "documents/{}/{}/sales_invoice/{}_{}.pdf",
            r.company_id,
            r.issue_date.year(),
            inv.id,
            number
        );
        let abs = r.data_dir.join(&rel);
        let dir = abs.parent().expect("has parent");
        std::fs::create_dir_all(dir).map_err(|e| AppError::Internal(e.into()))?;
        let tmp = dir.join(format!(".upload-{}.tmp", inv.id));
        std::fs::write(&tmp, &pdf).map_err(|e| AppError::Internal(e.into()))?;
        std::fs::rename(&tmp, &abs).map_err(|e| AppError::Internal(e.into()))?;
        let cleanup = |e: AppError| {
            let _ = std::fs::remove_file(&abs);
            e
        };
        let sha = sha256_hex(&pdf);
        let size = pdf.len() as i64;
        let original = format!("{number}.pdf");
        let doc_id = sqlx::query!(
            "INSERT INTO documents (company_id, kind, file_path, sha256, original_name, mime, size,
                                    doc_date, amount_cents, vat_cents, invoice_id, client_id,
                                    created_at, updated_at, created_by)
             VALUES (?, 'sales_invoice', ?, ?, ?, 'application/pdf', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            r.company_id,
            rel,
            sha,
            original,
            size,
            issue_s,
            totals.total_cents,
            totals.vat_cents,
            inv.id,
            inv.client_id,
            now,
            now,
            r.by
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| cleanup(e.into()))?
        .last_insert_rowid();
        audit::record_insert(
            &mut tx,
            Some(r.by),
            "documents",
            doc_id,
            &serde_json::json!({"kind": "sales_invoice", "file_path": rel, "sha256": sha, "invoice_id": inv.id}),
        )
        .await
        .map_err(|e| cleanup(AppError::Internal(e)))?;

        let company_json = serde_json::to_string(&company_snap).expect("snapshot serialises");
        let client_json = serde_json::to_string(&client_snap).expect("snapshot serialises");
        sqlx::query!(
            "UPDATE invoices SET number = ?, issue_date = ?, due_date = ?, subtotal_cents = ?,
                    vat_cents = ?, total_cents = ?, company_snapshot_json = ?, client_snapshot_json = ?,
                    issued_at = ?, pdf_document_id = ?, updated_at = ?
             WHERE id = ? AND company_id = ? AND number IS NULL",
            number,
            issue_s,
            due_s,
            totals.subtotal_cents,
            totals.vat_cents,
            totals.total_cents,
            company_json,
            client_json,
            now,
            doc_id,
            now,
            inv.id,
            r.company_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| cleanup(e.into()))?;
        let new = model::get(&mut tx, r.company_id, inv.id)
            .await
            .map_err(cleanup)?;
        audit::record_update(&mut tx, Some(r.by), "invoices", inv.id, &inv, &new)
            .await
            .map_err(|e| cleanup(AppError::Internal(e)))?;

        // A credit note may give the original's hours and km back.
        if inv.is_credit() && inv.release_entries {
            if let Some(orig) = inv.credit_for_invoice_id {
                let (hours, km) = model::linked_entries(&mut tx, r.company_id, orig).await?;
                for e in hours {
                    model::link_entry(&mut tx, "time_entries", r.company_id, e, None, Some(r.by))
                        .await
                        .map_err(cleanup)?;
                }
                for e in km {
                    model::link_entry(
                        &mut tx,
                        "mileage_entries",
                        r.company_id,
                        e,
                        None,
                        Some(r.by),
                    )
                    .await
                    .map_err(cleanup)?;
                }
            }
        }
        tx.commit().await.map_err(|e| cleanup(e.into()))?;
        return Ok(number);
    }
    Err(AppError::BadRequest(
        "Tijdens het maken van de PDF werd er ook iets anders gewijzigd. Probeer het opnieuw."
            .into(),
    ))
}

#[derive(Deserialize)]
pub struct IssueForm {
    #[serde(default)]
    issue_date: String,
}

pub async fn issue_submit(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<IssueForm>,
) -> AppResult<Response> {
    let date = format::parse_date(&f.issue_date).unwrap_or_else(format::today);
    let req = IssueRequest {
        company_id: company.id,
        invoice_id: id,
        issue_date: date,
        data_dir: &st.cfg.data_dir,
        by: user.id,
    };
    match issue(&st.pool, st.pdf.clone(), &req).await {
        Ok(number) => flash::ok(&session, &format!("Factuur {number} is definitief.")).await?,
        Err(AppError::BadRequest(msg)) => flash::error(&session, &msg).await?,
        Err(e) => return Err(e),
    }
    Ok(redirect(&format!("/facturen/{id}")))
}

/// The stored PDF of an issued invoice.
pub async fn pdf(
    State(st): State<AppState>,
    _user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let (inv, path, name) = {
        let mut conn = st.pool.acquire().await?;
        let inv = model::get(&mut conn, company.id, id).await?;
        let doc_id = inv.pdf_document_id.ok_or(AppError::NotFound)?;
        let row = sqlx::query!(
            "SELECT file_path, original_name FROM documents WHERE id = ? AND company_id = ?",
            doc_id,
            company.id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(AppError::NotFound)?;
        (inv, row.file_path, row.original_name)
    };
    let bytes = tokio::fs::read(st.cfg.data_dir.join(&path))
        .await
        .map_err(|_| AppError::NotFound)?;
    let _ = inv;
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{name}\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot JSON exactly as schema version 12 stores it. Never edit these
    /// strings: they stand for invoices already issued. When a field is
    /// added or renamed, this test must keep passing.
    const COMPANY_V12: &str = r#"{"name":"Alfa B.V.","street":"Kantoorweg 1","postal_code":"3511 AB","city":"Utrecht","kvk":"12345678","btw_id":"NL123456789B01","iban":"NL91 ABNA 0417 1643 00","email":"info@alfa.example","phone":"030 123 45 67","logo_path":null}"#;
    const CLIENT_V12: &str = r#"{"name":"Sportclub","contact_name":"Anouk","street":"Sportlaan 1","postal_code":"3584 AA","city":"Utrecht","kvk":"","btw_id":"","email":"penning@sportclub.example"}"#;

    #[test]
    fn stored_snapshots_stay_readable() {
        let c: CompanySnapshot = serde_json::from_str(COMPANY_V12).unwrap();
        assert_eq!(
            (c.name.as_str(), c.iban.as_str()),
            ("Alfa B.V.", "NL91 ABNA 0417 1643 00")
        );
        let k: ClientSnapshot = serde_json::from_str(CLIENT_V12).unwrap();
        assert_eq!(
            (k.name.as_str(), k.contact_name.as_str()),
            ("Sportclub", "Anouk")
        );
        // older JSON with fewer fields, and unknown extra fields, still parse
        let c: CompanySnapshot =
            serde_json::from_str(r#"{"name":"Oud B.V.","iban":"NL00"}"#).unwrap();
        assert_eq!(
            (c.name.as_str(), c.city.as_str(), c.logo_path),
            ("Oud B.V.", "", None)
        );
        let k: ClientSnapshot =
            serde_json::from_str(r#"{"name":"X","toekomstig_veld":1}"#).unwrap();
        assert_eq!(k.name, "X");
    }

    #[test]
    fn missing_data_is_listed_in_dutch() {
        let c = Company {
            id: 1,
            name: "A".into(),
            kvk: "".into(),
            btw_id: "".into(),
            iban: "".into(),
            street: "".into(),
            postal_code: "".into(),
            city: "".into(),
            email: "".into(),
            phone: "".into(),
            invoice_prefix: "A".into(),
            payment_term_days: 14,
            default_vat_bp: 2100,
            rounding_minutes: 0,
            logo_path: None,
            archived_at: None,
            created_at: "".into(),
            updated_at: "".into(),
            created_by: None,
        };
        let k = Client {
            id: 1,
            company_id: 1,
            name: "K".into(),
            contact_name: "".into(),
            email: "".into(),
            phone: "".into(),
            street: "".into(),
            postal_code: "".into(),
            city: "".into(),
            kvk: "".into(),
            btw_id: "".into(),
            notes: "".into(),
            invoice_prefix: None,
            archived_at: None,
            created_at: "".into(),
            updated_at: "".into(),
            created_by: None,
        };
        let m = missing_data(&c, &k, 0);
        assert_eq!(
            m,
            [
                "adres van het bedrijf",
                "KvK-nummer van het bedrijf",
                "btw-id van het bedrijf",
                "IBAN van het bedrijf",
                "adres van klant K",
                "factuurprefix van klant K (bij de klantgegevens)",
                "minstens één factuurregel"
            ]
        );
        let k = Client {
            invoice_prefix: Some("VH".into()),
            ..k
        };
        assert!(!missing_data(&c, &k, 1)
            .iter()
            .any(|m| m.contains("factuurprefix")));
    }
}
