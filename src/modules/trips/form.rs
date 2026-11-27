//! The trip form: parsing, validation, favourite rides (picking one, or saving the trip as a new one) and 'heen en terug', and working out the legs and tariff snapshot (`work_out`).

use serde::Deserialize;
use sqlx::SqlitePool;

use crate::auth::users;
use crate::format;
use crate::mileage::{self, Leg};
use crate::modules::clients::{self, ClientOption};
use crate::modules::routes::{self, Route, RouteData};
use crate::modules::vehicles;
use crate::tariff;
use crate::web::form::{self, checked, opt_id, Errors};
use crate::web::AppResult;

use super::model::*;

// ---------- form ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TripForm {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub vehicle_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub route_id: String,
    #[serde(default)]
    pub from_address: String,
    #[serde(default)]
    pub to_address: String,
    #[serde(default)]
    pub odometer_start: String,
    #[serde(default)]
    pub odometer_end: String,
    #[serde(default)]
    pub km: String,
    /// Hidden field, so an unticked "zakelijk" box can be told from a fresh form.
    #[serde(default)]
    pub business_present: String,
    #[serde(default)]
    pub business: Option<String>,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub tariff: String,
    #[serde(default)]
    pub return_trip: Option<String>,
    /// "Bewaar als favoriet" (new trips only).
    #[serde(default)]
    pub save_favorite: Option<String>,
    /// Name for the new favourite; empty means "van → naar".
    #[serde(default)]
    pub favorite_name: String,
    #[serde(default)]
    pub entry: String,
}

impl TripForm {
    pub fn is_business(&self) -> bool {
        self.business_present.is_empty() || checked(&self.business)
    }
    pub fn is_return(&self) -> bool {
        checked(&self.return_trip)
    }
    pub fn is_save_favorite(&self) -> bool {
        checked(&self.save_favorite)
    }
}

/// Fields refreshed by `/kilometers/voorstel`.
pub struct Deps {
    /// The chosen car is entered as "Aantal km" (readings optional).
    pub km_only: bool,
    pub from_address: String,
    pub to_address: String,
    pub odometer_start: String,
    pub odometer_end: String,
    pub km: String,
    pub business: bool,
    pub purpose: String,
    pub clients: Vec<ClientOption>,
    pub client_id: String,
    pub tariff: String,
    pub tariffs: Vec<(String, String)>,
    pub hint: String,
    /// A new trip (not an edit): offers "heen en terug" and "bewaar als favoriet".
    pub is_new: bool,
    /// "Heen en terug" ticked (or set by picking a round-trip favourite).
    pub return_trip: bool,
    /// A favourite is picked, so there is nothing to save as one.
    pub route_chosen: bool,
    pub save_favorite: bool,
    pub favorite_name: String,
    pub errors: Errors,
}

impl Deps {
    pub(super) fn err(&self, field: &str) -> &str {
        form::err(&self.errors, field)
    }
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub vehicle_id: i64,
    pub user_id: i64,
    pub date: String,
    pub from_address: String,
    pub to_address: String,
    pub purpose: String,
    pub client_id: Option<i64>,
    /// Odometer readings, when given (always for odometer-mode cars).
    pub readings: Option<(i64, i64)>,
    pub km: i64,
    pub business: bool,
    pub tariff: Option<(i64, i64, i64)>,
    pub route_id: Option<i64>,
    pub return_trip: bool,
    /// Save the trip as a new favourite too (in the same transaction).
    pub favorite: Option<RouteData>,
}

pub(super) fn parse_km(s: &str) -> Option<i64> {
    let digits: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '.')
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok().filter(|v: &i64| *v >= 0)
}

