//! User accounts (created from the CLI; there is no sign-up page).

use anyhow::{bail, Result};
use serde_json::json;
use sqlx::SqlitePool;

use super::password;
use crate::audit::{self, Action};
use crate::db;

/// What a user may do. Stored in English, shown in Dutch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    /// Beheerder: everything, user management and the security log; books
    /// hours and trips on behalf of others, never on itself.
    Admin,
    /// Medewerker: everything except user management and the security log.
    #[default]
    Employee,
    /// Boekhouder: read-only, only the companies in `user_companies`.
    Accountant,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Admin, Role::Employee, Role::Accountant];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Employee => "employee",
            Role::Accountant => "accountant",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Role::Admin => "Beheerder",
            Role::Employee => "Medewerker",
            Role::Accountant => "Boekhouder",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Role::Admin => "Alles, plus gebruikers en beveiliging. Schrijft uren en ritten namens medewerkers, niet voor zichzelf.",
            Role::Employee => "Alles behalve gebruikersbeheer en het beveiligingslog.",
            Role::Accountant => "Alleen lezen en downloaden, voor de gekozen bedrijven.",
        }
    }
    /// From the stored value; unknown values are treated as the least
    /// privileged role.
    pub fn from_db(s: &str) -> Role {
        match s {
            "admin" => Role::Admin,
            "employee" => Role::Employee,
            _ => Role::Accountant,
        }
    }
    /// From user input: the Dutch label or the stored value.
    pub fn parse(s: &str) -> Option<Role> {
        let s = s.trim().to_lowercase();
        Role::ALL
            .into_iter()
            .find(|r| r.as_str() == s || r.label().to_lowercase() == s)
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub password_hash: String,
    pub totp_secret: Option<String>,
    pub totp_enabled_at: Option<String>,
    pub totp_last_step: Option<i64>,
    pub archived_at: Option<String>,
    pub role: String,
    pub must_change_password: bool,
    pub last_login_at: Option<String>,
}

impl User {
    pub fn has_totp(&self) -> bool {
        self.totp_secret.is_some() && self.totp_enabled_at.is_some()
    }
    pub fn role(&self) -> Role {
        Role::from_db(&self.role)
    }
}

