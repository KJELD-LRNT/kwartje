//! Vehicles: global (one car can drive for several companies). Trips are per
//! company; odometer continuity is per vehicle across companies.

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

use crate::web::error::redirect;
use crate::web::form::{self, is_unique_violation, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, validate};

pub const KINDS: &[(&str, &str)] = &[
    ("private", "Privéauto (kilometervergoeding)"),
    ("business", "Auto van de zaak"),
];

/// How trips of a private car are entered. Business cars always use readings.
pub const MODES: &[(&str, &str)] = &[
    ("km", "Aantal km per rit (beginstand niet nodig)"),
    ("odometer", "Begin- en eindstand van de kilometerteller"),
];

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Vehicle {
    pub id: i64,
    pub name: String,
    pub plate: String,
    pub kind: String,
    /// `km` (private cars only) or `odometer`.
    pub odometer_mode: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

impl Vehicle {
    pub fn kind_label(&self) -> &'static str {
        KINDS
            .iter()
            .find(|(k, _)| *k == self.kind)
            .map(|(_, l)| *l)
            .unwrap_or("")
    }
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.plate)
    }
    /// Trips are entered as a number of km; readings are optional.
    pub fn km_only(&self) -> bool {
        self.odometer_mode == "km"
    }
    pub fn mode_label(&self) -> &'static str {
        if self.km_only() {
            "Aantal km"
        } else {
            "Kilometerstanden"
        }
    }
}

pub async fn get(conn: &mut SqliteConnection, id: i64) -> AppResult<Vehicle> {
    sqlx::query_as!(
        Vehicle,
        r#"SELECT id AS "id!", name, plate, kind, odometer_mode, archived_at, created_at, updated_at, created_by
           FROM vehicles WHERE id = ?"#,
        id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn list(pool: &SqlitePool, include_archived: bool) -> AppResult<Vec<Vehicle>> {
    Ok(sqlx::query_as!(
        Vehicle,
        r#"SELECT id AS "id!", name, plate, kind, odometer_mode, archived_at, created_at, updated_at, created_by
           FROM vehicles WHERE ? OR archived_at IS NULL
           ORDER BY archived_at IS NOT NULL, name COLLATE NOCASE"#,
        include_archived
    )
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, Default)]
pub struct VehicleData {
    pub name: String,
    pub plate: String,
    pub kind: String,
    pub odometer_mode: String,
}

pub async fn insert(pool: &SqlitePool, d: &VehicleData, by: Option<i64>) -> AppResult<i64> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let id = sqlx::query!(
        "INSERT INTO vehicles (name, plate, kind, odometer_mode, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        d.name,
        d.plate,
        d.kind,
        d.odometer_mode,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(&mut tx, id).await?;
    audit::record_insert(&mut tx, by, "vehicles", id, &row).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(pool: &SqlitePool, id: i64, d: &VehicleData, by: i64) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, id).await?;
    sqlx::query!(
        "UPDATE vehicles SET name = ?, plate = ?, kind = ?, odometer_mode = ?, updated_at = ?
         WHERE id = ?",
        d.name,
        d.plate,
        d.kind,
        d.odometer_mode,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, id).await?;
    audit::record_update(&mut tx, Some(by), "vehicles", id, &old, &new).await?;
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
        "UPDATE vehicles SET archived_at = ?, updated_at = ? WHERE id = ?",
        archived_at,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record_archive(
        &mut tx,
        Some(by),
        "vehicles",
        id,
        old.archived_at.as_deref(),
        archived_at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VehicleForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub plate: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub odometer_mode: String,
}

pub fn validate_form(f: &VehicleForm) -> Result<VehicleData, Errors> {
    let mut e = Errors::new();
    let name = f.name.trim().to_string();
    if name.is_empty() {
        e.insert(
            "name",
            "Naam is verplicht, bijvoorbeeld Volkswagen Golf.".into(),
        );
    }
    let plate = match validate::plate(&f.plate) {
        Ok(p) => p,
        Err(m) => {
            e.insert("plate", m.into());
            String::new()
        }
    };
    if !KINDS.iter().any(|(k, _)| *k == f.kind) {
        e.insert("kind", "Kies het soort auto.".into());
    }
    // A business car always keeps a full log with readings.
    let odometer_mode = if f.kind == "business" {
        "odometer".to_string()
    } else if f.odometer_mode.is_empty() {
        "km".to_string()
    } else if MODES.iter().any(|(m, _)| *m == f.odometer_mode) {
        f.odometer_mode.clone()
    } else {
        e.insert("odometer_mode", "Kies hoe ritten worden ingevoerd.".into());
        String::new()
    };
    if e.is_empty() {
        Ok(VehicleData {
            name,
            plate,
            kind: f.kind.clone(),
            odometer_mode,
        })
    } else {
        Err(e)
    }
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/voertuigen", routing::get(list_page).post(create))
        .route("/voertuigen/nieuw", routing::get(new_page))
        .route("/voertuigen/{id}", routing::get(edit_page).post(save))
        .route("/voertuigen/{id}/archiveren", post(archive))
        .route("/voertuigen/{id}/terugzetten", post(unarchive))
}

#[derive(Template, WebTemplate)]
#[template(path = "vehicles/list.html")]
struct ListPage {
    l: Layout,
    vehicles: Vec<Vehicle>,
    show_archived: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "vehicles/form.html")]
struct FormPage {
    l: Layout,
    vehicle: Option<Vehicle>,
    f: VehicleForm,
    errors: Errors,
    kinds: &'static [(&'static str, &'static str)],
    modes: &'static [(&'static str, &'static str)],
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
    Query(q): Query<ListQuery>,
) -> AppResult<ListPage> {
    let show_archived = q.archief.is_some();
    Ok(ListPage {
        l,
        vehicles: list(&st.pool, show_archived).await?,
        show_archived,
    })
}

async fn new_page(Page(l): Page) -> FormPage {
    FormPage {
        l,
        vehicle: None,
        f: VehicleForm {
            kind: "private".into(),
            odometer_mode: "km".into(),
            ..Default::default()
        },
        errors: Errors::new(),
        kinds: KINDS,
        modes: MODES,
    }
}

fn duplicate_plate() -> Errors {
    let mut e = Errors::new();
    e.insert("plate", "Er is al een voertuig met dit kenteken.".into());
    e
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    Form(f): Form<VehicleForm>,
) -> AppResult<Response> {
    let result = match validate_form(&f) {
        Ok(d) => match insert(&st.pool, &d, Some(user.id)).await {
            Ok(_) => Ok(d),
            Err(e) if is_unique_violation(&e) => Err(duplicate_plate()),
            Err(e) => return Err(e),
        },
        Err(e) => Err(e),
    };
    match result {
        Ok(d) => {
            flash::ok(&session, &format!("Voertuig {} toegevoegd.", d.plate)).await?;
            Ok(redirect("/voertuigen"))
        }
        Err(errors) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            FormPage {
                l,
                vehicle: None,
                f,
                errors,
                kinds: KINDS,
                modes: MODES,
            },
        )
            .into_response()),
    }
}

