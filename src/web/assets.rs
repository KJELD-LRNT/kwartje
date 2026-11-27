//! Vendored static files, embedded in the binary.

use axum::extract::Path;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::{AppError, AppState};

struct Asset {
    name: &'static str,
    mime: &'static str,
    body: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        name: "pico.min.css",
        mime: "text/css; charset=utf-8",
        body: include_bytes!("../../static/pico.min.css"),
    },
    Asset {
        name: "app.css",
        mime: "text/css; charset=utf-8",
        body: include_bytes!("../../static/app.css"),
    },
    Asset {
        name: "htmx.min.js",
        mime: "text/javascript; charset=utf-8",
        body: include_bytes!("../../static/htmx.min.js"),
    },
    Asset {
        name: "app.js",
        mime: "text/javascript; charset=utf-8",
        body: include_bytes!("../../static/app.js"),
    },
    Asset {
        name: "manrope.woff2",
        mime: "font/woff2",
        body: include_bytes!("../../static/manrope.woff2"),
    },
    Asset {
        name: "theme.js",
        mime: "text/javascript; charset=utf-8",
        body: include_bytes!("../../static/theme.js"),
    },
    Asset {
        name: "icons.svg",
        mime: "image/svg+xml",
        body: include_bytes!("../../static/icons.svg"),
    },
    Asset {
        name: "favicon.svg",
        mime: "image/svg+xml",
        body: include_bytes!("../../static/favicon.svg"),
    },
];

pub fn router() -> Router<AppState> {
    Router::new().route("/static/{name}", get(serve))
}

async fn serve(Path(name): Path<String>) -> Result<Response, AppError> {
    let asset = ASSETS
        .iter()
        .find(|a| a.name == name)
        .ok_or(AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, asset.mime),
            // URLs carry ?v=<app version>, so a long cache is safe.
            (header::CACHE_CONTROL, "public, max-age=604800, immutable"),
        ],
        asset.body,
    )
        .into_response())
}
