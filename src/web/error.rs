//! Application errors rendered as Dutch error pages.

use askama::Template;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("niet ingelogd")]
    LoginRequired,
    #[error("niet gevonden")]
    NotFound,
    #[error("nog geen bedrijf")]
    NoCompany,
    #[error("geen toegang")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(e.into())
    }
}

impl From<tower_sessions::session::Error> for AppError {
    fn from(e: tower_sessions::session::Error) -> Self {
        AppError::Internal(anyhow::anyhow!("session: {e}"))
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> Self {
        AppError::Internal(anyhow::anyhow!("template: {e}"))
    }
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub title: &'static str,
    pub message: String,
    pub version: &'static str,
}

pub fn error_page(status: StatusCode, title: &'static str, message: String) -> Response {
    let page = ErrorPage {
        title,
        message,
        version: env!("CARGO_PKG_VERSION"),
    };
    match page.render() {
        Ok(html) => (status, [("content-type", "text/html; charset=utf-8")], html).into_response(),
        Err(_) => (status, title).into_response(),
    }
}

/// Redirect that also works for htmx requests (HX-Redirect).
pub fn redirect(to: &str) -> Response {
    let mut resp = Redirect::to(to).into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        resp.headers_mut().insert("HX-Redirect", v);
    }
    resp
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::LoginRequired => redirect("/inloggen"),
            AppError::NoCompany => redirect("/bedrijven/nieuw"),
            AppError::NotFound => error_page(
                StatusCode::NOT_FOUND,
                "Niet gevonden",
                "Deze pagina bestaat niet (meer).".into(),
            ),
            AppError::Forbidden => error_page(
                StatusCode::FORBIDDEN,
                "Geen toegang",
                "Dit verzoek is geweigerd. Laad de pagina opnieuw en probeer het nog eens.".into(),
            ),
            AppError::BadRequest(msg) => {
                error_page(StatusCode::BAD_REQUEST, "Ongeldig verzoek", msg)
            }
            AppError::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                error_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Er ging iets mis",
                    "Er is een onverwachte fout opgetreden. De fout is gelogd.".into(),
                )
            }
        }
    }
}

pub async fn not_found() -> AppError {
    AppError::NotFound
}

/// True for requests made by htmx.
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("HX-Request").is_some()
}
