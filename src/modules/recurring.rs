//! Recurring invoices (terugkerende facturen): templates with lines that
//! create a draft invoice per period. The drafts are reviewed and issued by
//! hand; generation runs at startup, hourly, and via `kwartje recurring`.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use time::Date;
use tower_sessions::Session;

use super::clients::{self, ClientOption};
use super::companies::{CurrentCompany, VAT_CHOICES};
use super::invoices::model;
use crate::invoice::{self, DraftLine};
use crate::recurring::{self as rec, Interval};
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format};

/// At most this many periods are caught up in one run (three years monthly).
pub const CATCH_UP_CAP: usize = 36;
pub const PERIOD_PLACEHOLDER: &str = "{periode}";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Recurring {
    pub id: i64,
    pub company_id: i64,
    pub client_id: i64,
    pub name: String,
    pub interval: String,
    pub next_date: String,
    pub end_date: Option<String>,
    pub intro_text: String,
    pub footer_text: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

impl Recurring {
    pub fn interval(&self) -> Interval {
        Interval::parse(&self.interval).unwrap_or(Interval::Month)
    }
    pub fn interval_label(&self) -> &'static str {
        self.interval().label()
    }
    pub fn next(&self) -> Option<Date> {
        format::parse_iso_date(&self.next_date)
    }
    /// True when no more drafts will be made.
    pub fn finished(&self) -> bool {
        match (&self.end_date, self.next()) {
            (Some(e), Some(_)) => self.next_date.as_str() > e.as_str(),
            _ => false,
        }
    }
    pub fn next_label(&self) -> String {
        self.next()
            .map(|d| rec::period_label(d, self.interval()))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RecurringLine {
    pub id: i64,
    pub recurring_id: i64,
    pub position: i64,
    pub description: String,
    pub quantity_milli: i64,
    pub unit: String,
    pub unit_price_cents: i64,
    pub vat_bp: i64,
}

impl RecurringLine {
    pub fn quantity_label(&self) -> String {
        invoice::quantity_label(self.quantity_milli)
    }
    fn unit_static(&self) -> &'static str {
        invoice::unit(&self.unit)
    }
    pub fn draft(&self) -> AppResult<DraftLine> {
        invoice::manual_line(
            &self.description,
            self.quantity_milli,
            self.unit_static(),
            self.unit_price_cents,
            self.vat_bp,
        )
        .map_err(|e| AppError::BadRequest(e.to_string()))
    }
    pub fn total_cents(&self) -> i64 {
        self.draft().map(|d| d.line_total_cents).unwrap_or(0)
    }
}

// ---------- queries ----------

pub async fn get_one(
    conn: &mut SqliteConnection,
    company_id: i64,
    id: i64,
) -> AppResult<Recurring> {
    sqlx::query_as!(
        Recurring,
        r#"SELECT id AS "id!", company_id, client_id, name, interval, next_date, end_date,
                  intro_text, footer_text, archived_at, created_at, updated_at, created_by
           FROM recurring_invoices WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn lines(conn: &mut SqliteConnection, id: i64) -> AppResult<Vec<RecurringLine>> {
    Ok(sqlx::query_as!(
        RecurringLine,
        r#"SELECT id AS "id!", recurring_id, position, description, quantity_milli, unit,
                  unit_price_cents, vat_bp
           FROM recurring_lines WHERE recurring_id = ? ORDER BY position, id"#,
        id
    )
    .fetch_all(conn)
    .await?)
}

/// Create the drafts for every period that is due on `today`. Returns the
/// number of drafts made. Safe to run any number of times.
pub async fn generate_due(pool: &SqlitePool, today: Date) -> AppResult<usize> {
    let t = format::iso_date(today);
    let due = sqlx::query!(
        r#"SELECT r.id AS "id!", r.company_id FROM recurring_invoices r
           JOIN clients c ON c.id = r.client_id AND c.company_id = r.company_id
           JOIN companies co ON co.id = r.company_id
           WHERE r.archived_at IS NULL AND c.archived_at IS NULL AND co.archived_at IS NULL
             AND r.next_date <= ? AND (r.end_date IS NULL OR r.next_date <= r.end_date)
           ORDER BY r.id"#,
        t
    )
    .fetch_all(pool)
    .await?;
    let mut made = 0;
    for r in due {
        made += generate_one(pool, r.company_id, r.id, today).await?;
    }
    Ok(made)
}

async fn generate_one(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    today: Date,
) -> AppResult<usize> {
    let mut tx = pool.begin().await?;
    let r = get_one(&mut tx, company_id, id).await?;
    let (Some(next), interval) = (r.next(), r.interval()) else {
        return Ok(0);
    };
    let end = r.end_date.as_deref().and_then(format::parse_iso_date);
    let periods = rec::due_periods(next, interval, today, end, CATCH_UP_CAP);
    let Some(last) = periods.last().copied() else {
        return Ok(0);
    };
    let ls = lines(&mut tx, id).await?;
    let drafts = ls
        .iter()
        .map(|l| l.draft())
        .collect::<AppResult<Vec<_>>>()?;
    let now = db::now();
    for start in &periods {
        let (from, to) = rec::period(*start, interval);
        let (from, to) = (format::iso_date(from), format::iso_date(to));
        let intro = r
            .intro_text
            .replace(PERIOD_PLACEHOLDER, &rec::period_label(*start, interval));
        let inv = sqlx::query!(
            "INSERT INTO invoices (company_id, client_id, period_from, period_to, intro_text, footer_text,
                                   recurring_id, created_at, updated_at, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
            company_id,
            r.client_id,
            from,
            to,
            intro,
            r.footer_text,
            id,
            now,
            now
        )
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        let row = model::get(&mut tx, company_id, inv).await?;
        audit::record_insert(&mut tx, None, "invoices", inv, &row).await?;
        model::insert_lines(&mut tx, company_id, inv, &drafts, None).await?;
        model::recalc(&mut tx, inv).await?;
    }
    let new_next = format::iso_date(rec::advance(last, interval));
    sqlx::query!(
        "UPDATE recurring_invoices SET next_date = ?, updated_at = ? WHERE id = ? AND company_id = ?",
        new_next,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        None,
        "recurring_invoices",
        id,
        audit::Action::Update,
        &serde_json::json!({"next_date": [r.next_date, new_next]}),
    )
    .await?;
    tx.commit().await?;
    Ok(periods.len())
}

/// Background task for `serve`: generate at startup and then every hour.
pub fn spawn_generator(pool: SqlitePool) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match generate_due(&pool, format::today()).await {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!("{n} conceptfactuur/-facturen gemaakt uit terugkerende facturen")
                }
                Err(e) => tracing::error!("terugkerende facturen: {e:?}"),
            }
        }
    });
}

