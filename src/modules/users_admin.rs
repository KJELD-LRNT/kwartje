//! "Gebruikers" (beheerder only): create users with a temporary password,
//! change name, e-mail, role and a boekhouder's companies, reset the
//! password or MFA, forget trusted devices, archive and restore. Everything
//! the CLI can do, plus a security-log entry for each change.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use tower_sessions::Session;

use crate::auth::devices::{self, Device};
use crate::auth::password;
use crate::auth::users::{self, Listed, Role};
use crate::modules::companies::{self, CompanyOption};
use crate::security_log::{self, ClientInfo, Event};
use crate::web::error::redirect;
use crate::web::form::{self, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/gebruikers", get(list_page).post(create))
        .route("/gebruikers/nieuw", get(new_page))
        .route("/gebruikers/{id}", get(detail_page).post(save))
        .route("/gebruikers/{id}/wachtwoord", post(reset_password))
        .route("/gebruikers/{id}/mfa-reset", post(reset_mfa))
        .route("/gebruikers/{id}/archiveren", post(archive))
        .route("/gebruikers/{id}/herstellen", post(restore))
        .route("/gebruikers/{id}/apparaten/intrekken", post(forget_devices))
}

/// Defence in depth: the access middleware already answers 404.
fn admin(user: &AuthUser) -> AppResult<()> {
    if user.is_admin() {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "users/list.html")]
struct ListPage {
    l: Layout,
    users: Vec<Listed>,
}

async fn list_page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
) -> AppResult<ListPage> {
    admin(&user)?;
    Ok(ListPage {
        l,
        users: users::list_all(&st.pool).await?,
    })
}

/// The user form. Parsed by hand: a boekhouder's companies are repeated
/// `bedrijf` fields.
#[derive(Debug, Clone, Default)]
pub struct UserForm {
    name: String,
    email: String,
    role: String,
    password: String,
    companies: Vec<i64>,
}

#[derive(Template, WebTemplate)]
#[template(path = "users/form.html")]
struct FormPage {
    l: Layout,
    f: UserForm,
    errors: Errors,
    roles: Vec<(&'static str, &'static str, &'static str)>,
    companies: Vec<CompanyOption>,
}

impl FormPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
    fn has_company(&self, id: &i64) -> bool {
        self.f.companies.contains(id)
    }
}

fn roles() -> Vec<(&'static str, &'static str, &'static str)> {
    Role::ALL
        .iter()
        .map(|r| (r.as_str(), r.label(), r.description()))
        .collect()
}

/// Parse a urlencoded body with repeated `bedrijf` fields.
fn parse_form(body: &str) -> UserForm {
    let mut f = UserForm::default();
    for (k, v) in form_urlencoded_pairs(body) {
        match k.as_str() {
            "name" => f.name = v,
            "email" => f.email = v,
            "role" => f.role = v,
            "password" => f.password = v,
            "bedrijf" => {
                if let Ok(id) = v.parse() {
                    f.companies.push(id);
                }
            }
            _ => {}
        }
    }
    f
}

fn form_urlencoded_pairs(body: &str) -> Vec<(String, String)> {
    serde_urlencoded::from_str::<Vec<(String, String)>>(body).unwrap_or_default()
}

async fn new_page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
) -> AppResult<FormPage> {
    admin(&user)?;
    Ok(FormPage {
        l,
        f: UserForm {
            role: Role::Employee.as_str().into(),
            password: password::suggestion(),
            ..Default::default()
        },
        errors: Errors::new(),
        roles: roles(),
        companies: companies::options(&st.pool).await?,
    })
}

/// Common checks of the role and companies; returns the role.
fn check_role(f: &UserForm, all: &[CompanyOption], e: &mut Errors) -> Option<Role> {
    let role = Role::parse(&f.role);
    match role {
        None => {
            e.insert("role", "Kies een rol.".into());
        }
        Some(Role::Accountant) => {
            if f.companies.is_empty() {
                e.insert(
                    "bedrijf",
                    "Kies minstens één bedrijf voor de boekhouder.".into(),
                );
            } else if f.companies.iter().any(|c| !all.iter().any(|o| o.id == *c)) {
                e.insert("bedrijf", "Onbekend of gearchiveerd bedrijf.".into());
            }
        }
        Some(_) => {}
    }
    role
}

