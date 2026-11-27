//! The hour entry form: parsing, validation and working out task, client, project and the tariff snapshot (`work_out`).

use serde::Deserialize;
use sqlx::SqlitePool;

use crate::auth::users;
use crate::format;
use crate::modules::clients::{self, ClientOption};
use crate::modules::projects::{self, ProjectOption};
use crate::modules::tasks;
use crate::tariff::{self, Candidate};
use crate::web::form::{opt_id, Errors};
use crate::web::{AppError, AppResult};

use super::model::*;

// ---------- tariff choice ----------

/// What the tariff `<select>` says: automatic, explicitly none, or a tariff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Auto,
    None,
    Tariff(i64),
}

impl Choice {
    pub fn parse(s: &str) -> Result<Self, AppError> {
        match s.trim() {
            "" | "auto" => Ok(Choice::Auto),
            "none" => Ok(Choice::None),
            v => v
                .parse()
                .map(Choice::Tariff)
                .map_err(|_| AppError::BadRequest("Ongeldige tariefkeuze.".into())),
        }
    }

    pub fn of_entry(e: &Entry) -> Self {
        match e.tariff_id {
            Some(t) => Choice::Tariff(t),
            None => Choice::None,
        }
    }

    pub fn value(self) -> String {
        match self {
            Choice::Auto => "auto".into(),
            Choice::None => "none".into(),
            Choice::Tariff(id) => id.to_string(),
        }
    }
}

/// Form fields that depend on the task, client and date: rendered in the
/// entry form and refreshed by `/uren/voorstel`.
pub struct Deps {
    pub clients: Vec<ClientOption>,
    pub client_id: String,
    pub client_fixed: bool,
    pub projects: Vec<ProjectOption>,
    pub project_id: String,
    pub project_fixed: bool,
    pub tariff: String,
    pub auto_label: String,
    /// `(value, label)` of tariffs usable for this client/project on the date.
    pub tariffs: Vec<(String, String)>,
    pub client_err: String,
    pub project_err: String,
    pub tariff_err: String,
}

pub(super) fn tariff_label(c: &Candidate) -> String {
    format!("{} — {} per uur", c.name, format::amount(c.price_cents))
}

/// Inputs as strings (straight from the form).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EntryForm {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub tariff: String,
    #[serde(default)]
    pub duration: String,
    #[serde(default)]
    pub description: String,
    /// Set by the edit form so the live suggestion knows the stored entry.
    #[serde(default)]
    pub entry: String,
    /// The same hours on several days: comma-separated `jjjj-mm-dd` dates
    /// chosen in the calendar (new entries only). When set, `date` is ignored.
    #[serde(default)]
    pub dates: String,
}

/// Most days that can be written in one go.
pub const MAX_DAYS: usize = 62;

/// The dates chosen in the multi-day calendar, sorted and without
/// duplicates; `Err` with a Dutch message when a value is unusable.
pub fn parse_dates(raw: &str) -> Result<Vec<time::Date>, String> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let d = format::parse_iso_date(part)
            .or_else(|| format::parse_date(part))
            .ok_or_else(|| format!("Ongeldige datum in de kalender: {part}."))?;
        out.push(d);
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_DAYS {
        return Err(format!("Kies hoogstens {MAX_DAYS} dagen tegelijk."));
    }
    Ok(out)
}

/// The normalised entry, ready to save.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub user_id: i64,
    pub task_id: i64,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub date: String,
    pub minutes: i64,
    pub description: String,
    /// `(tariff_id, price_cents, vat_bp)`
    pub tariff: Option<(i64, i64, i64)>,
}

