//! Trips (ritten): quick entry with odometer prefill and saved routes,
//! "heen en terug", month list, edit/archive. Odometer continuity is checked
//! per vehicle across all companies (`mileage::check`).

mod form;
mod model;
mod pages;

pub use form::*;
pub use model::*;
pub use pages::*;
