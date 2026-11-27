//! Clients (per company): list with search, create/edit/archive, client page.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{self, post};
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use tower_sessions::Session;

use super::companies::CurrentCompany;
use super::projects::{self, Project};
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::form::{self, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, validate};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Client {
    pub id: i64,
    pub company_id: i64,
    pub name: String,
    pub contact_name: String,
    pub email: String,
    pub phone: String,
    pub street: String,
    pub postal_code: String,
    pub city: String,
    pub kvk: String,
    pub btw_id: String,
    pub notes: String,
    /// Prefix of this client's invoice and quote numbers (VH → VH-2026-0041).
    pub invoice_prefix: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

/// `id` + name, for `<select>`s.
#[derive(Debug, Clone)]
pub struct ClientOption {
    pub id: i64,
    pub name: String,
}

// ---------- queries (always scoped by company) ----------

pub async fn options(pool: &SqlitePool, company_id: i64) -> AppResult<Vec<ClientOption>> {
    Ok(sqlx::query_as!(
        ClientOption,
        r#"SELECT id AS "id!", name FROM clients
           WHERE company_id = ? AND archived_at IS NULL
           ORDER BY name COLLATE NOCASE"#,
        company_id
    )
    .fetch_all(pool)
    .await?)
}

pub async fn list(
    pool: &SqlitePool,
    company_id: i64,
    q: &str,
    include_archived: bool,
) -> AppResult<Vec<Client>> {
    let pattern = format!("%{}%", q.trim());
    Ok(sqlx::query_as!(
        Client,
        r#"SELECT id AS "id!", company_id, name, contact_name, email, phone, street, postal_code,
                  city, kvk, btw_id, notes, invoice_prefix, archived_at, created_at, updated_at, created_by
           FROM clients
           WHERE company_id = ?
             AND (? OR archived_at IS NULL)
             AND (name LIKE ? OR contact_name LIKE ? OR email LIKE ? OR city LIKE ?)
           ORDER BY archived_at IS NOT NULL, name COLLATE NOCASE"#,
        company_id,
        include_archived,
        pattern,
        pattern,
        pattern,
        pattern
    )
    .fetch_all(pool)
    .await?)
}

/// A client of this company, or 404.
pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Client> {
    sqlx::query_as!(
        Client,
        r#"SELECT id AS "id!", company_id, name, contact_name, email, phone, street, postal_code,
                  city, kvk, btw_id, notes, invoice_prefix, archived_at, created_at, updated_at, created_by
           FROM clients WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

/// An active client of this company (for use in a new record), or a Dutch error.
pub async fn get_active(
    conn: &mut SqliteConnection,
    company_id: i64,
    id: i64,
) -> AppResult<Client> {
    let c = get(conn, company_id, id).await?;
    if c.archived_at.is_some() {
        return Err(AppError::BadRequest(format!(
            "Klant {} is gearchiveerd.",
            c.name
        )));
    }
    Ok(c)
}

#[derive(Debug, Clone, Default)]
pub struct ClientData {
    pub name: String,
    pub contact_name: String,
    pub email: String,
    pub phone: String,
    pub street: String,
    pub postal_code: String,
    pub city: String,
    pub kvk: String,
    pub btw_id: String,
    pub notes: String,
    pub invoice_prefix: Option<String>,
}

pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    d: &ClientData,
    by: Option<i64>,
) -> AppResult<i64> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let id = sqlx::query!(
        "INSERT INTO clients (company_id, name, contact_name, email, phone, street, postal_code,
                              city, kvk, btw_id, notes, invoice_prefix, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        d.name,
        d.contact_name,
        d.email,
        d.phone,
        d.street,
        d.postal_code,
        d.city,
        d.kvk,
        d.btw_id,
        d.notes,
        d.invoice_prefix,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut tx, company_id, id).await?;
    audit::record_insert(&mut tx, by, "clients", id, &row).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    d: &ClientData,
    by: i64,
) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    sqlx::query!(
        "UPDATE clients SET name = ?, contact_name = ?, email = ?, phone = ?, street = ?,
                postal_code = ?, city = ?, kvk = ?, btw_id = ?, notes = ?, invoice_prefix = ?,
                updated_at = ?
         WHERE id = ? AND company_id = ?",
        d.name,
        d.contact_name,
        d.email,
        d.phone,
        d.street,
        d.postal_code,
        d.city,
        d.kvk,
        d.btw_id,
        d.notes,
        d.invoice_prefix,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "clients", id, &old, &new).await?;
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
    let archived_at = archived.then(|| now.clone());
    sqlx::query!(
        "UPDATE clients SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
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
        "clients",
        id,
        old.archived_at.as_deref(),
        archived_at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---------- form ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub contact_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub postal_code: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub kvk: String,
    #[serde(default)]
    pub btw_id: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub invoice_prefix: String,
}

impl ClientForm {
    fn from_client(c: &Client) -> Self {
        ClientForm {
            name: c.name.clone(),
            contact_name: c.contact_name.clone(),
            email: c.email.clone(),
            phone: c.phone.clone(),
            street: c.street.clone(),
            postal_code: c.postal_code.clone(),
            city: c.city.clone(),
            kvk: c.kvk.clone(),
            btw_id: c.btw_id.clone(),
            notes: c.notes.clone(),
            invoice_prefix: c.invoice_prefix.clone().unwrap_or_default(),
        }
    }
}

pub fn validate_form(f: &ClientForm) -> Result<ClientData, Errors> {
    let mut e = Errors::new();
    let mut d = ClientData {
        name: f.name.trim().to_string(),
        contact_name: f.contact_name.trim().to_string(),
        phone: f.phone.trim().to_string(),
        street: f.street.trim().to_string(),
        city: f.city.trim().to_string(),
        notes: f.notes.trim().to_string(),
        ..Default::default()
    };
    if d.name.is_empty() {
        e.insert("name", "Naam is verplicht.".into());
    } else if d.name.chars().count() > 200 {
        e.insert("name", "Naam is te lang (max. 200 tekens).".into());
    }
    if d.notes.chars().count() > 10_000 {
        e.insert(
            "notes",
            "Notities zijn te lang (max. 10.000 tekens).".into(),
        );
    }
    let mut optional = |key: &'static str,
                        raw: &str,
                        check: fn(&str) -> Result<String, &'static str>,
                        out: &mut String| {
        if !raw.trim().is_empty() {
            match check(raw) {
                Ok(v) => *out = v,
                Err(msg) => {
                    e.insert(key, msg.into());
                }
            }
        }
    };
    optional("email", &f.email, validate::email, &mut d.email);
    optional(
        "postal_code",
        &f.postal_code,
        validate::postal_code,
        &mut d.postal_code,
    );
    optional("kvk", &f.kvk, validate::kvk, &mut d.kvk);
    optional("btw_id", &f.btw_id, validate::btw_id, &mut d.btw_id);
    let prefix = f.invoice_prefix.trim().to_uppercase();
    if !prefix.is_empty() {
        if prefix.len() > 10
            || !prefix
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            e.insert(
                "invoice_prefix",
                "Factuurprefix: 1 tot 10 hoofdletters of cijfers, bijvoorbeeld VH.".into(),
            );
        } else {
            d.invoice_prefix = Some(prefix);
        }
    }
    if e.is_empty() {
        Ok(d)
    } else {
        Err(e)
    }
}

