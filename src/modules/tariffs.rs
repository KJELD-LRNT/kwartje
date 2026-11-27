//! Tariffs: list per level, create/edit/archive, "Nieuwe prijs vanaf…".

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{self, post};
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use time::Duration;
use tower_sessions::Session;

use super::clients::{self, ClientOption};
use super::companies::{CurrentCompany, VAT_CHOICES};
use super::projects::{self, ProjectOption};
use crate::tariff::periods_overlap;
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::form::{self, checked, opt_id, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format};

pub const UNITS: &[(&str, &str)] = &[
    ("hour", "per uur"),
    ("km", "per km"),
    ("piece", "per stuk"),
    ("fixed", "vast bedrag"),
];

pub fn unit_label(unit: &str) -> &'static str {
    UNITS
        .iter()
        .find(|(u, _)| *u == unit)
        .map(|(_, l)| *l)
        .unwrap_or("")
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Tariff {
    pub id: i64,
    pub company_id: i64,
    pub lineage_id: i64,
    pub name: String,
    pub unit: String,
    pub price_cents: i64,
    pub vat_bp: i64,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub is_default: bool,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

/// A tariff with its client/project names, for lists.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TariffRow {
    pub id: i64,
    pub lineage_id: i64,
    pub name: String,
    pub unit: String,
    pub price_cents: i64,
    pub vat_bp: i64,
    pub client_id: Option<i64>,
    pub client_name: Option<String>,
    pub project_name: Option<String>,
    pub is_default: bool,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub archived_at: Option<String>,
    /// A later version exists (this one was superseded by a price change).
    pub superseded: bool,
}

impl TariffRow {
    pub fn unit_label(&self) -> &'static str {
        unit_label(&self.unit)
    }

    pub fn level_label(&self) -> String {
        match (&self.client_name, &self.project_name) {
            (Some(c), Some(p)) => format!("{c} — {p}"),
            (Some(c), None) => c.clone(),
            _ => "Hele bedrijf".into(),
        }
    }

    pub fn period(&self) -> String {
        let from = format::parse_iso_date(&self.valid_from)
            .map(format::date)
            .unwrap_or_default();
        match self.valid_to.as_deref().and_then(format::parse_iso_date) {
            Some(to) => format!("{from} t/m {}", format::date(to)),
            None => format!("vanaf {from}"),
        }
    }

    /// `actief`, `verlopen`, `toekomstig`, `gearchiveerd`.
    pub fn status(&self) -> &'static str {
        let today = format::iso_date(format::today());
        if self.archived_at.is_some() {
            "gearchiveerd"
        } else if self.valid_from > today {
            "toekomstig"
        } else if self.valid_to.as_deref().is_some_and(|t| t < today.as_str()) {
            "verlopen"
        } else {
            "actief"
        }
    }
}

