//! `kwartje seed`: fake data for local development. Never real client data.

use anyhow::{bail, Result};
use sqlx::SqlitePool;

use time::Duration;

use crate::auth::users;
use crate::modules::clients::{self, ClientData};
use crate::modules::companies::{self, CompanyData};
use crate::modules::hours::{self, Prepared};
use crate::modules::tariffs::{self, TariffData};
use crate::modules::tasks::{self, TaskData};
use crate::{format, tariff};

/// Password of the seeded users (development only).
pub const DEV_PASSWORD: &str = "ontwikkelen-123";

pub struct SeedResult {
    pub users: Vec<String>,
    pub companies: Vec<String>,
    pub entries: usize,
    pub trips: usize,
    pub documents: usize,
}

fn company(
    name: &str,
    prefix: &str,
    kvk: &str,
    btw: &str,
    iban: &str,
    city: &str,
    rounding: i64,
) -> CompanyData {
    CompanyData {
        name: name.into(),
        kvk: kvk.into(),
        btw_id: btw.into(),
        iban: iban.into(),
        street: "Voorbeeldstraat 1".into(),
        postal_code: "1234 AB".into(),
        city: city.into(),
        email: format!("info@{}.example", prefix.to_lowercase()),
        phone: "030 123 45 67".into(),
        invoice_prefix: prefix.into(),
        payment_term_days: 14,
        default_vat_bp: 2100,
        rounding_minutes: rounding,
    }
}

/// Insert fake users and companies. Refuses on a non-empty database unless
/// `force` (which only adds what is missing).
pub async fn seed(
    pool: &SqlitePool,
    data_dir: &std::path::Path,
    force: bool,
) -> Result<SeedResult> {
    let n_users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    let n_companies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM companies")
        .fetch_one(pool)
        .await?;
    if (n_users > 0 || n_companies > 0) && !force {
        bail!("de database is niet leeg; gebruik --force om alleen ontbrekende testgegevens toe te voegen");
    }

    let mut out = SeedResult {
        users: Vec::new(),
        companies: Vec::new(),
        entries: 0,
        trips: 0,
        documents: 0,
    };
    // Anna is the beheerder (books on behalf of Bram), Bram the
    // medewerker, plus a read-only boekhouder for the first company.
    let mut accountant = None;
    for (name, email, role) in [
        ("Anna (test)", "anna@example.test", users::Role::Admin),
        ("Bram (test)", "bram@example.test", users::Role::Employee),
        (
            "Boekhouder (test)",
            "boekhouder@example.test",
            users::Role::Accountant,
        ),
    ] {
        if users::find_by_email(pool, email).await?.is_none() {
            let id = users::create(pool, name, email, DEV_PASSWORD, None).await?;
            users::set_role(pool, id, role, None).await?;
            if role == users::Role::Accountant {
                accountant = Some(id);
            }
            out.users.push(email.to_string());
        }
    }

    let data = [
        company(
            "Voorbeeld Advies B.V.",
            "VA",
            "12345678",
            "NL123456789B01",
            "NL91 ABNA 0417 1643 00",
            "Utrecht",
            15,
        ),
        company(
            "Testbedrijf Bakker",
            "TB",
            "87654321",
            "NL987654321B01",
            "NL20 INGB 0001 2345 67",
            "Amersfoort",
            0,
        ),
    ];
    let existing: Vec<String> = sqlx::query_scalar("SELECT invoice_prefix FROM companies")
        .fetch_all(pool)
        .await?;
    let mut first_new = None;
    for d in data {
        if !existing.contains(&d.invoice_prefix) {
            let id = companies::insert(pool, &d, None)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            first_new.get_or_insert(id);
            out.companies.push(d.name);
        }
    }
    if let (Some(id), Some(company_id)) = (accountant, first_new) {
        users::set_company_ids(pool, id, &[company_id], None).await?;
    }
    if let Some(company_id) = first_new {
        out.entries = seed_business(pool, company_id).await?;
        out.trips = seed_trips(pool, company_id).await?;
        out.documents = seed_documents(pool, data_dir, company_id).await?;
        seed_quote_and_recurring(pool, company_id).await?;
    }
    Ok(out)
}

