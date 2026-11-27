//! `GET /gezond`: health check for Uptime Kuma (no login, reveals no data).
//! 200 when the database answers and the newest backup is at most 36 hours
//! old; 503 otherwise.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::AppState;
use crate::backup::{list_backups, BackupManifest};
use crate::format;

pub const MAX_BACKUP_AGE_HOURS: i64 = 36;

pub fn router() -> Router<AppState> {
    Router::new().route("/gezond", get(health))
}

async fn health(State(st): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&st.pool)
        .await
        .is_ok();
    let latest = list_backups(&st.cfg.backup_dir)
        .ok()
        .and_then(|l| l.into_iter().next_back())
        .and_then(|(_, p)| BackupManifest::read(&p).ok())
        .and_then(|m| OffsetDateTime::parse(&m.created_at, &Rfc3339).ok());
    let backup_ok = latest
        .is_some_and(|t| (OffsetDateTime::now_utc() - t).whole_hours() <= MAX_BACKUP_AGE_HOURS);
    let backup_line = match latest {
        Some(t) => format!(
            "laatste backup: {}",
            format::datetime(&crate::db::timestamp(t))
        ),
        None => "geen backup gevonden".into(),
    };
    let status = if db_ok && backup_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = format!(
        "{}\ndatabase: {}\n{backup_line}\n",
        if status == StatusCode::OK {
            "ok"
        } else {
            "niet in orde"
        },
        if db_ok { "ok" } else { "fout" }
    );
    (
        status,
        [("content-type", "text/plain; charset=utf-8")],
        body,
    )
}
