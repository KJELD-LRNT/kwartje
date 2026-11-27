//! Pages and routes: month list with quick entry, htmx suggestions, edit/archive, the year overview and the rittenregistratie CSV.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{self, post};
use axum::{Form, Router};
use serde::Deserialize;
use time::{Date, Month};
use tower_sessions::Session;

use crate::auth::users::{self, UserOption};
use crate::format;
use crate::mileage::{self, Leg};
use crate::modules::companies::CurrentCompany;
use crate::modules::routes::{self, Route};
use crate::modules::vehicles::{self, Vehicle};
use crate::web::error::redirect;
use crate::web::filters;
use crate::web::form::{self, Errors};
use crate::web::layout::{AuthUser, Layout, Page};
use crate::web::{flash, AppError, AppResult, AppState};

use super::{form::*, model::*};

// ---------- month ----------

/// `2026-11` → first day of that month.
pub fn parse_month(s: &str) -> Option<Date> {
    let (y, m) = s.trim().split_once('-')?;
    let month = Month::try_from(m.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(y.parse().ok()?, month, 1).ok()
}

pub fn month_param(d: Date) -> String {
    format!("{:04}-{:02}", d.year(), u8::from(d.month()))
}

pub fn month_end(first: Date) -> Date {
    let next = if first.month() == Month::December {
        Date::from_calendar_date(first.year() + 1, Month::January, 1)
    } else {
        Date::from_calendar_date(first.year(), first.month().next(), 1)
    }
    .expect("valid date");
    next.previous_day().expect("valid date")
}

pub struct ListedTrip {
    pub row: TripRow,
    pub gap: i64,
}

// ---------- handlers ----------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/kilometers", routing::get(month_page).post(create))
        .route("/kilometers/voorstel", routing::get(suggestion))
        .route("/kilometers/jaar", routing::get(year_page))
        .route(
            "/kilometers/rittenregistratie.csv",
            routing::get(trip_log_csv),
        )
        .route("/kilometers/{id}", routing::get(edit_page).post(save))
        .route("/kilometers/{id}/verwijderen", post(remove))
}

#[derive(Template, WebTemplate)]
#[template(path = "trips/month.html")]
pub(super) struct MonthPage {
    l: Layout,
    f: TripForm,
    errors: Errors,
    deps: Deps,
    vehicles: Vec<Vehicle>,
    routes: Vec<Route>,
    users: Vec<UserOption>,
    trips: Vec<ListedTrip>,
    month: String,
    prev: String,
    next: String,
    vehicle_filter: String,
    business_km: i64,
    private_km: i64,
    billable_cents: i64,
    year: i32,
    /// Day view: the day shown (`jjjj-mm-dd`); empty in the list view.
    selected: String,
    title: String,
    days: Vec<KmDay>,
    week_label: String,
    week_km: i64,
    prev_week: String,
    prev_day: String,
    next_day: String,
    next_week: String,
    today: String,
    /// Open the "Rit toevoegen" window straight away (error, or ?nieuw=1).
    open_form: bool,
}

/// One tab of the week strip on the trips page.
pub struct KmDay {
    pub iso: String,
    pub short: String,
    pub num: u8,
    pub km: i64,
    pub is_today: bool,
}

impl MonthPage {
    fn is_day_view(&self) -> bool {
        !self.selected.is_empty()
    }
    /// Trips shown: all of the month, or only the selected day.
    fn shown(&self) -> Vec<&ListedTrip> {
        self.trips
            .iter()
            .filter(|t| self.selected.is_empty() || t.row.date == self.selected)
            .collect()
    }
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
    fn form_action(&self) -> String {
        "/kilometers".into()
    }
    fn is_edit(&self) -> bool {
        false
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "trips/edit.html")]
pub(super) struct EditPage {
    l: Layout,
    trip: Trip,
    f: TripForm,
    errors: Errors,
    deps: Deps,
    vehicles: Vec<Vehicle>,
    routes: Vec<Route>,
    users: Vec<UserOption>,
    back: String,
}

