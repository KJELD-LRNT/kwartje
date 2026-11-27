//! Invoices: drafts, issuing (number + PDF), sending, payments, reminders,
//! credit notes, overviews.

pub mod credit;
pub mod draft;
pub mod issue;
pub mod list;
pub mod model;
pub mod payments;
pub mod send;
pub mod show;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use super::companies::CurrentCompany;
use crate::web::layout::Page;
use crate::web::{AppResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/facturen", get(list::page).post(draft::create))
        .route("/facturen/ouderdom", get(list::ageing))
        .route("/facturen/nieuw", get(draft::new_page))
        .route("/facturen/{id}", get(show).post(draft::save))
        .route("/facturen/{id}/regels", post(draft::add_line))
        .route(
            "/facturen/{id}/regels/{line}/verwijderen",
            post(draft::remove_line),
        )
        .route("/facturen/{id}/verwijderen", post(draft::remove_draft))
        .route("/facturen/{id}/definitief", post(issue::issue_submit))
        .route("/facturen/{id}/pdf", get(issue::pdf))
        .route("/facturen/{id}/versturen", get(send::page).post(send::send))
        .route("/facturen/{id}/verzonden", post(send::mark_as_sent))
        .route("/facturen/{id}/betalingen", post(payments::add))
        .route(
            "/facturen/{id}/herinnering",
            get(credit::reminder_page).post(credit::reminder_send),
        )
        .route("/facturen/{id}/crediteren", post(credit::create_credit))
        .route(
            "/facturen/{id}/betalingen/{pid}/archiveren",
            post(payments::archive),
        )
}

async fn show(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let inv = {
        let mut conn = st.pool.acquire().await?;
        model::get(&mut conn, company.id, id).await?
    };
    if inv.is_draft() {
        Ok(draft::draft_page(&st, l, company.id, inv)
            .await?
            .into_response())
    } else {
        Ok(show::page(&st, l, company.id, inv).await?.into_response())
    }
}