/// Draft invoices made from templates and not yet issued (dashboard).
pub async fn waiting_drafts(pool: &SqlitePool, company_id: i64) -> AppResult<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM invoices
           WHERE company_id = ? AND recurring_id IS NOT NULL AND number IS NULL"#,
        company_id
    )
    .fetch_one(pool)
    .await?)
}

// ---------- pages ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/terugkerend", get(list_page).post(create))
        .route("/terugkerend/{id}", get(edit_page).post(save))
        .route("/terugkerend/{id}/regels", post(add_line))
        .route(
            "/terugkerend/{id}/regels/{line}/verwijderen",
            post(remove_line),
        )
        .route("/terugkerend/{id}/archiveren", post(archive))
}

pub struct Row {
    pub r: Recurring,
    pub client_name: String,
    pub drafts: i64,
}

#[derive(Template, WebTemplate)]
#[template(path = "recurring/list.html")]
struct ListPage {
    l: Layout,
    rows: Vec<Row>,
    clients: Vec<ClientOption>,
    intervals: Vec<(&'static str, &'static str)>,
    today: String,
}

fn intervals() -> Vec<(&'static str, &'static str)> {
    Interval::ALL
        .iter()
        .map(|i| (i.as_str(), i.label()))
        .collect()
}

async fn list_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
) -> AppResult<ListPage> {
    let templates = sqlx::query_as!(
        Recurring,
        r#"SELECT id AS "id!", company_id, client_id, name, interval, next_date, end_date,
                  intro_text, footer_text, archived_at, created_at, updated_at, created_by
           FROM recurring_invoices WHERE company_id = ?
           ORDER BY archived_at IS NOT NULL, next_date, name"#,
        company.id
    )
    .fetch_all(&st.pool)
    .await?;
    let drafts: std::collections::HashMap<i64, i64> = sqlx::query!(
        r#"SELECT recurring_id AS "id!: i64", COUNT(*) AS "n!: i64" FROM invoices
           WHERE company_id = ? AND recurring_id IS NOT NULL AND number IS NULL
           GROUP BY recurring_id"#,
        company.id
    )
    .fetch_all(&st.pool)
    .await?
    .into_iter()
    .map(|r| (r.id, r.n))
    .collect();
    let names = super::quotes::client_names(&st.pool, company.id).await?;
    let rows = templates
        .into_iter()
        .map(|r| Row {
            client_name: names.get(&r.client_id).cloned().unwrap_or_default(),
            drafts: drafts.get(&r.id).copied().unwrap_or(0),
            r,
        })
        .collect();
    Ok(ListPage {
        l,
        rows,
        clients: clients::options(&st.pool, company.id).await?,
        intervals: intervals(),
        today: format::iso_date(format::today()),
    })
}

