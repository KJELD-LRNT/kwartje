//! Pages and routes: week view, quick entry, htmx tariff suggestion, edit/archive and the year overview.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{self, post};
use axum::{Form, Router};
use serde::Deserialize;
use time::{Date, Duration, Weekday};
use tower_sessions::Session;

use crate::auth::users::{self, UserOption};
use crate::format;
use crate::modules::companies::CurrentCompany;
use crate::modules::tasks::{self, TaskOption};
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::form::{self, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};

use super::{form::*, model::*};

// ---------- week ----------

/// `2026-W48` → Monday of that ISO week.
pub fn parse_week(s: &str) -> Option<Date> {
    let (y, w) = s.trim().split_once("-W")?;
    Date::from_iso_week_date(y.parse().ok()?, w.parse().ok()?, Weekday::Monday).ok()
}

pub fn week_param(monday: Date) -> String {
    let (y, w) = format::iso_week(monday);
    format!("{y}-W{w:02}")
}

pub struct Day {
    pub label: String,
    /// "Ma", "Di", … for the day tabs.
    pub short: String,
    /// Day of the month.
    pub num: u8,
    pub iso: String,
    pub is_today: bool,
    pub entries: Vec<EntryRow>,
    pub minutes: i64,
}

pub struct WeekTotals {
    pub minutes: i64,
    pub billable_minutes: i64,
    pub internal_minutes: i64,
    pub amount_cents: i64,
}

/// "Ma", "Di", … (two letters, capitalised).
pub fn short_day(d: Date) -> String {
    let name = format::weekday_name(d.weekday());
    let mut c = name.chars();
    match (c.next(), c.next()) {
        (Some(a), Some(b)) => format!("{}{b}", a.to_uppercase()),
        _ => name.to_string(),
    }
}

pub fn build_days(monday: Date, rows: Vec<EntryRow>) -> (Vec<Day>, WeekTotals) {
    let today = format::today();
    let mut days: Vec<Day> = (0..7)
        .map(|i| {
            let d = monday + Duration::days(i);
            Day {
                label: format!("{} {}", format::weekday_name(d.weekday()), format::date(d)),
                short: short_day(d),
                num: d.day(),
                iso: format::iso_date(d),
                is_today: d == today,
                entries: Vec::new(),
                minutes: 0,
            }
        })
        .collect();
    let mut t = WeekTotals {
        minutes: 0,
        billable_minutes: 0,
        internal_minutes: 0,
        amount_cents: 0,
    };
    for r in rows {
        t.minutes += r.minutes;
        if r.billable() {
            t.billable_minutes += r.minutes;
            t.amount_cents += r.amount_cents();
        } else {
            t.internal_minutes += r.minutes;
        }
        if let Some(day) = days.iter_mut().find(|d| d.iso == r.date) {
            day.minutes += r.minutes;
            day.entries.push(r);
        }
    }
    (days, t)
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/uren", routing::get(week_page).post(create))
        .route("/uren/voorstel", routing::get(suggestion))
        .route("/uren/jaar", routing::get(year_page))
        .route("/uren/{id}", routing::get(edit_page).post(save))
        .route("/uren/{id}/verwijderen", post(remove))
}

#[derive(Template, WebTemplate)]
#[template(path = "hours/week.html")]
pub(super) struct WeekPage {
    l: Layout,
    f: EntryForm,
    errors: Errors,
    users: Vec<UserOption>,
    tasks: Vec<TaskOption>,
    deps: Deps,
    user_id: i64,
    week_label: String,
    prev: String,
    next: String,
    week: String,
    days: Vec<Day>,
    totals: WeekTotals,
    /// The day shown (`jjjj-mm-dd`), or empty for the whole week.
    selected: String,
    title: String,
    prev_day: String,
    next_day: String,
    today: String,
    /// Open the "Uren toevoegen" window straight away (after an error, or
    /// `?nieuw=1` without JavaScript).
    open_form: bool,
}

impl WeekPage {
    /// The entries of the selected day.
    fn day_entries(&self) -> Option<&Day> {
        self.days.iter().find(|d| d.iso == self.selected)
    }
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
    fn form_action(&self) -> String {
        "/uren".into()
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "hours/edit.html")]
pub(super) struct EditPage {
    l: Layout,
    entry: Entry,
    f: EntryForm,
    errors: Errors,
    users: Vec<UserOption>,
    tasks: Vec<TaskOption>,
    deps: Deps,
    back: String,
}

