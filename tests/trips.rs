mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::TestApp;
use kwartje::modules::vehicles::{self, VehicleData};

struct Fx {
    app: TestApp,
    company: i64,
    vehicle: i64,
    client: i64,
    km_tariff: i64,
    user: i64,
}

async fn fx() -> Fx {
    let (mut app, company) = TestApp::with_company().await;
    let user = app.count("SELECT MIN(id) FROM users").await;
    let vehicle = vehicles::insert(
        &app.pool,
        &VehicleData {
            name: "Golf".into(),
            plate: "AB-123-C".into(),
            kind: "private".into(),
            odometer_mode: "odometer".into(),
        },
        None,
    )
    .await
    .unwrap();
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant A")]).await;
    let client = app.last_id("clients").await;
    app.get("/tarieven/nieuw").await;
    app.post(
        "/tarieven",
        &[
            ("name", "Km"),
            ("unit", "km"),
            ("price", "0,23"),
            ("vat_bp", "2100"),
            ("valid_from", "2026-01-01"),
        ],
    )
    .await;
    let km_tariff = app.last_id("tariffs").await;
    Fx {
        app,
        company,
        vehicle,
        client,
        km_tariff,
        user,
    }
}

impl Fx {
    async fn trip(&mut self, extra: &[(&str, &str)]) -> common::Resp {
        self.app.get("/kilometers?maand=2026-11").await;
        let (v, u) = (self.vehicle.to_string(), self.user.to_string());
        let mut f: Vec<(&str, &str)> = vec![
            ("date", "2026-11-10"),
            ("vehicle_id", &v),
            ("user_id", &u),
            ("from_address", "Kantoorweg 1, Utrecht"),
            ("to_address", "Stadionlaan 2, Arnhem"),
            ("business_present", "1"),
            ("business", "1"),
            ("purpose", "Wedstrijd bekijken"),
        ];
        for (k, val) in extra {
            f.retain(|(kk, _)| kk != k);
            f.push((k, val));
        }
        let f: Vec<(&str, &str)> = f.into_iter().filter(|(_, v)| *v != "__remove").collect();
        self.app.post("/kilometers", &f).await
    }