async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    body: String,
) -> AppResult<Response> {
    admin(&user)?;
    let f = parse_form(&body);
    let all = companies::options(&st.pool).await?;
    let mut e = Errors::new();
    let role = check_role(&f, &all, &mut e);
    if let Some(msg) = password::check_strength(&f.password) {
        e.insert("password", msg);
    }
    if e.is_empty() {
        match users::create(&st.pool, &f.name, &f.email, &f.password, Some(user.id)).await {
            Ok(id) => {
                let role = role.expect("checked");
                users::set_role(&st.pool, id, role, Some(user.id)).await?;
                if role == Role::Accountant {
                    users::set_company_ids(&st.pool, id, &f.companies, Some(user.id)).await?;
                }
                users::set_temporary_password(&st.pool, id, &f.password, Some(user.id)).await?;
                security_log::record(
                    &st.pool,
                    Event::UserCreated,
                    Some(id),
                    &f.email,
                    &client,
                    json!({"role": role.label(), "by": user.name}),
                )
                .await;
                flash::ok(
                    &session,
                    &format!(
                        "{} is aangemaakt als {}. Geef het tijdelijke wachtwoord door: {} — bij de eerste login kiest {} een eigen wachtwoord en stelt tweestapsverificatie in.",
                        f.name.trim(),
                        role.label().to_lowercase(),
                        f.password,
                        f.name.trim()
                    ),
                )
                .await?;
                return Ok(redirect(&format!("/gebruikers/{id}")));
            }
            Err(err) => {
                let msg = err.to_string();
                let field = if msg.contains("naam") {
                    "name"
                } else {
                    "email"
                };
                e.insert(field, capitalise(&msg));
            }
        }
    }
    Ok((
        StatusCode::UNPROCESSABLE_ENTITY,
        FormPage {
            l,
            f,
            errors: e,
            roles: roles(),
            companies: all,
        },
    )
        .into_response())
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str() + ".",
        None => String::new(),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "users/detail.html")]
struct DetailPage {
    l: Layout,
    u: Listed,
    f: UserForm,
    errors: Errors,
    roles: Vec<(&'static str, &'static str, &'static str)>,
    companies: Vec<CompanyOption>,
    devices: Vec<Device>,
    is_self: bool,
    suggestion: String,
}

impl DetailPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
    fn has_company(&self, id: &i64) -> bool {
        self.f.companies.contains(id)
    }
}

