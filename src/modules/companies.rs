//! Companies: CRUD, archive, logo, and the company switcher.

use askama::Template;
use askama_web::WebTemplate;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{self, post};
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use tower_sessions::Session;

use crate::audit;
use crate::auth::session_keys::COMPANY_ID;
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::form::{self, is_unique_violation, Errors};
use crate::web::layout::{safe_local_path, AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{db, format, validate};

pub const VAT_CHOICES: &[i64] = &[2100, 900, 0];
pub const ROUNDING_CHOICES: &[i64] = &[0, 1, 5, 6, 10, 15, 30, 60];
pub const LOGO_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Company {
    pub id: i64,
    pub name: String,
    pub kvk: String,
    pub btw_id: String,
    pub iban: String,
    pub street: String,
    pub postal_code: String,
    pub city: String,
    pub email: String,
    pub phone: String,
    pub invoice_prefix: String,
    pub payment_term_days: i64,
    pub default_vat_bp: i64,
    pub rounding_minutes: i64,
    pub logo_path: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

/// Minimal company info for the switcher.
#[derive(Debug, Clone)]
pub struct CompanyOption {
    pub id: i64,
    pub name: String,
}

// ---------- queries ----------

pub async fn options(pool: &SqlitePool) -> AppResult<Vec<CompanyOption>> {
    Ok(sqlx::query_as!(
        CompanyOption,
        r#"SELECT id AS "id!", name FROM companies WHERE archived_at IS NULL ORDER BY name COLLATE NOCASE"#
    )
    .fetch_all(pool)
    .await?)
}

/// Active companies `user` may see: all of them, or for a boekhouder only
/// the ones assigned in `user_companies`.
pub async fn visible(pool: &SqlitePool, user: &AuthUser) -> AppResult<Vec<CompanyOption>> {
    let all = options(pool).await?;
    if !user.is_accountant() {
        return Ok(all);
    }
    let allowed = crate::auth::users::company_ids(pool, user.id).await?;
    Ok(all
        .into_iter()
        .filter(|c| allowed.contains(&c.id))
        .collect())
}

/// 404 unless `user` may see company `id` (archived ones included for
/// everyone but a boekhouder).
pub async fn ensure_visible(pool: &SqlitePool, user: &AuthUser, id: i64) -> AppResult<()> {
    if user.is_accountant() && !visible(pool, user).await?.iter().any(|c| c.id == id) {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn list(pool: &SqlitePool, include_archived: bool) -> AppResult<Vec<Company>> {
    Ok(sqlx::query_as!(
        Company,
        r#"SELECT id AS "id!", name, kvk, btw_id, iban, street, postal_code, city, email, phone,
                  invoice_prefix, payment_term_days, default_vat_bp, rounding_minutes, logo_path,
                  archived_at, created_at, updated_at, created_by
           FROM companies
           WHERE ? OR archived_at IS NULL
           ORDER BY archived_at IS NOT NULL, name COLLATE NOCASE"#,
        include_archived
    )
    .fetch_all(pool)
    .await?)
}

pub async fn get(conn: &mut SqliteConnection, id: i64) -> AppResult<Company> {
    sqlx::query_as!(
        Company,
        r#"SELECT id AS "id!", name, kvk, btw_id, iban, street, postal_code, city, email, phone,
                  invoice_prefix, payment_term_days, default_vat_bp, rounding_minutes, logo_path,
                  archived_at, created_at, updated_at, created_by
           FROM companies WHERE id = ?"#,
        id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

/// Field values after validation.
#[derive(Debug, Clone, Default)]
pub struct CompanyData {
    pub name: String,
    pub kvk: String,
    pub btw_id: String,
    pub iban: String,
    pub street: String,
    pub postal_code: String,
    pub city: String,
    pub email: String,
    pub phone: String,
    pub invoice_prefix: String,
    pub payment_term_days: i64,
    pub default_vat_bp: i64,
    pub rounding_minutes: i64,
}

pub async fn insert(pool: &SqlitePool, d: &CompanyData, by: Option<i64>) -> AppResult<i64> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let id = sqlx::query!(
        "INSERT INTO companies (name, kvk, btw_id, iban, street, postal_code, city, email, phone,
                                invoice_prefix, payment_term_days, default_vat_bp, rounding_minutes,
                                created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        d.name,
        d.kvk,
        d.btw_id,
        d.iban,
        d.street,
        d.postal_code,
        d.city,
        d.email,
        d.phone,
        d.invoice_prefix,
        d.payment_term_days,
        d.default_vat_bp,
        d.rounding_minutes,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut tx, id).await?;
    audit::record_insert(&mut tx, by, "companies", id, &row).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(pool: &SqlitePool, id: i64, d: &CompanyData, by: i64) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, id).await?;
    sqlx::query!(
        "UPDATE companies SET name = ?, kvk = ?, btw_id = ?, iban = ?, street = ?, postal_code = ?,
                city = ?, email = ?, phone = ?, invoice_prefix = ?, payment_term_days = ?,
                default_vat_bp = ?, rounding_minutes = ?, updated_at = ?
         WHERE id = ?",
        d.name,
        d.kvk,
        d.btw_id,
        d.iban,
        d.street,
        d.postal_code,
        d.city,
        d.email,
        d.phone,
        d.invoice_prefix,
        d.payment_term_days,
        d.default_vat_bp,
        d.rounding_minutes,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, id).await?;
    audit::record_update(&mut tx, Some(by), "companies", id, &old, &new).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_archived(pool: &SqlitePool, id: i64, archived: bool, by: i64) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, id).await?;
    if old.archived_at.is_some() == archived {
        return Ok(());
    }
    let archived_at = archived.then(|| now.clone());
    sqlx::query!(
        "UPDATE companies SET archived_at = ?, updated_at = ? WHERE id = ?",
        archived_at,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record_archive(
        &mut tx,
        Some(by),
        "companies",
        id,
        old.archived_at.as_deref(),
        archived_at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn set_logo(pool: &SqlitePool, id: i64, path: &str, by: i64) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, id).await?;
    sqlx::query!(
        "UPDATE companies SET logo_path = ?, updated_at = ? WHERE id = ?",
        path,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        Some(by),
        "companies",
        id,
        audit::Action::Update,
        &serde_json::json!({"logo_path": [old.logo_path, path]}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---------- current company ----------

/// Selected company from the session; falls back to the first active company
/// when nothing (or an archived/unknown company) is selected.
pub async fn resolve_current(
    session: &Session,
    list: &[CompanyOption],
) -> AppResult<Option<CompanyOption>> {
    let selected = session.get::<i64>(COMPANY_ID).await?;
    if let Some(c) = selected.and_then(|id| list.iter().find(|c| c.id == id)) {
        return Ok(Some(c.clone()));
    }
    match list.first() {
        Some(c) => {
            session.insert(COMPANY_ID, c.id).await?;
            Ok(Some(c.clone()))
        }
        None => {
            session.remove_value(COMPANY_ID).await?;
            Ok(None)
        }
    }
}

/// The active company for a request. Every business query in later phases
/// takes `company.id` from here, never from the request.
pub struct CurrentCompany(pub Company);

impl axum::extract::FromRequestParts<AppState> for CurrentCompany {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, AppError> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|(_, m)| AppError::Internal(anyhow::anyhow!("session layer: {m}")))?;
        let list = visible(&state.pool, &user).await?;
        let Some(opt) = resolve_current(&session, &list).await? else {
            return Err(AppError::NoCompany);
        };
        let mut conn = state.pool.acquire().await?;
        Ok(CurrentCompany(get(&mut conn, opt.id).await?))
    }
}

// ---------- form + validation ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompanyForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kvk: String,
    #[serde(default)]
    pub btw_id: String,
    #[serde(default)]
    pub iban: String,
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub postal_code: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub invoice_prefix: String,
    #[serde(default)]
    pub payment_term_days: String,
    #[serde(default)]
    pub default_vat_bp: String,
    #[serde(default)]
    pub rounding_minutes: String,
}

impl CompanyForm {
    fn from_company(c: &Company) -> Self {
        CompanyForm {
            name: c.name.clone(),
            kvk: c.kvk.clone(),
            btw_id: c.btw_id.clone(),
            iban: c.iban.clone(),
            street: c.street.clone(),
            postal_code: c.postal_code.clone(),
            city: c.city.clone(),
            email: c.email.clone(),
            phone: c.phone.clone(),
            invoice_prefix: c.invoice_prefix.clone(),
            payment_term_days: c.payment_term_days.to_string(),
            default_vat_bp: c.default_vat_bp.to_string(),
            rounding_minutes: c.rounding_minutes.to_string(),
        }
    }

    fn blank() -> Self {
        CompanyForm {
            payment_term_days: "14".into(),
            default_vat_bp: "2100".into(),
            rounding_minutes: "0".into(),
            ..Default::default()
        }
    }
}

/// Validate and normalise. Optional fields may be empty; when filled they must
/// be valid. (Issuing an invoice will later require KvK, BTW-id and address.)
pub fn validate_form(f: &CompanyForm) -> Result<CompanyData, Errors> {
    let mut e = Errors::new();
    let mut d = CompanyData {
        name: f.name.trim().to_string(),
        street: f.street.trim().to_string(),
        city: f.city.trim().to_string(),
        phone: f.phone.trim().to_string(),
        ..Default::default()
    };
    if d.name.is_empty() {
        e.insert("name", "Naam is verplicht.".into());
    } else if d.name.chars().count() > 200 {
        e.insert("name", "Naam is te lang (max. 200 tekens).".into());
    }
    let mut optional = |key: &'static str,
                        raw: &str,
                        check: fn(&str) -> Result<String, &'static str>,
                        out: &mut String| {
        if raw.trim().is_empty() {
            out.clear();
        } else {
            match check(raw) {
                Ok(v) => *out = v,
                Err(msg) => {
                    e.insert(key, msg.into());
                }
            }
        }
    };
    optional("kvk", &f.kvk, validate::kvk, &mut d.kvk);
    optional("btw_id", &f.btw_id, validate::btw_id, &mut d.btw_id);
    optional("iban", &f.iban, validate::iban, &mut d.iban);
    optional(
        "postal_code",
        &f.postal_code,
        validate::postal_code,
        &mut d.postal_code,
    );
    optional("email", &f.email, validate::email, &mut d.email);

    let prefix = f.invoice_prefix.trim().to_uppercase();
    if prefix.is_empty()
        || prefix.len() > 10
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        e.insert(
            "invoice_prefix",
            "Bedrijfscode: 1 tot 10 hoofdletters of cijfers, bijvoorbeeld JB.".into(),
        );
    }
    d.invoice_prefix = prefix;

    match f.payment_term_days.trim().parse::<i64>() {
        Ok(n) if (1..=365).contains(&n) => d.payment_term_days = n,
        _ => {
            e.insert(
                "payment_term_days",
                "Betaaltermijn: een aantal dagen van 1 tot 365.".into(),
            );
        }
    }
    match f.default_vat_bp.trim().parse::<i64>() {
        Ok(bp) if VAT_CHOICES.contains(&bp) => d.default_vat_bp = bp,
        _ => {
            e.insert("default_vat_bp", "Kies een btw-tarief.".into());
        }
    }
    match f.rounding_minutes.trim().parse::<i64>() {
        Ok(m) if ROUNDING_CHOICES.contains(&m) => d.rounding_minutes = m,
        _ => {
            e.insert("rounding_minutes", "Kies een afronding.".into());
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
        .route("/bedrijven", routing::get(list_page).post(create))
        .route("/bedrijven/nieuw", routing::get(new_page))
        .route("/bedrijven/{id}", routing::get(edit_page).post(save))
        .route("/bedrijven/{id}/archiveren", post(archive))
        .route("/bedrijven/{id}/terugzetten", post(unarchive))
        .route(
            "/bedrijven/{id}/logo",
            routing::get(logo)
                .post(upload_logo)
                .layer(DefaultBodyLimit::max(LOGO_MAX_BYTES + 64 * 1024)),
        )
        .route("/bedrijf/kiezen", post(switch))
}

#[derive(Template, WebTemplate)]
#[template(path = "companies/list.html")]
struct ListPage {
    l: Layout,
    companies: Vec<Company>,
    show_archived: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "companies/form.html")]
struct FormPage {
    l: Layout,
    company: Option<Company>,
    f: CompanyForm,
    errors: Errors,
    vat_choices: Vec<(String, String)>,
    rounding_choices: Vec<(String, String)>,
}

impl FormPage {
    fn new(l: Layout, company: Option<Company>, f: CompanyForm, errors: Errors) -> Self {
        FormPage {
            l,
            company,
            f,
            errors,
            vat_choices: VAT_CHOICES
                .iter()
                .map(|bp| (bp.to_string(), format::vat_pct(*bp)))
                .collect(),
            rounding_choices: ROUNDING_CHOICES
                .iter()
                .map(|m| {
                    let label = match m {
                        0 => "Niet afronden".to_string(),
                        1 => "Op 1 minuut".to_string(),
                        m => format!("Naar boven op {m} minuten"),
                    };
                    (m.to_string(), label)
                })
                .collect(),
        }
    }

    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    archief: Option<String>,
}

async fn list_page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> AppResult<ListPage> {
    let show_archived = q.archief.is_some();
    let mut companies = list(&st.pool, show_archived).await?;
    if user.is_accountant() {
        let allowed = visible(&st.pool, &user).await?;
        companies.retain(|c| allowed.iter().any(|a| a.id == c.id));
    }
    Ok(ListPage {
        l,
        companies,
        show_archived,
    })
}

async fn new_page(Page(l): Page) -> FormPage {
    FormPage::new(l, None, CompanyForm::blank(), Errors::new())
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    Form(f): Form<CompanyForm>,
) -> AppResult<Response> {
    let d = match validate_form(&f) {
        Ok(d) => d,
        Err(errors) => {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                FormPage::new(l, None, f, errors),
            )
                .into_response())
        }
    };
    match insert(&st.pool, &d, Some(user.id)).await {
        Ok(id) => {
            session.insert(COMPANY_ID, id).await?;
            flash::ok(&session, &format!("Bedrijf {} aangemaakt.", d.name)).await?;
            Ok(redirect("/bedrijven"))
        }
        Err(e) if is_unique_violation(&e) => {
            let mut errors = Errors::new();
            errors.insert(
                "invoice_prefix",
                "Deze factuurprefix is al in gebruik.".into(),
            );
            Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                FormPage::new(l, None, f, errors),
            )
                .into_response())
        }
        Err(e) => Err(e),
    }
}

