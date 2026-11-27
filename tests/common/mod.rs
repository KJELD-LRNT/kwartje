//! Test harness: in-memory SQLite, the real router, a tiny cookie-keeping client.
#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use kwartje::auth::{totp, users};
use kwartje::config::Config;
use kwartje::{db, web};
use sqlx::SqlitePool;
use tower::ServiceExt;

pub const PASSWORD: &str = "test-wachtwoord-123";

pub struct TestApp {
    pub router: Router,
    pub pool: SqlitePool,
    pub cfg: Arc<Config>,
    pub dir: tempfile::TempDir,
    pub cookie: Option<String>,
    /// The trusted-device cookie ("MFA 30 dagen onthouden"), kept apart.
    pub device: Option<String>,
    pub csrf: Option<String>,
    pub ip: String,
}

pub struct Resp {
    pub status: StatusCode,
    pub location: Option<String>,
    pub headers: axum::http::HeaderMap,
    pub body: String,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_services(fake_services(false)).await
    }

    pub async fn with_services(services: web::Services) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(Config::for_dir(dir.path()));
        let pool = db::connect_memory().await.unwrap();
        db::migrate(&pool).await.unwrap();
        let router = web::app_with(cfg.clone(), pool.clone(), services)
            .await
            .unwrap();
        TestApp {
            router,
            pool,
            cfg,
            dir,
            cookie: None,
            device: None,
            csrf: None,
            ip: "100.64.0.1".into(),
        }
    }

    /// Same app and DB, fresh browser (no cookies).
    pub fn new_client(&mut self) {
        self.cookie = None;
        self.device = None;
        self.csrf = None;
    }

    pub async fn send(&mut self, req: Request<Body>) -> Resp {
        let mut req = req;
        let jar: Vec<&str> = [&self.cookie, &self.device]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if !jar.is_empty() {
            req.headers_mut()
                .insert(header::COOKIE, jar.join("; ").parse().unwrap());
        }
        // As behind `tailscale serve`: the peer is loopback, the client is in
        // X-Forwarded-For.
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            ))));
        req.headers_mut()
            .insert("x-forwarded-for", self.ip.parse().unwrap());
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        for sc in headers.get_all(header::SET_COOKIE) {
            let v = sc.to_str().unwrap();
            let pair = v.split(';').next().unwrap().to_string();
            let gone = v.contains("Max-Age=0") || pair.ends_with('=');
            let slot = if pair.starts_with("kwartje_apparaat=") {
                &mut self.device
            } else {
                &mut self.cookie
            };
            *slot = if gone { None } else { Some(pair) };
        }
        let location = headers
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes).to_string();
        if let Some(t) = extract_csrf(&body) {
            self.csrf = Some(t);
        }
        Resp {
            status,
            location,
            headers,
            body,
        }
    }

    pub async fn get(&mut self, path: &str) -> Resp {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    /// POST a form; the CSRF token from the last page is added automatically.
    pub async fn post(&mut self, path: &str, fields: &[(&str, &str)]) -> Resp {
        let mut pairs: Vec<(String, String)> = fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(t) = &self.csrf {
            pairs.push(("csrf_token".into(), t.clone()));
        }
        self.post_raw(path, &pairs).await
    }

    pub async fn post_raw(&mut self, path: &str, pairs: &[(String, String)]) -> Resp {
        let body = serde_urlencoded::to_string(pairs).unwrap();
        self.send(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    /// Multipart POST (CSRF token in the query string, like the upload forms).
    pub async fn upload(
        &mut self,
        path: &str,
        fields: &[(&str, &str)],
        file: Option<(&str, &str, &[u8])>,
    ) -> Resp {
        let boundary = "XKWARTJEBOUNDARYX";
        let mut body = Vec::new();
        for (k, v) in fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
                )
                .as_bytes(),
            );
        }
        if let Some((field, name, bytes)) = file {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!(
            "{path}{sep}csrf_token={}",
            self.csrf.clone().unwrap_or_default()
        );
        self.send(
            Request::post(url)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    pub async fn create_user(&self, name: &str, email: &str) -> i64 {
        users::create(&self.pool, name, email, PASSWORD, None)
            .await
            .unwrap()
    }

    /// Full login incl. TOTP enrolment (first time) or verification.
    /// Returns the user's TOTP secret.
    pub async fn login(&mut self, email: &str) -> String {
        self.get("/inloggen").await;
        let r = self
            .post("/inloggen", &[("email", email), ("password", PASSWORD)])
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "login: {}", r.body);
        let user = users::find_by_email(&self.pool, email)
            .await
            .unwrap()
            .unwrap();
        match r.location.as_deref() {
            Some("/2fa/instellen") => {
                let page = self.get("/2fa/instellen").await;
                let secret = extract_secret(&page.body).expect("secret on setup page");
                // Use the previous step so a later login in the same test can use the current one.
                let code = totp::code_at(&secret, totp::unix_now() - 30).unwrap();
                let r = self.post("/2fa/instellen", &[("code", &code)]).await;
                assert_eq!(r.location.as_deref(), Some("/"), "setup: {}", r.body);
                secret
            }
            Some("/2fa") => {
                let secret = user.totp_secret.unwrap();
                self.get("/2fa").await;
                let code = totp::code_at(&secret, totp::unix_now()).unwrap();
                let r = self.post("/2fa", &[("code", &code)]).await;
                assert_eq!(r.location.as_deref(), Some("/"), "2fa: {}", r.body);
                secret
            }
            other => panic!("unexpected redirect {other:?}"),
        }
    }
}

impl TestApp {
    /// A company created directly in the DB (fake data).
    pub async fn create_company(&self, name: &str, prefix: &str) -> i64 {
        let d = kwartje::modules::companies::CompanyData {
            name: name.into(),
            invoice_prefix: prefix.into(),
            payment_term_days: 14,
            default_vat_bp: 2100,
            ..Default::default()
        };
        kwartje::modules::companies::insert(&self.pool, &d, None)
            .await
            .unwrap()
    }

    /// Select a company with the switcher (must be logged in).
    pub async fn select_company(&mut self, id: i64) {
        self.get("/").await;
        let r = self
            .post("/bedrijf/kiezen", &[("company_id", &id.to_string())])
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
    }

    /// Logged-in app with one company selected; returns (app, company id).
    pub async fn with_company() -> (Self, i64) {
        Self::with_company_and(fake_services(false)).await
    }

    pub async fn with_company_and(services: web::Services) -> (Self, i64) {
        let mut app = TestApp::with_services(services).await;
        app.create_user("Anna", "anna@example.test").await;
        let company = app.create_company("Alfa B.V.", "AA").await;
        app.login("anna@example.test").await;
        app.select_company(company).await;
        (app, company)
    }

    /// Id of the last row in a table (for tests).
    pub async fn last_id(&self, table: &str) -> i64 {
        // Test helper: the SQL is written by the test itself.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT MAX(id) FROM {table}")))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

pub fn fake_services(pdf_fails: bool) -> web::Services {
    web::Services {
        pdf: Arc::new(kwartje::pdf::FakePdf { fail: pdf_fails }),
        mailer: kwartje::mail::Mailer::Disabled,
    }
}

pub fn extract_csrf(html: &str) -> Option<String> {
    let marker = "name=\"csrf_token\" value=\"";
    let i = html.find(marker)? + marker.len();
    let end = html[i..].find('"')?;
    Some(html[i..i + end].to_string())
}

pub fn extract_secret(html: &str) -> Option<String> {
    let marker = "<p class=\"secret\">";
    let i = html.find(marker)? + marker.len();
    let end = html[i..].find('<')?;
    Some(html[i..i + end].trim().to_string())
}
