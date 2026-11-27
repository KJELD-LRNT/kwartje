//! Projects: optional grouping of work per client. Managed on the client page.

use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::post;
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use tower_sessions::Session;

use super::clients;
use super::companies::CurrentCompany;
use crate::web::error::redirect;
use crate::web::layout::AuthUser;
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Project {
    pub id: i64,
    pub company_id: i64,
    pub client_id: i64,
    pub name: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

/// For `<select>`s: "Klant — Project".
#[derive(Debug, Clone)]
pub struct ProjectOption {
    pub id: i64,
    pub client_id: i64,
    pub label: String,
}

pub async fn list_for_client(
    pool: &SqlitePool,
    company_id: i64,
    client_id: i64,
    include_archived: bool,
) -> AppResult<Vec<Project>> {
    Ok(sqlx::query_as!(
        Project,
        r#"SELECT id AS "id!", company_id, client_id, name, archived_at, created_at, updated_at,
                  created_by
           FROM projects
           WHERE company_id = ? AND client_id = ? AND (? OR archived_at IS NULL)
           ORDER BY archived_at IS NOT NULL, name COLLATE NOCASE"#,
        company_id,
        client_id,
        include_archived
    )
    .fetch_all(pool)
    .await?)
}

pub async fn options(pool: &SqlitePool, company_id: i64) -> AppResult<Vec<ProjectOption>> {
    Ok(sqlx::query_as!(
        ProjectOption,
        r#"SELECT p.id AS "id!", p.client_id, c.name || ' — ' || p.name AS "label!: String"
           FROM projects p JOIN clients c ON c.id = p.client_id
           WHERE p.company_id = ? AND p.archived_at IS NULL AND c.archived_at IS NULL
           ORDER BY c.name COLLATE NOCASE, p.name COLLATE NOCASE"#,
        company_id
    )
    .fetch_all(pool)
    .await?)
}

/// A project of this company, or 404.
pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Project> {
    sqlx::query_as!(
        Project,
        r#"SELECT id AS "id!", company_id, client_id, name, archived_at, created_at, updated_at,
                  created_by
           FROM projects WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn get_active(
    conn: &mut SqliteConnection,
    company_id: i64,
    id: i64,
) -> AppResult<Project> {
    let p = get(conn, company_id, id).await?;
    if p.archived_at.is_some() {
        return Err(AppError::BadRequest(format!(
            "Project {} is gearchiveerd.",
            p.name
        )));
    }
    Ok(p)
}

fn clean_name(name: &str) -> AppResult<String> {
    let n = name.trim().to_string();
    if n.is_empty() {
        return Err(AppError::BadRequest("Projectnaam is verplicht.".into()));
    }
    if n.chars().count() > 200 {
        return Err(AppError::BadRequest(
            "Projectnaam is te lang (max. 200 tekens).".into(),
        ));
    }
    Ok(n)
}

pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    client_id: i64,
    name: &str,
    by: Option<i64>,
) -> AppResult<i64> {
    let name = clean_name(name)?;
    let now = db::now();
    let mut tx = pool.begin().await?;
    clients::get_active(&mut tx, company_id, client_id).await?;
    let id = sqlx::query!(
        "INSERT INTO projects (company_id, client_id, name, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?)",
        company_id,
        client_id,
        name,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut tx, company_id, id).await?;
    audit::record_insert(&mut tx, by, "projects", id, &row).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn rename(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    name: &str,
    by: i64,
) -> AppResult<Project> {
    let name = clean_name(name)?;
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    sqlx::query!(
        "UPDATE projects SET name = ?, updated_at = ? WHERE id = ? AND company_id = ?",
        name,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "projects", id, &old, &new).await?;
    tx.commit().await?;
    Ok(new)
}

pub async fn set_archived(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    archived: bool,
    by: i64,
) -> AppResult<Project> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.archived_at.is_some() != archived {
        let archived_at = archived.then(|| now.clone());
        sqlx::query!(
            "UPDATE projects SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
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
            "projects",
            id,
            old.archived_at.as_deref(),
            archived_at.as_deref(),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(old)
}

// ---------- handlers (forms live on the client page) ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/klanten/{id}/projecten", post(create))
        .route("/projecten/{id}", post(save))
        .route("/projecten/{id}/archiveren", post(archive))
        .route("/projecten/{id}/terugzetten", post(unarchive))
}

#[derive(Deserialize)]
struct NameForm {
    #[serde(default)]
    name: String,
}

async fn create(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(client_id): Path<i64>,
    Form(f): Form<NameForm>,
) -> AppResult<Response> {
    let back = format!("/klanten/{client_id}");
    match insert(&st.pool, company.id, client_id, &f.name, Some(user.id)).await {
        Ok(_) => flash::ok(&session, &format!("Project {} toegevoegd.", f.name.trim())).await?,
        Err(AppError::BadRequest(msg)) => flash::error(&session, &msg).await?,
        Err(e) => return Err(e),
    }
    Ok(redirect(&back))
}

async fn save(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<NameForm>,
) -> AppResult<Response> {
    let client_id = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?.client_id
    };
    match rename(&st.pool, company.id, id, &f.name, user.id).await {
        Ok(p) => flash::ok(&session, &format!("Project {} opgeslagen.", p.name)).await?,
        Err(AppError::BadRequest(msg)) => flash::error(&session, &msg).await?,
        Err(e) => return Err(e),
    }
    Ok(redirect(&format!("/klanten/{client_id}")))
}

async fn archive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let p = set_archived(&st.pool, company.id, id, true, user.id).await?;
    flash::ok(&session, &format!("Project {} gearchiveerd.", p.name)).await?;
    Ok(redirect(&format!("/klanten/{}", p.client_id)))
}

async fn unarchive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let p = set_archived(&st.pool, company.id, id, false, user.id).await?;
    flash::ok(&session, &format!("Project {} teruggezet.", p.name)).await?;
    Ok(redirect(&format!("/klanten/{}", p.client_id)))
}