async fn detail(
    st: &AppState,
    l: Layout,
    me: &AuthUser,
    id: i64,
    f: Option<UserForm>,
    errors: Errors,
) -> AppResult<DetailPage> {
    let u = users::find_any(&st.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let f = match f {
        Some(f) => f,
        None => UserForm {
            name: u.name.clone(),
            email: u.email.clone(),
            role: u.role.clone(),
            companies: users::company_ids(&st.pool, id).await?,
            ..Default::default()
        },
    };
    Ok(DetailPage {
        l,
        f,
        errors,
        roles: roles(),
        companies: companies::options(&st.pool).await?,
        devices: devices::active(&st.pool, id).await?,
        is_self: me.id == id,
        suggestion: password::suggestion(),
        u,
    })
}

async fn detail_page(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<DetailPage> {
    admin(&user)?;
    detail(&st, l, &user, id, None, Errors::new()).await
}

async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    Path(id): Path<i64>,
    body: String,
) -> AppResult<Response> {
    admin(&user)?;
    let target = users::find_any(&st.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let f = parse_form(&body);
    let all = companies::options(&st.pool).await?;
    let mut e = Errors::new();
    let role = check_role(&f, &all, &mut e);
    if id == user.id && role.is_some_and(|r| r != Role::Admin) {
        e.insert(
            "role",
            "Je kunt je eigen beheerdersrol niet afnemen.".into(),
        );
    }
    if e.is_empty() {
        if let Err(err) =
            users::update_profile(&st.pool, id, &f.name, &f.email, Some(user.id)).await
        {
            let msg = err.to_string();
            let field = if msg.contains("naam") {
                "name"
            } else {
                "email"
            };
            e.insert(field, capitalise(&msg));
        }
    }
    if e.is_empty() {
        let role = role.expect("checked");
        if role.as_str() != target.role {
            if let Err(err) = users::set_role(&st.pool, id, role, Some(user.id)).await {
                e.insert("role", capitalise(&err.to_string()));
            } else {
                security_log::record(
                    &st.pool,
                    Event::RoleChanged,
                    Some(id),
                    &f.email,
                    &client,
                    json!({"role": role.label(), "by": user.name}),
                )
                .await;
            }
        }
        if e.is_empty() {
            let wanted: Vec<i64> = if role == Role::Accountant {
                f.companies.clone()
            } else {
                Vec::new()
            };
            if users::company_ids(&st.pool, id).await? != wanted {
                users::set_company_ids(&st.pool, id, &wanted, Some(user.id)).await?;
                security_log::record(
                    &st.pool,
                    Event::AccessChanged,
                    Some(id),
                    &f.email,
                    &client,
                    json!({"by": user.name}),
                )
                .await;
            }
            if target.name != f.name.trim() || target.email != f.email.trim() {
                security_log::record(
                    &st.pool,
                    Event::UserUpdated,
                    Some(id),
                    &f.email,
                    &client,
                    json!({"by": user.name}),
                )
                .await;
            }
            flash::ok(&session, "Gebruiker opgeslagen.").await?;
            return Ok(redirect(&format!("/gebruikers/{id}")));
        }
    }
    let page = detail(&st, l, &user, id, Some(f), e).await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
}

#[derive(Deserialize)]
struct PasswordForm {
    #[serde(default)]
    password: String,
}

async fn reset_password(
    State(st): State<AppState>,
    Page(l): Page,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<PasswordForm>,
) -> AppResult<Response> {
    admin(&user)?;
    let target = users::find_any(&st.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(msg) = password::check_strength(&f.password) {
        let mut e = Errors::new();
        e.insert("password", msg);
        let page = detail(&st, l, &user, id, None, e).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    users::set_temporary_password(&st.pool, id, &f.password, Some(user.id)).await?;
    security_log::record(
        &st.pool,
        Event::PasswordReset,
        Some(id),
        &target.email,
        &client,
        json!({"by": user.name}),
    )
    .await;
    flash::ok(
        &session,
        &format!(
            "Nieuw tijdelijk wachtwoord voor {}: {} — geef het door. Bij de volgende login moet {} een eigen wachtwoord kiezen. Vertrouwde apparaten zijn vergeten.",
            target.name, f.password, target.name
        ),
    )
    .await?;
    Ok(redirect(&format!("/gebruikers/{id}")))
}

async fn reset_mfa(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    admin(&user)?;
    let target = users::find_any(&st.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    users::reset_totp(&st.pool, id, Some(user.id)).await?;
    security_log::record(
        &st.pool,
        Event::MfaReset,
        Some(id),
        &target.email,
        &client,
        json!({"by": user.name}),
    )
    .await;
    flash::ok(
        &session,
        &format!(
            "Tweestapsverificatie van {} is gereset; bij de volgende login stelt {} een nieuwe in.",
            target.name, target.name
        ),
    )
    .await?;
    Ok(redirect(&format!("/gebruikers/{id}")))
}

async fn set_archived(
    st: &AppState,
    user: &AuthUser,
    session: &Session,
    client: &ClientInfo,
    id: i64,
    archived: bool,
) -> AppResult<Response> {
    admin(user)?;
    let target = users::find_any(&st.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    match users::set_archived(&st.pool, id, archived, user.id).await {
        Ok(()) => {
            let event = if archived {
                Event::UserArchived
            } else {
                Event::UserRestored
            };
            security_log::record(
                &st.pool,
                event,
                Some(id),
                &target.email,
                client,
                json!({"by": user.name}),
            )
            .await;
            let msg = if archived {
                format!("{} is gearchiveerd en kan niet meer inloggen.", target.name)
            } else {
                format!("{} is hersteld en kan weer inloggen.", target.name)
            };
            flash::ok(session, &msg).await?;
        }
        Err(err) => flash::error(session, &capitalise(&err.to_string())).await?,
    }
    Ok(redirect(&format!("/gebruikers/{id}")))
}

async fn archive(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st, &user, &session, &client, id, true).await
}

async fn restore(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    set_archived(&st, &user, &session, &client, id, false).await
}

async fn forget_devices(
    State(st): State<AppState>,
    user: AuthUser,
    session: Session,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    admin(&user)?;
    let target = users::find_any(&st.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut tx = st.pool.begin().await?;
    let n = devices::revoke_all(&mut tx, id, Some(user.id)).await?;
    tx.commit().await?;
    if n > 0 {
        security_log::record(
            &st.pool,
            Event::DeviceRevoked,
            Some(id),
            &target.email,
            &client,
            json!({"by": user.name, "method": format!("{n} apparaten")}),
        )
        .await;
    }
    flash::ok(&session, &format!("{n} vertrouwde apparaten vergeten.")).await?;
    Ok(redirect(&format!("/gebruikers/{id}")))
}
