//! Read-only view of the audit log (Logboek).

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::audit::Action;
use crate::web::filters;
use crate::web::layout::{Layout, Page};
use crate::web::{AppResult, AppState};

const LIMIT: i64 = 200;

pub struct Entry {
    pub at: String,
    pub user_name: Option<String>,
    pub table_name: String,
    pub row_id: i64,
    pub action: String,
    pub diff_json: String,
}

impl Entry {
    pub fn table_label(&self) -> &str {
        table_label(&self.table_name)
    }
    pub fn action_label(&self) -> &'static str {
        Action::label_nl(&self.action)
    }
    pub fn diff_pretty(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.diff_json)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| self.diff_json.clone())
    }
}

/// Dutch name of a table, for display (from the export registry).
pub fn table_label(t: &str) -> &str {
    match t {
        "trusted_devices" => "Vertrouwde apparaten",
        _ => crate::export::find(t).map(|e| e.label).unwrap_or(t),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "audit/list.html")]
struct AuditPage {
    l: Layout,
    entries: Vec<Entry>,
    table: String,
    tables: Vec<(String, String)>,
    limit: i64,
}

#[derive(Deserialize)]
struct Q {
    #[serde(default)]
    tabel: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/logboek", get(page))
}

async fn page(
    State(st): State<AppState>,
    Page(l): Page,
    Query(q): Query<Q>,
) -> AppResult<AuditPage> {
    let table = q.tabel.trim().to_string();
    let all = table.is_empty();
    let entries = sqlx::query_as!(
        Entry,
        r#"SELECT a.at, u.name AS "user_name?", a.table_name, a.row_id, a.action, a.diff_json
           FROM audit_log a LEFT JOIN users u ON u.id = a.user_id
           WHERE ? OR a.table_name = ?
           ORDER BY a.id DESC LIMIT ?"#,
        all,
        table,
        LIMIT
    )
    .fetch_all(&st.pool)
    .await?;
    let tables: Vec<String> =
        sqlx::query_scalar!("SELECT DISTINCT table_name FROM audit_log ORDER BY table_name")
            .fetch_all(&st.pool)
            .await?;
    Ok(AuditPage {
        l,
        entries,
        table,
        tables: tables
            .into_iter()
            .map(|t| {
                let label = table_label(&t).to_string();
                (t, label)
            })
            .collect(),
        limit: LIMIT,
    })
}