/// The other client of this company that already uses `prefix`, if any.
pub async fn prefix_owner(
    pool: &SqlitePool,
    company_id: i64,
    prefix: &str,
    except_id: Option<i64>,
) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar!(
        "SELECT name FROM clients WHERE company_id = ? AND invoice_prefix = ? AND id IS NOT ?",
        company_id,
        prefix,
        except_id
    )
    .fetch_optional(pool)
    .await?)
}

async fn check_prefix(
    pool: &SqlitePool,
    company_id: i64,
    d: &ClientData,
    except_id: Option<i64>,
) -> AppResult<Result<(), Errors>> {
    if let Some(p) = &d.invoice_prefix {
        if let Some(other) = prefix_owner(pool, company_id, p, except_id).await? {
            let mut e = Errors::new();
            e.insert(
                "invoice_prefix",
                format!("Prefix {p} wordt al gebruikt door klant {other}."),
            );
            return Ok(Err(e));
        }
    }
    Ok(Ok(()))
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/klanten", routing::get(list_page).post(create))
        .route("/klanten/nieuw", routing::get(new_page))
        .route("/klanten/{id}", routing::get(show_page))
        .route("/klanten/{id}/bewerken", routing::get(edit_page).post(save))
        .route("/klanten/{id}/archiveren", post(archive))
        .route("/klanten/{id}/terugzetten", post(unarchive))
}

#[derive(Template, WebTemplate)]
#[template(path = "clients/list.html")]
struct ListPage {
    l: Layout,
    clients: Vec<Client>,
    q: String,
    show_archived: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "clients/form.html")]
