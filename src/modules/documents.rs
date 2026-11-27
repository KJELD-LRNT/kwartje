//! Document store: upload purchase invoices, receipts and contracts (PDF/JPG/PNG, also
//! from a phone camera), list/search per quarter, view, edit metadata,
//! archive. Files live under data_dir; nothing is ever deleted.

use askama::Template;
use askama_web::WebTemplate;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{self, post};
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use tower_sessions::Session;

use super::clients::{self, ClientOption};
use super::companies::CurrentCompany;
use crate::documents::{self as docs, KINDS, MAX_UPLOAD_BYTES, UPLOAD_KINDS};
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::form::{self, opt_id, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Document {
    pub id: i64,
    pub company_id: i64,
    pub kind: String,
    pub file_path: String,
    pub sha256: String,
    pub original_name: String,
    pub mime: String,
    pub size: i64,
    pub doc_date: String,
    pub supplier: Option<String>,
    pub description: String,
    pub amount_cents: Option<i64>,
    pub vat_cents: Option<i64>,
    pub invoice_id: Option<i64>,
    pub client_id: Option<i64>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

impl Document {
    pub fn kind_label(&self) -> &'static str {
        docs::kind_label(&self.kind)
    }
    /// Issued invoice or quote PDF, made and managed by the app.
    pub fn is_generated(&self) -> bool {
        crate::documents::is_generated(&self.kind)
    }
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
    pub fn missing_amounts(&self) -> bool {
        crate::documents::is_purchase(&self.kind)
            && (self.amount_cents.is_none() || self.vat_cents.is_none())
    }
    pub fn supplier_label(&self) -> &str {
        self.supplier.as_deref().unwrap_or("")
    }
    pub fn size_label(&self) -> String {
        if self.size >= 1024 * 1024 {
            format!("{} MB", format::decimal(self.size * 100 / (1024 * 1024)))
        } else {
            format!("{} kB", (self.size + 1023) / 1024)
        }
    }
    pub fn retain_until(&self) -> String {
        format::parse_iso_date(&self.doc_date)
            .map(|d| format::date(docs::retain_until(d)))
            .unwrap_or_default()
    }
}

// ---------- queries ----------

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Document> {
    sqlx::query_as!(
        Document,
        r#"SELECT id AS "id!", company_id, kind, file_path, sha256, original_name, mime, size,
                  doc_date, supplier, description, amount_cents, vat_cents, invoice_id, client_id,
                  archived_at, created_at, updated_at, created_by
           FROM documents WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

async fn find_by_sha(
    conn: &mut SqliteConnection,
    company_id: i64,
    sha: &str,
) -> AppResult<Option<(i64, bool)>> {
    Ok(sqlx::query!(
        r#"SELECT id AS "id!", archived_at IS NOT NULL AS "archived!: bool" FROM documents
           WHERE company_id = ? AND sha256 = ? LIMIT 1"#,
        company_id,
        sha
    )
    .fetch_optional(conn)
    .await?
    .map(|r| (r.id, r.archived)))
}

pub async fn suppliers(pool: &SqlitePool, company_id: i64) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT DISTINCT supplier AS "s!" FROM documents
           WHERE company_id = ? AND supplier IS NOT NULL AND supplier <> ''
           ORDER BY supplier COLLATE NOCASE"#,
        company_id
    )
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub from: Option<String>,
    pub to: Option<String>,
    pub kind: Option<String>,
    pub q: String,
    pub missing_only: bool,
    pub include_archived: bool,
    pub client_id: Option<i64>,
}

pub async fn list(pool: &SqlitePool, company_id: i64, f: &ListFilter) -> AppResult<Vec<Document>> {
    let pattern = format!("%{}%", f.q.trim());
    let rows = sqlx::query_as!(
        Document,
        r#"SELECT id AS "id!", company_id, kind, file_path, sha256, original_name, mime, size,
                  doc_date, supplier, description, amount_cents, vat_cents, invoice_id, client_id,
                  archived_at, created_at, updated_at, created_by
           FROM documents
           WHERE company_id = ?
             AND (? IS NULL OR doc_date >= ?) AND (? IS NULL OR doc_date <= ?)
             AND (? IS NULL OR kind = ?)
             AND (? IS NULL OR client_id = ?)
             AND (? OR archived_at IS NULL)
             AND (COALESCE(supplier, '') LIKE ? OR description LIKE ? OR original_name LIKE ?)
           ORDER BY doc_date DESC, id DESC"#,
        company_id,
        f.from,
        f.from,
        f.to,
        f.to,
        f.kind,
        f.kind,
        f.client_id,
        f.client_id,
        f.include_archived,
        pattern,
        pattern,
        pattern
    )
    .fetch_all(pool)
    .await?;
    Ok(if f.missing_only {
        rows.into_iter().filter(|d| d.missing_amounts()).collect()
    } else {
        rows
    })
}

// ---------- storing ----------

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub kind: String,
    pub doc_date: String,
    pub supplier: Option<String>,
    pub description: String,
    pub amount_cents: Option<i64>,
    pub vat_cents: Option<i64>,
    pub client_id: Option<i64>,
}