impl EditPage {
    fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
    fn form_action(&self) -> String {
        format!("/kilometers/{}", self.trip.id)
    }
    fn is_edit(&self) -> bool {
        true
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "trips/deps.html")]
pub(super) struct DepsFragment {
    deps: Deps,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MonthQuery {
    #[serde(default)]
    maand: String,
    /// A single day: the day view with the week strip.
    #[serde(default)]
    dag: String,
    #[serde(default)]
    voertuig: String,
    #[serde(default)]
    nieuw: String,
}

/// What the trips page shows: a month list, or one day with its week.
#[derive(Clone, Copy)]
pub(super) enum TripView {
    List(Date),
    Day(Date),
}

pub(super) struct ViewOpts {
    pub view: TripView,
    pub vehicle: Option<i64>,
    pub open_form: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn render_month(
    st: &AppState,
    company_id: i64,
    l: Layout,
    user: &AuthUser,
    opts: ViewOpts,
    f: Option<TripForm>,
    errors: Errors,
) -> AppResult<MonthPage> {
    let vehicle_filter = opts.vehicle;
    let open_form = opts.open_form || f.is_some();
    // The list view reads the month; the day view the week around the day.
    let (first, last, day) = match opts.view {
        TripView::List(first) => (first, month_end(first), None),
        TripView::Day(d) => {
            let monday = format::week_start(d);
            (monday, monday + time::Duration::days(6), Some(d))
        }
    };
    let rows = rows_between(
        &st.pool,
        company_id,
        vehicle_filter,
        &format::iso_date(first),
        &format::iso_date(last),
    )
    .await?;
    let mut trips = Vec::new();
    let mut leg_cache: Vec<(i64, Vec<Leg>)> = Vec::new();
    {
        let mut conn = st.pool.acquire().await?;
        for row in rows {
            if !leg_cache.iter().any(|(v, _)| *v == row.vehicle_id) {
                let l = legs(&mut conn, row.vehicle_id).await?;
                leg_cache.push((row.vehicle_id, l));
            }
            let vlegs = &leg_cache
                .iter()
                .find(|(v, _)| *v == row.vehicle_id)
                .expect("cached")
                .1;
            let gap = match row.odometer_start {
                Some(start) if !row.vehicle_km_only => mileage::gap_before(start, vlegs),
                _ => 0,
            };
            trips.push(ListedTrip { row, gap });
        }
    }
    let business_km = trips
        .iter()
        .filter(|t| t.row.business)
        .map(|t| t.row.km)
        .sum();
    let private_km = trips
        .iter()
        .filter(|t| !t.row.business)
        .map(|t| t.row.km)
        .sum();
    let billable_cents = trips
        .iter()
        .map(|t| t.row.price_cents.map(|p| p * t.row.km).unwrap_or(0))
        .sum();
    let vehicles_list = vehicles::list(&st.pool, false).await?;
    let today = format::today();
    let default_driver = users::default_worker(&st.pool, user.id).await?;
    let f = f.unwrap_or_else(|| TripForm {
        date: format::iso_date(day.unwrap_or(if (first..=last).contains(&today) {
            today
        } else {
            first
        })),
        user_id: default_driver.to_string(),
        vehicle_id: vehicle_filter
            .or_else(|| (vehicles_list.len() == 1).then(|| vehicles_list[0].id))
            .map(|v| v.to_string())
            .unwrap_or_default(),
        ..Default::default()
    });
    let trigger = if f.odometer_start.is_empty() {
        "vehicle_id"
    } else {
        ""
    };
    let (mut deps, _, _) = work_out(&st.pool, company_id, &f, None, trigger).await?;
    if errors.is_empty() {
        deps.errors = Errors::new(); // a fresh form shows no errors
    }
    // List view: month navigation; day view: the week strip.
    let month_first = day.unwrap_or(first).replace_day(1).expect("day 1");
    let prev = (month_first - time::Duration::days(1))
        .replace_day(1)
        .expect("day 1");
    let next = month_end(month_first) + time::Duration::days(1);
    let days: Vec<KmDay> = match day {
        Some(_) => (0..7)
            .map(|i| {
                let d = first + time::Duration::days(i);
                let iso = format::iso_date(d);
                KmDay {
                    km: trips
                        .iter()
                        .filter(|t| t.row.date == iso)
                        .map(|t| t.row.km)
                        .sum(),
                    short: crate::modules::hours::short_day(d),
                    num: d.day(),
                    is_today: d == today,
                    iso,
                }
            })
            .collect(),
        None => Vec::new(),
    };
    let anchor = day.unwrap_or(today);
    let (_, w) = format::iso_week(first);
    let title = match day {
        Some(d) => format!(
            "{} {} {} {}",
            format::weekday_name(d.weekday()),
            d.day(),
            format::month_name(d.month()),
            d.year()
        ),
        None => format!("{} {}", format::month_name(first.month()), first.year()),
    };
    Ok(MonthPage {
        selected: day.map(format::iso_date).unwrap_or_default(),
        title,
        week_km: days.iter().map(|d| d.km).sum(),
        days,
        week_label: format!("Week {w}"),
        prev_week: format::iso_date(anchor - time::Duration::days(7)),
        prev_day: format::iso_date(anchor - time::Duration::days(1)),
        next_day: format::iso_date(anchor + time::Duration::days(1)),
        next_week: format::iso_date(anchor + time::Duration::days(7)),
        today: format::iso_date(today),
        open_form,
        l,
        errors,
        deps,
        vehicles: vehicles_list,
        routes: routes::list(&st.pool, company_id, false).await?,
        users: users::workers(&st.pool).await?,
        trips,
        month: month_param(month_first),
        prev: month_param(prev),
        next: month_param(next),
        vehicle_filter: vehicle_filter.map(|v| v.to_string()).unwrap_or_default(),
        business_km,
        private_km,
        billable_cents,
        year: day.unwrap_or(first).year(),
        f,
    })
}

pub(super) async fn month_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    Query(q): Query<MonthQuery>,
) -> AppResult<MonthPage> {
    // ?maand= is the list; ?dag= (or nothing: today) the day view.
    let view = match (parse_month(&q.maand), format::parse_iso_date(&q.dag)) {
        (_, Some(d)) => TripView::Day(d),
        (Some(first), None) => TripView::List(first),
        (None, None) => TripView::Day(format::today()),
    };
    let opts = ViewOpts {
        view,
        vehicle: q.voertuig.trim().parse().ok(),
        open_form: !q.nieuw.is_empty(),
    };
    render_month(&st, company.id, l, &user, opts, None, Errors::new()).await
}

pub(super) async fn create(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Form(f): Form<TripForm>,
) -> AppResult<Response> {
    let (_, errors, prepared) = work_out(&st.pool, company.id, &f, None, "").await?;
    let fail = |errors: Errors, f: TripForm, l: Layout| async {
        let opts = ViewOpts {
            view: TripView::Day(format::parse_date(&f.date).unwrap_or_else(format::today)),
            vehicle: None,
            open_form: true,
        };
        let page = render_month(&st, company.id, l, &user, opts, Some(f), errors).await?;
        Ok::<_, AppError>((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
    };
    let Some(p) = prepared else {
        return fail(errors, f, l).await;
    };
    match insert(&st.pool, company.id, &p, Some(user.id)).await {
        Ok(ids) => {
            let km: i64 = p.km * ids.len() as i64;
            let date = format::parse_iso_date(&p.date).expect("normalised");
            flash::ok(
                &session,
                &format!(
                    "{} opgeslagen: {km} km op {}.",
                    if ids.len() == 2 {
                        "Heen- en terugrit"
                    } else {
                        "Rit"
                    },
                    format::date(date)
                ),
            )
            .await?;
            Ok(redirect(&format!(
                "/kilometers?dag={}",
                format::iso_date(date)
            )))
        }
        Err(AppError::BadRequest(msg)) => {
            let mut errors = Errors::new();
            errors.insert("odometer_start", msg);
            fail(errors, f, l).await
        }
        Err(e) => Err(e),
    }
}

pub(super) async fn suggestion(
    State(st): State<AppState>,
    _user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    headers: HeaderMap,
    Query(f): Query<TripForm>,
) -> AppResult<DepsFragment> {
    let trigger = headers
        .get("hx-trigger-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("change")
        .to_string();
    let existing = match f.entry.trim().parse::<i64>() {
        Ok(id) => {
            let mut conn = st.pool.acquire().await?;
            Some(get(&mut conn, company.id, id).await?)
        }
        Err(_) => None,
    };
    let (mut deps, _, _) = work_out(&st.pool, company.id, &f, existing.as_ref(), &trigger).await?;
    // Live hints only for what the user already touched.
    deps.errors.retain(|k, _| {
        matches!(
            *k,
            "odometer_start" | "odometer_end" | "tariff" | "client_id" | "return_trip"
        )
    });
    if deps.odometer_end.is_empty() {
        deps.errors.remove("odometer_end");
    }
    Ok(DepsFragment { deps })
}

/// Back to the trip's day after editing or deleting it.
pub(super) fn back_to_month(t: &Trip) -> String {
    format!("/kilometers?dag={}", t.date)
}

pub(super) async fn edit_page(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    Path(id): Path<i64>,
) -> AppResult<EditPage> {
    let t = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if t.archived_at.is_some() {
        return Err(AppError::NotFound);
    }
    let f = form_of(&t);
    let (deps, _, _) = work_out(&st.pool, company.id, &f, Some(&t), "").await?;
    Ok(EditPage {
        l,
        back: back_to_month(&t),
        f,
        errors: Errors::new(),
        deps,
        vehicles: vehicles::list(&st.pool, true).await?,
        routes: routes::list(&st.pool, company.id, false).await?,
        users: users::workers(&st.pool).await?,
        trip: t,
    })
}

pub(super) async fn save(
    State(st): State<AppState>,
    Page(l): Page,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
    Form(f): Form<TripForm>,
) -> AppResult<Response> {
    let t = {
        let mut conn = st.pool.acquire().await?;
        get(&mut conn, company.id, id).await?
    };
    if t.invoice_line_id.is_some() {
        return Err(AppError::BadRequest(LOCKED.into()));
    }
    if t.archived_at.is_some() {
        return Err(AppError::NotFound);
    }
    let (deps, mut errors, prepared) = work_out(&st.pool, company.id, &f, Some(&t), "").await?;
    if let Some(p) = &prepared {
        match update(&st.pool, company.id, id, p, user.id).await {
            Ok(()) => {
                flash::ok(&session, "Rit opgeslagen.").await?;
                let saved = {
                    let mut conn = st.pool.acquire().await?;
                    get(&mut conn, company.id, id).await?
                };
                return Ok(redirect(&back_to_month(&saved)));
            }
            Err(AppError::BadRequest(msg)) => {
                errors.insert("odometer_start", msg);
            }
            Err(e) => return Err(e),
        }
    }
    let page = EditPage {
        l,
        back: back_to_month(&t),
        f,
        errors,
        deps,
        vehicles: vehicles::list(&st.pool, true).await?,
        routes: routes::list(&st.pool, company.id, false).await?,
        users: users::workers(&st.pool).await?,
        trip: t,
    };
    Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
}

pub(super) async fn remove(
    State(st): State<AppState>,
    CurrentCompany(company): CurrentCompany,
    user: AuthUser,
    session: Session,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let t = archive(&st.pool, company.id, id, user.id).await?;
    flash::ok(
        &session,
        &format!("Rit van {} km verwijderd (bewaard in het archief).", t.km),
    )
    .await?;
    Ok(redirect(&back_to_month(&t)))
}

#[derive(Template, WebTemplate)]
#[template(path = "trips/year.html")]
pub(super) struct YearPage {
    l: Layout,
    year: i32,
    vehicles: Vec<VehicleYear>,
    rate_cents: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct YearQuery {
    #[serde(default)]
    jaar: Option<i32>,
    #[serde(default)]
    voertuig: String,
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
        vehicles: year_overview(&st.pool, company.id, year, q.voertuig.trim().parse().ok()).await?,
        rate_cents: crate::modules::mileage_rates::for_year(&st.pool, year).await?,
        year,
    })
}

/// Rittenregistratie for one vehicle and year, this company's trips only.
pub(super) async fn trip_log_csv(
    State(st): State<AppState>,
    _user: AuthUser,
    CurrentCompany(company): CurrentCompany,
    Query(q): Query<YearQuery>,
) -> AppResult<Response> {
    let year = q
        .jaar
        .filter(|y| (2000..=2100).contains(y))
        .unwrap_or_else(|| format::today().year());
    let vehicle_id: i64 = q
        .voertuig
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("Kies een voertuig.".into()))?;
    let v = {
        let mut conn = st.pool.acquire().await?;
        vehicles::get(&mut conn, vehicle_id).await?
    };
    let rows = rows_between(
        &st.pool,
        company.id,
        Some(vehicle_id),
        &format!("{year:04}-01-01"),
        &format!("{year:04}-12-31"),
    )
    .await?;
    let mut buf: Vec<u8> = "\u{feff}".as_bytes().to_vec();
    {
        let mut w = csv::WriterBuilder::new()
            .delimiter(b';')
            .terminator(csv::Terminator::CRLF)
            .from_writer(&mut buf);
        let csv_err = |e: csv::Error| AppError::Internal(e.into());
        w.write_record([
            "Datum",
            "Kenteken",
            "Bestuurder",
            "Van",
            "Naar",
            "Zakelijk",
            "Doel",
            "Klant",
            "Beginstand",
            "Eindstand",
            "Kilometers",
        ])
        .map_err(csv_err)?;
        for r in &rows {
            // A download for a spreadsheet: free text can't become a formula.
            let t = |v: &str| crate::export::spreadsheet_text(v).into_owned();
            w.write_record([
                r.date.clone(),
                r.plate.clone(),
                t(&r.user_name),
                t(&r.from_address),
                t(&r.to_address),
                if r.business { "ja" } else { "nee" }.to_string(),
                t(&r.purpose),
                t(r.client_name.as_deref().unwrap_or_default()),
                r.odometer_start.map(|v| v.to_string()).unwrap_or_default(),
                r.odometer_end.map(|v| v.to_string()).unwrap_or_default(),
                r.km.to_string(),
            ])
            .map_err(csv_err)?;
        }
        w.flush().map_err(|e| AppError::Internal(e.into()))?;
    }
    let name = format!("rittenregistratie-{}-{year}.csv", v.plate.replace('-', ""));
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/csv; charset=utf-8".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        buf,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn months() {
        assert_eq!(parse_month("2026-11"), Some(date!(2026 - 11 - 01)));
        assert_eq!(parse_month("2026-13"), None);
        assert_eq!(month_param(date!(2026 - 02 - 01)), "2026-02");
        assert_eq!(month_end(date!(2026 - 02 - 01)), date!(2026 - 02 - 28));
        assert_eq!(month_end(date!(2026 - 12 - 01)), date!(2026 - 12 - 31));
    }

    #[test]
    fn km_numbers() {
        assert_eq!(parse_km("12.345"), Some(12_345));
        assert_eq!(parse_km(" 64 "), Some(64));
        assert_eq!(parse_km(""), None);
        assert_eq!(parse_km("-5"), None);
        assert_eq!(parse_km("abc"), None);
    }

    #[test]
    fn business_default() {
        assert!(TripForm::default().is_business());
        let f = TripForm {
            business_present: "1".into(),
            ..Default::default()
        };
        assert!(!f.is_business());
    }
}
