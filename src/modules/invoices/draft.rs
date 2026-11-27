//! Drafts: create from Te factureren, edit, add/remove lines, rebuild with
//! another line style, delete (releases the hours and km).

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Form;
use serde::Deserialize;
use sqlx::{SqliteConnection, SqlitePool};
use tower_sessions::Session;

use super::model::{self, Invoice, Line};
use crate::invoice::{self, HourItem, KmItem, LineStyle};
use crate::modules::clients::{self, Client};
use crate::modules::companies::{CurrentCompany, VAT_CHOICES};
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format, money};

fn overflow(e: money::Overflow) -> AppError {
    AppError::BadRequest(format!("Bedrag te groot: {e}"))
}

fn pairs_get<'a>(pairs: &'a [(String, String)], key: &str) -> &'a str {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

fn pairs_ids(pairs: &[(String, String)], key: &str) -> Vec<i64> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .filter_map(|(_, v)| v.parse().ok())
        .collect()
}

fn pairs_checked(pairs: &[(String, String)], key: &str) -> bool {
    pairs
        .iter()
        .any(|(k, v)| k == key && !v.is_empty() && v != "0")
}

pub struct DraftOptions {
    pub style: LineStyle,
    pub hide_zero_lines: bool,
    pub include_hours_spec: bool,
}

/// What to put on a new draft.
pub struct DraftRequest<'a> {
    pub company_id: i64,
    pub rounding_minutes: i64,
    pub client_id: i64,
    pub hour_ids: &'a [i64],
    pub km_ids: &'a [i64],
    pub opts: DraftOptions,
}