#[derive(Debug)]
pub enum StoreError {
    Duplicate { id: i64, archived: bool },
    App(AppError),
}

impl From<AppError> for StoreError {
    fn from(e: AppError) -> Self {
        StoreError::App(e)
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::App(e.into())
    }
}

/// Store an uploaded file with its metadata. `meta` is validated already.
pub async fn store(
    pool: &SqlitePool,
    data_dir: &std::path::Path,
    company_id: i64,
    original_name: &str,
    bytes: &[u8],
    meta: &Meta,
    by: i64,
) -> Result<i64, StoreError> {
    let (ext, mime) = docs::sniff(bytes).ok_or_else(|| {
        AppError::BadRequest("Alleen PDF, JPG of PNG kan worden opgeslagen.".into())
    })?;
    let sha = sha256_hex(bytes);
    let mut tx = pool.begin().await?;
    if let Some(c) = meta.client_id {
        clients::get(&mut tx, company_id, c).await?;
    }
    if let Some((id, archived)) = find_by_sha(&mut tx, company_id, &sha).await? {
        return Err(StoreError::Duplicate { id, archived });
    }
    let date = format::parse_iso_date(&meta.doc_date)
        .ok_or_else(|| AppError::BadRequest("Ongeldige datum.".into()))?;
    let now = db::now();
    let pending = format!("pending/{sha}");
    let size = bytes.len() as i64;
    let original = original_name.chars().take(200).collect::<String>();
    let id = sqlx::query!(
        "INSERT INTO documents (company_id, kind, file_path, sha256, original_name, mime, size,
                                doc_date, supplier, description, amount_cents, vat_cents, client_id,
                                created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        meta.kind,
        pending,
        sha,
        original,
        mime,
        size,
        meta.doc_date,
        meta.supplier,
        meta.description,
        meta.amount_cents,
        meta.vat_cents,
        meta.client_id,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let rel = docs::path(
        company_id,
        date,
        &meta.kind,
        id,
        &docs::safe_name(original_name, ext),
    );
    let abs = data_dir.join(&rel);
    let dir = abs.parent().expect("has parent").to_path_buf();
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".upload-{id}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &abs)
    };
    write().map_err(|e| AppError::Internal(anyhow::anyhow!("bestand opslaan: {e}")))?;
    let result: Result<(), StoreError> = async {
        sqlx::query!("UPDATE documents SET file_path = ? WHERE id = ?", rel, id)
            .execute(&mut *tx)
            .await?;
        let row = get(&mut tx, company_id, id).await?;
        audit::record_insert(&mut tx, Some(by), "documents", id, &row)
            .await
            .map_err(AppError::Internal)?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&abs);
        return Err(e);
    }
    if let Err(e) = tx.commit().await {
        let _ = std::fs::remove_file(&abs);
        return Err(e.into());
    }
    Ok(id)
}

// ---------- forms ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetaForm {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub doc_date: String,
    #[serde(default)]
    pub supplier: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub vat: String,
    #[serde(default)]
    pub client_id: String,
}