fn app(e: crate::web::AppError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Clients, tariffs, tasks and two weeks of hours for one company.
async fn seed_business(pool: &SqlitePool, company_id: i64) -> Result<usize> {
    let client = |name: &str, contact: &str, city: &str, prefix: &str| ClientData {
        invoice_prefix: Some(prefix.into()),
        name: name.into(),
        contact_name: contact.into(),
        email: format!("{}@voorbeeld.example", contact.to_lowercase()),
        city: city.into(),
        street: "Sportlaan 10".into(),
        postal_code: "3584 AA".into(),
        ..Default::default()
    };
    let sportclub = clients::insert(
        pool,
        company_id,
        &client("Sportclub De Vliegende Hollander", "Anouk", "Utrecht", "VH"),
        None,
    )
    .await
    .map_err(app)?;
    let atletiek = clients::insert(
        pool,
        company_id,
        &client("Atletiekvereniging Snel", "Bram", "Zeist", "AS"),
        None,
    )
    .await
    .map_err(app)?;
    let academie = clients::insert(
        pool,
        company_id,
        &client("Jeugdacademie Oost", "Carla", "Amersfoort", "JO"),
        None,
    )
    .await
    .map_err(app)?;
    let transfer =
        crate::modules::projects::insert(pool, company_id, sportclub, "Transfer zomer 2026", None)
            .await
            .map_err(app)?;

    let year_start = format!("{}-01-01", format::today().year());
    let tariff =
        |name: &str, unit: &str, cents: i64, client: Option<i64>, default: bool| TariffData {
            name: name.into(),
            unit: unit.into(),
            price_cents: cents,
            vat_bp: 2100,
            client_id: client,
            project_id: None,
            is_default: default,
            valid_from: year_start.clone(),
            valid_to: None,
        };
    let mut ids = Vec::new();
    for d in [
        tariff("Standaard uurtarief", "hour", 9500, None, true),
        tariff("Kennismaking", "hour", 0, None, false),
        tariff(
            "Tarief Vliegende Hollander",
            "hour",
            11_000,
            Some(sportclub),
            true,
        ),
        tariff("Kilometervergoeding", "km", 23, None, false),
    ] {
        let id = tariffs::insert(pool, company_id, d, None)
            .await
            .map_err(|_| anyhow::anyhow!("seeding tariffs failed"))?;
        ids.push(id);
    }
    let free = ids[1];

    let task = |name: &str, tariff_id: Option<i64>, billable: bool| TaskData {
        name: name.into(),
        client_id: None,
        project_id: None,
        tariff_id,
        billable,
    };
    let mut task_ids = Vec::new();
    for d in [
        task("Scouting", None, true),
        task("Contractonderhandeling", None, true),
        task("Kennismakingsgesprek", Some(free), true),
        task("Administratie", None, false),
    ] {
        let id = tasks::insert(pool, company_id, d, None)
            .await
            .map_err(|e| anyhow::anyhow!("seeding tasks failed: {e:?}"))?;
        task_ids.push(id);
    }
    let (scouting, contract, intro, admin) = (task_ids[0], task_ids[1], task_ids[2], task_ids[3]);

    let user_ids: Vec<i64> = users::workers(pool).await?.iter().map(|u| u.id).collect();
    let mut conn = pool.acquire().await?;
    let candidates = tariff::hour_candidates(&mut conn, company_id)
        .await
        .map_err(app)?;
    drop(conn);

    // (weekday offset, task, client, project, minutes, description)
    type Plan = (i64, i64, Option<i64>, Option<i64>, i64, &'static str);
    let week: [Plan; 9] = [
        (
            0,
            scouting,
            Some(sportclub),
            None,
            150,
            "Wedstrijd O19 bekeken",
        ),
        (0, admin, None, None, 45, "Mail en planning"),
        (
            1,
            contract,
            Some(sportclub),
            Some(transfer),
            120,
            "Voorstel contract doorgesproken",
        ),
        (
            1,
            intro,
            Some(academie),
            None,
            60,
            "Kennismaking met de technisch directeur",
        ),
        (2, scouting, Some(atletiek), None, 210, "Talentendag"),
        (
            3,
            contract,
            Some(sportclub),
            Some(transfer),
            90,
            "Onderhandeling, tweede ronde",
        ),
        (3, admin, None, None, 30, "Declaraties"),
        (
            4,
            scouting,
            Some(academie),
            None,
            180,
            "Toernooi jeugdteams",
        ),
        (4, admin, None, None, 60, "Weekafsluiting"),
    ];
    let this_monday = format::week_start(format::today());
    let mut count = 0;
    for (u_i, user_id) in user_ids.iter().enumerate() {
        for weeks_back in [1, 0] {
            let monday = this_monday - Duration::days(7 * weeks_back);
            for (n, (day, task_id, client_id, project_id, minutes, description)) in
                week.iter().enumerate()
            {
                // the second user logs a bit less
                if u_i == 1 && n % 2 == 1 {
                    continue;
                }
                let date = monday + Duration::days(*day);
                if date > format::today() {
                    continue;
                }
                let date_s = format::iso_date(date);
                let task_lineage = if *task_id == intro { Some(free) } else { None };
                let ctx = tariff::Context {
                    task_billable: *task_id != admin,
                    task_lineage,
                    client_id: *client_id,
                    project_id: *project_id,
                    date: &date_s,
                };
                let t = tariff::resolve(&ctx, &candidates)
                    .map(|(c, _)| (c.id, c.price_cents, c.vat_bp));
                let p = Prepared {
                    user_id: *user_id,
                    task_id: *task_id,
                    client_id: *client_id,
                    project_id: *project_id,
                    date: date_s,
                    minutes: *minutes,
                    description: description.to_string(),
                    tariff: t,
                };
                hours::insert(pool, company_id, &p, None)
                    .await
                    .map_err(app)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// A test car, two favourite rides (heen en terug), last month's trips (with private gaps) and
/// km allowance rates (development data only; check the real rate per year).
async fn seed_trips(pool: &SqlitePool, company_id: i64) -> Result<usize> {
    use crate::modules::{mileage_rates, routes, trips, vehicles};
    let today = format::today();
    for year in [today.year() - 1, today.year()] {
        if mileage_rates::for_year(pool, year)
            .await
            .map_err(app)?
            .is_none()
        {
            mileage_rates::set(pool, i64::from(year), 23, None)
                .await
                .map_err(app)?;
        }
    }
    let vehicle = match vehicles::list(pool, true)
        .await
        .map_err(app)?
        .into_iter()
        .find(|v| v.plate == "TS-123-K")
    {
        Some(v) => v.id,
        None => vehicles::insert(
            pool,
            &vehicles::VehicleData {
                name: "Volkswagen Golf (test)".into(),
                plate: "TS-123-K".into(),
                kind: "private".into(),
                odometer_mode: "odometer".into(),
            },
            None,
        )
        .await
        .map_err(app)?,
    };
    let office = "Kantoorweg 1, Utrecht";
    let stadium = "Stadionlaan 2, Utrecht";
    let track = "Atletiekbaan 5, Zeist";
    let r1 = routes::insert(
        pool,
        company_id,
        &routes::RouteData {
            name: "Kantoor → stadion".into(),
            from_address: office.into(),
            to_address: stadium.into(),
            km: 12,
            round_trip: true,
        },
        None,
    )
    .await
    .map_err(app)?;
    let r2 = routes::insert(
        pool,
        company_id,
        &routes::RouteData {
            name: "Kantoor → atletiekbaan".into(),
            from_address: office.into(),
            to_address: track.into(),
            km: 18,
            round_trip: true,
        },
        None,
    )
    .await
    .map_err(app)?;
    let user = users::workers(pool)
        .await?
        .last()
        .map(|u| u.id)
        .unwrap_or(1);
    let sportclub: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM clients WHERE company_id = ? AND name LIKE 'Sportclub%'",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;

    // Last month: Tue/Thu stadium, Wed athletics track, private km in between.
    let first = today.replace_day(1).expect("day 1") - Duration::days(1);
    let first = first.replace_day(1).expect("day 1");
    let mut odo = 48_000;
    let mut count = 0;
    let mut d = first;
    while d.month() == first.month() {
        let (route, to, km, purpose) = match d.weekday() {
            time::Weekday::Tuesday | time::Weekday::Thursday => {
                (r1, stadium, 12, "Wedstrijd bekijken")
            }
            time::Weekday::Wednesday => (r2, track, 18, "Talentendag"),
            _ => {
                d = d.next_day().expect("valid date");
                continue;
            }
        };
        odo += 35; // private km since the last trip
        let p = trips::Prepared {
            vehicle_id: vehicle,
            user_id: user,
            date: format::iso_date(d),
            from_address: office.into(),
            to_address: to.into(),
            purpose: purpose.into(),
            client_id: if route == r1 { sportclub } else { None },
            readings: Some((odo, odo + km)),
            km,
            business: true,
            tariff: None,
            route_id: Some(route),
            return_trip: true,
            favorite: None,
        };
        trips::insert(pool, company_id, &p, None)
            .await
            .map_err(app)?;
        odo += 2 * km;
        count += 2;
        d = d.next_day().expect("valid date");
    }
    Ok(count)
}

/// A draft quote and a monthly recurring invoice (starting this month, so
/// `serve` makes its first draft right away).
async fn seed_quote_and_recurring(pool: &SqlitePool, company_id: i64) -> Result<()> {
    let client: i64 =
        sqlx::query_scalar("SELECT id FROM clients WHERE company_id = ? ORDER BY id LIMIT 1")
            .bind(company_id)
            .fetch_one(pool)
            .await?;
    let now = crate::db::now();
    let today = format::today();
    let mut tx = pool.begin().await?;
    let valid = format::iso_date(today + Duration::days(30));
    let q = sqlx::query(
        "INSERT INTO quotes (company_id, client_id, valid_until, intro_text, created_at, updated_at)
         VALUES (?, ?, ?, 'Naar aanleiding van ons gesprek bieden wij u het volgende aan.', ?, ?)",
    )
    .bind(company_id)
    .bind(client)
    .bind(&valid)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    let mut vat_lines = Vec::new();
    for (pos, (desc, qty, unit, price)) in [
        ("Workshop strategie (dagdeel)", 2_000, "stuk", 65_000),
        ("Uitwerking adviesrapport", 12_000, "uur", 9_500),
    ]
    .into_iter()
    .enumerate()
    {
        let total =
            crate::money::line_total_cents(qty, price).map_err(|e| anyhow::anyhow!("{e}"))?;
        vat_lines.push((total, 2100));
        sqlx::query(
            "INSERT INTO quote_lines (quote_id, position, description, quantity_milli, unit, unit_price_cents,
                                      vat_bp, line_total_cents, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 2100, ?, ?, ?)",
        )
        .bind(q)
        .bind(pos as i64 + 1)
        .bind(desc)
        .bind(qty)
        .bind(unit)
        .bind(price)
        .bind(total)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    let t = crate::money::totals(&vat_lines).map_err(|e| anyhow::anyhow!("{e}"))?;
    sqlx::query(
        "UPDATE quotes SET subtotal_cents = ?, vat_cents = ?, total_cents = ? WHERE id = ?",
    )
    .bind(t.subtotal_cents)
    .bind(t.vat_cents)
    .bind(t.total_cents)
    .bind(q)
    .execute(&mut *tx)
    .await?;
    crate::audit::record_insert(
        &mut tx,
        None,
        "quotes",
        q,
        &serde_json::json!({"seed": true, "client_id": client}),
    )
    .await?;

    let start = format::iso_date(today.replace_day(1).expect("day 1"));
    let r = sqlx::query(
        "INSERT INTO recurring_invoices (company_id, client_id, name, interval, next_date, intro_text,
                                         created_at, updated_at)
         VALUES (?, ?, 'Hosting en onderhoud', 'month', ?, 'Hierbij ontvangt u de factuur voor {periode}.', ?, ?)",
    )
    .bind(company_id)
    .bind(client)
    .bind(&start)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    for (pos, (desc, price)) in [("Hosting website", 2_500), ("Onderhoud en updates", 7_500)]
        .into_iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO recurring_lines (recurring_id, position, description, quantity_milli, unit,
                                          unit_price_cents, vat_bp, created_at, updated_at)
             VALUES (?, ?, ?, 1000, 'stuk', ?, 2100, ?, ?)",
        )
        .bind(r)
        .bind(pos as i64 + 1)
        .bind(desc)
        .bind(price)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    crate::audit::record_insert(
        &mut tx,
        None,
        "recurring_invoices",
        r,
        &serde_json::json!({"seed": true, "client_id": client}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Fake purchase invoices and receipts for the current quarter (tiny PDFs).
async fn seed_documents(
    pool: &SqlitePool,
    data_dir: &std::path::Path,
    company_id: i64,
) -> Result<usize> {
    use crate::modules::documents::{store, Meta, StoreError};
    let user = users::workers(pool)
        .await?
        .first()
        .map(|u| u.id)
        .unwrap_or(1);
    let (from, _) = crate::documents::quarter_range(
        format::today().year(),
        crate::documents::quarter_of(format::today()),
    );
    // (kind, supplier, day in quarter, (total, VAT), description)
    type Item<'a> = (&'a str, &'a str, i64, Option<(i64, i64)>, &'a str);
    let items: [Item; 7] = [
        (
            "purchase_invoice",
            "KPN",
            3,
            Some((4_235, 735)),
            "Telefoonabonnement",
        ),
        (
            "purchase_invoice",
            "Ziggo",
            5,
            Some((6_050, 1_050)),
            "Internet kantoor",
        ),
        (
            "purchase_invoice",
            "Bol.com",
            12,
            Some((36_179, 6_279)),
            "Monitor",
        ),
        ("receipt", "Shell", 18, Some((7_865, 1_365)), "Tanken"),
        ("receipt", "Albert Heijn", 20, None, "Lunch met klant"),
        (
            "purchase_invoice",
            "Microsoft",
            33,
            Some((1_452, 252)),
            "Microsoft 365",
        ),
        (
            "receipt",
            "NS",
            40,
            Some((2_440, 138)),
            "Treinreis Amsterdam",
        ),
    ];
    let mut n = 0;
    for (i, (kind, supplier, day, amounts, description)) in items.iter().enumerate() {
        let date = from + Duration::days(*day);
        if date > format::today() {
            continue;
        }
        let pdf = format!("%PDF-1.4\n% testdocument {i} {supplier} {description}\n%%EOF\n");
        let meta = Meta {
            kind: kind.to_string(),
            doc_date: format::iso_date(date),
            supplier: Some(supplier.to_string()),
            description: description.to_string(),
            amount_cents: amounts.map(|a| a.0),
            vat_cents: amounts.map(|a| a.1),
            client_id: None,
        };
        let name = format!("{supplier} {description}.pdf");
        match store(
            pool,
            data_dir,
            company_id,
            &name,
            pdf.as_bytes(),
            &meta,
            user,
        )
        .await
        {
            Ok(_) | Err(StoreError::Duplicate { .. }) => n += 1,
            Err(StoreError::App(e)) => return Err(anyhow::anyhow!("{e}")),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn seeds_once_and_force_is_idempotent() {
        let pool = db::connect_memory().await.unwrap();
        db::migrate(&pool).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let r = seed(&pool, dir.path(), false).await.unwrap();
        assert_eq!(r.users.len(), 3);
        assert_eq!(r.companies.len(), 2);
        let roles: Vec<String> = sqlx::query_scalar("SELECT role FROM users ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(roles, ["admin", "employee", "accountant"]);
        let access: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_companies")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(access, 1);
        assert!(r.entries > 0);
        let billable: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM time_entries WHERE tariff_id IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(billable > 0);
        assert!(r.trips > 0);
        assert!(seed(&pool, dir.path(), false).await.is_err());
        let r = seed(&pool, dir.path(), true).await.unwrap();
        assert!(r.users.is_empty() && r.companies.is_empty());
        // Seeded values pass the same validation as the form.
        for iban in ["NL91ABNA0417164300", "NL20INGB0001234567"] {
            assert!(crate::validate::iban(iban).is_ok());
        }
    }
}