async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<FormPage> {
    ensure_visible(&st.pool, &user, id).await?;
    let mut conn = st.pool.acquire().await?;
    let c = get(&mut conn, id).await?;
    let f = CompanyForm::from_company(&c);
    Ok(FormPage::new(l, Some(c), f, Errors::new()))
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<CompanyForm>,
) -> AppResult<Response> {
    let company = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, id).await?
    };
    if company.archived_at.is_some() {
        return Err(AppError::BadRequest(
            "Een gearchiveerd bedrijf kan niet worden gewijzigd. Zet het eerst terug.".into(),
        ));
    }
    let d = match validate_form(&f) {
        Ok(d) => d,
        Err(errors) => {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                FormPage::new(l, Some(company), f, errors),
            )
                .into_response())
        }
    };
    match update(&st.pool, id, &d, user.id).await {
        Ok(()) => {
            flash::ok(&session, &format!("Bedrijf {} opgeslagen.", d.name)).await?;
            Ok(redirect("/bedrijven"))
        }
        Err(e) if is_unique_violation(&e) => {
            let mut errors = Errors::new();
            errors.insert(
                "invoice_prefix",
                "Deze factuurprefix is al in gebruik.".into(),
            );
            Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                FormPage::new(l, Some(company), f, errors),
            )
                .into_response())
        }
        Err(e) => Err(e),
    }
}