impl MetaForm {
    fn from_doc(d: &Document) -> Self {
        let a = |v: Option<i64>| v.map(format::decimal).unwrap_or_default();
        MetaForm {
            kind: d.kind.clone(),
            doc_date: d.doc_date.clone(),
            supplier: d.supplier.clone().unwrap_or_default(),
            description: d.description.clone(),
            amount: a(d.amount_cents),
            vat: a(d.vat_cents),
            client_id: d.client_id.map(|c| c.to_string()).unwrap_or_default(),
        }
    }
}

pub fn validate_meta(f: &MetaForm) -> Result<Meta, Errors> {
    let mut e = Errors::new();
    if !UPLOAD_KINDS.contains(&f.kind.as_str()) {
        e.insert("kind", "Kies het soort document.".into());
    }
    let date = format::parse_date(&f.doc_date);
    if date.is_none() {
        e.insert("doc_date", "Vul de datum van het document in.".into());
    }
    let supplier = f.supplier.trim().to_string();
    if supplier.is_empty() && crate::documents::is_purchase(&f.kind) {
        e.insert("supplier", "Vul de leverancier of winkel in.".into());
    }
    let amount = |s: &str, key: &'static str, e: &mut Errors| -> Option<i64> {
        if s.trim().is_empty() {
            return None;
        }
        match format::parse_amount(s) {
            Some(v) => Some(v),
            None => {
                e.insert(
                    key,
                    "Vul een bedrag in zoals 121,00 of laat het leeg.".into(),
                );
                None
            }
        }
    };
    let amount_cents = amount(&f.amount, "amount", &mut e);
    let vat_cents = amount(&f.vat, "vat", &mut e);
    if let (Some(a), Some(v)) = (amount_cents, vat_cents) {
        if v.abs() > a.abs() {
            e.insert(
                "vat",
                "Het btw-bedrag kan niet hoger zijn dan het totaal.".into(),
            );
        }
    }
    let client_id = match opt_id(&f.client_id) {
        Ok(c) => c,
        Err(_) => {
            e.insert("client_id", "Ongeldige klant.".into());
            None
        }
    };
    let description = f.description.trim().to_string();
    if description.chars().count() > 2000 {
        e.insert("description", "De omschrijving is te lang.".into());
    }
    if !e.is_empty() {
        return Err(e);
    }
    Ok(Meta {
        kind: f.kind.clone(),
        doc_date: format::iso_date(date.expect("checked")),
        supplier: (!supplier.is_empty()).then_some(supplier),
        description,
        amount_cents,
        vat_cents,
        client_id,
    })
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/documenten", routing::get(list_page))
        .route(
            "/documenten/nieuw",
            routing::get(upload_page)
                .post(upload)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES + 256 * 1024)),
        )
        .route("/documenten/{id}", routing::get(show_page).post(save))
        .route("/documenten/{id}/bestand", routing::get(file))
        .route("/documenten/{id}/archiveren", post(archive))
        .route("/documenten/{id}/terugzetten", post(unarchive))
}

#[derive(Template, WebTemplate)]
#[template(path = "documents/upload.html")]
struct UploadPage {
    l: Layout,
    f: MetaForm,
    errors: Errors,
    suppliers: Vec<String>,
    clients: Vec<ClientOption>,
    kinds: Vec<(&'static str, &'static str)>,
    duplicate: Option<(i64, bool)>,
}

impl UploadPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
}

fn upload_kinds() -> Vec<(&'static str, &'static str)> {
    KINDS
        .iter()
        .copied()
        .filter(|(k, _)| UPLOAD_KINDS.contains(k))
        .collect()
}

async fn render_upload(
    st: &AppState,
    company_id: i64,
    l: Layout,
    f: MetaForm,
    errors: Errors,
    duplicate: Option<(i64, bool)>,
) -> AppResult<UploadPage> {
    Ok(UploadPage {
        l,
        f,
        errors,
        suppliers: suppliers(&st.pool, company_id).await?,
        clients: clients::options(&st.pool, company_id).await?,
        kinds: upload_kinds(),
        duplicate,
    })
}

#[derive(Deserialize)]
struct UploadQuery {
    #[serde(default)]
    soort: String,
}

async fn upload_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<UploadQuery>,
) -> AppResult<UploadPage> {
    let kind = if UPLOAD_KINDS.contains(&q.soort.as_str()) {
        q.soort
    } else {
        "purchase_invoice".into()
    };
    let f = MetaForm {
        kind,
        doc_date: format::iso_date(format::today()),
        ..Default::default()
    };
    render_upload(&st, company.id, l, f, Errors::new(), None).await
}