// ---------- queries ----------

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Tariff> {
    sqlx::query_as!(
        Tariff,
        r#"SELECT id AS "id!", company_id, lineage_id AS "lineage_id!", name, unit, price_cents,
                  vat_bp, client_id, project_id, is_default AS "is_default: bool", valid_from,
                  valid_to, archived_at, created_at, updated_at, created_by
           FROM tariffs WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

/// All tariffs of the company (optionally only one client's), newest first.
pub async fn list_rows(
    pool: &SqlitePool,
    company_id: i64,
    client_id: Option<i64>,
    include_archived: bool,
) -> AppResult<Vec<TariffRow>> {
    Ok(sqlx::query_as!(
        TariffRow,
        r#"SELECT t.id AS "id!", t.lineage_id AS "lineage_id!", t.name, t.unit, t.price_cents,
                  t.vat_bp, t.client_id, c.name AS "client_name?", p.name AS "project_name?",
                  t.is_default AS "is_default: bool", t.valid_from, t.valid_to, t.archived_at,
                  EXISTS (SELECT 1 FROM tariffs n
                          WHERE n.lineage_id = t.lineage_id AND n.valid_from > t.valid_from)
                    AS "superseded!: bool"
           FROM tariffs t
           LEFT JOIN clients c ON c.id = t.client_id
           LEFT JOIN projects p ON p.id = t.project_id
           WHERE t.company_id = ?
             AND (? IS NULL OR t.client_id = ?)
             AND (? OR t.archived_at IS NULL)
           ORDER BY t.client_id IS NOT NULL, c.name COLLATE NOCASE, t.project_id IS NOT NULL,
                    p.name COLLATE NOCASE, t.unit, t.name COLLATE NOCASE, t.valid_from DESC"#,
        company_id,
        client_id,
        client_id,
        include_archived
    )
    .fetch_all(pool)
    .await?)
}

pub async fn list_for_client(
    pool: &SqlitePool,
    company_id: i64,
    client_id: i64,
) -> AppResult<Vec<TariffRow>> {
    list_rows(pool, company_id, Some(client_id), false).await
}

/// Hour tariffs for `<select>`s (current and future versions only).
#[derive(Debug, Clone)]
pub struct TariffOption {
    pub id: i64,
    pub label: String,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
}

pub async fn hour_options(pool: &SqlitePool, company_id: i64) -> AppResult<Vec<TariffOption>> {
    let today = format::iso_date(format::today());
    let rows = sqlx::query!(
        r#"SELECT t.id AS "id!", t.name, t.price_cents, t.client_id, t.project_id,
                  c.name AS "client_name?", p.name AS "project_name?"
           FROM tariffs t
           LEFT JOIN clients c ON c.id = t.client_id
           LEFT JOIN projects p ON p.id = t.project_id
           WHERE t.company_id = ? AND t.unit = 'hour' AND t.archived_at IS NULL
             AND (t.valid_to IS NULL OR t.valid_to >= ?)
             AND NOT EXISTS (SELECT 1 FROM tariffs n WHERE n.lineage_id = t.lineage_id
                             AND n.valid_from > t.valid_from AND n.valid_from <= ?)
           ORDER BY t.client_id IS NOT NULL, c.name, t.name COLLATE NOCASE"#,
        company_id,
        today,
        today
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let level = match (r.client_name, r.project_name) {
                (Some(c), Some(p)) => format!(" ({c} — {p})"),
                (Some(c), None) => format!(" ({c})"),
                _ => String::new(),
            };
            TariffOption {
                id: r.id,
                label: format!("{} — {}{level}", r.name, format::amount(r.price_cents)),
                client_id: r.client_id,
                project_id: r.project_id,
            }
        })
        .collect())
}

#[derive(Debug, Clone, Default)]
pub struct TariffData {
    pub name: String,
    pub unit: String,
    pub price_cents: i64,
    pub vat_bp: i64,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub is_default: bool,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

/// Another non-archived default tariff at the same level and unit whose
/// period overlaps. `exclude_lineage` skips the tariff's own versions.
async fn overlapping_default(
    conn: &mut SqliteConnection,
    company_id: i64,
    d: &TariffData,
    exclude_lineage: Option<i64>,
) -> AppResult<Option<String>> {
    let rows = sqlx::query!(
        r#"SELECT name, lineage_id AS "lineage_id!", valid_from, valid_to FROM tariffs
           WHERE company_id = ? AND is_default = 1 AND archived_at IS NULL AND unit = ?
             AND client_id IS ? AND project_id IS ?"#,
        company_id,
        d.unit,
        d.client_id,
        d.project_id
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| Some(r.lineage_id) != exclude_lineage)
        .find(|r| {
            periods_overlap(
                &d.valid_from,
                d.valid_to.as_deref(),
                &r.valid_from,
                r.valid_to.as_deref(),
            )
        })
        .map(|r| r.name))
}

/// Check level consistency and default overlap inside the transaction.
async fn check_refs(
    conn: &mut SqliteConnection,
    company_id: i64,
    d: &mut TariffData,
    exclude_lineage: Option<i64>,
    errors: &mut Errors,
) -> AppResult<()> {
    if let Some(pid) = d.project_id {
        let p = projects::get(conn, company_id, pid).await?;
        if d.client_id.is_some_and(|c| c != p.client_id) {
            errors.insert(
                "project_id",
                "Dit project hoort niet bij de gekozen klant.".into(),
            );
        }
        d.client_id = Some(p.client_id);
    }
    if let Some(cid) = d.client_id {
        clients::get(conn, company_id, cid).await?;
    }
    if d.is_default {
        if let Some(other) = overlapping_default(conn, company_id, d, exclude_lineage).await? {
            errors.insert(
                "is_default",
                format!(
                    "Er is al een standaardtarief ({other}) op dit niveau in deze periode. Zet daar eerst een einddatum of haal het vinkje weg."
                ),
            );
        }
    }
    Ok(())
}