/// Create a draft for a client from the chosen unbilled entries.
pub async fn create_draft(pool: &SqlitePool, r: &DraftRequest<'_>, by: i64) -> AppResult<i64> {
    let (company_id, client_id, hour_ids, km_ids, opts) =
        (r.company_id, r.client_id, r.hour_ids, r.km_ids, &r.opts);
    let rounding_minutes = r.rounding_minutes;
    let mut tx = pool.begin().await?;
    clients::get_active(&mut tx, company_id, client_id).await?;
    let hours: Vec<HourItem> = model::unbilled_hours(&mut tx, company_id, client_id, None, None)
        .await?
        .into_iter()
        .filter(|h| hour_ids.contains(&h.entry_id))
        .collect();
    let km: Vec<KmItem> = model::unbilled_km(&mut tx, company_id, client_id, None, None)
        .await?
        .into_iter()
        .filter(|k| km_ids.contains(&k.entry_id))
        .collect();
    if hours.len() != hour_ids.len() || km.len() != km_ids.len() {
        return Err(AppError::BadRequest(
            "Een deel van de uren of kilometers is intussen al gefactureerd of gewijzigd. Begin opnieuw vanuit Te factureren.".into(),
        ));
    }
    if hours.is_empty() && km.is_empty() {
        return Err(AppError::BadRequest(
            "Kies minstens één urenregel of rit.".into(),
        ));
    }
    let dates = hours
        .iter()
        .map(|h| &h.date)
        .chain(km.iter().map(|k| &k.date));
    let from = dates.clone().min().cloned().expect("not empty");
    let to = dates.max().cloned().expect("not empty");
    let lines =
        invoice::build_lines(&hours, &km, opts.style, rounding_minutes).map_err(overflow)?;

    let now = db::now();
    let style = opts.style.key();
    let id = sqlx::query!(
        "INSERT INTO invoices (company_id, client_id, period_from, period_to, line_style,
                               hide_zero_lines, include_hours_spec, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        client_id,
        from,
        to,
        style,
        opts.hide_zero_lines,
        opts.include_hours_spec,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = model::get(&mut tx, company_id, id).await?;
    audit::record_insert(&mut tx, Some(by), "invoices", id, &row).await?;
    model::insert_lines(&mut tx, company_id, id, &lines, Some(by)).await?;
    model::recalc(&mut tx, id).await?;
    tx.commit().await?;
    Ok(id)
}

async fn delete_line_rows(tx: &mut SqliteConnection, line: &Line, by: i64) -> AppResult<()> {
    let (hours, km) = model::entries_of_line(tx, line.id).await?;
    let company_id: i64 = sqlx::query_scalar!(
        "SELECT company_id FROM invoices WHERE id = ?",
        line.invoice_id
    )
    .fetch_one(&mut *tx)
    .await?;
    for e in hours {
        model::link_entry(tx, "time_entries", company_id, e, None, Some(by)).await?;
    }
    for e in km {
        model::link_entry(tx, "mileage_entries", company_id, e, None, Some(by)).await?;
    }
    sqlx::query!("DELETE FROM invoice_lines WHERE id = ?", line.id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        tx,
        Some(by),
        "invoice_lines",
        line.id,
        audit::Action::Delete,
        &serde_json::to_value(line).expect("row serialises"),
    )
    .await?;
    Ok(())
}

/// Rebuild the hours/km lines with another style; manual lines stay.
async fn rebuild(
    tx: &mut SqliteConnection,
    inv: &Invoice,
    style: LineStyle,
    rounding_minutes: i64,
    by: i64,
) -> AppResult<()> {
    let (hour_ids, km_ids) = model::linked_entries(tx, inv.company_id, inv.id).await?;
    for l in model::lines(tx, inv.id).await? {
        if l.source != "manual" {
            delete_line_rows(tx, &l, by).await?;
        }
    }
    let hours: Vec<HourItem> = model::unbilled_hours(tx, inv.company_id, inv.client_id, None, None)
        .await?
        .into_iter()
        .filter(|h| hour_ids.contains(&h.entry_id))
        .collect();
    let km: Vec<KmItem> = model::unbilled_km(tx, inv.company_id, inv.client_id, None, None)
        .await?
        .into_iter()
        .filter(|k| km_ids.contains(&k.entry_id))
        .collect();
    let lines = invoice::build_lines(&hours, &km, style, rounding_minutes).map_err(overflow)?;
    model::insert_lines(tx, inv.company_id, inv.id, &lines, Some(by)).await?;
    Ok(())
}

pub async fn delete_draft(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    by: i64,
) -> AppResult<Invoice> {
    let mut tx = pool.begin().await?;
    let inv = model::get_draft(&mut tx, company_id, id).await?;
    for l in model::lines(&mut tx, id).await? {
        delete_line_rows(&mut tx, &l, by).await?;
    }
    sqlx::query!(
        "DELETE FROM invoices WHERE id = ? AND company_id = ? AND number IS NULL",
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        Some(by),
        "invoices",
        id,
        audit::Action::Delete,
        &audit::snapshot(&serde_json::to_value(&inv).expect("row serialises")),
    )
    .await?;
    tx.commit().await?;
    Ok(inv)
}

// ---------- pages ----------

pub struct ItemRow {
    pub id: i64,
    pub date: String,
    pub label: String,
    pub quantity: String,
    pub price_cents: i64,
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/new.html")]
pub struct NewPage {
    pub l: Layout,
    pub client: Client,
    pub hours: Vec<ItemRow>,
    pub km: Vec<ItemRow>,
    pub styles: Vec<(&'static str, &'static str)>,
    pub rounding_minutes: i64,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct NewQuery {
    #[serde(default)]
    klant: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

fn opt_date(s: &str) -> Option<String> {
    format::parse_date(s).map(format::iso_date)
}

pub async fn new_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<NewQuery>,
) -> AppResult<Response> {
    let Ok(client_id) = q.klant.trim().parse::<i64>() else {
        return Ok(redirect("/te-factureren"));
    };
    let (from, to) = (opt_date(&q.from), opt_date(&q.to));
    let mut conn = st.pool.acquire().await?;
    let client = clients::get(&mut conn, company.id, client_id).await?;
    let hours = model::unbilled_hours(
        &mut conn,
        company.id,
        client_id,
        from.as_deref(),
        to.as_deref(),
    )
    .await?;
    let km = model::unbilled_km(
        &mut conn,
        company.id,
        client_id,
        from.as_deref(),
        to.as_deref(),
    )
    .await?;
    drop(conn);
    Ok(NewPage {
        l,
        client,
        hours: hours
            .into_iter()
            .map(|h| ItemRow {
                id: h.entry_id,
                date: h.date,
                label: if h.description.is_empty() {
                    h.task_name
                } else {
                    format!("{}: {}", h.task_name, h.description)
                },
                quantity: format::duration(h.minutes),
                price_cents: h.price_cents,
            })
            .collect(),
        km: km
            .into_iter()
            .map(|k| ItemRow {
                id: k.entry_id,
                date: k.date,
                label: k.tariff_name,
                quantity: format!("{} km", k.km),
                price_cents: k.price_cents,
            })
            .collect(),
        styles: LineStyle::ALL.iter().map(|(_, k, l)| (*k, *l)).collect(),
        rounding_minutes: company.rounding_minutes,
        error: None,
    }
    .into_response())
}

pub async fn create(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let client_id: i64 = pairs_get(&pairs, "client_id")
        .parse()
        .map_err(|_| AppError::BadRequest("Kies een klant.".into()))?;
    let opts = DraftOptions {
        style: LineStyle::parse(pairs_get(&pairs, "line_style")).unwrap_or(LineStyle::Task),
        hide_zero_lines: pairs_checked(&pairs, "hide_zero_lines"),
        include_hours_spec: pairs_checked(&pairs, "include_hours_spec"),
    };
    let (hour_ids, km_ids) = (pairs_ids(&pairs, "uren"), pairs_ids(&pairs, "km"));
    let req = DraftRequest {
        company_id: company.id,
        rounding_minutes: company.rounding_minutes,
        client_id,
        hour_ids: &hour_ids,
        km_ids: &km_ids,
        opts,
    };
    let result = create_draft(&st.pool, &req, user.id).await;
    match result {
        Ok(id) => {
            flash::ok(
                &session,
                "Conceptfactuur gemaakt. Controleer de regels en maak hem definitief.",
            )
            .await?;
            Ok(redirect(&format!("/facturen/{id}")))
        }
        Err(AppError::BadRequest(msg)) => {
            flash::error(&session, &msg).await?;
            Ok(redirect(&format!("/facturen/nieuw?klant={client_id}")))
        }
        Err(e) => Err(e),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "invoices/draft.html")]
pub struct DraftPage {
    pub l: Layout,
    pub inv: Invoice,
    pub client: Client,
    pub lines: Vec<Line>,
    pub totals: money::Totals,
    pub styles: Vec<(&'static str, &'static str)>,
    pub vat_choices: Vec<(String, String)>,
    pub today: String,
    pub credit_of: Option<String>,
}

pub async fn draft_page(
    st: &AppState,
    l: Layout,
    company_id: i64,
    inv: Invoice,
) -> AppResult<DraftPage> {
    let mut conn = st.pool.acquire().await?;
    let client = clients::get(&mut conn, company_id, inv.client_id).await?;
    let lines = model::lines(&mut conn, inv.id).await?;
    let totals = model::totals_of(&mut conn, inv.id).await?;
    let credit_of = match inv.credit_for_invoice_id {
        Some(o) => model::get(&mut conn, company_id, o).await?.number,
        None => None,
    };
    Ok(DraftPage {
        l,
        client,
        lines,
        totals,
        styles: LineStyle::ALL.iter().map(|(_, k, l)| (*k, *l)).collect(),
        vat_choices: VAT_CHOICES
            .iter()
            .map(|bp| (bp.to_string(), format::vat_pct(*bp)))
            .collect(),
        today: format::iso_date(format::today()),
        credit_of,
        inv,
    })
}

/// Save descriptions, texts and options; rebuild when the line style changed.
pub async fn save(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    let old = model::get_draft(&mut tx, company.id, id).await?;
    let intro = pairs_get(&pairs, "intro_text").trim().to_string();
    let footer = pairs_get(&pairs, "footer_text").trim().to_string();
    let hide = pairs_checked(&pairs, "hide_zero_lines");
    let spec = pairs_checked(&pairs, "include_hours_spec");
    let style = LineStyle::parse(pairs_get(&pairs, "line_style")).unwrap_or(LineStyle::Task);
    let now = db::now();
    for l in model::lines(&mut tx, id).await? {
        let key = format!("desc_{}", l.id);
        let desc = pairs_get(&pairs, &key).trim();
        if !desc.is_empty() && desc != l.description {
            sqlx::query!(
                "UPDATE invoice_lines SET description = ?, updated_at = ? WHERE id = ?",
                desc,
                now,
                l.id
            )
            .execute(&mut *tx)
            .await?;
            audit::record(
                &mut tx,
                Some(user.id),
                "invoice_lines",
                l.id,
                audit::Action::Update,
                &serde_json::json!({ "description": [l.description, desc] }),
            )
            .await?;
        }
    }
    let style_key = style.key();
    sqlx::query!(
        "UPDATE invoices SET intro_text = ?, footer_text = ?, hide_zero_lines = ?,
                include_hours_spec = ?, line_style = ?, updated_at = ?
         WHERE id = ? AND company_id = ? AND number IS NULL",
        intro,
        footer,
        hide,
        spec,
        style_key,
        now,
        id,
        company.id
    )
    .execute(&mut *tx)
    .await?;
    if style.key() != old.line_style && !old.is_credit() {
        rebuild(&mut tx, &old, style, company.rounding_minutes, user.id).await?;
    }
    model::recalc(&mut tx, id).await?;
    let new = model::get(&mut tx, company.id, id).await?;
    audit::record_update(&mut tx, Some(user.id), "invoices", id, &old, &new).await?;
    tx.commit().await?;
    flash::ok(&session, "Concept opgeslagen.").await?;
    Ok(redirect(&format!("/facturen/{id}")))
}

pub async fn add_line(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<invoice::ManualLineForm>,
) -> AppResult<Response> {
    let back = format!("/facturen/{id}");
    let line = match f.parse(VAT_CHOICES) {
        Ok(l) => l,
        Err(m) => {
            flash::error(&session, &m).await?;
            return Ok(redirect(&back));
        }
    };
    let mut tx = st.pool.begin().await?;
    model::get_draft(&mut tx, company.id, id).await?;
    model::insert_lines(&mut tx, company.id, id, &[line], Some(user.id)).await?;
    model::recalc(&mut tx, id).await?;
    tx.commit().await?;
    flash::ok(&session, "Regel toegevoegd.").await?;
    Ok(redirect(&back))
}

pub async fn remove_line(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path((id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    model::get_draft(&mut tx, company.id, id).await?;
    let line = model::lines(&mut tx, id)
        .await?
        .into_iter()
        .find(|l| l.id == line_id)
        .ok_or(AppError::NotFound)?;
    delete_line_rows(&mut tx, &line, user.id).await?;
    model::recalc(&mut tx, id).await?;
    tx.commit().await?;
    flash::ok(
        &session,
        if line.source == "manual" {
            "Regel verwijderd."
        } else {
            "Regel verwijderd; de uren of kilometers staan weer onder Te factureren."
        },
    )
    .await?;
    Ok(redirect(&format!("/facturen/{id}")))
}

pub async fn remove_draft(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    delete_draft(&st.pool, company.id, id, user.id).await?;
    flash::ok(
        &session,
        "Concept verwijderd. De uren en kilometers staan weer onder Te factureren.",
    )
    .await?;
    Ok(redirect("/te-factureren"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_helpers() {
        let p = vec![
            ("uren".to_string(), "3".to_string()),
            ("uren".to_string(), "5".to_string()),
            ("hide_zero_lines".to_string(), "1".to_string()),
            ("x".to_string(), "y".to_string()),
        ];
        assert_eq!(pairs_ids(&p, "uren"), vec![3, 5]);
        assert!(pairs_checked(&p, "hide_zero_lines"));
        assert!(!pairs_checked(&p, "include_hours_spec"));
        assert_eq!(pairs_get(&p, "x"), "y");
    }
}
