//! Tax-free km allowance per year (e.g. € 0,23), entered by hand. Global.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tower_sessions::Session;

use crate::web::error::redirect;
use crate::web::filters;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppResult, AppState};
use crate::{audit, db, format};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Rate {
    pub id: i64,
    pub year: i64,
    pub rate_cents_per_km: i64,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Rate>> {
    Ok(sqlx::query_as!(
        Rate,
        r#"SELECT id AS "id!", year, rate_cents_per_km, created_at, updated_at, created_by
           FROM mileage_rates ORDER BY year DESC"#
    )
    .fetch_all(pool)
    .await?)
}

pub async fn for_year(pool: &SqlitePool, year: i32) -> AppResult<Option<i64>> {
    let y = i64::from(year);
    Ok(sqlx::query_scalar!(
        "SELECT rate_cents_per_km FROM mileage_rates WHERE year = ?",
        y
    )
    .fetch_optional(pool)
    .await?)
}

/// Insert or update the rate for a year (audited).
pub async fn set(pool: &SqlitePool, year: i64, cents: i64, by: Option<i64>) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = sqlx::query_as!(
        Rate,
        r#"SELECT id AS "id!", year, rate_cents_per_km, created_at, updated_at, created_by
           FROM mileage_rates WHERE year = ?"#,
        year
    )
    .fetch_optional(&mut *tx)
    .await?;
    match old {
        Some(old) => {
            sqlx::query!(
                "UPDATE mileage_rates SET rate_cents_per_km = ?, updated_at = ? WHERE id = ?",
                cents,
                now,
                old.id
            )
            .execute(&mut *tx)
            .await?;
            let new = Rate {
                rate_cents_per_km: cents,
                updated_at: now,
                ..old.clone()
            };
            audit::record_update(&mut tx, by, "mileage_rates", old.id, &old, &new).await?;
        }
        None => {
            let id = sqlx::query!(
                "INSERT INTO mileage_rates (year, rate_cents_per_km, created_at, updated_at, created_by)
                 VALUES (?, ?, ?, ?, ?)",
                year,
                cents,
                now,
                now,
                by
            )
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
            let row = Rate {
                id,
                year,
                rate_cents_per_km: cents,
                created_at: now.clone(),
                updated_at: now,
                created_by: by,
            };
            audit::record_insert(&mut tx, by, "mileage_rates", id, &row).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new().route("/kilometervergoeding", get(page).post(submit))
}

#[derive(Template, WebTemplate)]
#[template(path = "vehicles/rates.html")]
struct RatesPage {
    l: Layout,
    rates: Vec<Rate>,
    year: String,
    rate: String,
    error: Option<String>,
}

async fn page(State(st): State<AppState>, Page(l): Page) -> AppResult<RatesPage> {
    Ok(RatesPage {
        l,
        rates: list(&st.pool).await?,
        year: format::today().year().to_string(),
        rate: String::new(),
        error: None,
    })
}

#[derive(Deserialize)]
struct RateForm {
    #[serde(default)]
    year: String,
    #[serde(default)]
    rate: String,
}

async fn submit(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    Form(f): Form<RateForm>,
) -> AppResult<Response> {
    let year = f
        .year
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|y| (2000..=2100).contains(y));
    let cents = format::parse_amount(&f.rate).filter(|c| (0..=1000).contains(c));
    match (year, cents) {
        (Some(y), Some(c)) => {
            set(&st.pool, y, c, Some(user.id)).await?;
            flash::ok(
                &session,
                &format!("Kilometervergoeding {y}: {} per km.", format::amount(c)),
            )
            .await?;
            Ok(redirect("/kilometervergoeding"))
        }
        _ => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            RatesPage {
                l,
                rates: list(&st.pool).await?,
                year: f.year,
                rate: f.rate,
                error: Some(
                    "Vul een jaar en een bedrag per km in, bijvoorbeeld 2026 en 0,23.".into(),
                ),
            },
        )
            .into_response()),
    }
}