pub enum SaveError {
    Invalid(Errors),
    App(AppError),
}

impl From<AppError> for SaveError {
    fn from(e: AppError) -> Self {
        SaveError::App(e)
    }
}

impl From<anyhow::Error> for SaveError {
    fn from(e: anyhow::Error) -> Self {
        SaveError::App(e.into())
    }
}

impl From<sqlx::Error> for SaveError {
    fn from(e: sqlx::Error) -> Self {
        SaveError::App(e.into())
    }
}

pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    mut d: TariffData,
    by: Option<i64>,
) -> Result<i64, SaveError> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let mut errors = Errors::new();
    check_refs(&mut tx, company_id, &mut d, None, &mut errors).await?;
    if !errors.is_empty() {
        return Err(SaveError::Invalid(errors));
    }
    let id = sqlx::query!(
        "INSERT INTO tariffs (company_id, name, unit, price_cents, vat_bp, client_id, project_id,
                              is_default, valid_from, valid_to, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        d.name,
        d.unit,
        d.price_cents,
        d.vat_bp,
        d.client_id,
        d.project_id,
        d.is_default,
        d.valid_from,
        d.valid_to,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut tx, company_id, id).await?;
    audit::record_insert(&mut tx, by, "tariffs", id, &row).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    mut d: TariffData,
    by: i64,
) -> Result<(), SaveError> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    let mut errors = Errors::new();
    // Other versions of the same lineage never conflict with each other.
    check_refs(
        &mut tx,
        company_id,
        &mut d,
        Some(old.lineage_id),
        &mut errors,
    )
    .await?;
    if !errors.is_empty() {
        return Err(SaveError::Invalid(errors));
    }
    sqlx::query!(
        "UPDATE tariffs SET name = ?, unit = ?, price_cents = ?, vat_bp = ?, client_id = ?,
                project_id = ?, is_default = ?, valid_from = ?, valid_to = ?, updated_at = ?
         WHERE id = ? AND company_id = ?",
        d.name,
        d.unit,
        d.price_cents,
        d.vat_bp,
        d.client_id,
        d.project_id,
        d.is_default,
        d.valid_from,
        d.valid_to,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "tariffs", id, &old, &new).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_archived(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    archived: bool,
    by: i64,
) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.archived_at.is_some() == archived {
        return Ok(());
    }
    if !archived && old.is_default {
        let d = data_of(&old);
        if let Some(other) =
            overlapping_default(&mut tx, company_id, &d, Some(old.lineage_id)).await?
        {
            return Err(AppError::BadRequest(format!(
                "Terugzetten kan niet: standaardtarief {other} geldt al in deze periode."
            )));
        }
    }
    let archived_at = archived.then(|| now.clone());
    sqlx::query!(
        "UPDATE tariffs SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
        archived_at,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record_archive(
        &mut tx,
        Some(by),
        "tariffs",
        id,
        old.archived_at.as_deref(),
        archived_at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn data_of(t: &Tariff) -> TariffData {
    TariffData {
        name: t.name.clone(),
        unit: t.unit.clone(),
        price_cents: t.price_cents,
        vat_bp: t.vat_bp,
        client_id: t.client_id,
        project_id: t.project_id,
        is_default: t.is_default,
        valid_from: t.valid_from.clone(),
        valid_to: t.valid_to.clone(),
    }
}

/// End the current version the day before `from` and start a new version
/// with `price_cents` from `from`. Returns the new tariff id.
pub async fn new_price(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    price_cents: i64,
    from: time::Date,
    by: i64,
) -> AppResult<i64> {
    let now = db::now();
    let from_s = format::iso_date(from);
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.archived_at.is_some() {
        return Err(AppError::BadRequest("Dit tarief is gearchiveerd.".into()));
    }
    let newer: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM tariffs WHERE lineage_id = ? AND valid_from > ?",
        old.lineage_id,
        old.valid_from
    )
    .fetch_one(&mut *tx)
    .await?;
    if newer > 0 {
        return Err(AppError::BadRequest(
            "Er is al een nieuwere versie van dit tarief; pas die aan.".into(),
        ));
    }
    if from_s <= old.valid_from {
        return Err(AppError::BadRequest(format!(
            "De nieuwe prijs moet ingaan na {} (de begindatum van de huidige prijs).",
            format::parse_iso_date(&old.valid_from)
                .map(format::date)
                .unwrap_or_default()
        )));
    }
    if old.valid_to.as_deref().is_some_and(|t| from_s.as_str() > t) {
        return Err(AppError::BadRequest(
            "De ingangsdatum ligt na de einddatum van dit tarief.".into(),
        ));
    }
    let end = format::iso_date(from - Duration::days(1));
    sqlx::query!(
        "UPDATE tariffs SET valid_to = ?, updated_at = ? WHERE id = ?",
        end,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    let ended = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "tariffs", id, &old, &ended).await?;
    let new_id = sqlx::query!(
        "INSERT INTO tariffs (company_id, lineage_id, name, unit, price_cents, vat_bp, client_id,
                              project_id, is_default, valid_from, valid_to, created_at, updated_at,
                              created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        old.lineage_id,
        old.name,
        old.unit,
        price_cents,
        old.vat_bp,
        old.client_id,
        old.project_id,
        old.is_default,
        from_s,
        old.valid_to,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut tx, company_id, new_id).await?;
    audit::record_insert(&mut tx, Some(by), "tariffs", new_id, &row).await?;
    tx.commit().await?;
    Ok(new_id)
}

// ---------- form ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TariffForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub vat_bp: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub is_default: Option<String>,
    #[serde(default)]
    pub valid_from: String,
    #[serde(default)]
    pub valid_to: String,
}