#[derive(Deserialize)]
struct TemplateForm {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    interval: String,
    #[serde(default)]
    next_date: String,
    #[serde(default)]
    end_date: String,
    #[serde(default)]
    intro_text: String,
    #[serde(default)]
    footer_text: String,
}

struct Valid {
    name: String,
    interval: Interval,
    next_date: String,
    end_date: Option<String>,
}

fn validate(f: &TemplateForm) -> Result<Valid, &'static str> {
    let name = f.name.trim().to_string();
    if name.is_empty() {
        return Err("Geef de terugkerende factuur een naam.");
    }
    let interval = Interval::parse(&f.interval).ok_or("Kies een interval.")?;
    let next = format::parse_date(&f.next_date).ok_or("Vul een geldige startdatum in.")?;
    let end = match f.end_date.trim() {
        "" => None,
        s => Some(format::parse_date(s).ok_or("Vul een geldige einddatum in, of laat hem leeg.")?),
    };
    if end.is_some_and(|e| e < next) {
        return Err("De einddatum ligt voor de volgende periode.");
    }
    Ok(Valid {
        name,
        interval,
        next_date: format::iso_date(next),
        end_date: end.map(format::iso_date),
    })
}

async fn create(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<TemplateForm>,
) -> AppResult<Response> {
    let client_id: i64 = f
        .client_id
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("Kies een klant.".into()))?;
    let v = match validate(&f) {
        Ok(v) => v,
        Err(m) => {
            flash::error(&session, m).await?;
            return Ok(redirect("/terugkerend"));
        }
    };
    let mut tx = st.pool.begin().await?;
    clients::get_active(&mut tx, company.id, client_id).await?;
    let now = db::now();
    let intro = if f.intro_text.trim().is_empty() {
        format!("Hierbij ontvangt u de factuur voor {PERIOD_PLACEHOLDER}.")
    } else {
        f.intro_text.trim().to_string()
    };
    let interval = v.interval.as_str();
    let footer = f.footer_text.trim();
    let id = sqlx::query!(
        "INSERT INTO recurring_invoices (company_id, client_id, name, interval, next_date, end_date,
                                         intro_text, footer_text, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company.id,
        client_id,
        v.name,
        interval,
        v.next_date,
        v.end_date,
        intro,
        footer,
        now,
        now,
        user.id
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get_one(&mut tx, company.id, id).await?;
    audit::record_insert(&mut tx, Some(user.id), "recurring_invoices", id, &row).await?;
    tx.commit().await?;
    flash::ok(
        &session,
        "Terugkerende factuur aangemaakt. Voeg nu de regels toe.",
    )
    .await?;
    Ok(redirect(&format!("/terugkerend/{id}")))
}

pub struct Generated {
    pub id: i64,
    pub number: Option<String>,
    pub period_from: String,
    pub period_to: String,
    pub total_cents: i64,
}

#[derive(Template, WebTemplate)]
#[template(path = "recurring/edit.html")]
struct EditPage {
    l: Layout,
    r: Recurring,
    client_name: String,
    lines: Vec<RecurringLine>,
    total_excl_cents: i64,
    vat_choices: Vec<(String, String)>,
    intervals: Vec<(&'static str, &'static str)>,
    generated: Vec<Generated>,
}