async fn archive(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, id, true, user.id).await?;
    flash::ok(&session, "Bedrijf gearchiveerd.").await?;
    Ok(redirect("/bedrijven"))
}

async fn unarchive(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, id, false, user.id).await?;
    flash::ok(&session, "Bedrijf teruggezet.").await?;
    Ok(redirect("/bedrijven?archief=1"))
}

#[derive(Deserialize)]
struct SwitchForm {
    company_id: i64,
    #[serde(default)]
    back: String,
}

async fn switch(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Form(f): Form<SwitchForm>,
) -> AppResult<Response> {
    let list = visible(&st.pool, &user).await?;
    if !list.iter().any(|c| c.id == f.company_id) {
        flash::error(&session, "Dit bedrijf bestaat niet of is gearchiveerd.").await?;
        return Ok(redirect("/"));
    }
    session.insert(COMPANY_ID, f.company_id).await?;
    Ok(redirect(safe_local_path(&f.back)))
}

/// Detect the image type from its first bytes (never trust the file name).
fn sniff_logo(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "image/jpeg"))
    } else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_lowercase();
        let head = head.trim_start_matches('\u{feff}').trim_start();
        (head.starts_with("<svg") || head.starts_with("<?xml"))
            .then_some(("svg", "image/svg+xml"))
            .filter(|_| head.contains("<svg"))
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