impl TariffForm {
    fn from_tariff(t: &Tariff) -> Self {
        TariffForm {
            name: t.name.clone(),
            unit: t.unit.clone(),
            price: format::decimal(t.price_cents),
            vat_bp: t.vat_bp.to_string(),
            client_id: t.client_id.map(|v| v.to_string()).unwrap_or_default(),
            project_id: t.project_id.map(|v| v.to_string()).unwrap_or_default(),
            is_default: t.is_default.then(|| "1".into()),
            valid_from: t.valid_from.clone(),
            valid_to: t.valid_to.clone().unwrap_or_default(),
        }
    }

    fn default_checked(&self) -> bool {
        checked(&self.is_default)
    }
}

pub fn validate_form(f: &TariffForm) -> Result<TariffData, Errors> {
    let mut e = Errors::new();
    let mut d = TariffData {
        name: f.name.trim().to_string(),
        unit: f.unit.trim().to_string(),
        is_default: checked(&f.is_default),
        ..Default::default()
    };
    if d.name.is_empty() {
        e.insert("name", "Naam is verplicht.".into());
    }
    if !UNITS.iter().any(|(u, _)| *u == d.unit) {
        e.insert("unit", "Kies een eenheid.".into());
    }
    match format::parse_amount(&f.price) {
        Some(c) if c >= 0 => d.price_cents = c,
        _ => {
            e.insert(
                "price",
                "Vul een prijs in, bijvoorbeeld 95,00 (0 mag).".into(),
            );
        }
    }
    match f.vat_bp.trim().parse::<i64>() {
        Ok(bp) if VAT_CHOICES.contains(&bp) => d.vat_bp = bp,
        _ => {
            e.insert("vat_bp", "Kies een btw-tarief.".into());
        }
    }
    match (opt_id(&f.client_id), opt_id(&f.project_id)) {
        (Ok(c), Ok(p)) => {
            d.client_id = c;
            d.project_id = p;
        }
        _ => {
            e.insert("client_id", "Ongeldige keuze.".into());
        }
    }
    match format::parse_date(&f.valid_from) {
        Some(date) => d.valid_from = format::iso_date(date),
        None => {
            e.insert("valid_from", "Vul een geldige ingangsdatum in.".into());
        }
    }
    if !f.valid_to.trim().is_empty() {
        match format::parse_date(&f.valid_to) {
            Some(date) => {
                let to = format::iso_date(date);
                if !d.valid_from.is_empty() && to < d.valid_from {
                    e.insert("valid_to", "De einddatum ligt voor de ingangsdatum.".into());
                }
                d.valid_to = Some(to);
            }
            None => {
                e.insert(
                    "valid_to",
                    "Vul een geldige einddatum in of laat leeg.".into(),
                );
            }
        }
    }
    if e.is_empty() {
        Ok(d)
    } else {
        Err(e)
    }
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tarieven", routing::get(list_page).post(create))
        .route("/tarieven/nieuw", routing::get(new_page))
        .route("/tarieven/{id}", routing::get(edit_page).post(save))
        .route("/tarieven/{id}/archiveren", post(archive))
        .route("/tarieven/{id}/terugzetten", post(unarchive))
        .route(
            "/tarieven/{id}/nieuwe-prijs",
            routing::get(new_price_page).post(new_price_submit),
        )
}