async fn upload(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let mut f = MetaForm::default();
    let mut file: Option<(String, Bytes)> = None;
    while let Some(field) = multipart.next_field().await.map_err(|_| {
        AppError::BadRequest(
            "Het bestand is te groot (max. 20 MB) of de upload is afgebroken.".into(),
        )
    })? {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "photo" {
            let original = field.file_name().unwrap_or("").to_string();
            let bytes = field.bytes().await.map_err(|_| {
                AppError::BadRequest("Het bestand is te groot (max. 20 MB).".into())
            })?;
            if !bytes.is_empty() {
                file = Some((original, bytes));
            }
            continue;
        }
        let value = field.text().await.unwrap_or_default();
        match name.as_str() {
            "kind" => f.kind = value,
            "doc_date" => f.doc_date = value,
            "supplier" => f.supplier = value,
            "description" => f.description = value,
            "amount" => f.amount = value,
            "vat" => f.vat = value,
            "client_id" => f.client_id = value,
            _ => {}
        }
    }
    let mut errors = match validate_meta(&f) {
        Ok(_) => Errors::new(),
        Err(e) => e,
    };
    match &file {
        None => {
            errors.insert("file", "Kies een bestand of maak een foto.".into());
        }
        Some((_, b)) if b.len() > MAX_UPLOAD_BYTES => {
            errors.insert("file", "Het bestand is te groot (max. 20 MB).".into());
        }
        Some((_, b)) if docs::sniff(b).is_none() => {
            errors.insert(
                "file",
                "Alleen PDF, JPG of PNG kan worden opgeslagen.".into(),
            );
        }
        _ => {}
    }
    if !errors.is_empty() {
        let page = render_upload(&st, company.id, l, f, errors, None).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    let meta = validate_meta(&f).expect("validated above");
    let (original, bytes) = file.expect("checked above");
    match store(
        &st.pool,
        &st.cfg.data_dir,
        company.id,
        &original,
        &bytes,
        &meta,
        user.id,
    )
    .await
    {
        Ok(id) => {
            flash::ok(
                &session,
                &format!(
                    "{} opgeslagen. Nog een uploaden?",
                    docs::kind_label(&meta.kind)
                ),
            )
            .await?;
            let _ = id;
            Ok(redirect(&format!("/documenten/nieuw?soort={}", meta.kind)))
        }
        Err(StoreError::Duplicate { id, archived }) => {
            let mut errors = Errors::new();
            errors.insert("file", "Dit bestand is al opgeslagen.".into());
            let page = render_upload(&st, company.id, l, f, errors, Some((id, archived))).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(StoreError::App(AppError::BadRequest(msg))) => {
            let mut errors = Errors::new();
            errors.insert("file", msg);
            let page = render_upload(&st, company.id, l, f, errors, None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(StoreError::App(e)) => Err(e),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "documents/list.html")]
struct ListPage {
    l: Layout,
    docs: Vec<Document>,
    year: i32,
    quarter: String,
    kind: String,
    q: String,
    missing_only: bool,
    show_archived: bool,
    kinds: &'static [(&'static str, &'static str)],
    purchase_total: i64,
    purchase_vat: i64,
    missing_count: usize,
    period_label: String,
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    jaar: String,
    #[serde(default)]
    kwartaal: String,
    #[serde(default)]
    soort: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    zonder: Option<String>,
    #[serde(default)]
    archief: Option<String>,
}

async fn list_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<ListQuery>,
) -> AppResult<ListPage> {
    let today = format::today();
    let fresh = q.jaar.is_empty() && q.kwartaal.is_empty();
    let year: i32 = q
        .jaar
        .trim()
        .parse()
        .ok()
        .filter(|y| (2000..=2100).contains(y))
        .unwrap_or(today.year());
    let quarter: String = if fresh {
        docs::quarter_of(today).to_string()
    } else {
        q.kwartaal.trim().to_string()
    };
    let (from, to, period_label) = match quarter.parse::<u8>().ok().filter(|q| (1..=4).contains(q))
    {
        Some(qq) => {
            let (a, b) = docs::quarter_range(year, qq);
            (a, b, format!("Q{qq} {year}"))
        }
        None => (
            time::Date::from_calendar_date(year, time::Month::January, 1).expect("valid"),
            time::Date::from_calendar_date(year, time::Month::December, 31).expect("valid"),
            format!("{year}"),
        ),
    };
    let kind = q.soort.trim().to_string();
    let filter = ListFilter {
        from: Some(format::iso_date(from)),
        to: Some(format::iso_date(to)),
        kind: KINDS.iter().any(|(k, _)| *k == kind).then(|| kind.clone()),
        q: q.q.clone(),
        missing_only: q.zonder.is_some(),
        include_archived: q.archief.is_some(),
        client_id: None,
    };
    let docs = list(&st.pool, company.id, &filter).await?;
    let purchases = docs
        .iter()
        .filter(|d| crate::documents::is_purchase(&d.kind) && d.archived_at.is_none());
    Ok(ListPage {
        purchase_total: purchases.clone().filter_map(|d| d.amount_cents).sum(),
        purchase_vat: purchases.clone().filter_map(|d| d.vat_cents).sum(),
        missing_count: docs
            .iter()
            .filter(|d| d.missing_amounts() && d.archived_at.is_none())
            .count(),
        l,
        docs,
        year,
        quarter,
        kind,
        q: q.q.trim().to_string(),
        missing_only: q.zonder.is_some(),
        show_archived: q.archief.is_some(),
        kinds: KINDS,
        period_label,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "documents/show.html")]
struct ShowPage {
    l: Layout,
    d: Document,
    f: MetaForm,
    errors: Errors,
    intact: Option<bool>,
    invoice_number: Option<String>,
    clients: Vec<ClientOption>,
    suppliers: Vec<String>,
    kinds: Vec<(&'static str, &'static str)>,
}

impl ShowPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
}

/// `Some(true)` when the file exists and its SHA-256 matches.
pub fn check_file(data_dir: &std::path::Path, d: &Document) -> Option<bool> {
    match std::fs::read(data_dir.join(&d.file_path)) {
        Ok(b) => Some(sha256_hex(&b) == d.sha256),
        Err(_) => None,
    }
}

async fn render_show(
    st: &AppState,
    company_id: i64,
    l: Layout,
    d: Document,
    f: Option<MetaForm>,
    errors: Errors,
) -> AppResult<ShowPage> {
    let invoice_number = match d.invoice_id {
        Some(i) => sqlx::query_scalar!(
            "SELECT number FROM invoices WHERE id = ? AND company_id = ?",
            i,
            company_id
        )
        .fetch_optional(&st.pool)
        .await?
        .flatten(),
        None => None,
    };
    Ok(ShowPage {
        intact: check_file(&st.cfg.data_dir, &d),
        f: f.unwrap_or_else(|| MetaForm::from_doc(&d)),
        errors,
        invoice_number,
        clients: clients::options(&st.pool, company_id).await?,
        suppliers: suppliers(&st.pool, company_id).await?,
        kinds: upload_kinds(),
        l,
        d,
    })
}

async fn show_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<ShowPage> {
    let d = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    render_show(&st, company.id, l, d, None, Errors::new()).await
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<MetaForm>,
) -> AppResult<Response> {
    let old = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if old.is_generated() {
        return Err(AppError::BadRequest(
            "Deze PDF wordt door de app beheerd en kan hier niet worden gewijzigd.".into(),
        ));
    }
    let meta = match validate_meta(&f) {
        Ok(m) => m,
        Err(errors) => {
            let page = render_show(&st, company.id, l, old, Some(f), errors).await?;
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
        }
    };
    let now = db::now();
    let mut tx = st.pool.begin().await?;
    if let Some(c) = meta.client_id {
        clients::get(&mut tx, company.id, c).await?;
    }
    sqlx::query!(
        "UPDATE documents SET kind = ?, doc_date = ?, supplier = ?, description = ?, amount_cents = ?,
                vat_cents = ?, client_id = ?, updated_at = ?
         WHERE id = ? AND company_id = ? AND kind NOT IN ('sales_invoice', 'quote')",
        meta.kind,
        meta.doc_date,
        meta.supplier,
        meta.description,
        meta.amount_cents,
        meta.vat_cents,
        meta.client_id,
        now,
        id,
        company.id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company.id, id).await?;
    audit::record_update(&mut tx, Some(user.id), "documents", id, &old, &new).await?;
    tx.commit().await?;
    flash::ok(&session, "Gegevens opgeslagen.").await?;
    Ok(redirect(&format!("/documenten/{id}")))
}

async fn set_archived(
    st: &AppState,
    company_id: i64,
    id: i64,
    archived: bool,
    by: i64,
) -> AppResult<Document> {
    let now = db::now();
    let mut tx = st.pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.is_generated() {
        return Err(AppError::BadRequest(
            "Een PDF van een factuur of offerte kan niet worden gearchiveerd.".into(),
        ));
    }
    if old.archived_at.is_some() != archived {
        let archived_at = archived.then(|| now.clone());
        sqlx::query!(
            "UPDATE documents SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
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
            "documents",
            id,
            old.archived_at.as_deref(),
            archived_at.as_deref(),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(old)
}

async fn archive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st, company.id, id, true, user.id).await?;
    flash::ok(
        &session,
        "Document gearchiveerd. Het bestand blijft bewaard.",
    )
    .await?;
    Ok(redirect(&format!("/documenten/{id}")))
}

async fn unarchive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st, company.id, id, false, user.id).await?;
    flash::ok(&session, "Document teruggezet.").await?;
    Ok(redirect(&format!("/documenten/{id}")))
}

async fn file(
    State(st): State<AppState>,
    _user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let d = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    let bytes = tokio::fs::read(st.cfg.data_dir.join(&d.file_path))
        .await
        .map_err(|_| AppError::NotFound)?;
    let ext = d.file_path.rsplit('.').next().unwrap_or("pdf");
    let name = docs::safe_name(&d.original_name, ext);
    let mut resp = (
        [
            (header::CONTENT_TYPE, d.mime.clone()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{name}\""),
            ),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
        ],
        bytes,
    )
        .into_response();
    // Allow the preview iframe on our own detail page.
    let h = resp.headers_mut();
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(if d.is_image() {
            "default-src 'none'; img-src 'self'; frame-ancestors 'self'"
        } else {
            "frame-ancestors 'self'"
        }),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> MetaForm {
        MetaForm {
            kind: "purchase_invoice".into(),
            doc_date: "15-10-2026".into(),
            supplier: "KPN".into(),
            amount: "121,00".into(),
            vat: "21,00".into(),
            ..Default::default()
        }
    }

    #[test]
    fn meta_validation() {
        let m = validate_meta(&f()).unwrap();
        assert_eq!(m.doc_date, "2026-10-15");
        assert_eq!((m.amount_cents, m.vat_cents), (Some(12_100), Some(2100)));
        let m = validate_meta(&MetaForm {
            amount: "".into(),
            vat: "".into(),
            ..f()
        })
        .unwrap();
        assert_eq!(m.amount_cents, None);
        let e = validate_meta(&MetaForm {
            supplier: "".into(),
            doc_date: "".into(),
            kind: "x".into(),
            vat: "500".into(),
            ..f()
        })
        .unwrap_err();
        for k in ["doc_date", "kind", "vat"] {
            assert!(e.contains_key(k), "{k}");
        }
        // purchase invoices and receipts need a supplier
        for kind in ["purchase_invoice", "receipt"] {
            let e = validate_meta(&MetaForm {
                kind: kind.into(),
                supplier: "".into(),
                ..f()
            })
            .unwrap_err();
            assert!(e.contains_key("supplier"), "{kind}");
        }
        // contracts need no supplier or amounts
        let m = validate_meta(&MetaForm {
            kind: "contract".into(),
            supplier: "".into(),
            amount: "".into(),
            vat: "".into(),
            ..f()
        })
        .unwrap();
        assert_eq!((m.kind.as_str(), m.amount_cents), ("contract", None));
        // "other" does not need a supplier
        assert!(validate_meta(&MetaForm {
            kind: "other".into(),
            supplier: "".into(),
            ..f()
        })
        .is_ok());
    }
}
