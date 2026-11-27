//! Payments on issued invoices (full or partial, by hand). A mistaken payment
//! is archived, never deleted. For a credit note the amount is a refund.

use axum::extract::{Path, State};
use axum::response::Response;
use axum::Form;
use serde::{Deserialize, Serialize};
use tower_sessions::Session;

use super::model;
use crate::modules::companies::CurrentCompany;
use crate::web::error::redirect;
use crate::web::layout::AuthUser;
use crate::web::{flash, AppError, AppResult, AppState};
use crate::{audit, db, format};

pub const METHODS: &[(&str, &str)] = &[
    ("bank", "Bankoverschrijving"),
    ("contant", "Contant"),
    ("anders", "Anders"),
];

pub fn method_label(m: &str) -> &'static str {
    METHODS
        .iter()
        .find(|(k, _)| *k == m)
        .map(|(_, l)| *l)
        .unwrap_or("Anders")
}

#[derive(Deserialize)]
pub struct PaymentForm {
    #[serde(default)]
    date: String,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    note: String,
}

#[derive(Serialize)]
struct PaymentRow<'a> {
    invoice_id: i64,
    date: &'a str,
    amount_cents: i64,
    method: &'a str,
    note: &'a str,
}

pub async fn add(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<PaymentForm>,
) -> AppResult<Response> {
    let back = format!("/facturen/{id}");
    let mut tx = st.pool.begin().await?;
    let inv = model::get(&mut tx, company.id, id).await?;
    if inv.is_draft() {
        return Err(AppError::BadRequest(
            "Maak de factuur eerst definitief.".into(),
        ));
    }
    let Some(date) = format::parse_date(&f.date) else {
        drop(tx);
        flash::error(&session, "Vul een geldige betaaldatum in.").await?;
        return Ok(redirect(&back));
    };
    let Some(amount) = format::parse_amount(&f.amount)
        .map(i64::abs)
        .filter(|a| *a > 0)
    else {
        drop(tx);
        flash::error(&session, "Vul een bedrag groter dan € 0,00 in.").await?;
        return Ok(redirect(&back));
    };
    let amount = if inv.is_credit() { -amount } else { amount };
    let method = METHODS
        .iter()
        .find(|(k, _)| *k == f.method)
        .map(|(k, _)| *k)
        .unwrap_or("bank");
    let note = f.note.trim();
    let date_s = format::iso_date(date);
    let now = db::now();
    let pid = sqlx::query!(
        "INSERT INTO payments (invoice_id, date, amount_cents, method, note, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        date_s,
        amount,
        method,
        note,
        now,
        now,
        user.id
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    audit::record_insert(
        &mut tx,
        Some(user.id),
        "payments",
        pid,
        &PaymentRow {
            invoice_id: id,
            date: &date_s,
            amount_cents: amount,
            method,
            note,
        },
    )
    .await?;
    let paid = model::paid_cents(&mut tx, id).await?;
    tx.commit().await?;
    let over = if inv.total_cents >= 0 {
        paid > inv.total_cents
    } else {
        paid < inv.total_cents
    };
    if over {
        flash::error(
            &session,
            &format!(
                "Let op: er is {} betaald, meer dan het factuurbedrag van {}.",
                format::amount(paid.abs()),
                format::amount(inv.total_cents.abs())
            ),
        )
        .await?;
    } else {
        flash::ok(
            &session,
            &format!(
                "Betaling van {} geregistreerd.",
                format::amount(amount.abs())
            ),
        )
        .await?;
    }
    Ok(redirect(&back))
}

pub async fn archive(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path((id, pid)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let mut tx = st.pool.begin().await?;
    model::get(&mut tx, company.id, id).await?;
    let now = db::now();
    let n = sqlx::query!(
        "UPDATE payments SET archived_at = ?, updated_at = ? WHERE id = ? AND invoice_id = ? AND archived_at IS NULL",
        now,
        now,
        pid,
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(AppError::NotFound);
    }
    audit::record_archive(&mut tx, Some(user.id), "payments", pid, None, Some(&now)).await?;
    tx.commit().await?;
    flash::ok(&session, "Betaling verwijderd (bewaard in het archief).").await?;
    Ok(redirect(&format!("/facturen/{id}")))
}