impl EditPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
    fn form_action(&self) -> String {
        format!("/uren/{}", self.entry.id)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "hours/deps.html")]
pub(super) struct DepsFragment {
    deps: Deps,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct WeekQuery {
    #[serde(default)]
    week: String,
    /// A single day (`jjjj-mm-dd`); its week is shown with that day selected.
    #[serde(default)]
    dag: String,
    #[serde(default)]
    gebruiker: String,
    #[serde(default)]
    nieuw: String,
}

/// What the hours page shows: which week, which day (or the whole week) and
/// whether the entry window starts open.
pub(super) struct View {
    pub monday: Date,
    pub day: Option<Date>,
    pub open_form: bool,
}

impl View {
    pub fn week(monday: Date) -> Self {
        View {
            monday,
            day: None,
            open_form: false,
        }
    }
    pub fn day(d: Date) -> Self {
        View {
            monday: format::week_start(d),
            day: Some(d),
            open_form: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn render_week(
    st: &AppState,
    company_id: i64,
    l: Layout,
    _viewer: &AuthUser,
    view: View,
    user_id: i64,
    f: Option<EntryForm>,
    errors: Errors,
) -> AppResult<WeekPage> {
    let monday = view.monday;
    let open_form = view.open_form || f.is_some();
    let users_list = users::workers(&st.pool).await?;
    let sunday = monday + Duration::days(6);
    let rows = rows_for_user(
        &st.pool,
        company_id,
        user_id,
        &format::iso_date(monday),
        &format::iso_date(sunday),
    )
    .await?;
    let (days, totals) = build_days(monday, rows);
    let today = format::today();
    let f = f.unwrap_or_else(|| EntryForm {
        date: format::iso_date(view.day.unwrap_or(if (monday..=sunday).contains(&today) {
            today
        } else {
            monday
        })),
        user_id: user_id.to_string(),
        tariff: "auto".into(),
        ..Default::default()
    });
    let (deps, _, _) = work_out(&st.pool, company_id, &f, None).await?;
    let (_, w) = format::iso_week(monday);
    let title = match view.day {
        Some(d) => format!(
            "{} {} {} {}",
            format::weekday_name(d.weekday()),
            d.day(),
            format::month_name(d.month()),
            d.year()
        ),
        None => format!("week {w}"),
    };
    let anchor = view.day.unwrap_or(monday);
    Ok(WeekPage {
        selected: view.day.map(format::iso_date).unwrap_or_default(),
        title,
        prev_day: format::iso_date(anchor - Duration::days(1)),
        next_day: format::iso_date(anchor + Duration::days(1)),
        today: format::iso_date(today),
        open_form,
        l,
        errors,
        users: users_list,
        tasks: tasks::options(&st.pool, company_id).await?,
        deps,
        user_id,
        week_label: format!(
            "Week {w} · {} t/m {}",
            format::date(monday),
            format::date(sunday)
        ),
        prev: week_param(monday - Duration::days(7)),
        next: week_param(monday + Duration::days(7)),
        week: week_param(monday),
        days,
        totals,
        f,
    })
}

pub(super) async fn week_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    Query(q): Query<WeekQuery>,
) -> AppResult<WeekPage> {
    // ?dag= shows that day; ?week= the whole week; nothing: today.
    let mut view = match (format::parse_iso_date(&q.dag), parse_week(&q.week)) {
        (Some(d), _) => View::day(d),
        (None, Some(monday)) => View::week(monday),
        (None, None) => View::day(format::today()),
    };
    view.open_form = !q.nieuw.is_empty();
    let user_id = match q.gebruiker.trim().parse() {
        Ok(id) => id,
        Err(_) => users::default_worker(&st.pool, user.id).await?,
    };
    render_week(
        &st,
        company.id,
        l,
        &user,
        view,
        user_id,
        None,
        Errors::new(),
    )
    .await
}

pub(super) async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<EntryForm>,
) -> AppResult<Response> {
    let back_to = |date: Option<Date>| {
        (
            View::day(date.unwrap_or_else(format::today)),
            f.user_id.trim().parse().unwrap_or(user.id),
        )
    };
    // Several days chosen in the calendar: one entry per day, each with the
    // tariff valid on that day; all saved together or none.
    let dates = match parse_dates(&f.dates) {
        Ok(d) => d,
        Err(msg) => {
            let mut errors = Errors::new();
            errors.insert("date", msg);
            let (view, uid) = back_to(None);
            let page = render_week(&st, company.id, l, &user, view, uid, Some(f), errors).await?;
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
        }
    };
    if !dates.is_empty() {
        let mut all = Vec::with_capacity(dates.len());
        for d in &dates {
            let day = EntryForm {
                date: format::iso_date(*d),
                ..f.clone()
            };
            let (_, mut errors, prepared) = work_out(&st.pool, company.id, &day, None).await?;
            let Some(p) = prepared else {
                if let Some(msg) = errors.remove("date") {
                    errors.insert("date", format!("{}: {msg}", format::date(*d)));
                } else {
                    errors.insert(
                        "date",
                        format!("Op {} kan dit niet worden geschreven.", format::date(*d)),
                    );
                }
                let (view, uid) = back_to(Some(*d));
                let page =
                    render_week(&st, company.id, l, &user, view, uid, Some(f), errors).await?;
                return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
            };
            all.push(p);
        }
        insert_all(&st.pool, company.id, &all, Some(user.id)).await?;
        let first = dates[0];
        let total: i64 = all.iter().map(|p| p.minutes).sum();
        flash::ok(
            &session,
            &format!(
                "{} geschreven op {} dagen ({} t/m {}), samen {}.",
                format::duration(all[0].minutes),
                dates.len(),
                format::date(first),
                format::date(*dates.last().expect("not empty")),
                format::duration(total)
            ),
        )
        .await?;
        return Ok(redirect(&format!(
            "/uren?week={}&gebruiker={}",
            week_param(format::week_start(first)),
            all[0].user_id
        )));
    }

    let (_, errors, prepared) = work_out(&st.pool, company.id, &f, None).await?;
    let Some(p) = prepared else {
        let view = View::day(format::parse_date(&f.date).unwrap_or_else(format::today));
        let uid = f.user_id.trim().parse().unwrap_or(user.id);
        let page = render_week(&st, company.id, l, &user, view, uid, Some(f), errors).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    };
    insert(&st.pool, company.id, &p, Some(user.id)).await?;
    let date = format::parse_iso_date(&p.date).expect("normalised");
    flash::ok(
        &session,
        &format!(
            "{} geschreven op {}.",
            format::duration(p.minutes),
            format::date(date)
        ),
    )
    .await?;
    Ok(redirect(&format!(
        "/uren?dag={}&gebruiker={}",
        format::iso_date(date),
        p.user_id
    )))
}

pub(super) async fn suggestion(
    State(st): State<AppState>,
    _user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Query(f): Query<EntryForm>,
) -> AppResult<DepsFragment> {
    // When editing, the stored entry keeps its tariff and snapshot selectable.
    let existing = match f.entry.trim().parse::<i64>() {
        Ok(id) => {
            let mut conn = st.pool.acquire().await?;
            Some(get(&mut conn, company.id, id).await?)
        }
        Err(_) => None,
    };
    let (deps, _, _) = work_out(&st.pool, company.id, &f, existing.as_ref()).await?;
    Ok(DepsFragment { deps })
}

/// Back to the entry's day after editing or deleting it.
pub(super) fn back_to_week(e: &Entry) -> String {
    format!("/uren?dag={}&gebruiker={}", e.date, e.user_id)
}

pub(super) async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let e = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if e.archived_at.is_some() {
        return Err(AppError::NotFound);
    }
    let s = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
    let f = EntryForm {
        date: e.date.clone(),
        user_id: e.user_id.to_string(),
        task_id: e.task_id.to_string(),
        client_id: s(e.client_id),
        project_id: s(e.project_id),
        tariff: Choice::of_entry(&e).value(),
        duration: format::duration(e.minutes),
        description: e.description.clone(),
        entry: e.id.to_string(),
        dates: String::new(),
    };
    let (deps, _, _) = work_out(&st.pool, company.id, &f, Some(&e)).await?;
    Ok(EditPage {
        l,
        back: back_to_week(&e),
        f,
        errors: Errors::new(),
        users: users::workers(&st.pool).await?,
        tasks: tasks::options(&st.pool, company.id).await?,
        deps,
        entry: e,
    }
    .into_response())
}

pub(super) async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<EntryForm>,
) -> AppResult<Response> {
    let e = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if e.is_locked() {
        return Err(AppError::BadRequest(LOCKED.into()));
    }
    if e.archived_at.is_some() {
        return Err(AppError::NotFound);
    }
    let (deps, errors, prepared) = work_out(&st.pool, company.id, &f, Some(&e)).await?;
    let Some(p) = prepared else {
        let page = EditPage {
            l,
            back: back_to_week(&e),
            f,
            errors,
            users: users::workers(&st.pool).await?,
            tasks: tasks::options(&st.pool, company.id).await?,
            deps,
            entry: e,
        };
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    };
    update(&st.pool, company.id, id, &p, user.id).await?;
    flash::ok(&session, "Uren opgeslagen.").await?;
    let saved = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    Ok(redirect(&back_to_week(&saved)))
}

