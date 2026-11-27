//! Favoriete ritten: saved routes per company for one-tap trip entry, either
//! one way (enkele reis) or a round trip (heen en terug).

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
use crate::web::error::redirect;
use crate::web::form::{self, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Route {
    pub id: i64,
    pub company_id: i64,
    pub name: String,
    pub from_address: String,
    pub to_address: String,
    /// Distance of one leg.
    pub km: i64,
    /// Heen en terug: the trip form saves the return leg too.
    pub round_trip: bool,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Route> {
    sqlx::query_as!(
        Route,
        r#"SELECT id AS "id!", company_id, name, from_address, to_address, km,
                  round_trip AS "round_trip: bool", archived_at,
                  created_at, updated_at, created_by
           FROM saved_routes WHERE id = ? AND company_id = ?"#,
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
) -> AppResult<Vec<Route>> {
    Ok(sqlx::query_as!(
        Route,
        r#"SELECT id AS "id!", company_id, name, from_address, to_address, km,
                  round_trip AS "round_trip: bool", archived_at,
                  created_at, updated_at, created_by
           FROM saved_routes WHERE company_id = ? AND (? OR archived_at IS NULL)
           ORDER BY archived_at IS NOT NULL, name COLLATE NOCASE"#,
        company_id,
        include_archived
    )
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, Default)]
pub struct RouteData {
    pub name: String,
    pub from_address: String,
    pub to_address: String,
    pub km: i64,
    pub round_trip: bool,
}

impl Route {
    /// "enkele reis" or "heen en terug", for lists and the chooser.
    pub fn kind_label(&self) -> &'static str {
        if self.round_trip {
            "heen en terug"
        } else {
            "enkele reis"
        }
    }
}

/// An active favourite with this name already exists (names are how people
/// pick one, so they stay unique per company).
pub async fn name_taken(
    conn: &mut SqliteConnection,
    company_id: i64,
    name: &str,
    except: Option<i64>,
) -> AppResult<bool> {
    let except = except.unwrap_or(0);
    let n: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM saved_routes
         WHERE company_id = ? AND archived_at IS NULL AND name = ? COLLATE NOCASE AND id != ?",
        company_id,
        name,
        except
    )
    .fetch_one(conn)
    .await?;
    Ok(n > 0)
}

/// Insert inside a caller's transaction (the trip form saves a new favourite
/// together with the trip).
pub async fn insert_in(
    tx: &mut SqliteConnection,
    company_id: i64,
    d: &RouteData,
    by: Option<i64>,
) -> AppResult<i64> {
    let now = db::now();
    let id = sqlx::query!(
        "INSERT INTO saved_routes (company_id, name, from_address, to_address, km, round_trip,
                                   created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        d.name,
        d.from_address,
        d.to_address,
        d.km,
        d.round_trip,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(tx, company_id, id).await?;
    audit::record_insert(tx, by, "saved_routes", id, &row).await?;
    Ok(id)
}

pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    d: &RouteData,
    by: Option<i64>,
) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    let id = insert_in(&mut tx, company_id, d, by).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    d: &RouteData,
    by: i64,
) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    sqlx::query!(
        "UPDATE saved_routes SET name = ?, from_address = ?, to_address = ?, km = ?, round_trip = ?,
                                 updated_at = ?
         WHERE id = ? AND company_id = ?",
        d.name,
        d.from_address,
        d.to_address,
        d.km,
        d.round_trip,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "saved_routes", id, &old, &new).await?;
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
        "UPDATE saved_routes SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
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
        "saved_routes",
        id,
        old.archived_at.as_deref(),
        archived_at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RouteForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub from_address: String,
    #[serde(default)]
    pub to_address: String,
    #[serde(default)]
    pub km: String,
    /// "enkel" or "retour".
    #[serde(default)]
    pub kind: String,
}

impl RouteForm {
    pub fn is_round_trip(&self) -> bool {
        self.kind == "retour"
    }
}