/// Resolve the task, client, project and tariff for the given form values.
/// Used both for the live suggestion and for saving. `existing` keeps an
/// edited entry's snapshot when neither its tariff nor its date changed.
pub(super) async fn work_out(
    pool: &SqlitePool,
    company_id: i64,
    f: &EntryForm,
    existing: Option<&Entry>,
) -> AppResult<(Deps, Errors, Option<Prepared>)> {
    let mut errors = Errors::new();
    let mut conn = pool.acquire().await?;

    let date = format::parse_date(&f.date);
    if date.is_none() {
        errors.insert("date", "Vul een geldige datum in.".into());
    }
    let date_s = date
        .map(format::iso_date)
        .unwrap_or_else(|| format::iso_date(format::today()));

    let task = match opt_id(&f.task_id)? {
        Some(id) => {
            let t = tasks::get(&mut conn, company_id, id).await?;
            if t.archived_at.is_some() && existing.is_none_or(|e| e.task_id != t.id) {
                errors.insert("task_id", "Deze taak is gearchiveerd.".into());
            }
            Some(t)
        }
        None => {
            errors.insert("task_id", "Kies een taak.".into());
            None
        }
    };

    // Client / project: fixed by the task where it has them.
    let mut client_id = opt_id(&f.client_id)?;
    let mut project_id = opt_id(&f.project_id)?;
    let mut client_err = String::new();
    let project_err = String::new();
    let client_fixed = task.as_ref().is_some_and(|t| t.client_id.is_some());
    let project_fixed = task.as_ref().is_some_and(|t| t.project_id.is_some());
    if let Some(t) = &task {
        if t.client_id.is_some() {
            client_id = t.client_id;
        }
        if t.project_id.is_some() {
            project_id = t.project_id;
        }
    }
    if let Some(pid) = project_id {
        let p = projects::get(&mut conn, company_id, pid).await?;
        if client_id.is_some_and(|c| c != p.client_id) {
            if project_fixed {
                client_id = Some(p.client_id);
            } else {
                project_id = None; // client changed: the old project no longer applies
            }
        } else {
            client_id = Some(p.client_id);
        }
    }
    if let Some(cid) = client_id {
        let c = clients::get(&mut conn, company_id, cid).await?;
        if c.archived_at.is_some() && existing.is_none_or(|e| e.client_id != Some(cid)) {
            client_err = format!("Klant {} is gearchiveerd.", c.name);
        }
    }

    // Tariff.
    let candidates = tariff::hour_candidates(&mut conn, company_id).await?;
    let ctx = tariff::Context {
        task_billable: task.as_ref().is_none_or(|t| t.billable),
        task_lineage: match task.as_ref().and_then(|t| t.tariff_id) {
            Some(tid) => Some(
                crate::modules::tariffs::get(&mut conn, company_id, tid)
                    .await?
                    .lineage_id,
            ),
            None => None,
        },
        client_id,
        project_id,
        date: &date_s,
    };
    let resolved = tariff::resolve(&ctx, &candidates);
    let auto_label = match (&task, resolved) {
        (None, _) => "Automatisch (kies eerst een taak)".to_string(),
        (_, Some((c, src))) => format!("Automatisch: {} ({})", tariff_label(c), src.label()),
        (Some(t), None) if !t.billable => "Automatisch: geen tarief (interne taak)".into(),
        (Some(_), None) => "Automatisch: geen tarief gevonden (niet declarabel)".into(),
    };

    let choice = Choice::parse(&f.tariff)?;
    let mut tariff_err = String::new();
    let chosen: Option<(i64, i64, i64)> = match choice {
        _ if task.is_none() => None, // nothing to decide until a task is chosen
        Choice::None => None,
        Choice::Auto => resolved.map(|(c, _)| (c.id, c.price_cents, c.vat_bp)),
        Choice::Tariff(id) => {
            let keep = existing.filter(|e| e.tariff_id == Some(id) && e.date == date_s);
            if let Some(e) = keep {
                Some((id, e.price_cents.unwrap_or(0), e.vat_bp.unwrap_or(0)))
            } else {
                let t = crate::modules::tariffs::get(&mut conn, company_id, id).await?;
                match tariff::version_on(&candidates, t.lineage_id, &date_s) {
                    Some(v) if v.fits(client_id, project_id) => {
                        Some((v.id, v.price_cents, v.vat_bp))
                    }
                    Some(_) => {
                        tariff_err =
                            "Dit tarief hoort bij een andere klant of een ander project.".into();
                        None
                    }
                    None => {
                        tariff_err = "Dit tarief geldt niet op deze datum.".into();
                        None
                    }
                }
            }
        }
    };
    if chosen.is_some() && client_id.is_none() {
        client_err = "Kies een klant: declarabele uren moeten bij een klant horen.".into();
    }

    let mut tariffs_list = Vec::new();
    for c in candidates
        .iter()
        .filter(|c| c.valid_on(&date_s) && c.fits(client_id, project_id))
    {
        tariffs_list.push((c.id.to_string(), tariff_label(c)));
    }
    // An edited entry's own (possibly expired) tariff stays selectable.
    if let Choice::Tariff(id) = choice {
        if !tariffs_list.iter().any(|(v, _)| *v == id.to_string()) {
            if let Some(e) = existing.filter(|e| e.tariff_id == Some(id)) {
                let t = crate::modules::tariffs::get(&mut conn, company_id, id).await?;
                tariffs_list.push((
                    id.to_string(),
                    format!(
                        "{} — {} per uur (opgeslagen prijs)",
                        t.name,
                        format::amount(e.price_cents.unwrap_or(0))
                    ),
                ));
            }
        }
    }
    drop(conn);

    let all_clients = clients::options(pool, company_id).await?;
    let clients = match (client_fixed, client_id) {
        (true, Some(cid)) => all_clients.into_iter().filter(|c| c.id == cid).collect(),
        _ => all_clients,
    };
    let projects: Vec<ProjectOption> = projects::options(pool, company_id)
        .await?
        .into_iter()
        .filter(|p| client_id == Some(p.client_id))
        .filter(|p| !project_fixed || project_id == Some(p.id))
        .collect();

    // Only medewerkers have hours; a beheerder books on their behalf.
    let user_id = match f.user_id.trim().parse::<i64>() {
        Ok(id) if users::is_worker(pool, id).await? => Some(id),
        _ => {
            errors.insert("user_id", "Kies een medewerker.".into());
            None
        }
    };
    let minutes = format::parse_duration(&f.duration).filter(|m| (1..=1440).contains(m));
    if minutes.is_none() {
        errors.insert(
            "duration",
            "Vul een duur in van 1 minuut tot 24 uur, bijv. 1:30, 1,5 of 45m.".into(),
        );
    }
    let description = f.description.trim().to_string();
    if description.chars().count() > MAX_DESCRIPTION {
        errors.insert(
            "description",
            "De omschrijving is te lang (max. 2.000 tekens).".into(),
        );
    }
    for (k, v) in [
        ("client_id", &client_err),
        ("project_id", &project_err),
        ("tariff", &tariff_err),
    ] {
        if !v.is_empty() {
            errors.insert(k, v.clone());
        }
    }

    let s = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
    let deps = Deps {
        clients,
        client_id: s(client_id),
        client_fixed,
        projects,
        project_id: s(project_id),
        project_fixed,
        tariff: choice.value(),
        auto_label,
        tariffs: tariffs_list,
        client_err,
        project_err,
        tariff_err,
    };
    let prepared = match (errors.is_empty(), user_id, task, minutes) {
        (true, Some(user_id), Some(task), Some(minutes)) => Some(Prepared {
            user_id,
            task_id: task.id,
            client_id,
            project_id,
            date: date_s,
            minutes,
            description,
            tariff: chosen,
        }),
        _ => None,
    };
    Ok((deps, errors, prepared))
}