pub(super) async fn remove(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let e = archive(&st.pool, company.id, id, user.id).await?;
    flash::ok(
        &session,
        &format!(
            "{} verwijderd (bewaard in het archief).",
            format::duration(e.minutes)
        ),
    )
    .await?;
    Ok(redirect(&back_to_week(&e)))
}

#[derive(Template, WebTemplate)]
#[template(path = "hours/year.html")]
pub(super) struct YearPage {
    l: Layout,
    year: i32,
    users: Vec<UserYear>,
    month_labels: [&'static str; 12],
}

#[derive(Deserialize)]
pub(super) struct YearQuery {
    #[serde(default)]
    jaar: Option<i32>,
}

pub(super) async fn year_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<YearQuery>,
) -> AppResult<YearPage> {
    let year = q
        .jaar
        .filter(|y| (2000..=2100).contains(y))
        .unwrap_or_else(|| format::today().year());
    Ok(YearPage {
        l,
        users: year_totals(&st.pool, company.id, year).await?,
        year,
        month_labels: MONTHS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn iso_week_params() {
        assert_eq!(parse_week("2026-W48"), Some(date!(2026 - 11 - 23)));
        assert_eq!(parse_week("2026-W01"), Some(date!(2025 - 12 - 29)));
        assert_eq!(parse_week("2026-W54"), None);
        assert_eq!(parse_week("rommel"), None);
        assert_eq!(week_param(date!(2026 - 11 - 23)), "2026-W48");
        assert_eq!(week_param(date!(2025 - 12 - 29)), "2026-W01");
    }

    fn row(date: &str, minutes: i64, price: Option<i64>) -> EntryRow {
        EntryRow {
            id: 1,
            date: date.into(),
            user_id: 1,
            user_name: "K".into(),
            task_name: "T".into(),
            client_id: None,
            client_name: None,
            project_name: None,
            tariff_name: price.map(|_| "Std".into()),
            minutes,
            description: String::new(),
            price_cents: price,
            invoice_line_id: None,
        }
    }

    #[test]
    fn week_totals() {
        let rows = vec![
            row("2026-11-23", 90, Some(9000)),
            row("2026-11-23", 30, None),
            row("2026-11-29", 20, Some(9000)),
            row("2026-11-24", 60, Some(0)),
        ];
        let (days, t) = build_days(date!(2026 - 11 - 23), rows);
        assert_eq!(days.len(), 7);
        assert!(days[0].label.starts_with("maandag 23-11-2026"));
        assert_eq!(days[0].minutes, 120);
        assert_eq!(days[6].minutes, 20);
        assert_eq!(t.minutes, 200);
        assert_eq!(t.billable_minutes, 170); // € 0 counts as billable
        assert_eq!(t.internal_minutes, 30);
        assert_eq!(t.amount_cents, 13_500 + 3000);
    }

    #[test]
    fn choice_values() {
        assert_eq!(Choice::parse("").unwrap(), Choice::Auto);
        assert_eq!(Choice::parse("none").unwrap(), Choice::None);
        assert_eq!(Choice::parse("12").unwrap(), Choice::Tariff(12));
        assert!(Choice::parse("x").is_err());
    }
}
