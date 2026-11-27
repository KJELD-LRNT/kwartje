//! Tasks: what hours are logged on (Scouting, Contractonderhandeling,
//! Administratie…). Optionally bound to a client/project and/or a tariff, or
//! marked not billable.

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

use super::clients::{self, ClientOption};
use super::companies::CurrentCompany;
use super::projects::{self, ProjectOption};
use super::tariffs::{self, TariffOption};
use crate::web::error::redirect;
use crate::web::form::{self, checked, opt_id, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Task {
    pub id: i64,
    pub company_id: i64,
    pub name: String,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub tariff_id: Option<i64>,
    pub billable: bool,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
    pub id: i64,
    pub name: String,
    pub client_name: Option<String>,
    pub project_name: Option<String>,
    pub tariff_name: Option<String>,
    pub tariff_price: Option<i64>,
    pub billable: bool,
    pub archived_at: Option<String>,
}

impl TaskRow {
    pub fn tariff_label(&self) -> String {
        if !self.billable {
            return "Niet declarabel".into();
        }
        match (&self.tariff_name, self.tariff_price) {
            (Some(n), Some(p)) => format!("{n} ({})", crate::format::amount(p)),
            _ => "Standaardtarief".into(),
        }
    }
}

/// For the hours form.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskOption {
    pub id: i64,
    pub name: String,
    pub client_id: Option<i64>,
    pub client_name: Option<String>,
    pub project_id: Option<i64>,
    pub billable: bool,
}

impl TaskOption {
    pub fn label(&self) -> String {
        match &self.client_name {
            Some(c) => format!("{} ({c})", self.name),
            None => self.name.clone(),
        }
    }
}

