//! HTTP layer: router, middleware, shared state.

pub mod assets;
pub mod error;
pub mod filters;
pub mod flash;
pub mod form;
pub mod health;
pub mod layout;
pub mod security;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use axum::Router;
use sqlx::SqlitePool;
use tower_sessions::cookie::{Key, SameSite};
use tower_sessions::{Expiry, SessionManagerLayer};
use tower_sessions_sqlx_store::SqliteStore;

use crate::auth::ratelimit::RateLimiter;
use crate::config::Config;
use crate::{auth, db, modules};

pub use error::{AppError, AppResult};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub cfg: Arc<Config>,
    pub limiter: Arc<RateLimiter>,
    pub pdf: Arc<dyn crate::pdf::PdfRenderer>,
    pub mailer: crate::mail::Mailer,
}

/// External services the app talks to; replaced by fakes in tests.
#[derive(Clone)]
pub struct Services {
    pub pdf: Arc<dyn crate::pdf::PdfRenderer>,
    pub mailer: crate::mail::Mailer,
}

impl Services {
    pub fn real(cfg: &Config) -> Result<Self> {
        Ok(Services {
            pdf: Arc::new(crate::pdf::TypstCli {
                binary: cfg.typst_path.clone(),
                timeout: std::time::Duration::from_secs(60),
            }),
            mailer: crate::mail::Mailer::from_config(cfg.smtp.as_ref())?,
        })
    }
}

pub const SESSION_COOKIE: &str = "kwartje_sid";
/// Logged-in sessions expire after this much inactivity.
pub const SESSION_IDLE: time::Duration = time::Duration::hours(12);
/// Default request body limit (forms). Uploads set their own.
pub const BODY_LIMIT: usize = 256 * 1024;

/// Build the full application with the real services.
pub async fn app(cfg: Arc<Config>, pool: SqlitePool) -> Result<Router> {
    let services = Services::real(&cfg)?;
    app_with(cfg, pool, services).await
}

/// Build the full application. The session store table is created here.
pub async fn app_with(cfg: Arc<Config>, pool: SqlitePool, services: Services) -> Result<Router> {
    let store = SqliteStore::new(pool.clone());
    store.migrate().await.context("aanmaken van sessietabel")?;

    let key = Key::try_from(cfg.session_secret.as_bytes())
        .map_err(|e| anyhow::anyhow!("session_secret unusable as key: {e}"))?;
    let sessions = SessionManagerLayer::new(store)
        .with_name(SESSION_COOKIE)
        .with_http_only(true)
        // Lax, not Strict: after a redirect from an SSO gate the first request
        // is cross-site and a Strict cookie would not be sent. Every POST is
        // still protected by the CSRF token, Origin and Sec-Fetch-Site checks.
        .with_same_site(SameSite::Lax)
        .with_secure(cfg.cookie_secure)
        .with_expiry(Expiry::OnInactivity(SESSION_IDLE))
        .with_signed(key);

    let cfg_secure = cfg.cookie_secure;
    let state = AppState {
        pool,
        cfg,
        limiter: Arc::new(RateLimiter::new()),
        pdf: services.pdf,
        mailer: services.mailer,
    };

    let router = Router::new()
        .merge(auth::routes::router())
        .merge(modules::router())
        .merge(assets::router())
        .merge(health::router())
        .fallback(error::not_found)
        .layer(axum::middleware::from_fn(security::csrf))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            security::access,
        ))
        .layer(axum::middleware::from_fn(remember_expiry))
        .layer(sessions)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(axum::middleware::from_fn(security::headers))
        // HSTS only when served over HTTPS (cookie_secure). A proxy may set
        // its own; it is then left alone.
        .layer(tower::util::option_layer(cfg_secure.then(|| {
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::STRICT_TRANSPORT_SECURITY,
                axum::http::HeaderValue::from_static("max-age=31536000"),
            )
        })))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);
    Ok(router)
}

/// "Ingelogd blijven": the session layer gives every session the inactivity
/// expiry above; a session that carries `REMEMBER_UNTIL` instead keeps its
/// cookie and stored record until that moment. The expiry is not stored by
/// tower-sessions, so it is set again whenever the session is saved.
async fn remember_expiry(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let session = req.extensions().get::<tower_sessions::Session>().cloned();
    let res = next.run(req).await;
    if let Some(s) = session.filter(|s| s.is_modified()) {
        if let Ok(Some(until)) = s.get::<u64>(auth::session_keys::REMEMBER_UNTIL).await {
            if let Ok(t) = time::OffsetDateTime::from_unix_timestamp(until as i64) {
                s.set_expiry(Some(Expiry::AtDateTime(t)));
            }
        }
    }
    res
}

/// `kwartje serve`
pub async fn serve(cfg: Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("aanmaken van {}", cfg.data_dir.display()))?;
    // Held for the lifetime of the server; `restore` refuses to run while it is.
    let _lock = crate::backup::ServeLock::acquire(&cfg.data_dir)?;

    let pool = db::connect(&cfg.db_path()).await?;
    db::migrate_with_snapshot(&pool, &cfg.data_dir).await?;
    let cfg = Arc::new(cfg);
    let app = app(cfg.clone(), pool.clone()).await?;

    // Recurring invoices: drafts at startup and every hour.
    crate::modules::recurring::spawn_generator(pool.clone());

    // Remove expired sessions every hour.
    let cleanup_store = SqliteStore::new(pool.clone());
    tokio::spawn(async move {
        use tower_sessions::session_store::ExpiredDeletion;
        let _ = cleanup_store
            .continuously_delete_expired(tokio::time::Duration::from_secs(3600))
            .await;
    });

    // Security log: drop events older than the retention, at startup and hourly.
    let log_pool = pool.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match crate::security_log::cleanup(&log_pool).await {
                Ok(n) if n > 0 => tracing::info!(removed = n, "security log cleanup"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "security log cleanup failed"),
            }
        }
    });

    if cfg.listen.ip().is_unspecified() {
        tracing::warn!(
            listen = %cfg.listen,
            "Kwartje luistert op alle netwerken (allow_all_interfaces = true): iedereen die deze machine kan bereiken ziet de inlogpagina"
        );
    }
    if !cfg.cookie_secure {
        tracing::warn!("cookie_secure = false: wachtwoorden en sessies gaan onversleuteld over het netwerk; gebruik dit alleen op een vertrouwd netwerk");
    }

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("luisteren op {}", cfg.listen))?;
    tracing::info!("listening on http://{}", cfg.listen);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    pool.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}
