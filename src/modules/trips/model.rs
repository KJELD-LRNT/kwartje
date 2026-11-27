//! Trips: the rows and queries, the insert/update/archive writes (with odometer continuity) and the year overview per vehicle.

use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};

use crate::mileage::{self, Leg};
use crate::modules::routes;
use crate::modules::vehicles::{self, Vehicle};
use crate::web::{AppError, AppResult};
use crate::{audit, db, format};

use super::form::*;

pub const MAX_KM: i64 = 2000;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Trip {
    pub id: i64,
    pub company_id: i64,
    pub vehicle_id: i64,
    pub user_id: i64,
    pub date: String,
    pub from_address: String,
    pub to_address: String,
    pub purpose: String,
    pub client_id: Option<i64>,
    pub odometer_start: Option<i64>,
    pub odometer_end: Option<i64>,
    pub km: i64,
    pub business: bool,
    pub tariff_id: Option<i64>,
    pub price_cents: Option<i64>,
    pub vat_bp: Option<i64>,
    pub route_id: Option<i64>,
    pub invoice_line_id: Option<i64>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TripRow {
    pub id: i64,
    pub date: String,
    pub vehicle_id: i64,
    pub vehicle_name: String,
    pub plate: String,
    /// The car is entered as "Aantal km" (no unregistered-km hints).
    pub vehicle_km_only: bool,
    pub user_name: String,
    pub from_address: String,
    pub to_address: String,
    pub purpose: String,
    pub client_name: Option<String>,
    pub odometer_start: Option<i64>,
    pub odometer_end: Option<i64>,
    pub km: i64,
    pub business: bool,
    pub tariff_name: Option<String>,
    pub price_cents: Option<i64>,
    pub invoice_line_id: Option<i64>,
}

impl TripRow {
    /// "48283 – 48295", or empty for a trip without readings.
    pub fn readings(&self) -> String {
        match (self.odometer_start, self.odometer_end) {
            (Some(a), Some(b)) => format!("{a} – {b}"),
            _ => String::new(),
        }
    }

    pub fn billing_label(&self) -> String {
        match (&self.tariff_name, self.price_cents) {
            (Some(n), Some(p)) => format!("{n} · {}/km", format::amount(p)),
            _ => String::new(),
        }
    }
}

pub(super) const LOCKED: &str = "Deze rit is gefactureerd en kan niet meer worden gewijzigd.";

// ---------- queries ----------

pub async fn get(conn: &mut SqliteConnection, company_id: i64, id: i64) -> AppResult<Trip> {
    sqlx::query_as!(
        Trip,
        r#"SELECT id AS "id!", company_id, vehicle_id, user_id, date, from_address, to_address,
                  purpose, client_id, odometer_start, odometer_end, km,
                  business AS "business: bool", tariff_id, price_cents, vat_bp, route_id,
                  invoice_line_id, archived_at, created_at, updated_at, created_by
           FROM mileage_entries WHERE id = ? AND company_id = ?"#,
        id,
        company_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

/// All non-archived trips of a vehicle, **all companies** (for continuity
/// checks only; never shown in detail).
pub async fn legs(conn: &mut SqliteConnection, vehicle_id: i64) -> AppResult<Vec<Leg>> {
    let rows = sqlx::query!(
        r#"SELECT id AS "id!", company_id, date, odometer_start, odometer_end, km,
                  business AS "business: bool"
           FROM mileage_entries WHERE vehicle_id = ? AND archived_at IS NULL
           ORDER BY odometer_start"#,
        vehicle_id
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Leg {
            id: r.id,
            company_id: r.company_id,
            date: r.date,
            start: r.odometer_start,
            end: r.odometer_end,
            km: r.km,
            business: r.business,
        })
        .collect())
}

pub async fn rows_between(
    pool: &SqlitePool,
    company_id: i64,
    vehicle_id: Option<i64>,
    from: &str,
    to: &str,
) -> AppResult<Vec<TripRow>> {
    Ok(sqlx::query_as!(
        TripRow,
        r#"SELECT m.id AS "id!", m.date, m.vehicle_id, v.name AS vehicle_name, v.plate,
                  v.odometer_mode = 'km' AS "vehicle_km_only!: bool",
                  u.name AS user_name, m.from_address, m.to_address, m.purpose,
                  c.name AS "client_name?", m.odometer_start, m.odometer_end, m.km,
                  m.business AS "business: bool", t.name AS "tariff_name?", m.price_cents,
                  m.invoice_line_id
           FROM mileage_entries m
           JOIN vehicles v ON v.id = m.vehicle_id
           JOIN users u ON u.id = m.user_id
           LEFT JOIN clients c ON c.id = m.client_id
           LEFT JOIN tariffs t ON t.id = m.tariff_id
           WHERE m.company_id = ? AND m.archived_at IS NULL AND m.date BETWEEN ? AND ?
             AND (? IS NULL OR m.vehicle_id = ?)
           ORDER BY m.date, m.odometer_start"#,
        company_id,
        from,
        to,
        vehicle_id,
        vehicle_id
    )
    .fetch_all(pool)
    .await?)
}

/// One leg of a trip to insert: (from, to, odometer readings if any, km).
pub(super) type LegSpec<'a> = (&'a str, &'a str, Option<(i64, i64)>, i64);

pub(super) async fn insert_one(
    tx: &mut SqliteConnection,
    company_id: i64,
    p: &Prepared,
    (from, to, readings, km): LegSpec<'_>,
    by: Option<i64>,
) -> AppResult<i64> {
    let now = db::now();
    let (start, end) = (readings.map(|r| r.0), readings.map(|r| r.1));
    let (tariff_id, price, vat) = match p.tariff {
        Some((t, pr, v)) => (Some(t), Some(pr), Some(v)),
        None => (None, None, None),
    };
    let id = sqlx::query!(
        "INSERT INTO mileage_entries (company_id, vehicle_id, user_id, date, from_address,
                 to_address, purpose, client_id, odometer_start, odometer_end, km, business,
                 tariff_id, price_cents, vat_bp, route_id, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        company_id,
        p.vehicle_id,
        p.user_id,
        p.date,
        from,
        to,
        p.purpose,
        p.client_id,
        start,
        end,
        km,
        p.business,
        tariff_id,
        price,
        vat,
        p.route_id,
        now,
        now,
        by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let row = get(tx, company_id, id).await?;
    audit::record_insert(tx, by, "mileage_entries", id, &row).await?;
    Ok(id)
}

/// Save one trip (or two for "heen en terug") in one transaction, plus the
/// new favourite when "bewaar als favoriet" was ticked; the continuity check
/// is repeated inside the transaction.
pub async fn insert(
    pool: &SqlitePool,
    company_id: i64,
    p: &Prepared,
    by: Option<i64>,
) -> AppResult<Vec<i64>> {
    let mut tx = pool.begin().await?;
    let with_route;
    let p = match &p.favorite {
        Some(d) => {
            let route_id = routes::insert_in(&mut tx, company_id, d, by).await?;
            with_route = Prepared {
                route_id: Some(route_id),
                ..p.clone()
            };
            &with_route
        }
        None => p,
    };
    let others = legs(&mut tx, p.vehicle_id).await?;
    if let Some((s, e)) = p.readings {
        if let Some(c) = mileage::check(company_id, &p.date, s, e, None, &others) {
            return Err(AppError::BadRequest(c.message()));
        }
    }
    let mut ids = vec![
        insert_one(
            &mut tx,
            company_id,
            p,
            (&p.from_address, &p.to_address, p.readings, p.km),
            by,
        )
        .await?,
    ];
    if p.return_trip {
        let back = p.readings.map(|(_, e)| (e, e + p.km));
        if let Some((s, e)) = back {
            let others = legs(&mut tx, p.vehicle_id).await?;
            if let Some(c) = mileage::check(company_id, &p.date, s, e, None, &others) {
                return Err(AppError::BadRequest(format!("Terugrit: {}", c.message())));
            }
        }
        ids.push(
            insert_one(
                &mut tx,
                company_id,
                p,
                (&p.to_address, &p.from_address, back, p.km),
                by,
            )
            .await?,
        );
    }
    tx.commit().await?;
    Ok(ids)
}

pub async fn update(
    pool: &SqlitePool,
    company_id: i64,
    id: i64,
    p: &Prepared,
    by: i64,
) -> AppResult<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.invoice_line_id.is_some() {
        return Err(AppError::BadRequest(LOCKED.into()));
    }
    let others = legs(&mut tx, p.vehicle_id).await?;
    if let Some((s, e)) = p.readings {
        if let Some(c) = mileage::check(company_id, &p.date, s, e, Some(id), &others) {
            return Err(AppError::BadRequest(c.message()));
        }
    }
    let km = p.km;
    let (start, end) = (p.readings.map(|r| r.0), p.readings.map(|r| r.1));
    let (tariff_id, price, vat) = match p.tariff {
        Some((t, pr, v)) => (Some(t), Some(pr), Some(v)),
        None => (None, None, None),
    };
    sqlx::query!(
        "UPDATE mileage_entries SET vehicle_id = ?, user_id = ?, date = ?, from_address = ?,
                to_address = ?, purpose = ?, client_id = ?, odometer_start = ?, odometer_end = ?,
                km = ?, business = ?, tariff_id = ?, price_cents = ?, vat_bp = ?, route_id = ?,
                updated_at = ?
         WHERE id = ? AND company_id = ? AND invoice_line_id IS NULL",
        p.vehicle_id,
        p.user_id,
        p.date,
        p.from_address,
        p.to_address,
        p.purpose,
        p.client_id,
        start,
        end,
        km,
        p.business,
        tariff_id,
        price,
        vat,
        p.route_id,
        now,
        id,
        company_id
    )
    .execute(&mut *tx)
    .await?;
    let new = get(&mut tx, company_id, id).await?;
    audit::record_update(&mut tx, Some(by), "mileage_entries", id, &old, &new).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn archive(pool: &SqlitePool, company_id: i64, id: i64, by: i64) -> AppResult<Trip> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    let old = get(&mut tx, company_id, id).await?;
    if old.invoice_line_id.is_some() {
        return Err(AppError::BadRequest(LOCKED.into()));
    }
    if old.archived_at.is_none() {
        sqlx::query!(
            "UPDATE mileage_entries SET archived_at = ?, updated_at = ? WHERE id = ? AND company_id = ?",
            now,
            now,
            id,
            company_id
        )
        .execute(&mut *tx)
        .await?;
        audit::record_archive(&mut tx, Some(by), "mileage_entries", id, None, Some(&now)).await?;
    }
    tx.commit().await?;
    Ok(old)
}

// ---------- year overview ----------

pub struct VehicleYear {
    pub vehicle: Vehicle,
    pub balance: mileage::Balance,
    pub months: Vec<(&'static str, i64)>,
    pub rate_cents: Option<i64>,
    pub allowance_cents: Option<i64>,
}

impl VehicleYear {
    pub fn is_private_car(&self) -> bool {
        self.vehicle.kind == "private"
    }
    /// Company car: more than 500 private km → bijtelling risk.
    pub fn over_500(&self) -> bool {
        !self.is_private_car() && self.balance.private_total() > 500
    }
}

pub(super) const MONTH_LABELS: [&str; 12] = [
    "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
];

/// Balance per vehicle that has trips (any company) in `year`.
pub async fn year_overview(
    pool: &SqlitePool,
    company_id: i64,
    year: i32,
    vehicle_filter: Option<i64>,
) -> AppResult<Vec<VehicleYear>> {
    let rate = crate::modules::mileage_rates::for_year(pool, year).await?;
    let prefix = format!("{year:04}-");
    let mut out = Vec::new();
    for v in vehicles::list(pool, true).await? {
        if vehicle_filter.is_some_and(|f| f != v.id) {
            continue;
        }
        let all = {
            let mut conn = pool.acquire().await?;
            legs(&mut conn, v.id).await?
        };
        let in_year: Vec<Leg> = all
            .into_iter()
            .filter(|l| l.date.starts_with(&prefix))
            .collect();
        if in_year.is_empty() {
            continue;
        }
        let balance = mileage::year_balance(company_id, &in_year);
        let allowance_cents = if v.kind == "private" {
            balance.allowance_cents(rate)
        } else {
            None
        };
        let months = MONTH_LABELS
            .iter()
            .zip(balance.business_by_month)
            .map(|(m, km)| (*m, km))
            .collect();
        out.push(VehicleYear {
            vehicle: v,
            balance,
            months,
            rate_cents: rate,
            allowance_cents,
        });
    }
    Ok(out)
}

/// Business km of this company in a date range (dashboard).
pub async fn business_km_between(
    pool: &SqlitePool,
    company_id: i64,
    from: &str,
    to: &str,
) -> AppResult<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(km), 0) AS "km!: i64" FROM mileage_entries
           WHERE company_id = ? AND business = 1 AND archived_at IS NULL AND date BETWEEN ? AND ?"#,
        company_id,
        from,
        to
    )
    .fetch_one(pool)
    .await?)
}