// ---------- queries ----------

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Task> {
    sqlx::query_as!(
        Task,
        r#"SELECT id AS "id!", company_id, name, client_id, project_id, tariff_id,
                  billable AS "billable: bool", archived_at, created_at, updated_at, created_by
           FROM tasks WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn list(
    pool: &SqlitePool,
    company_id: i64,
    include_archived: bool,
) -> AppResult<Vec<TaskRow>> {
    Ok(sqlx::query_as!(
        TaskRow,
        r#"SELECT k.id AS "id!", k.name, c.name AS "client_name?", p.name AS "project_name?",
                  t.name AS "tariff_name?", t.price_cents AS "tariff_price?",
                  k.billable AS "billable: bool", k.archived_at
           FROM tasks k
           LEFT JOIN clients c ON c.id = k.client_id
           LEFT JOIN projects p ON p.id = k.project_id
           LEFT JOIN tariffs t ON t.id = k.tariff_id
           WHERE k.company_id = ? AND (? OR k.archived_at IS NULL)
           ORDER BY k.archived_at IS NOT NULL, k.name COLLATE NOCASE"#,
        company_id,
        include_archived
    )
    .fetch_all(pool)
    .await?)
}

pub async fn options(pool: &SqlitePool, company_id: i64) -> AppResult<Vec<TaskOption>> {
    Ok(sqlx::query_as!(
        TaskOption,
        r#"SELECT k.id AS "id!", k.name, k.client_id, c.name AS "client_name?", k.project_id,
                  k.billable AS "billable: bool"
           FROM tasks k LEFT JOIN clients c ON c.id = k.client_id
           WHERE k.company_id = ? AND k.archived_at IS NULL
             AND (c.id IS NULL OR c.archived_at IS NULL)
           ORDER BY k.client_id IS NOT NULL, k.name COLLATE NOCASE"#,
        company_id
    )
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, Default)]
pub struct TaskData {
    pub name: String,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub tariff_id: Option<i64>,
    pub billable: bool,
}

/// Check references inside the transaction; fills `client_id` from the project.
async fn check_refs(
    conn: &mut SqliteConnection,
    company_id: i64,
    d: &mut TaskData,
    errors: &mut Errors,
) -> AppResult<()> {
    if let Some(pid) = d.project_id {
        let p = projects::get_active(conn, company_id, pid).await?;
        if d.client_id.is_some_and(|c| c != p.client_id) {
            errors.insert(
                "project_id",
                "Dit project hoort niet bij de gekozen klant.".into(),
            );
        }
        d.client_id = Some(p.client_id);
    }
    if let Some(cid) = d.client_id {
        clients::get_active(conn, company_id, cid).await?;
    }
    if !d.billable {
        d.tariff_id = None;
    }
    if let Some(tid) = d.tariff_id {
        let t = tariffs::get(conn, company_id, tid).await?;
        if t.unit != "hour" || t.archived_at.is_some() {
            errors.insert(
                "tariff_id",
                "Kies een (niet-gearchiveerd) uurtarief.".into(),
            );
        }
        let fits = match (t.client_id, t.project_id) {
            (None, _) => true,
            (Some(c), None) => d.client_id == Some(c),
            (Some(c), Some(p)) => d.client_id == Some(c) && d.project_id == Some(p),
        };
        if !fits {
            errors.insert(
                "tariff_id",
                "Dit tarief hoort bij een andere klant of een ander project dan de taak.".into(),
            );
        }
    }
    Ok(())
}

pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    mut d: TaskData,
    by: Option<i64>,
) -> Result<i64, Errors> {
    let run = async {
        let now = db::now();
        let mut tx = pool.begin().await?;
        let mut errors = Errors::new();
        check_refs(&mut tx, company_id, &mut d, &mut errors).await?;
        if !errors.is_empty() {
            return Ok(Err(errors));
        }
        let id = sqlx::query!(
            "INSERT INTO tasks (company_id, name, client_id, project_id, tariff_id, billable,
                                created_at, updated_at, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            company_id,
            d.name,
            d.client_id,
            d.project_id,
            d.tariff_id,
            d.billable,
            now,
            now,
            by
        )
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        let row = get(&mut tx, company_id, id).await?;
        audit::record_insert(&mut tx, by, "tasks", id, &row).await?;
        tx.commit().await?;
        Ok::<_, AppError>(Ok(id))
    };
    match run.await {
        Ok(r) => r,
        Err(e) => Err(app_error(e)),
    }
}

pub async fn update(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    mut d: TaskData,
    by: i64,
) -> Result<(), Errors> {
    let run = async {
        let now = db::now();
        let mut tx = pool.begin().await?;
        let old = get(&mut tx, company_id, id).await?;
        let mut errors = Errors::new();
        check_refs(&mut tx, company_id, &mut d, &mut errors).await?;
        if !errors.is_empty() {
            return Ok(Err(errors));
        }
        sqlx::query!(
            "UPDATE tasks SET name = ?, client_id = ?, project_id = ?, tariff_id = ?, billable = ?,
                    updated_at = ?
             WHERE id = ? AND company_id = ?",
            d.name,
            d.client_id,
            d.project_id,
            d.tariff_id,
            d.billable,
            now,
            id,
            company_id
        )
        .execute(&mut *tx)
        .await?;
        let new = get(&mut tx, company_id, id).await?;
        audit::record_update(&mut tx, Some(by), "tasks", id, &old, &new).await?;
        tx.commit().await?;
        Ok::<_, AppError>(Ok(()))
    };
    match run.await {
        Ok(r) => r,
        Err(e) => Err(app_error(e)),
    }
}

/// Reference errors (archived client etc.) become a form-level message;
/// anything else is logged and shown as a generic error.
fn app_error(e: AppError) -> Errors {
    let mut errors = Errors::new();
    let msg = match e {
        AppError::BadRequest(m) => m,
        AppError::NotFound => "Een gekozen klant, project of tarief bestaat niet.".into(),
        other => {
            tracing::error!("saving task: {other}");
            "Opslaan is mislukt door een onverwachte fout.".into()
        }
    };
    errors.insert("form", msg);
    errors
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
        "UPDATE tasks SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
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
        "tasks",
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
pub struct TaskForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub tariff_id: String,
    /// Checkbox "Niet declarabel".
    #[serde(default)]
    pub internal: Option<String>,
}

impl TaskForm {
    fn from_task(t: &Task) -> Self {
        let s = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
        TaskForm {
            name: t.name.clone(),
            client_id: s(t.client_id),
            project_id: s(t.project_id),
            tariff_id: s(t.tariff_id),
            internal: (!t.billable).then(|| "1".into()),
        }
    }

    fn internal_checked(&self) -> bool {
        checked(&self.internal)
    }
}

pub fn validate_form(f: &TaskForm) -> Result<TaskData, Errors> {
    let mut e = Errors::new();
    let name = f.name.trim().to_string();
    if name.is_empty() {
        e.insert("name", "Naam is verplicht.".into());
    } else if name.chars().count() > 200 {
        e.insert("name", "Naam is te lang (max. 200 tekens).".into());
    }
    let ids = (
        opt_id(&f.client_id),
        opt_id(&f.project_id),
        opt_id(&f.tariff_id),
    );
    let (Ok(client_id), Ok(project_id), Ok(tariff_id)) = ids else {
        e.insert("form", "Ongeldige keuze in het formulier.".into());
        return Err(e);
    };
    if !e.is_empty() {
        return Err(e);
    }
    Ok(TaskData {
        name,
        client_id,
        project_id,
        tariff_id,
        billable: !checked(&f.internal),
    })
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/taken", routing::get(list_page).post(create))
        .route("/taken/nieuw", routing::get(new_page))
        .route("/taken/{id}", routing::get(edit_page).post(save))
        .route("/taken/{id}/archiveren", post(archive))
        .route("/taken/{id}/terugzetten", post(unarchive))
}

#[derive(Template, WebTemplate)]
#[template(path = "tasks/list.html")]
struct ListPage {
    l: Layout,
    tasks: Vec<TaskRow>,
    show_archived: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "tasks/form.html")]