pub fn validate_form(f: &RouteForm) -> Result<RouteData, Errors> {
    let mut e = Errors::new();
    let d = RouteData {
        name: f.name.trim().to_string(),
        from_address: f.from_address.trim().to_string(),
        to_address: f.to_address.trim().to_string(),
        km: f.km.trim().parse().unwrap_or(0),
        round_trip: f.is_round_trip(),
    };
    if d.name.is_empty() {
        e.insert(
            "name",
            "Naam is verplicht, bijvoorbeeld Kantoor → stadion.".into(),
        );
    }
    if d.from_address.is_empty() {
        e.insert("from_address", "Vul het vertrekadres in.".into());
    }
    if d.to_address.is_empty() {
        e.insert("to_address", "Vul het bestemmingsadres in.".into());
    }
    if !matches!(f.kind.as_str(), "" | "enkel" | "retour") {
        e.insert("kind", "Kies enkele reis of heen en terug.".into());
    }
    if !(1..=2000).contains(&d.km) {
        e.insert(
            "km",
            "Vul de afstand in hele kilometers in (1 tot 2.000).".into(),
        );
    }
    if e.is_empty() {
        Ok(d)
    } else {
        Err(e)
    }
}

pub const NAME_TAKEN: &str = "Er is al een favoriete rit met deze naam.";

/// `validate_form` plus the unique-name check.
async fn validated(
    pool: &SqlitePool,
    company_id: i64,
    f: &RouteForm,
    except: Option<i64>,
) -> AppResult<Result<RouteData, Errors>> {
    let d = validate_form(f);
    let mut conn = pool.acquire().await?;
    Ok(match d {
        Ok(d) if name_taken(&mut conn, company_id, &d.name, except).await? => {
            let mut e = Errors::new();
            e.insert("name", NAME_TAKEN.into());
            Err(e)
        }
        other => other,
    })
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/routes", routing::get(list_page).post(create))
        .route("/routes/nieuw", routing::get(new_page))
        .route("/routes/{id}", routing::get(edit_page).post(save))
        .route("/routes/{id}/archiveren", post(archive))
        .route("/routes/{id}/terugzetten", post(unarchive))
}

#[derive(Template, WebTemplate)]
#[template(path = "routes/list.html")]
struct ListPage {
    l: Layout,
    routes: Vec<Route>,
    show_archived: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "routes/form.html")]
struct FormPage {
    l: Layout,
    route: Option<Route>,
    f: RouteForm,
    errors: Errors,
}

impl FormPage {
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
        routes: list(&st.pool, company.id, show_archived).await?,
        show_archived,
    })
}

async fn new_page(Page(l): Page, _c: CurrentCompany) -> FormPage {
    FormPage {
        l,
        route: None,
        f: RouteForm::default(),
        errors: Errors::new(),
    }
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<RouteForm>,
) -> AppResult<Response> {
    match validated(&st.pool, company.id, &f, None).await? {
        Ok(d) => {
            insert(&st.pool, company.id, &d, Some(user.id)).await?;
            flash::ok(&session, &format!("Favoriete rit {} opgeslagen.", d.name)).await?;
            Ok(redirect("/routes"))
        }
        Err(errors) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            FormPage {
                l,
                route: None,
                f,
                errors,
            },
        )
            .into_response()),
    }
}

async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<FormPage> {
    let mut conn = st.pool.acquire().await?;
    let r = get(&mut conn, company.id, id).await?;
    Ok(FormPage {
        l,
        f: RouteForm {
            name: r.name.clone(),
            from_address: r.from_address.clone(),
            to_address: r.to_address.clone(),
            km: r.km.to_string(),
            kind: if r.round_trip { "retour" } else { "enkel" }.into(),
        },
        route: Some(r),
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
    Form(f): Form<RouteForm>,
) -> AppResult<Response> {
    let r = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    match validated(&st.pool, company.id, &f, Some(id)).await? {
        Ok(d) => {
            update(&st.pool, company.id, id, &d, user.id).await?;
            flash::ok(&session, &format!("Favoriete rit {} opgeslagen.", d.name)).await?;
            Ok(redirect("/routes"))
        }
        Err(errors) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            FormPage {
                l,
                route: Some(r),
                f,
                errors,
            },
        )
            .into_response()),
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
    flash::ok(&session, "Favoriete rit gearchiveerd.").await?;
    Ok(redirect("/routes"))
}

async fn unarchive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, company.id, id, false, user.id).await?;
    flash::ok(&session, "Favoriete rit teruggezet.").await?;
    Ok(redirect("/routes?archief=1"))
}
