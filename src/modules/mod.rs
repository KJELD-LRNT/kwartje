//! Business modules. Phase 1: dashboard, companies, audit log viewer.

pub mod account;
pub mod audit_view;
pub mod btw;
pub mod clients;
pub mod companies;
pub mod dashboard;
pub mod documents;
pub mod export_view;
pub mod hours;
pub mod invoices;
pub mod mileage_rates;
pub mod projects;
pub mod quotes;
pub mod recurring;
pub mod routes;
pub mod security_view;
pub mod tariffs;
pub mod tasks;
pub mod trips;
pub mod unbilled;
pub mod users_admin;
pub mod vehicles;

use axum::Router;

use crate::web::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(dashboard::router())
        .merge(account::router())
        .merge(security_view::router())
        .merge(users_admin::router())
        .merge(companies::router())
        .merge(audit_view::router())
        .merge(export_view::router())
        .merge(clients::router())
        .merge(projects::router())
        .merge(tariffs::router())
        .merge(tasks::router())
        .merge(hours::router())
        .merge(unbilled::router())
        .merge(vehicles::router())
        .merge(mileage_rates::router())
        .merge(routes::router())
        .merge(trips::router())
        .merge(invoices::router())
        .merge(documents::router())
        .merge(btw::router())
        .merge(quotes::router())
        .merge(recurring::router())
}
