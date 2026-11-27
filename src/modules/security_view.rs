//! "Beveiliging": the security log for the beheerder (sign-ins and other
//! security events, with IP, route and browser). Admin only: the access
//! middleware answers 404 to everyone else.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::export::spreadsheet_text;
use crate::format;
use crate::security_log::{self, Event, Filter, Row};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{AppError, AppResult, AppState};

const LIMIT: i64 = 200;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/beveiliging", get(page))
        .route("/beveiliging.csv", get(csv_download))
}

#[derive(Debug, Default, Deserialize)]
struct Q {
    #[serde(default)]
    gebruiker: String,
    #[serde(default)]
    gebeurtenis: String,
    #[serde(default)]
    van: String,
    #[serde(default)]
    tot: String,
    #[serde(default)]
    mislukt: String,
}

impl Q {
    fn filter(&self) -> AppResult<Filter> {
        let date = |s: &str| -> AppResult<Option<String>> {
            if s.trim().is_empty() {
                return Ok(None);
            }
            format::parse_date(s)
                .map(|d| Some(format::iso_date(d)))
                .ok_or_else(|| AppError::BadRequest("Ongeldige datum: gebruik dd-mm-jjjj.".into()))
        };
        Ok(Filter {
            user_id: self.gebruiker.trim().parse().ok(),
            event: Event::from_db(self.gebeurtenis.trim()).map(|e| e.as_str().to_string()),
            from: date(&self.van)?,
            to: date(&self.tot)?,
            only_warnings: !self.mislukt.is_empty(),
        })
    }

    fn query_string(&self) -> String {
        let pairs: Vec<(&str, &str)> = [
            ("gebruiker", self.gebruiker.trim()),
            ("gebeurtenis", self.gebeurtenis.trim()),
            ("van", self.van.trim()),
            ("tot", self.tot.trim()),
            ("mislukt", self.mislukt.trim()),
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

#[derive(Template, WebTemplate)]
#[template(path = "security.html")]
struct SecurityPage {
    l: Layout,
    rows: Vec<Row>,
    users: Vec<(i64, String)>,
    events: Vec<(&'static str, &'static str)>,
    q: Q,
    csv_href: String,
    summary: security_log::Summary,
    last_logins: Vec<(String, String)>,
    limit: i64,
}

async fn all_users(st: &AppState) -> AppResult<Vec<(i64, String)>> {
    Ok(sqlx::query_as::<_, (i64, String)>(
        "SELECT id, name FROM users ORDER BY archived_at IS NOT NULL, name COLLATE NOCASE",
    )
    .fetch_all(&st.pool)
    .await?)
}

async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    _user: AuthUser,
    Query(q): Query<Q>,
) -> AppResult<SecurityPage> {
    let f = q.filter()?;
    let rows = security_log::query(&st.pool, &f, LIMIT).await?;
    let last_logins: Vec<(String, String)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT name, last_login_at FROM users WHERE archived_at IS NULL ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(&st.pool)
    .await?
    .into_iter()
    .map(|(n, at)| (n, at.as_deref().map(format::datetime).unwrap_or_else(|| "nog nooit".into())))
    .collect();
    Ok(SecurityPage {
        l,
        rows,
        users: all_users(&st).await?,
        events: Event::ALL.iter().map(|e| (e.as_str(), e.label())).collect(),
        csv_href: format!("/beveiliging.csv{}", q.query_string()),
        q,
        summary: security_log::summary(&st.pool).await?,
        last_logins,
        limit: LIMIT,
    })
}

async fn csv_download(
    State(st): State<AppState>,
    _user: AuthUser,
    Query(q): Query<Q>,
) -> AppResult<Response> {
    let f = q.filter()?;
    let rows = security_log::query(&st.pool, &f, 100_000).await?;
    let mut buf: Vec<u8> = "\u{feff}".as_bytes().to_vec();
    {
        let mut w = csv::WriterBuilder::new()
            .delimiter(b';')
            .terminator(csv::Terminator::CRLF)
            .from_writer(&mut buf);
        let csv_err = |e: csv::Error| AppError::Internal(e.into());
        w.write_record([
            "Tijdstip",
            "Gebeurtenis",
            "Gebruiker",
            "E-mailadres",
            "IP-adres",
            "Route",
            "Browser",
            "Details",
        ])
        .map_err(csv_err)?;
        for r in &rows {
            let t = |v: &str| spreadsheet_text(v).into_owned();
            w.write_record([
                r.at.clone(),
                r.label(),
                t(r.user_name.as_deref().unwrap_or_default()),
                t(&r.email),
                r.ip.clone(),
                r.via.clone(),
                t(&r.user_agent),
                t(&r.detail()),
            ])
            .map_err(csv_err)?;
        }
        w.flush().map_err(|e| AppError::Internal(e.into()))?;
    }
    let today = format::iso_date(format::today());
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"beveiligingslog-{today}.csv\""),
            ),
        ],
        buf,
    )
        .into_response())
}