struct FormPage {
    l: Layout,
    task: Option<Task>,
    f: TaskForm,
    errors: Errors,
    clients: Vec<ClientOption>,
    projects: Vec<ProjectOption>,
    tariffs: Vec<TariffOption>,
}

impl FormPage {
    async fn new(
        st: &AppState,
        company_id: i64,
        l: Layout,
        task: Option<Task>,
        f: TaskForm,
        errors: Errors,
    ) -> AppResult<Self> {
        Ok(FormPage {
            clients: clients::options(&st.pool, company_id).await?,
            projects: projects::options(&st.pool, company_id).await?,
            tariffs: tariffs::hour_options(&st.pool, company_id).await?,
            l,
            task,
            f,
            errors,
        })
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
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<ListQuery>,
) -> AppResult<ListPage> {
    let show_archived = q.archief.is_some();
    Ok(ListPage {
        l,
        tasks: list(&st.pool, company.id, show_archived).await?,
        show_archived,
    })
}

async fn new_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
) -> AppResult<FormPage> {
    FormPage::new(&st, company.id, l, None, TaskForm::default(), Errors::new()).await
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<TaskForm>,
) -> AppResult<Response> {
    let result = match validate_form(&f) {
        Ok(d) => insert(&st.pool, company.id, d, Some(user.id)).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(_) => {
            flash::ok(&session, &format!("Taak {} aangemaakt.", f.name.trim())).await?;
            Ok(redirect("/taken"))
        }
        Err(errors) => {
            let page = FormPage::new(&st, company.id, l, None, f, errors).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
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
    let f = TaskForm::from_task(&t);
    FormPage::new(&st, company.id, l, Some(t), f, Errors::new()).await
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<TaskForm>,
) -> AppResult<Response> {
    let t = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if t.archived_at.is_some() {
        return Err(AppError::BadRequest(
            "Een gearchiveerde taak kan niet worden gewijzigd.".into(),
        ));
    }
    let result = match validate_form(&f) {
        Ok(d) => update(&st.pool, company.id, id, d, user.id).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => {
            flash::ok(&session, &format!("Taak {} opgeslagen.", f.name.trim())).await?;
            Ok(redirect("/taken"))
        }
        Err(errors) => {
            let page = FormPage::new(&st, company.id, l, Some(t), f, errors).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
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
    flash::ok(&session, "Taak gearchiveerd.").await?;
    Ok(redirect("/taken"))
}

async fn unarchive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, company.id, id, false, user.id).await?;
    flash::ok(&session, "Taak teruggezet.").await?;
    Ok(redirect("/taken?archief=1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_validation() {
        let d = validate_form(&TaskForm {
            name: " Administratie ".into(),
            internal: Some("1".into()),
            tariff_id: "".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(d.name, "Administratie");
        assert!(!d.billable);
        assert!(validate_form(&TaskForm::default())
            .unwrap_err()
            .contains_key("name"));
        assert!(validate_form(&TaskForm {
            name: "X".into(),
            client_id: "abc".into(),
            ..Default::default()
        })
        .unwrap_err()
        .contains_key("form"));
    }
}