pub async fn find_by_email(pool: &SqlitePool, email: &str) -> Result<Option<User>> {
    let email = email.trim();
    Ok(sqlx::query_as!(
        User,
        r#"SELECT id AS "id!", name, email, password_hash, totp_secret, totp_enabled_at,
                  totp_last_step, archived_at, role,
                  must_change_password AS "must_change_password: bool", last_login_at
           FROM users WHERE email = ? AND archived_at IS NULL"#,
        email
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<User>> {
    Ok(sqlx::query_as!(
        User,
        r#"SELECT id AS "id!", name, email, password_hash, totp_secret, totp_enabled_at,
                  totp_last_step, archived_at, role,
                  must_change_password AS "must_change_password: bool", last_login_at
           FROM users WHERE id = ? AND archived_at IS NULL"#,
        id
    )
    .fetch_optional(pool)
    .await?)
}

pub fn validate_email(email: &str) -> Result<()> {
    let email = email.trim();
    let ok = email.len() <= 254
        && email
            .split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.'));
    if !ok {
        bail!("ongeldig e-mailadres: {email}");
    }
    Ok(())
}

/// Create a user (CLI and seed). Returns the new id.
pub async fn create(
    pool: &SqlitePool,
    name: &str,
    email: &str,
    password: &str,
    created_by: Option<i64>,
) -> Result<i64> {
    let name = name.trim();
    let email = email.trim();
    if name.is_empty() {
        bail!("naam is verplicht");
    }
    validate_email(email)?;
    if let Some(msg) = password::check_strength(password) {
        bail!(msg);
    }
    if find_by_email(pool, email).await?.is_some() {
        bail!("er bestaat al een gebruiker met e-mailadres {email}");
    }
    let hash = password::hash(password)?;
    let now = db::now();
    let mut tx = pool.begin().await?;
    let id = sqlx::query!(
        "INSERT INTO users (name, email, password_hash, created_at, updated_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?)",
        name,
        email,
        hash,
        now,
        now,
        created_by
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    audit::record(
        &mut tx,
        created_by,
        "users",
        id,
        Action::Insert,
        &audit::snapshot(&json!({
            "id": id, "name": name, "email": email, "password_hash": hash,
            "created_at": now, "created_by": created_by,
        })),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn set_password(
    pool: &SqlitePool,
    id: i64,
    password: &str,
    by: Option<i64>,
) -> Result<()> {
    if let Some(msg) = password::check_strength(password) {
        bail!(msg);
    }
    let hash = password::hash(password)?;
    let now = db::now();
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?",
        hash,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        by,
        "users",
        id,
        Action::Update,
        &audit::diff(
            &json!({"password_hash": ""}),
            &json!({"password_hash": hash}),
        ),
    )
    .await?;
    // A new password ends every "MFA onthouden" trust of this user.
    super::devices::revoke_all(&mut tx, id, by).await?;
    tx.commit().await?;
    Ok(())
}

/// Remove 2FA so the user enrols again at the next login.
pub async fn reset_totp(pool: &SqlitePool, id: i64, by: Option<i64>) -> Result<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "UPDATE users SET totp_secret = NULL, totp_enabled_at = NULL, totp_last_step = NULL,
                          updated_at = ?
         WHERE id = ?",
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        by,
        "users",
        id,
        Action::Update,
        &json!({"totp_secret": ["gewijzigd", null], "totp_enabled_at": ["", null]}),
    )
    .await?;
    super::devices::revoke_all(&mut tx, id, by).await?;
    tx.commit().await?;
    Ok(())
}

/// Store a confirmed TOTP secret and the step it was confirmed with.
pub async fn enable_totp(pool: &SqlitePool, id: i64, secret: &str, step: i64) -> Result<()> {
    let now = db::now();
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "UPDATE users SET totp_secret = ?, totp_enabled_at = ?, totp_last_step = ?, updated_at = ?
         WHERE id = ?",
        secret,
        now,
        step,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        Some(id),
        "users",
        id,
        Action::Update,
        &audit::diff(
            &json!({"totp_secret": null, "totp_enabled_at": null}),
            &json!({"totp_secret": secret, "totp_enabled_at": now}),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Remember the last accepted TOTP step (replay protection). Not audited: it
/// changes at every login and carries no business meaning.
pub async fn set_totp_step(pool: &SqlitePool, id: i64, step: i64) -> Result<()> {
    sqlx::query!("UPDATE users SET totp_last_step = ? WHERE id = ?", step, id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Active users for pickers.
#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: i64,
    pub name: String,
}

/// The people hours and trips are booked on: active medewerkers. A beheerder
/// books on their behalf and a boekhouder only reads, so neither is listed.
pub async fn workers(pool: &SqlitePool) -> Result<Vec<UserOption>> {
    Ok(sqlx::query_as!(
        UserOption,
        r#"SELECT id AS "id!", name FROM users
           WHERE archived_at IS NULL AND role = 'employee'
           ORDER BY name COLLATE NOCASE"#
    )
    .fetch_all(pool)
    .await?)
}

/// Whose hours and trips a page shows by default: the viewer when they are
/// a medewerker, otherwise (a beheerder) the first medewerker.
pub async fn default_worker(pool: &SqlitePool, viewer: i64) -> Result<i64> {
    let list = workers(pool).await?;
    Ok(if list.iter().any(|u| u.id == viewer) {
        viewer
    } else {
        list.first().map(|u| u.id).unwrap_or(viewer)
    })
}

/// `true` when hours and trips may be booked on `user_id`.
pub async fn is_worker(pool: &SqlitePool, user_id: i64) -> Result<bool> {
    Ok(workers(pool).await?.iter().any(|u| u.id == user_id))
}

/// Stamp the time of a completed login (not audited: changes every login).
pub async fn touch_login(pool: &SqlitePool, id: i64) -> Result<()> {
    let now = db::now();
    sqlx::query!("UPDATE users SET last_login_at = ? WHERE id = ?", now, id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Change a user's role, refusing to leave no active beheerder.
pub async fn set_role(pool: &SqlitePool, id: i64, role: Role, by: Option<i64>) -> Result<()> {
    let mut tx = pool.begin().await?;
    let old: String = sqlx::query_scalar!("SELECT role FROM users WHERE id = ?", id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("onbekende gebruiker"))?;
    if old == role.as_str() {
        return Ok(());
    }
    if old == "admin" && active_admins(&mut tx).await? <= 1 {
        bail!("dit is de laatste beheerder; maak eerst iemand anders beheerder");
    }
    let now = db::now();
    let r = role.as_str();
    sqlx::query!(
        "UPDATE users SET role = ?, updated_at = ? WHERE id = ?",
        r,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        by,
        "users",
        id,
        Action::Update,
        &json!({"role": [old, r]}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn active_admins(conn: &mut sqlx::SqliteConnection) -> Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM users WHERE role = 'admin' AND archived_at IS NULL"#
    )
    .fetch_one(conn)
    .await?)
}

/// Companies an accountant may see.
pub async fn company_ids(pool: &SqlitePool, user_id: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT company_id AS "company_id!" FROM user_companies WHERE user_id = ?
           ORDER BY company_id"#,
        user_id
    )
    .fetch_all(pool)
    .await?)
}

/// Replace the companies an accountant may see (audited per change).
pub async fn set_company_ids(
    pool: &SqlitePool,
    user_id: i64,
    ids: &[i64],
    by: Option<i64>,
) -> Result<()> {
    let current = company_ids(pool, user_id).await?;
    let now = db::now();
    let mut tx = pool.begin().await?;
    for c in current.iter().filter(|c| !ids.contains(c)) {
        sqlx::query!(
            "DELETE FROM user_companies WHERE user_id = ? AND company_id = ?",
            user_id,
            c
        )
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            by,
            "user_companies",
            user_id,
            Action::Delete,
            &json!({"user_id": user_id, "company_id": c}),
        )
        .await?;
    }
    for c in ids.iter().filter(|c| !current.contains(c)) {
        sqlx::query!(
            "INSERT INTO user_companies (user_id, company_id, created_at, created_by)
             VALUES (?, ?, ?, ?)",
            user_id,
            c,
            now,
            by
        )
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            by,
            "user_companies",
            user_id,
            Action::Insert,
            &json!({"user_id": user_id, "company_id": c, "created_at": now}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------- user management (beheerder) ----------

/// A user as listed on /gebruikers.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Listed {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: String,
    pub has_totp: bool,
    pub must_change_password: bool,
    pub last_login_at: Option<String>,
    pub archived_at: Option<String>,
    pub devices: i64,
}

impl Listed {
    pub fn role_label(&self) -> &'static str {
        Role::from_db(&self.role).label()
    }
    pub fn last_login(&self) -> String {
        self.last_login_at
            .as_deref()
            .map(crate::format::datetime)
            .unwrap_or_else(|| "nog nooit".into())
    }
}

/// Every user (archived ones last), with MFA state and trusted devices.
pub async fn list_all(pool: &SqlitePool) -> Result<Vec<Listed>> {
    let now = db::now();
    Ok(sqlx::query_as!(
        Listed,
        r#"SELECT u.id AS "id!", u.name, u.email, u.role,
                  (u.totp_enabled_at IS NOT NULL) AS "has_totp!: bool",
                  u.must_change_password AS "must_change_password: bool",
                  u.last_login_at, u.archived_at,
                  (SELECT COUNT(*) FROM trusted_devices d
                   WHERE d.user_id = u.id AND d.revoked_at IS NULL AND d.expires_at > ?) AS "devices!: i64"
           FROM users u
           ORDER BY u.archived_at IS NOT NULL, u.name COLLATE NOCASE"#,
        now
    )
    .fetch_all(pool)
    .await?)
}

/// One user including archived ones (the admin page).
pub async fn find_any(pool: &SqlitePool, id: i64) -> Result<Option<Listed>> {
    Ok(list_all(pool).await?.into_iter().find(|u| u.id == id))
}

/// Change name and e-mail (unique).
pub async fn update_profile(
    pool: &SqlitePool,
    id: i64,
    name: &str,
    email: &str,
    by: Option<i64>,
) -> Result<()> {
    let name = name.trim();
    let email = email.trim();
    if name.is_empty() {
        bail!("naam is verplicht");
    }
    validate_email(email)?;
    let mut tx = pool.begin().await?;
    let old = sqlx::query!("SELECT name, email FROM users WHERE id = ?", id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("onbekende gebruiker"))?;
    let taken: Option<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM users WHERE email = ? AND id != ?"#,
        email,
        id
    )
    .fetch_optional(&mut *tx)
    .await?;
    if taken.is_some() {
        bail!("er bestaat al een gebruiker met e-mailadres {email}");
    }
    if old.name == name && old.email == email {
        return Ok(());
    }
    let now = db::now();
    sqlx::query!(
        "UPDATE users SET name = ?, email = ?, updated_at = ? WHERE id = ?",
        name,
        email,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        by,
        "users",
        id,
        Action::Update,
        &audit::diff(
            &json!({"name": old.name, "email": old.email}),
            &json!({"name": name, "email": email}),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A temporary password from a beheerder: must be changed at the next
/// login; all trusted devices are forgotten.
pub async fn set_temporary_password(
    pool: &SqlitePool,
    id: i64,
    password: &str,
    by: Option<i64>,
) -> Result<()> {
    set_password(pool, id, password, by).await?;
    set_must_change(pool, id, true, by).await
}

/// The user's own new password (clears the "must change" flag).
pub async fn change_own_password(pool: &SqlitePool, id: i64, password: &str) -> Result<()> {
    set_password(pool, id, password, Some(id)).await?;
    set_must_change(pool, id, false, Some(id)).await
}

async fn set_must_change(pool: &SqlitePool, id: i64, on: bool, by: Option<i64>) -> Result<()> {
    let mut tx = pool.begin().await?;
    let old: bool = sqlx::query_scalar!(
        r#"SELECT must_change_password AS "m: bool" FROM users WHERE id = ?"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if old == on {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE users SET must_change_password = ? WHERE id = ?",
        on,
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        by,
        "users",
        id,
        Action::Update,
        &json!({"must_change_password": [old, on]}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Archive or restore a user. An archived user can't log in and their
/// sessions end at the next request. Refuses yourself and the last beheerder.
pub async fn set_archived(pool: &SqlitePool, id: i64, archived: bool, by: i64) -> Result<()> {
    if archived && id == by {
        bail!("je kunt jezelf niet archiveren");
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query!("SELECT role, archived_at FROM users WHERE id = ?", id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("onbekende gebruiker"))?;
    if row.archived_at.is_some() == archived {
        return Ok(());
    }
    if archived && row.role == "admin" && active_admins(&mut tx).await? <= 1 {
        bail!("dit is de laatste beheerder");
    }
    let now = db::now();
    let at = archived.then(|| now.clone());
    sqlx::query!(
        "UPDATE users SET archived_at = ?, updated_at = ? WHERE id = ?",
        at,
        now,
        id
    )
    .execute(&mut *tx)
    .await?;
    if archived {
        super::devices::revoke_all(&mut tx, id, Some(by)).await?;
    }
    audit::record_archive(
        &mut tx,
        Some(by),
        "users",
        id,
        row.archived_at.as_deref(),
        at.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