async fn upload_logo(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> AppResult<Response> {
    {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, id).await?;
    }
    let back = format!("/bedrijven/{id}");
    let mut data: Option<Bytes> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("Het bestand is te groot of beschadigd.".into()))?
    {
        if field.name() == Some("logo") {
            data = Some(field.bytes().await.map_err(|_| {
                AppError::BadRequest("Het bestand is te groot (max. 1 MB).".into())
            })?);
        }
    }
    let Some(data) = data.filter(|d| !d.is_empty()) else {
        flash::error(&session, "Kies eerst een bestand.").await?;
        return Ok(redirect(&back));
    };
    if data.len() > LOGO_MAX_BYTES {
        flash::error(&session, "Het logo is te groot (max. 1 MB).").await?;
        return Ok(redirect(&back));
    }
    let Some((ext, _)) = sniff_logo(&data) else {
        flash::error(&session, "Alleen PNG, JPG of SVG is toegestaan.").await?;
        return Ok(redirect(&back));
    };
    // documents/<company_id>/logo-<timestamp>.<ext>; older logos are kept.
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let rel = format!("documents/{id}/logo-{stamp}.{ext}");
    let abs = st.cfg.data_dir.join(&rel);
    let dir = abs.parent().expect("has parent").to_path_buf();
    let tmp = dir.join(format!(".upload-{stamp}.tmp"));
    let write = {
        let data = data.clone();
        let abs = abs.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&tmp, &data)?;
            std::fs::rename(&tmp, &abs)
        })
    };
    write
        .await
        .map_err(|e| anyhow::anyhow!("join: {e}"))?
        .map_err(|e| anyhow::anyhow!("writing logo: {e}"))?;
    set_logo(&st.pool, id, &rel, user.id).await?;
    flash::ok(&session, "Logo opgeslagen.").await?;
    Ok(redirect(&back))
}

