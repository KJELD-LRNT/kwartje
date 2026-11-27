mod common;

use axum::http::StatusCode;
use common::TestApp;

struct Fx {
    app: TestApp,
    company: i64,
    client_a: i64,
    client_b: i64,
    std: i64,
    tariff_a: i64,
    scouting: i64,
    admin: i64,
    intro: i64,
    user: i64,
}

/// Company default € 95, client A default € 110, a € 0 tariff on "Kennismaking",
/// "Administratie" not billable.
async fn fx() -> Fx {
    let (mut app, company) = TestApp::with_company().await;
    let user = app.count("SELECT MIN(id) FROM users").await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Klant A")]).await;
    let client_a = app.last_id("clients").await;
    app.post("/klanten", &[("name", "Klant B")]).await;
    let client_b = app.last_id("clients").await;
    let tariff = |name: &'static str, price: &'static str, client: Option<i64>, default: bool| {
        let mut f: Vec<(String, String)> = vec![
            ("name".into(), name.into()),
            ("unit".into(), "hour".into()),
            ("price".into(), price.into()),
            ("vat_bp".into(), "2100".into()),
            ("valid_from".into(), "2025-01-01".into()),
        ];
        if let Some(c) = client {
            f.push(("client_id".into(), c.to_string()));
        }
        if default {
            f.push(("is_default".into(), "1".into()));
        }
        f
    };
    app.get("/tarieven/nieuw").await;
    let csrf = app.csrf.clone().unwrap();
    let post_tariff = |f: Vec<(String, String)>| {
        let mut f = f;
        f.push(("csrf_token".into(), csrf.clone()));
        f
    };
    for f in [
        tariff("Standaard", "95", None, true),
        tariff("Tarief A", "110", Some(client_a), true),
        tariff("Kennismaking", "0", None, false),
    ] {
        let r = app.post_raw("/tarieven", &post_tariff(f)).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM tariffs ORDER BY id")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    let (std, tariff_a, free) = (ids[0], ids[1], ids[2]);
    app.get("/taken/nieuw").await;
    app.post("/taken", &[("name", "Scouting")]).await;
    let scouting = app.last_id("tasks").await;
    app.post("/taken", &[("name", "Administratie"), ("internal", "1")])
        .await;
    let admin = app.last_id("tasks").await;
    app.post(
        "/taken",
        &[("name", "Kennismaking"), ("tariff_id", &free.to_string())],
    )
    .await;
    let intro = app.last_id("tasks").await;
    Fx {
        app,
        company,
        client_a,
        client_b,
        std,
        tariff_a,
        scouting,
        admin,
        intro,
        user,
    }
}

impl Fx {
    async fn log(&mut self, fields: &[(&str, &str)]) -> common::Resp {
        self.app.get("/uren").await;
        let user = self.user.to_string();
        let mut f: Vec<(&str, &str)> = vec![("user_id", &user), ("tariff", "auto")];
        for (k, v) in fields {
            f.retain(|(kk, _)| kk != k);
            f.push((k, v));
        }
        self.app.post("/uren", &f).await
    }

    async fn entry(&self, id: i64) -> (Option<i64>, Option<i64>, Option<i64>, i64) {
        sqlx::query_as(
            "SELECT tariff_id, price_cents, client_id, minutes FROM time_entries WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.app.pool)
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn tariff_is_resolved_and_snapshotted() {
    let mut x = fx().await;
    let (s, a, b) = (
        x.scouting.to_string(),
        x.client_a.to_string(),
        x.client_b.to_string(),
    );

    let r = x
        .log(&[
            ("date", "2026-11-23"),
            ("task_id", &s),
            ("client_id", &b),
            ("duration", "1,5"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        r.location.as_deref(),
        Some(&*format!("/uren?dag=2026-11-23&gebruiker={}", x.user))
    );
    let e_b = x.app.last_id("time_entries").await;
    assert_eq!(
        x.entry(e_b).await,
        (Some(x.std), Some(9500), Some(x.client_b), 90)
    );

    x.log(&[
        ("date", "2026-11-23"),
        ("task_id", &s),
        ("client_id", &a),
        ("duration", "45m"),
    ])
    .await;
    let e_a = x.app.last_id("time_entries").await;
    assert_eq!(
        x.entry(e_a).await,
        (Some(x.tariff_a), Some(11_000), Some(x.client_a), 45)
    );

    // Changing the tariff afterwards does not touch the entries.
    x.app.get(&format!("/tarieven/{}", x.std)).await;
    x.app
        .post(
            &format!("/tarieven/{}", x.std),
            &[
                ("name", "Standaard"),
                ("unit", "hour"),
                ("price", "200"),
                ("vat_bp", "2100"),
                ("valid_from", "2025-01-01"),
                ("is_default", "1"),
            ],
        )
        .await;
    assert_eq!(x.entry(e_b).await.1, Some(9500));

    // Editing only the description keeps the snapshot; changing the date re-resolves.
    x.app.get(&format!("/uren/{e_b}")).await;
    let std_s = x.std.to_string();
    let user = x.user.to_string();
    let base = |date: &'static str| {
        vec![
            ("date", date),
            ("user_id", user.as_str()),
            ("task_id", s.as_str()),
            ("client_id", b.as_str()),
            ("tariff", std_s.as_str()),
            ("duration", "1:30"),
            ("description", "Wedstrijd bekeken"),
        ]
    };
    let r = x
        .app
        .post(&format!("/uren/{e_b}"), &base("2026-11-23"))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(x.entry(e_b).await.1, Some(9500));
    x.app.get(&format!("/uren/{e_b}")).await;
    x.app
        .post(&format!("/uren/{e_b}"), &base("2026-11-24"))
        .await;
    assert_eq!(x.entry(e_b).await.1, Some(20_000));

    let week = x.app.get("/uren?week=2026-W48").await;
    assert!(week.body.contains("maandag 23-11-2026"));
    assert!(week.body.contains("Wedstrijd bekeken"));
    assert!(week.body.contains("2:15"), "week total 1:30 + 0:45");
}

#[tokio::test]
async fn internal_zero_and_none_tariffs() {
    let mut x = fx().await;
    let (adm, intro, s, a) = (
        x.admin.to_string(),
        x.intro.to_string(),
        x.scouting.to_string(),
        x.client_a.to_string(),
    );

    // Internal task: no client needed, no tariff even though a company default exists.
    let r = x
        .log(&[("date", "2026-11-24"), ("task_id", &adm), ("duration", "1")])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(x.entry(x.app.last_id("time_entries").await).await.0, None);

    // € 0 tariff is billable at € 0,00 (and needs a client).
    let r = x
        .log(&[
            ("date", "2026-11-24"),
            ("task_id", &intro),
            ("duration", "1"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r
        .body
        .contains("Kies een klant: declarabele uren moeten bij een klant horen."));
    x.log(&[
        ("date", "2026-11-24"),
        ("task_id", &intro),
        ("client_id", &a),
        ("duration", "1"),
    ])
    .await;
    let (_, price, _, _) = x.entry(x.app.last_id("time_entries").await).await;
    assert_eq!(price, Some(0));

    // Billable task without a client → refused; explicitly "Geen tarief" → fine.
    let before = x.app.count("SELECT COUNT(*) FROM time_entries").await;
    let r = x
        .log(&[("date", "2026-11-24"), ("task_id", &s), ("duration", "2")])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        x.app.count("SELECT COUNT(*) FROM time_entries").await,
        before
    );
    let r = x
        .log(&[
            ("date", "2026-11-24"),
            ("task_id", &s),
            ("duration", "2"),
            ("tariff", "none"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);

    let week = x.app.get("/uren?week=2026-W48").await;
    assert!(week.body.contains("intern"));
    // declarabel: only the € 0 hour; intern: admin 1h + scouting without tariff 2h
    assert!(week.body.contains("3:00"));
}

#[tokio::test]
async fn validation_messages() {
    let mut x = fx().await;
    let r = x.log(&[("date", "31-02-2026"), ("duration", "lang")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Vul een geldige datum in."));
    assert!(r.body.contains("Kies een taak."));
    assert!(r.body.contains("Vul een duur in"));
    let s = x.scouting.to_string();
    let b = x.client_b.to_string();
    let r = x
        .log(&[
            ("date", "2026-11-24"),
            ("task_id", &s),
            ("client_id", &b),
            ("duration", "25:00"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(x.app.count("SELECT COUNT(*) FROM time_entries").await, 0);
}

#[tokio::test]
async fn suggestion_fragment_shows_the_resolved_tariff() {
    let mut x = fx().await;
    let r = x
        .app
        .get(&format!(
            "/uren/voorstel?task_id={}&client_id={}&date=2026-11-24&tariff=auto",
            x.scouting, x.client_a
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.starts_with("<div id=\"entry-deps\""), "{}", r.body);
    assert!(r
        .body
        .contains("Automatisch: Tarief A — € 110,00 per uur (standaardtarief van de klant)"));
    let r = x
        .app
        .get(&format!(
            "/uren/voorstel?task_id={}&date=2026-11-24",
            x.admin
        ))
        .await;
    assert!(r.body.contains("geen tarief (interne taak)"));
}

#[tokio::test]
async fn task_with_fixed_client_wins() {
    let mut x = fx().await;
    let a = x.client_a.to_string();
    x.app.get("/taken/nieuw").await;
    x.app
        .post("/taken", &[("name", "Onderhandeling A"), ("client_id", &a)])
        .await;
    let task = x.app.last_id("tasks").await.to_string();
    let b = x.client_b.to_string();
    x.log(&[
        ("date", "2026-11-24"),
        ("task_id", &task),
        ("client_id", &b),
        ("duration", "1"),
    ])
    .await;
    let (tariff, _, client, _) = x.entry(x.app.last_id("time_entries").await).await;
    assert_eq!(client, Some(x.client_a));
    assert_eq!(tariff, Some(x.tariff_a));
}

#[tokio::test]
async fn back_dated_entry_gets_the_old_price() {
    let mut x = fx().await;
    x.app
        .get(&format!("/tarieven/{}/nieuwe-prijs", x.std))
        .await;
    x.app
        .post(
            &format!("/tarieven/{}/nieuwe-prijs", x.std),
            &[("price", "100"), ("from", "2026-11-01")],
        )
        .await;
    let (s, b) = (x.scouting.to_string(), x.client_b.to_string());
    x.log(&[
        ("date", "2026-10-31"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "1"),
    ])
    .await;
    assert_eq!(
        x.entry(x.app.last_id("time_entries").await).await.1,
        Some(9500)
    );
    x.log(&[
        ("date", "2026-11-01"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "1"),
    ])
    .await;
    assert_eq!(
        x.entry(x.app.last_id("time_entries").await).await.1,
        Some(10_000)
    );
}

#[tokio::test]
async fn archived_and_invoiced_entries() {
    let mut x = fx().await;
    let (s, b) = (x.scouting.to_string(), x.client_b.to_string());
    x.log(&[
        ("date", "2026-11-24"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "7:15"),
    ])
    .await;
    let id = x.app.last_id("time_entries").await;
    assert!(x.app.get("/uren?week=2026-W48").await.body.contains("7:15"));
    x.app.get(&format!("/uren/{id}")).await;
    let r = x.app.post(&format!("/uren/{id}/verwijderen"), &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let week = x.app.get("/uren?week=2026-W48").await;
    assert!(week
        .body
        .contains("7:15 verwijderd (bewaard in het archief)."));
    let week = x.app.get("/uren?week=2026-W48").await;
    assert!(!week.body.contains("7:15"), "archived entries do not count");
    assert_eq!(
        x.app
            .count(&format!(
                "SELECT COUNT(*) FROM time_entries WHERE id = {id}"
            ))
            .await,
        1
    );

    // Invoiced (phase 4 sets invoice_line_id): locked.
    x.log(&[
        ("date", "2026-11-24"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "1"),
    ])
    .await;
    let id = x.app.last_id("time_entries").await;
    sqlx::query("UPDATE time_entries SET invoice_line_id = 1 WHERE id = ?")
        .bind(id)
        .execute(&x.app.pool)
        .await
        .unwrap();
    assert!(x
        .app
        .get("/uren?week=2026-W48")
        .await
        .body
        .contains("gefactureerd"));
    let r = x
        .app
        .post(&format!("/uren/{id}"), &[("date", "2026-11-24")])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("gefactureerd"));
    let r = x.app.post(&format!("/uren/{id}/verwijderen"), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn other_companys_entries_and_tasks_are_off_limits() {
    let mut x = fx().await;
    let (s, b) = (x.scouting.to_string(), x.client_b.to_string());
    x.log(&[
        ("date", "2026-11-24"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "1"),
    ])
    .await;
    let entry = x.app.last_id("time_entries").await;
    let other = x.app.create_company("Beta", "BB").await;
    x.app.select_company(other).await;
    assert_eq!(
        x.app.get(&format!("/uren/{entry}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!x
        .app
        .get("/uren?week=2026-W48")
        .await
        .body
        .contains("Scouting"));
    // using company A's task while company B is selected
    let r = x
        .log(&[("date", "2026-11-24"), ("task_id", &s), ("duration", "1")])
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let _ = x.company;
}

#[tokio::test]
async fn te_factureren_lists_only_billable_uninvoiced_hours() {
    let mut x = fx().await;
    let (s, adm, intro, a, b) = (
        x.scouting.to_string(),
        x.admin.to_string(),
        x.intro.to_string(),
        x.client_a.to_string(),
        x.client_b.to_string(),
    );
    x.log(&[
        ("date", "2026-11-23"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "1,5"),
    ])
    .await;
    x.log(&[
        ("date", "2026-11-24"),
        ("task_id", &intro),
        ("client_id", &a),
        ("duration", "1"),
    ])
    .await;
    x.log(&[("date", "2026-11-24"), ("task_id", &adm), ("duration", "3")])
        .await;
    // archived and invoiced billable entries do not show
    x.log(&[
        ("date", "2026-11-25"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "4"),
    ])
    .await;
    let archived = x.app.last_id("time_entries").await;
    x.app
        .post(&format!("/uren/{archived}/verwijderen"), &[])
        .await;
    x.log(&[
        ("date", "2026-11-25"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "5"),
    ])
    .await;
    let invoiced = x.app.last_id("time_entries").await;
    sqlx::query("UPDATE time_entries SET invoice_line_id = 1 WHERE id = ?")
        .bind(invoiced)
        .execute(&x.app.pool)
        .await
        .unwrap();

    x.app.get("/").await; // consume flash
    let r = x.app.get("/te-factureren").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Klant A") && r.body.contains("Klant B"));
    assert!(
        r.body.contains("Kennismaking · € 0,00/u"),
        "€ 0 lines are billable"
    );
    assert!(r.body.contains("€ 142,50"), "1,5 h × € 95");
    assert!(r.body.contains("Totaal: 2:30 uur · € 142,50"), "{}", r.body);
    assert!(!r.body.contains("Administratie"));

    // period filter
    let r = x.app.get("/te-factureren?from=24-11-2026").await;
    assert!(!r.body.contains("Klant B"));

    // client page summary + recent hours
    let page = x.app.get(&format!("/klanten/{}", x.client_b)).await;
    assert!(page.body.contains("€ 142,50 excl. btw"));
    assert!(page.body.contains("23-11-2026"));

    // dashboard tile
    let dash = x.app.get("/").await;
    assert!(dash.body.contains("Te factureren"));
    assert!(dash.body.contains("€ 142,50"));
}

#[tokio::test]
async fn year_overview_per_month() {
    let mut x = fx().await;
    let (s, adm, b) = (
        x.scouting.to_string(),
        x.admin.to_string(),
        x.client_b.to_string(),
    );
    x.log(&[
        ("date", "2026-11-23"),
        ("task_id", &s),
        ("client_id", &b),
        ("duration", "1:30"),
    ])
    .await;
    x.log(&[("date", "2026-03-02"), ("task_id", &adm), ("duration", "2")])
        .await;
    x.log(&[("date", "2025-12-31"), ("task_id", &adm), ("duration", "9")])
        .await;

    // hours at another company are not included
    let other = x.app.create_company("Beta", "BB").await;
    x.app.select_company(other).await;
    x.app.get("/taken/nieuw").await;
    x.app
        .post("/taken", &[("name", "Intern"), ("internal", "1")])
        .await;
    let t = x.app.last_id("tasks").await.to_string();
    x.log(&[("date", "2026-05-05"), ("task_id", &t), ("duration", "10")])
        .await;
    x.app.select_company(x.company).await;

    let r = x.app.get("/uren/jaar?jaar=2026").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains("3:30 uur bij dit bedrijf, waarvan 1:30 declarabel"),
        "{}",
        r.body
    );
    assert!(!r.body.contains("10:00"), "other company's hours stay out");
    assert!(!r.body.contains("progress"));
    let r = x.app.get("/uren/jaar?jaar=2025").await;
    assert!(r.body.contains("9:00 uur bij dit bedrijf"));
}

#[tokio::test]
async fn empty_form_shows_no_errors() {
    let mut x = fx().await;
    let r = x.app.get("/uren").await;
    assert!(
        !r.body.contains("field-error"),
        "fresh form must not show errors"
    );
}

#[tokio::test]
async fn same_hours_on_several_days_each_with_its_own_tariff() {
    let mut x = fx().await;
    // client A's tariff gets a new price from 15-11-2026
    let ta = x.tariff_a;
    x.app.get(&format!("/tarieven/{ta}/nieuwe-prijs")).await;
    let r = x
        .app
        .post(
            &format!("/tarieven/{ta}/nieuwe-prijs"),
            &[("price", "120"), ("from", "2026-11-15")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let (s, a) = (x.scouting.to_string(), x.client_a.to_string());
    let before = x.app.count("SELECT COUNT(*) FROM time_entries").await;
    let r = x
        .log(&[
            ("date", "2026-11-10"),
            ("dates", "2026-11-17,2026-11-10,2026-11-12,2026-11-12"),
            ("task_id", &s),
            ("client_id", &a),
            ("duration", "2:00"),
            ("description", "Training"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // one entry per (distinct) day, the week of the first day is shown
    assert_eq!(
        r.location.as_deref(),
        Some(&*format!("/uren?week=2026-W46&gebruiker={}", x.user))
    );
    let rows: Vec<(String, i64, Option<i64>, String)> = sqlx::query_as(
        "SELECT date, minutes, price_cents, description FROM time_entries ORDER BY date",
    )
    .fetch_all(&x.app.pool)
    .await
    .unwrap();
    assert_eq!(
        x.app.count("SELECT COUNT(*) FROM time_entries").await - before,
        3
    );
    assert_eq!(
        rows,
        vec![
            ("2026-11-10".into(), 120, Some(11000), "Training".into()),
            ("2026-11-12".into(), 120, Some(11000), "Training".into()),
            ("2026-11-17".into(), 120, Some(12000), "Training".into()),
        ]
    );
    let page = x.app.get("/uren?week=2026-W46").await;
    assert!(
        page.body
            .contains("2:00 geschreven op 3 dagen (10-11-2026 t/m 17-11-2026), samen 6:00."),
        "{}",
        page.body
    );
}

#[tokio::test]
async fn several_days_are_saved_all_or_none() {
    let mut x = fx().await;
    let (s, a) = (x.scouting.to_string(), x.client_a.to_string());
    // the tariff starts 01-01-2025: a day before that can't be billed with it
    let t = x.tariff_a.to_string();
    let r = x
        .log(&[
            ("date", "2026-11-10"),
            ("dates", "2024-12-31,2026-11-10"),
            ("task_id", &s),
            ("client_id", &a),
            ("tariff", &t),
            ("duration", "1:00"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("31-12-2024"), "{}", r.body);
    assert_eq!(x.app.count("SELECT COUNT(*) FROM time_entries").await, 0);
    // garbage in the hidden field, and too many days
    let r = x
        .log(&[
            ("date", "2026-11-10"),
            ("dates", "morgen"),
            ("task_id", &s),
            ("duration", "1:00"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Ongeldige datum in de kalender"));
    let many: Vec<String> = (1..=63)
        .map(|i| format!("2026-{:02}-{:02}", 1 + (i - 1) / 28, 1 + (i - 1) % 28))
        .collect();
    let many = many.join(",");
    let r = x
        .log(&[
            ("date", "2026-11-10"),
            ("dates", &many),
            ("task_id", &s),
            ("duration", "1:00"),
        ])
        .await;
    assert!(r.body.contains("Kies hoogstens 62 dagen"), "{}", r.body);
    assert_eq!(x.app.count("SELECT COUNT(*) FROM time_entries").await, 0);
    // without `dates` the single date works as before
    let r = x
        .log(&[
            ("date", "2026-11-10"),
            ("task_id", &s),
            ("client_id", &a),
            ("duration", "1:00"),
        ])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(x.app.count("SELECT COUNT(*) FROM time_entries").await, 1);
}