/// Work out prefills, validation and the trip(s) to save. `trigger` is the
/// name of the field that changed (htmx `HX-Trigger-Name`).
pub(super) async fn work_out(
    pool: &SqlitePool,
    company_id: i64,
    f: &TripForm,
    existing: Option<&Trip>,
    trigger: &str,
) -> AppResult<(Deps, Errors, Option<Prepared>)> {
    let mut e = Errors::new();
    let mut conn = pool.acquire().await?;

    let date = format::parse_date(&f.date);
    if date.is_none() {
        e.insert("date", "Vul een geldige datum in.".into());
    }
    let date_s = date
        .map(format::iso_date)
        .unwrap_or_else(|| format::iso_date(format::today()));

    let vehicle = match opt_id(&f.vehicle_id)? {
        Some(id) => {
            let v = vehicles::get(&mut conn, id).await?;
            if v.archived_at.is_some() && existing.is_none_or(|t| t.vehicle_id != id) {
                e.insert("vehicle_id", "Dit voertuig is gearchiveerd.".into());
            }
            Some(v)
        }
        None => {
            e.insert("vehicle_id", "Kies een voertuig.".into());
            None
        }
    };
    let legs = match &vehicle {
        Some(v) => legs(&mut conn, v.id).await?,
        None => Vec::new(),
    };
    let km_only = vehicle.as_ref().is_some_and(|v| v.km_only());

    let route: Option<Route> = match opt_id(&f.route_id)? {
        Some(id) => Some(routes::get(&mut conn, company_id, id).await?),
        None => None,
    };

    // Prefills.
    let mut from_address = f.from_address.trim().to_string();
    let mut to_address = f.to_address.trim().to_string();
    let mut start_s = f.odometer_start.trim().to_string();
    let mut end_s = f.odometer_end.trim().to_string();
    let mut km_s = f.km.trim().to_string();
    let others: Vec<Leg> = legs
        .iter()
        .filter(|l| existing.is_none_or(|t| t.id != l.id))
        .cloned()
        .collect();
    if km_only && trigger == "vehicle_id" && existing.is_none() {
        // Switched to a km-only car: readings are optional, start empty.
        start_s.clear();
        end_s.clear();
    } else if !km_only && (trigger == "vehicle_id" || start_s.is_empty()) && existing.is_none() {
        if let Some(r) = mileage::last_reading(&others) {
            start_s = r.to_string();
            if trigger == "vehicle_id" {
                end_s.clear();
            }
        }
    }
    let mut return_trip = f.is_return() && existing.is_none();
    if trigger == "route_id" {
        if let Some(r) = &route {
            from_address = r.from_address.clone();
            to_address = r.to_address.clone();
            km_s = r.km.to_string();
            end_s.clear();
            // The favourite decides: one way or there and back.
            return_trip = r.round_trip && existing.is_none();
        }
    }

    // Odometer and km.
    let start = parse_km(&start_s);
    let mut end = parse_km(&end_s);
    let km_in = parse_km(&km_s);
    match (start, end, km_in) {
        (Some(s), None, Some(k)) => {
            end = Some(s + k);
            end_s = (s + k).to_string();
        }
        (Some(s), Some(en), _) => km_s = (en - s).to_string(),
        _ => {}
    }
    let km = match (start, end) {
        (Some(s), Some(en)) => Some(en - s),
        (None, _) if km_only => km_in,
        _ => None,
    };
    if km_only && start.is_none() && !end_s.is_empty() {
        e.insert(
            "odometer_start",
            "Vul ook de beginstand in, of laat de eindstand leeg.".into(),
        );
    } else if start.is_none() && !km_only {
        e.insert(
            "odometer_start",
            "Vul de beginstand van de kilometerteller in.".into(),
        );
    }
    match km {
        Some(k) if (1..=MAX_KM).contains(&k) => {}
        Some(k) if k <= 0 => {
            e.insert(
                "odometer_end",
                "De eindstand moet hoger zijn dan de beginstand.".into(),
            );
        }
        Some(_) => {
            e.insert(
                "odometer_end",
                "Meer dan 2.000 km in één rit: controleer de kilometerstand.".into(),
            );
        }
        None if km_only => {
            e.insert("km", "Vul het aantal kilometers in.".into());
        }
        None => {
            e.insert(
                "odometer_end",
                "Vul de eindstand of het aantal kilometers in.".into(),
            );
        }
    }

    if from_address.is_empty() {
        e.insert("from_address", "Vul het vertrekadres in.".into());
    }
    if to_address.is_empty() {
        e.insert("to_address", "Vul het bestemmingsadres in.".into());
    }
    let business = f.is_business();
    let purpose = f.purpose.trim().to_string();
    if business && purpose.is_empty() && trigger.is_empty() {
        e.insert("purpose", "Vul het doel van de zakelijke rit in.".into());
    }

    let client_id = opt_id(&f.client_id)?;
    if let Some(cid) = client_id {
        let c = clients::get(&mut conn, company_id, cid).await?;
        if c.archived_at.is_some() && existing.is_none_or(|t| t.client_id != Some(cid)) {
            e.insert("client_id", format!("Klant {} is gearchiveerd.", c.name));
        }
    }

    // Km tariff: only when chosen.
    let candidates = tariff::candidates(&mut conn, company_id, "km").await?;
    let tariff_id = opt_id(&f.tariff)?;
    let chosen = match tariff_id {
        None => None,
        Some(id) => {
            let keep = existing.filter(|t| t.tariff_id == Some(id) && t.date == date_s);
            if let Some(t) = keep {
                Some((id, t.price_cents.unwrap_or(0), t.vat_bp.unwrap_or(0)))
            } else {
                let t = crate::modules::tariffs::get(&mut conn, company_id, id).await?;
                match tariff::version_on(&candidates, t.lineage_id, &date_s) {
                    Some(v) if v.fits(client_id, None) => Some((v.id, v.price_cents, v.vat_bp)),
                    Some(_) => {
                        e.insert("tariff", "Dit km-tarief hoort bij een andere klant.".into());
                        None
                    }
                    None => {
                        e.insert("tariff", "Dit km-tarief geldt niet op deze datum.".into());
                        None
                    }
                }
            }
        }
    };
    if chosen.is_some() && !business {
        e.insert(
            "tariff",
            "Alleen zakelijke ritten kunnen worden doorbelast.".into(),
        );
    }
    if chosen.is_some() && client_id.is_none() {
        e.insert(
            "client_id",
            "Kies de klant aan wie de kilometers worden doorbelast.".into(),
        );
    }
    let tariffs: Vec<(String, String)> = candidates
        .iter()
        .filter(|c| c.valid_on(&date_s) && c.fits(client_id, None))
        .map(|c| {
            (
                c.id.to_string(),
                format!("{} — {} per km", c.name, format::amount(c.price_cents)),
            )
        })
        .collect();

    // Continuity (also for the return trip).
    let mut hint = String::new();
    if let (Some(v), Some(s), Some(en)) = (&vehicle, start, end) {
        let gap = mileage::gap_before(s, &others);
        if gap > 0 && !km_only {
            hint = format!("{gap} km sinds de vorige rit met deze auto (telt als privé).");
        }
        let exclude = existing.map(|t| t.id);
        if let Some(c) = mileage::check(company_id, &date_s, s, en, exclude, &others) {
            e.insert("odometer_start", c.message());
        } else if return_trip {
            let mut with_first = others.clone();
            with_first.push(Leg {
                id: -1,
                company_id,
                date: date_s.clone(),
                start: Some(s),
                end: Some(en),
                km: en - s,
                business,
            });
            if let Some(c) =
                mileage::check(company_id, &date_s, en, en + (en - s), None, &with_first)
            {
                e.insert("return_trip", format!("Terugrit: {}", c.message()));
            }
        }
        let _ = v;
    }

    // "Bewaar als favoriet": a new favourite from this trip.
    let save_favorite = f.is_save_favorite() && existing.is_none() && route.is_none();
    let favorite_name = f.favorite_name.trim().to_string();
    let mut favorite = None;
    if save_favorite {
        let name = if favorite_name.is_empty() {
            format!("{from_address} → {to_address}")
        } else {
            favorite_name.clone()
        };
        if routes::name_taken(&mut conn, company_id, &name, None).await? {
            e.insert("favorite_name", routes::NAME_TAKEN.into());
        }
        favorite = Some(RouteData {
            name,
            from_address: from_address.clone(),
            to_address: to_address.clone(),
            km: km.unwrap_or(0),
            round_trip: return_trip,
        });
    }

    drop(conn);
    let user_id = match f.user_id.trim().parse::<i64>() {
        // Only medewerkers drive; a beheerder books on their behalf.
        Ok(id) if users::is_worker(pool, id).await? => Some(id),
        _ => {
            e.insert("user_id", "Kies een bestuurder.".into());
            None
        }
    };

    let deps = Deps {
        km_only,
        from_address: from_address.clone(),
        to_address: to_address.clone(),
        odometer_start: start_s,
        odometer_end: end_s,
        km: km_s,
        business,
        purpose: purpose.clone(),
        clients: clients::options(pool, company_id).await?,
        client_id: client_id.map(|c| c.to_string()).unwrap_or_default(),
        tariff: tariff_id.map(|t| t.to_string()).unwrap_or_default(),
        tariffs,
        hint,
        is_new: existing.is_none(),
        return_trip,
        route_chosen: route.is_some(),
        save_favorite,
        favorite_name,
        errors: e.clone(),
    };
    let prepared = match (e.is_empty(), vehicle, user_id, km) {
        (true, Some(v), Some(user_id), Some(km)) => Some(Prepared {
            vehicle_id: v.id,
            user_id,
            date: date_s,
            from_address,
            to_address,
            purpose,
            client_id,
            readings: start.zip(end),
            km,
            business,
            tariff: chosen,
            route_id: route.map(|r| r.id),
            return_trip,
            favorite,
        }),
        _ => None,
    };
    Ok((deps, e, prepared))
}

pub(super) fn form_of(t: &Trip) -> TripForm {
    let s = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
    TripForm {
        date: t.date.clone(),
        vehicle_id: t.vehicle_id.to_string(),
        user_id: t.user_id.to_string(),
        route_id: s(t.route_id),
        from_address: t.from_address.clone(),
        to_address: t.to_address.clone(),
        odometer_start: s(t.odometer_start),
        odometer_end: s(t.odometer_end),
        km: t.km.to_string(),
        business_present: "1".into(),
        business: t.business.then(|| "1".into()),
        purpose: t.purpose.clone(),
        client_id: s(t.client_id),
        tariff: s(t.tariff_id),
        return_trip: None,
        save_favorite: None,
        favorite_name: String::new(),
        entry: t.id.to_string(),
    }
}