async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<EditPage> {
    let mut conn = st.pool.acquire().await?;
    let r = get_one(&mut conn, company.id, id).await?;
    let client = clients::get(&mut conn, company.id, r.client_id).await?;
    let ls = lines(&mut conn, id).await?;
    let generated = sqlx::query_as!(
        Generated,
        r#"SELECT id AS "id!", number, period_from, period_to, total_cents FROM invoices
           WHERE recurring_id = ? AND company_id = ? ORDER BY period_from DESC, id DESC"#,
        id,
        company.id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(EditPage {
        l,
        total_excl_cents: ls.iter().map(|l| l.total_cents()).sum(),
        lines: ls,
        client_name: client.name,
        vat_choices: VAT_CHOICES
            .iter()
            .map(|bp| (bp.to_string(), format::vat_pct(*bp)))
            .collect(),
        intervals: intervals(),
        generated,
        r,
    })
}

async fn save(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<TemplateForm>,
) -> AppResult<Response> {
    let back = format!("/terugkerend/{id}");
    let v = match validate(&f) {
        Ok(v) => v,
        Err(m) => {
            flash::error(&session, m).await?;
            return Ok(redirect(&back));
        }
    };
    let mut tx = st.pool.begin().await?;
    let old = get_one(&mut tx, company.id, id).await?;
    let now = db::now();
    let interval = v.interval.as_str();
    let (intro, footer) = (f.intro_text.trim(), f.footer_text.trim());
    sqlx::query!(
        "UPDATE recurring_invoices SET name = ?, interval = ?, next_date = ?, end_date = ?, intro_text = ?,
                footer_text = ?, updated_at = ?
         WHERE id = ? AND company_id = ?",
        v.name,
        interval,
        v.next_date,
        v.end_date,
        intro,
        footer,
        now,
        id,
        company.id
    )
    .execute(&mut *tx)
    .await?;
    let new = get_one(&mut tx, company.id, id).await?;
    audit::record_update(&mut tx, Some(user.id), "recurring_invoices", id, &old, &new).await?;
    tx.commit().await?;
    flash::ok(&session, "Terugkerende factuur opgeslagen.").await?;
    Ok(redirect(&back))
}

async fn add_line(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<invoice::ManualLineForm>,
) -> AppResult<Response> {
    let back = format!("/terugkerend/{id}");
    let line = match f.parse(VAT_CHOICES) {
        Ok(l) => l,
        Err(m) => {
            flash::error(&session, &m).await?;
            return Ok(redirect(&back));
        }
    };
    let (desc, q, unit, p, v) = (
        line.description.as_str(),
        line.quantity_milli,
        line.unit,
        line.unit_price_cents,
        line.vat_bp,
    );
    let mut tx = st.pool.begin().await?;
    get_one(&mut tx, company.id, id).await?;
    let now = db::now();
    let pos: i64 = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(position), 0) + 1 AS "p!: i64" FROM recurring_lines WHERE recurring_id = ?"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let lid = sqlx::query!(
        "INSERT INTO recurring_lines (recurring_id, position, description, quantity_milli, unit,
                                      unit_price_cents, vat_bp, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        pos,
        desc,
        q,
        unit,
        p,
        v,
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
        "recurring_lines",
        lid,
        &serde_json::json!({"recurring_id": id, "description": desc, "quantity_milli": q, "unit": unit, "unit_price_cents": p, "vat_bp": v}),
    )
    .await?;
    tx.commit().await?;
    Ok(redirect(&back))
}

/// Template lines are configuration, not a business record: like draft
/// lines they may be removed (audited). Invoices already made keep theirs.
async fn remove_line(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    Path((id, line)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    get_one(&mut tx, company.id, id).await?;
    let old = sqlx::query_as!(
        RecurringLine,
        r#"SELECT id AS "id!", recurring_id, position, description, quantity_milli, unit,
                  unit_price_cents, vat_bp
           FROM recurring_lines WHERE id = ? AND recurring_id = ?"#,
        line,
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query!(
        "DELETE FROM recurring_lines WHERE id = ? AND recurring_id = ?",
        line,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        Some(user.id),
        "recurring_lines",
        line,
        audit::Action::Delete,
        &serde_json::to_value(&old).expect("row serialises"),
    )
    .await?;
    tx.commit().await?;
    Ok(redirect(&format!("/terugkerend/{id}")))
}

async fn archive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    let old = get_one(&mut tx, company.id, id).await?;
    let new_at = if old.archived_at.is_some() {
        None
    } else {
        Some(db::now())
    };
    let now = db::now();
    sqlx::query!(
        "UPDATE recurring_invoices SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
        new_at,
        now,
        id,
        company.id
    )
    .execute(&mut *tx)
    .await?;
    audit::record_archive(
        &mut tx,
        Some(user.id),
        "recurring_invoices",
        id,
        old.archived_at.as_deref(),
        new_at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    flash::ok(
        &session,
        if new_at.is_some() {
            "Terugkerende factuur gearchiveerd: er worden geen concepten meer gemaakt."
        } else {
            "Terugkerende factuur weer actief."
        },
    )
    .await?;
    Ok(redirect(&format!("/terugkerend/{id}")))
}