async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    Path(id): Path<i64>,
) -> AppResult<FormPage> {
    let mut conn = st.pool.acquire().await?;
    let v = get(&mut conn, id).await?;
    Ok(FormPage {
        l,
        f: VehicleForm {
            name: v.name.clone(),
            plate: v.plate.clone(),
            kind: v.kind.clone(),
            odometer_mode: v.odometer_mode.clone(),
        },
        vehicle: Some(v),
        errors: Errors::new(),
        kinds: KINDS,
        modes: MODES,
    })
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<VehicleForm>,
) -> AppResult<Response> {
    let v = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, id).await?
    };
    let result = match validate_form(&f) {
        Ok(d) => match update(&st.pool, id, &d, user.id).await {
            Ok(()) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(duplicate_plate()),
            Err(e) => return Err(e),
        },
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => {
            flash::ok(&session, "Voertuig opgeslagen.").await?;
            Ok(redirect("/voertuigen"))
        }
        Err(errors) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            FormPage {
                l,
                vehicle: Some(v),
                f,
                errors,
                kinds: KINDS,
                modes: MODES,
            },
        )
            .into_response()),
    }
}

async fn archive(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, id, true, user.id).await?;
    flash::ok(
        &session,
        "Voertuig gearchiveerd. De ritten blijven bewaard.",
    )
    .await?;
    Ok(redirect("/voertuigen"))
}

async fn unarchive(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st.pool, id, false, user.id).await?;
    flash::ok(&session, "Voertuig teruggezet.").await?;
    Ok(redirect("/voertuigen?archief=1"))
}