struct FormPage {
    l: Layout,
    client: Option<Client>,
    f: ClientForm,
    errors: Errors,
}

impl FormPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "clients/show.html")]
struct ShowPage {
    l: Layout,
    c: Client,
    projects: Vec<Project>,
    tariffs: Vec<super::tariffs::TariffRow>,
    recent: Vec<super::hours::EntryRow>,
    summary: super::hours::ClientSummary,
    invoices: Vec<super::invoices::list::Row>,
    documents: Vec<super::documents::Document>,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    q: String,
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
        clients: list(&st.pool, company.id, &q.q, show_archived).await?,
        q: q.q.trim().to_string(),
        show_archived,
    })
}

async fn new_page(Page(l): Page, _c: CurrentCompany) -> FormPage {
    FormPage {
        l,
        client: None,
        f: ClientForm::default(),
        errors: Errors::new(),
    }
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<ClientForm>,
) -> AppResult<Response> {
    let d = match validate_form(&f) {
        Ok(d) => d,
        Err(errors) => {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                FormPage {
                    l,
                    client: None,
                    f,
                    errors,
                },
            )
                .into_response())
        }
    };
    if let Err(errors) = check_prefix(&st.pool, company.id, &d, None).await? {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            FormPage {
                l,
                client: None,
                f,
                errors,
            },
        )
            .into_response());
    }
    let id = insert(&st.pool, company.id, &d, Some(user.id)).await?;
    flash::ok(&session, &format!("Klant {} aangemaakt.", d.name)).await?;
    Ok(redirect(&format!("/klanten/{id}")))
}

async fn show_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<ShowPage> {
    let c = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    Ok(ShowPage {
        projects: projects::list_for_client(&st.pool, company.id, id, true).await?,
        tariffs: super::tariffs::list_for_client(&st.pool, company.id, id).await?,
        recent: super::hours::recent_for_client(&st.pool, company.id, id, 20).await?,
        summary: super::hours::client_summary(&st.pool, company.id, id).await?,
        invoices: super::invoices::list::rows(&st.pool, company.id, Some(id)).await?,
        documents: super::documents::list(
            &st.pool,
            company.id,
            &super::documents::ListFilter {
                client_id: Some(id),
                ..Default::default()
            },
        )
        .await?,
        l,
        c,
    })
}

async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<FormPage> {
    let mut conn = st.pool.acquire().await?;
    let c = get(&mut conn, company.id, id).await?;
    Ok(FormPage {
        l,
        f: ClientForm::from_client(&c),
        client: Some(c),
        errors: Errors::new(),
    })
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<ClientForm>,
) -> AppResult<Response> {
    let c = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if c.archived_at.is_some() {
        return Err(AppError::BadRequest(
            "Een gearchiveerde klant kan niet worden gewijzigd. Zet de klant eerst terug.".into(),
        ));
    }
    let d = match validate_form(&f) {
        Ok(d) => d,
        Err(errors) => {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                FormPage {
                    l,
                    client: Some(c),
                    f,
                    errors,
                },
            )
                .into_response())
        }
    };
    if let Err(errors) = check_prefix(&st.pool, company.id, &d, Some(id)).await? {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            FormPage {
                l,
                client: Some(c),
                f,
                errors,
            },
        )
            .into_response());
    }
    update(&st.pool, company.id, id, &d, user.id).await?;
    flash::ok(&session, &format!("Klant {} opgeslagen.", d.name)).await?;
    Ok(redirect(&format!("/klanten/{id}")))
}

async fn archive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, company.id, id, true, user.id).await?;
    flash::ok(&session, "Klant gearchiveerd.").await?;
    Ok(redirect("/klanten"))
}

async fn unarchive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, company.id, id, false, user.id).await?;
    flash::ok(&session, "Klant teruggezet.").await?;
    Ok(redirect(&format!("/klanten/{id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        let ok = ClientForm {
            name: "Bakker".into(),
            email: "info@bakker.example".into(),
            postal_code: "1234ab".into(),
            ..Default::default()
        };
        let d = validate_form(&ok).unwrap();
        assert_eq!(d.postal_code, "1234 AB");
        let bad = ClientForm {
            name: "".into(),
            email: "geen-mail".into(),
            kvk: "12".into(),
            ..Default::default()
        };
        let e = validate_form(&bad).unwrap_err();
        assert!(e.contains_key("name") && e.contains_key("email") && e.contains_key("kvk"));
    }
}