#[derive(Template, WebTemplate)]
#[template(path = "tariffs/list.html")]
struct ListPage {
    l: Layout,
    tariffs: Vec<TariffRow>,
    show_archived: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "tariffs/form.html")]
struct FormPage {
    l: Layout,
    tariff: Option<Tariff>,
    f: TariffForm,
    errors: Errors,
    clients: Vec<ClientOption>,
    projects: Vec<ProjectOption>,
    units: &'static [(&'static str, &'static str)],
    vat_choices: Vec<(String, String)>,
}

impl FormPage {
    async fn new(
        st: &AppState,
        company_id: i64,
        l: Layout,
        tariff: Option<Tariff>,
        f: TariffForm,
        errors: Errors,
    ) -> AppResult<Self> {
        Ok(FormPage {
            clients: clients::options(&st.pool, company_id).await?,
            projects: projects::options(&st.pool, company_id).await?,
            units: UNITS,
            vat_choices: VAT_CHOICES
                .iter()
                .map(|bp| (bp.to_string(), format::vat_pct(*bp)))
                .collect(),
            l,
            tariff,
            f,
            errors,
        })
    }

    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "tariffs/new_price.html")]
struct NewPricePage {
    l: Layout,
    t: Tariff,
    price: String,
    from: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    archief: Option<String>,
}

async fn list_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<ListQuery>,
) -> AppResult<ListPage> {
    let show_archived = q.archief.is_some();
    Ok(ListPage {
        l,
        tariffs: list_rows(&st.pool, company.id, None, show_archived).await?,
        show_archived,
    })
}

#[derive(Deserialize)]
struct NewQuery {
    #[serde(default)]
    klant: String,
}

async fn new_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<NewQuery>,
) -> AppResult<FormPage> {
    let f = TariffForm {
        unit: "hour".into(),
        vat_bp: company.default_vat_bp.to_string(),
        valid_from: format::iso_date(format::today()),
        client_id: q.klant.trim().to_string(),
        ..Default::default()
    };
    FormPage::new(&st, company.id, l, None, f, Errors::new()).await
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<TariffForm>,
) -> AppResult<Response> {
    let result = match validate_form(&f) {
        Ok(d) => insert(&st.pool, company.id, d, Some(user.id)).await,
        Err(e) => Err(SaveError::Invalid(e)),
    };
    match result {
        Ok(_) => {
            flash::ok(&session, &format!("Tarief {} aangemaakt.", f.name.trim())).await?;
            Ok(redirect("/tarieven"))
        }
        Err(SaveError::Invalid(errors)) => {
            let page = FormPage::new(&st, company.id, l, None, f, errors).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(SaveError::App(e)) => Err(e),
    }
}

async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<FormPage> {
    let t = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    let f = TariffForm::from_tariff(&t);
    FormPage::new(&st, company.id, l, Some(t), f, Errors::new()).await
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<TariffForm>,
) -> AppResult<Response> {
    let t = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if t.archived_at.is_some() {
        return Err(AppError::BadRequest(
            "Een gearchiveerd tarief kan niet worden gewijzigd.".into(),
        ));
    }
    let result = match validate_form(&f) {
        Ok(d) => update(&st.pool, company.id, id, d, user.id).await,
        Err(e) => Err(SaveError::Invalid(e)),
    };
    match result {
        Ok(()) => {
            flash::ok(&session, &format!("Tarief {} opgeslagen.", f.name.trim())).await?;
            Ok(redirect("/tarieven"))
        }
        Err(SaveError::Invalid(errors)) => {
            let page = FormPage::new(&st, company.id, l, Some(t), f, errors).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(SaveError::App(e)) => Err(e),
    }
}

async fn archive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, company.id, id, true, user.id).await?;
    flash::ok(
        &session,
        "Tarief gearchiveerd. Bestaande uren houden hun prijs.",
    )
    .await?;
    Ok(redirect("/tarieven"))
}