    async fn rows(&self) -> Vec<(String, String, i64, i64, i64, bool, Option<i64>)> {
        sqlx::query_as(
            "SELECT from_address, to_address, odometer_start, odometer_end, km, business, price_cents
             FROM mileage_entries WHERE archived_at IS NULL ORDER BY odometer_start",
        )
        .fetch_all(&self.app.pool)
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn trip_with_km_or_end_reading_and_return_trip() {
    let mut x = fx().await;
    let r = x.trip(&[("odometer_start", "10.000"), ("km", "64")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location.as_deref(), Some("/kilometers?dag=2026-11-10"));
    let r = x
        .trip(&[
            ("date", "2026-11-11"),
            ("odometer_start", "10100"),
            ("km", "64"),
            ("return_trip", "1"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let rows = x.rows().await;
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].2, rows[0].3, rows[0].4), (10_000, 10_064, 64));
    // return trip: continues the odometer, addresses swapped
    assert_eq!((rows[1].2, rows[1].3), (10_100, 10_164));
    assert_eq!((rows[2].2, rows[2].3), (10_164, 10_228));
    assert_eq!(rows[2].0, "Stadionlaan 2, Arnhem");
    assert_eq!(rows[2].1, "Kantoorweg 1, Utrecht");

    let page = x.app.get("/kilometers?maand=2026-11").await;
    assert!(page
        .body
        .contains("Heen- en terugrit opgeslagen: 128 km op 11-11-2026."));
    let page = x.app.get("/kilometers?maand=2026-11").await;
    assert!(
        page.body.contains("36 km niet geregistreerd (privé)"),
        "gap 10064 → 10100"
    );
    assert!(page.body.contains("192 km"), "business km this month");
    // the form is prefilled with the last reading
    assert!(page
        .body
        .contains("name=\"odometer_start\" value=\"10228\""));
}

#[tokio::test]
async fn prefill_fragment_uses_last_reading_and_route() {
    let mut x = fx().await;
    x.trip(&[("odometer_start", "5000"), ("odometer_end", "5050")])
        .await;
    x.app.get("/routes/nieuw").await;
    x.app
        .post(
            "/routes",
            &[
                ("name", "Naar Arnhem"),
                ("from_address", "Thuis"),
                ("to_address", "Arnhem"),
                ("km", "64"),
            ],
        )
        .await;
    let route = x.app.last_id("saved_routes").await;
    let url = format!(
        "/kilometers/voorstel?date=2026-11-12&vehicle_id={}&route_id={route}&odometer_start=",
        x.vehicle
    );
    let req = Request::get(&url)
        .header("hx-trigger-name", "route_id")
        .body(Body::empty())
        .unwrap();
    let r = x.app.send(req).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("value=\"Thuis\"") && r.body.contains("value=\"Arnhem\""));
    assert!(
        r.body.contains("name=\"odometer_start\" value=\"5050\""),
        "{}",
        r.body
    );
    assert!(r.body.contains("name=\"odometer_end\" value=\"5114\""));
}

#[tokio::test]
async fn overlap_with_another_companys_trip_is_refused_without_details() {
    let mut x = fx().await;
    x.trip(&[("odometer_start", "1000"), ("km", "50")]).await;
    // company Beta uses the same car
    let beta = x.app.create_company("Beta B.V.", "BB").await;
    x.app.select_company(beta).await;
    let r = x
        .trip(&[
            ("date", "2026-11-12"),
            ("odometer_start", "1050"),
            ("km", "100"),
            ("purpose", "Geheime klant"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    x.app.select_company(x.company).await;

    // prefill continues after Beta's trip
    let page = x.app.get("/kilometers?maand=2026-11").await;
    assert!(page.body.contains("name=\"odometer_start\" value=\"1150\""));
    assert!(
        !page.body.contains("Geheime klant"),
        "other company's trips are not listed"
    );

    let r = x
        .trip(&[
            ("date", "2026-11-13"),
            ("odometer_start", "1100"),
            ("km", "20"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        r.body
            .contains("overlapt met een rit van 12-11-2026 (ander bedrijf)"),
        "{}",
        r.body
    );
    assert!(!r.body.contains("Geheime klant"));
    // date order: a later date with a lower reading
    let r = x
        .trip(&[
            ("date", "2026-11-20"),
            ("odometer_start", "900"),
            ("km", "20"),
        ])
        .await;
    assert!(r.body.contains("past niet bij de datum"));
    assert_eq!(x.app.count("SELECT COUNT(*) FROM mileage_entries").await, 2);
}

#[tokio::test]
async fn billing_needs_explicit_tariff_client_and_business() {
    let mut x = fx().await;
    let (t, c) = (x.km_tariff.to_string(), x.client.to_string());
    let r = x
        .trip(&[("odometer_start", "100"), ("km", "10"), ("tariff", &t)])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r
        .body
        .contains("Kies de klant aan wie de kilometers worden doorbelast."));
    let r = x
        .trip(&[
            ("odometer_start", "100"),
            ("km", "10"),
            ("tariff", &t),
            ("client_id", &c),
            ("business", "__remove"),
        ])
        .await;
    assert!(r
        .body
        .contains("Alleen zakelijke ritten kunnen worden doorbelast."));
    let r = x
        .trip(&[("odometer_start", "100"), ("km", "10"), ("purpose", "")])
        .await;
    assert!(r.body.contains("Vul het doel van de zakelijke rit in."));

    // private trip without purpose: fine, never billable
    let r = x
        .trip(&[
            ("odometer_start", "100"),
            ("km", "10"),
            ("purpose", ""),
            ("business", "__remove"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // billable trip: price snapshot
    let r = x
        .trip(&[
            ("date", "2026-11-11"),
            ("odometer_start", "110"),
            ("km", "100"),
            ("tariff", &t),
            ("client_id", &c),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let rows = x.rows().await;
    assert!(!rows[0].5);
    assert_eq!(rows[0].6, None);
    assert_eq!(rows[1].6, Some(23));
    let page = x.app.get("/kilometers?maand=2026-11").await;
    let page = if page.body.contains("Rit opgeslagen") {
        x.app.get("/kilometers?maand=2026-11").await
    } else {
        page
    };
    assert!(page.body.contains("€ 23,00"), "100 km × € 0,23");
}

#[tokio::test]
async fn edit_archive_lock_and_company_scope() {
    let mut x = fx().await;
    x.trip(&[("odometer_start", "100"), ("km", "10")]).await;
    let id = x.app.last_id("mileage_entries").await;
    x.app.get(&format!("/kilometers/{id}")).await;
    let (v, u) = (x.vehicle.to_string(), x.user.to_string());
    let r = x
        .app
        .post(
            &format!("/kilometers/{id}"),
            &[
                ("date", "2026-11-10"),
                ("vehicle_id", &v),
                ("user_id", &u),
                ("from_address", "A"),
                ("to_address", "B"),
                ("odometer_start", "100"),
                ("odometer_end", "130"),
                ("business_present", "1"),
                ("business", "1"),
                ("purpose", "Gesprek"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(x.rows().await[0].4, 30);

    // another company cannot see it
    let beta = x.app.create_company("Beta", "BB").await;
    x.app.select_company(beta).await;
    assert_eq!(
        x.app.get(&format!("/kilometers/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    x.app.select_company(x.company).await;

    // archive → excluded, prefill falls back
    x.app.get(&format!("/kilometers/{id}")).await;
    x.app
        .post(&format!("/kilometers/{id}/verwijderen"), &[])
        .await;
    assert!(x.rows().await.is_empty());

    // locked once invoiced
    x.trip(&[("odometer_start", "200"), ("km", "10")]).await;
    let id = x.app.last_id("mileage_entries").await;
    sqlx::query("UPDATE mileage_entries SET invoice_line_id = 1 WHERE id = ?")
        .bind(id)
        .execute(&x.app.pool)
        .await
        .unwrap();
    let r = x
        .app
        .post(&format!("/kilometers/{id}/verwijderen"), &[])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'mileage_entries' ORDER BY id",
    )
    .fetch_all(&x.app.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["insert", "update", "archive", "insert"]);
}

#[tokio::test]
async fn year_overview_balances_with_the_odometer() {
    let mut x = fx().await;
    let (t, c) = (x.km_tariff.to_string(), x.client.to_string());
    x.trip(&[
        ("date", "2026-03-02"),
        ("odometer_start", "10000"),
        ("km", "100"),
    ])
    .await;
    x.trip(&[
        ("date", "2026-11-10"),
        ("odometer_start", "10400"),
        ("km", "64"),
        ("return_trip", "1"),
        ("tariff", &t),
        ("client_id", &c),
    ])
    .await;
    x.trip(&[
        ("date", "2026-11-12"),
        ("odometer_start", "10528"),
        ("km", "20"),
        ("purpose", ""),
        ("business", "__remove"),
    ])
    .await;
    // another company drove 50 km in between
    let beta = x.app.create_company("Beta", "BB").await;
    x.app.select_company(beta).await;
    x.trip(&[
        ("date", "2026-11-15"),
        ("odometer_start", "10548"),
        ("km", "50"),
    ])
    .await;
    x.app.select_company(x.company).await;

    // no rate yet → warning
    let r = x.app.get("/kilometers/jaar?jaar=2026").await;
    assert!(r
        .body
        .contains("Er is nog geen kilometervergoeding voor 2026"));
    x.app
        .post(
            "/kilometervergoeding",
            &[("year", "2026"), ("rate", "0,23")],
        )
        .await;
    x.app.get("/").await; // flash
    let r = x.app.get("/kilometers/jaar?jaar=2026").await;
    assert_eq!(r.status, StatusCode::OK);
    let b = &r.body;
    assert!(b.contains("228 km"), "business: 100 + 64 + 64");
    assert!(b.contains("<td class=\"num\">20 km</td>"), "private logged");
    assert!(
        b.contains("<td class=\"num\">50 km</td>"),
        "other companies"
    );
    assert!(
        b.contains("<td class=\"num\">300 km</td>"),
        "gap 10100 → 10400"
    );
    assert!(b.contains("598 km"), "odometer 10000 → 10598");
    assert!(b.contains("✓"), "balance matches the odometer");
    assert!(b.contains("€ 52,44"), "228 km × € 0,23");

    // rittenregistratie CSV: this company's trips only
    let r = x
        .app
        .get(&format!(
            "/kilometers/rittenregistratie.csv?jaar=2026&voertuig={}",
            x.vehicle
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.starts_with('\u{feff}'));
    let lines: Vec<&str> = r.body.trim_start_matches('\u{feff}').lines().collect();
    assert!(lines[0].starts_with("Datum;Kenteken;Bestuurder;Van;Naar;Zakelijk"));
    assert_eq!(lines.len(), 1 + 4, "header + 4 own trips");

    // Te factureren: the billed km, both legs
    let r = x.app.get("/te-factureren").await;
    assert!(r.body.contains("Kilometers"));
    assert!(r.body.contains("128 km"));
    assert!(r.body.contains("€ 29,44"), "128 × € 0,23");
    // dashboard tile (November is not necessarily the current month; just render)
    assert_eq!(x.app.get("/").await.status, StatusCode::OK);
}

async fn km_mode(x: &Fx, mode: &str) {
    sqlx::query("UPDATE vehicles SET odometer_mode = ? WHERE id = ?")
        .bind(mode)
        .bind(x.vehicle)
        .execute(&x.app.pool)
        .await
        .unwrap();
}

async fn readings(x: &Fx) -> Vec<(Option<i64>, Option<i64>, i64)> {
    sqlx::query_as(
        "SELECT odometer_start, odometer_end, km FROM mileage_entries
         WHERE archived_at IS NULL ORDER BY id",
    )
    .fetch_all(&x.app.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn private_car_in_km_mode_needs_no_readings() {
    let mut x = fx().await;
    km_mode(&x, "km").await;
    // the form asks for km, the beginstand is optional
    let page = x.app.get("/kilometers?dag=2026-11-10").await;
    assert!(page.body.contains("Aantal km *"), "{}", page.body);
    assert!(page.body.contains("(optioneel)"));

    let r = x.trip(&[("km", "42")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // with a beginstand the eindstand is worked out
    let r = x
        .trip(&[
            ("date", "2026-11-11"),
            ("odometer_start", "8000"),
            ("km", "15"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // heen en terug without readings
    let r = x
        .trip(&[("date", "2026-11-12"), ("km", "10"), ("return_trip", "1")])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        readings(&x).await,
        vec![
            (None, None, 42),
            (Some(8000), Some(8015), 15),
            (None, None, 10),
            (None, None, 10)
        ]
    );
    // no km: refused
    let r = x.trip(&[("date", "2026-11-13")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Vul het aantal kilometers in."));

    // no gap hints for a km-only car, the totals count every trip
    let page = x.app.get("/kilometers?maand=2026-11").await;
    assert!(!page.body.contains("niet geregistreerd"), "{}", page.body);
    assert!(page.body.contains("77 km"));
    let year = x.app.get("/kilometers/jaar?jaar=2026").await;
    assert!(
        year.body.contains("Totaal ingevoerd")
            || year.body.contains("Waarvan zonder kilometerstand"),
        "{}",
        year.body
    );
    assert!(year.body.contains("77 km"));
    let csv = x
        .app
        .get(&format!(
            "/kilometers/rittenregistratie.csv?jaar=2026&voertuig={}",
            x.vehicle
        ))
        .await;
    assert_eq!(csv.status, StatusCode::OK);
}

#[tokio::test]
async fn km_only_trips_never_block_odometer_trips() {
    let mut x = fx().await;
    km_mode(&x, "km").await;
    x.trip(&[("km", "30")]).await;
    // switch the car to readings: a trip with any range is still accepted
    km_mode(&x, "odometer").await;
    let page = x.app.get("/kilometers?dag=2026-11-10").await;
    assert!(page.body.contains("Beginstand *"));
    let r = x
        .trip(&[("odometer_start", "1000"), ("odometer_end", "1030")])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // odometer mode: an empty beginstand continues from the last reading
    let r = x.trip(&[("date", "2026-11-11"), ("km", "5")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        readings(&x).await.last().copied(),
        Some((Some(1030), Some(1035), 5))
    );
}

#[tokio::test]
async fn vehicle_form_sets_the_mode_and_business_cars_keep_readings() {
    let mut x = fx().await;
    x.app.get("/voertuigen/nieuw").await;
    let r = x
        .app
        .post(
            "/voertuigen",
            &[
                ("name", "Privé"),
                ("plate", "PR-111-V"),
                ("kind", "private"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    x.app.get("/voertuigen/nieuw").await;
    x.app
        .post(
            "/voertuigen",
            &[
                ("name", "Zaak"),
                ("plate", "ZA-222-K"),
                ("kind", "business"),
                ("odometer_mode", "km"),
            ],
        )
        .await;
    let modes: Vec<(String, String)> = sqlx::query_as(
        "SELECT plate, odometer_mode FROM vehicles WHERE plate IN ('PR-111-V','ZA-222-K') ORDER BY plate",
    )
    .fetch_all(&x.app.pool)
    .await
    .unwrap();
    assert_eq!(
        modes,
        vec![
            ("PR-111-V".to_string(), "km".to_string()),
            ("ZA-222-K".to_string(), "odometer".to_string())
        ]
    );
    let list = x.app.get("/voertuigen").await;
    assert!(list.body.contains("Aantal km") && list.body.contains("Kilometerstanden"));
}
