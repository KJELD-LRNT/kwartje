//! Export page and download routes: `/export/<table>.csv`, `/export/all.zip`.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tower_sessions::Session;

use super::companies;
use crate::export::{self, Filter, TABLES};
use crate::format;
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{AppError, AppResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/export", get(page))
        .route("/export/{file}", get(download))
}

#[derive(Debug, Default, Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    company: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

impl ExportQuery {
    fn filter(&self) -> Result<Filter, AppError> {
        let date = |s: &str, label: &str| -> Result<Option<time::Date>, AppError> {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                format::parse_date(s).map(Some).ok_or_else(|| {
                    AppError::BadRequest(format!("Ongeldige {label}: gebruik dd-mm-jjjj."))
                })
            }
        };
        let company_id = match self.company.trim() {
            "" => None,
            s => Some(
                s.parse::<i64>()
                    .map_err(|_| AppError::BadRequest("Ongeldig bedrijf.".into()))?,
            ),
        };
        Ok(Filter {
            company_id,
            from: date(&self.from, "begindatum")?,
            to: date(&self.to, "einddatum")?,
        })
    }

    fn query_string(&self) -> String {
        let pairs: Vec<(&str, &str)> = [
            ("company", self.company.trim()),
            ("from", self.from.trim()),
            ("to", self.to.trim()),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
        if pairs.is_empty() {
            String::new()
        } else {
            format!(
                "?{}",
                serde_urlencoded::to_string(pairs).unwrap_or_default()
            )
        }
    }
}

pub struct TableLink {
    pub label: &'static str,
    pub href: String,
    pub global: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "export/index.html")]
struct ExportPage {
    l: Layout,
    companies: Vec<companies::Company>,
    company: String,
    from: String,
    to: String,
    tables: Vec<TableLink>,
    zip_href: String,
}

/// A boekhouder always exports one of their own companies: the current one
/// unless they chose another of theirs.
async fn scoped(
    st: &AppState,
    user: &AuthUser,
    session: &Session,
    mut q: ExportQuery,
) -> AppResult<ExportQuery> {
    if user.is_accountant() {
        let mine = companies::visible(&st.pool, user).await?;
        let current = companies::resolve_current(session, &mine)
            .await?
            .map(|c| c.id)
            .unwrap_or(0);
        let chosen = q.company.trim().parse::<i64>().ok();
        let id = match chosen {
            Some(c) if mine.iter().any(|m| m.id == c) => c,
            Some(_) => return Err(AppError::NotFound),
            None => current,
        };
        if !mine.iter().any(|m| m.id == id) {
            return Err(AppError::NotFound);
        }
        q.company = id.to_string();
    }
    Ok(q)
}

async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    Query(q): Query<ExportQuery>,
) -> AppResult<ExportPage> {
    let q = scoped(&st, &user, &session, q).await?;
    q.filter()?; // validate early so the links are good
    let qs = q.query_string();
    let mut list = companies::list(&st.pool, true).await?;
    if user.is_accountant() {
        let mine = companies::visible(&st.pool, &user).await?;
        list.retain(|c| mine.iter().any(|m| m.id == c.id));
    }
    Ok(ExportPage {
        l,
        companies: list,
        company: q.company.trim().to_string(),
        from: q.from.trim().to_string(),
        to: q.to.trim().to_string(),
        tables: TABLES
            .iter()
            .filter(|t| !user.is_accountant() || export::accountant_may_export(t))
            .map(|t| TableLink {
                label: t.label,
                href: format!("/export/{}.csv{qs}", t.name),
                global: t.company_col.is_none(),
            })
            .collect(),
        zip_href: format!("/export/all.zip{qs}"),
    })
}

fn attachment(name: &str, mime: &'static str, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

async fn download(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    Path(file): Path<String>,
    Query(q): Query<ExportQuery>,
) -> AppResult<Response> {
    let q = scoped(&st, &user, &session, q).await?;
    let f = q.filter()?;
    let today = format::iso_date(format::today());
    let accountant = user.is_accountant();
    if file == "all.zip" {
        let bytes = export::zip_where(&st.pool, &f, |t| {
            !accountant || export::accountant_may_export(t)
        })
        .await?;
        return Ok(attachment(
            &format!("export-{today}.zip"),
            "application/zip",
            bytes,
        ));
    }
    let name = file.strip_suffix(".csv").ok_or(AppError::NotFound)?;
    let t = export::find(name)
        .filter(|t| !accountant || export::accountant_may_export(t))
        .ok_or(AppError::NotFound)?;
    let bytes = export::table_csv(&st.pool, t, &f).await?;
    Ok(attachment(
        &format!("{name}-{today}.csv"),
        "text/csv; charset=utf-8",
        bytes,
    ))
}