async fn unarchive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    match set_archived(&st.pool, company.id, id, false, user.id).await {
        Ok(()) => flash::ok(&session, "Tarief teruggezet.").await?,
        Err(AppError::BadRequest(msg)) => flash::error(&session, &msg).await?,
        Err(e) => return Err(e),
    }
    Ok(redirect("/tarieven?archief=1"))
}

async fn new_price_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<NewPricePage> {
    let mut conn = st.pool.acquire().await?;
    let t = get(&mut conn, company.id, id).await?;
    let next_year =
        time::Date::from_calendar_date(format::today().year() + 1, time::Month::January, 1)
            .expect("valid date");
    Ok(NewPricePage {
        l,
        price: format::decimal(t.price_cents),
        from: format::iso_date(next_year),
        t,
        error: None,
    })
}

#[derive(Deserialize)]
struct NewPriceForm {
    #[serde(default)]
    price: String,
    #[serde(default)]
    from: String,
}

async fn new_price_submit(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<NewPriceForm>,
) -> AppResult<Response> {
    let t = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    let fail = |l, t, msg: String| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            NewPricePage {
                l,
                t,
                price: f.price.clone(),
                from: f.from.clone(),
                error: Some(msg),
            },
        )
            .into_response()
    };
    let Some(price) = format::parse_amount(&f.price).filter(|p| *p >= 0) else {
        return Ok(fail(l, t, "Vul een geldige prijs in.".into()));
    };
    let Some(from) = format::parse_date(&f.from) else {
        return Ok(fail(l, t, "Vul een geldige ingangsdatum in.".into()));
    };
    match new_price(&st.pool, company.id, id, price, from, user.id).await {
        Ok(_) => {
            flash::ok(
                &session,
                &format!(
                    "Nieuwe prijs voor {}: {} vanaf {}.",
                    t.name,
                    format::amount(price),
                    format::date(from)
                ),
            )
            .await?;
            Ok(redirect("/tarieven"))
        }
        Err(AppError::BadRequest(msg)) => Ok(fail(l, t, msg)),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> TariffForm {
        TariffForm {
            name: "Standaard".into(),
            unit: "hour".into(),
            price: "95,00".into(),
            vat_bp: "2100".into(),
            valid_from: "01-01-2026".into(),
            ..Default::default()
        }
    }

    #[test]
    fn valid_form() {
        let d = validate_form(&form()).unwrap();
        assert_eq!(d.price_cents, 9500);
        assert_eq!(d.valid_from, "2026-01-01");
        assert!(!d.is_default);
        let zero = TariffForm {
            price: "0".into(),
            is_default: Some("1".into()),
            ..form()
        };
        let d = validate_form(&zero).unwrap();
        assert_eq!(d.price_cents, 0);
        assert!(d.is_default);
    }

    #[test]
    fn invalid_form() {
        let f = TariffForm {
            name: "".into(),
            unit: "dag".into(),
            price: "-5".into(),
            vat_bp: "1900".into(),
            valid_from: "".into(),
            ..form()
        };
        let e = validate_form(&f).unwrap_err();
        for k in ["name", "unit", "price", "vat_bp", "valid_from"] {
            assert!(e.contains_key(k), "{k}");
        }
        let f = TariffForm {
            valid_to: "31-12-2025".into(),
            ..form()
        };
        assert!(validate_form(&f).unwrap_err().contains_key("valid_to"));
    }
}