async fn logo(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    ensure_visible(&st.pool, &user, id).await?;
    let c = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, id).await?
    };
    let rel = c.logo_path.ok_or(AppError::NotFound)?;
    let bytes = tokio::fs::read(st.cfg.data_dir.join(&rel))
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, mime_for(&rel)),
            (header::CACHE_CONTROL, "private, max-age=300"),
            // An uploaded SVG must never run scripts.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox",
            ),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_form() -> CompanyForm {
        CompanyForm {
            name: "Voorbeeld B.V.".into(),
            kvk: "12345678".into(),
            btw_id: "NL123456789B01".into(),
            iban: "NL91ABNA0417164300".into(),
            postal_code: "1234ab".into(),
            invoice_prefix: "vb".into(),
            payment_term_days: "30".into(),
            default_vat_bp: "2100".into(),
            rounding_minutes: "15".into(),
            ..Default::default()
        }
    }

    #[test]
    fn valid_form_is_normalised() {
        let d = validate_form(&valid_form()).unwrap();
        assert_eq!(d.invoice_prefix, "VB");
        assert_eq!(d.iban, "NL91 ABNA 0417 1643 00");
        assert_eq!(d.postal_code, "1234 AB");
        assert_eq!(d.default_vat_bp, 2100);
        assert_eq!(d.rounding_minutes, 15);
    }

    #[test]
    fn invalid_fields_get_dutch_messages() {
        let f = CompanyForm {
            name: " ".into(),
            kvk: "123".into(),
            iban: "NL00ABNA0417164300".into(),
            invoice_prefix: "J B".into(),
            payment_term_days: "0".into(),
            default_vat_bp: "1900".into(),
            rounding_minutes: "7".into(),
            ..valid_form()
        };
        let e = validate_form(&f).unwrap_err();
        for key in [
            "name",
            "kvk",
            "iban",
            "invoice_prefix",
            "payment_term_days",
            "default_vat_bp",
            "rounding_minutes",
        ] {
            assert!(e.contains_key(key), "missing error for {key}");
        }
        assert_eq!(e["name"], "Naam is verplicht.");
    }

    #[test]
    fn optional_fields_may_be_empty() {
        let f = CompanyForm {
            kvk: String::new(),
            btw_id: String::new(),
            iban: String::new(),
            postal_code: String::new(),
            ..valid_form()
        };
        assert!(validate_form(&f).is_ok());
    }

    #[test]
    fn logo_sniffing() {
        assert_eq!(sniff_logo(b"\x89PNG\r\n\x1a\nrest").unwrap().0, "png");
        assert_eq!(sniff_logo(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap().0, "jpg");
        assert_eq!(sniff_logo(b"<svg xmlns=\"...\"></svg>").unwrap().0, "svg");
        assert_eq!(
            sniff_logo(b"<?xml version=\"1.0\"?><svg></svg>").unwrap().0,
            "svg"
        );
        assert!(sniff_logo(b"%PDF-1.4").is_none());
        assert!(sniff_logo(b"<?xml version=\"1.0\"?><html/>").is_none());
    }
}
