mod common;

use axum::http::StatusCode;
use common::TestApp;
use kwartje::modules::recurring::generate_due;
use time::macros::date;

struct Fx {
    app: TestApp,
    client: i64,
}

async fn fx() -> Fx {
    let (mut app, _company) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    app.post(
        "/klanten",
        &[
            ("name", "Sportclub"),
            ("street", "Sportlaan 1"),
            ("postal_code", "3584AA"),
            ("city", "Utrecht"),
        ],
    )
    .await;
    let client = app.last_id("clients").await;
    Fx { app, client }
}

impl Fx {
    /// Monthly template "Hosting" (€ 25 + € 10 per month) starting `start`.
    async fn template(&mut self, interval: &str, start: &str, end: &str) -> i64 {
        self.app.get("/terugkerend").await;
        let c = self.client.to_string();
        let r = self
            .app
            .post(
                "/terugkerend",
                &[
                    ("name", "Hosting"),
                    ("client_id", &c),
                    ("interval", interval),
                    ("next_date", start),
                    ("end_date", end),
                    ("intro_text", ""),
                    ("footer_text", ""),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let id = self.app.last_id("recurring_invoices").await;
        assert_eq!(r.location, Some(format!("/terugkerend/{id}")));
        self.app.get(&format!("/terugkerend/{id}")).await;
        for (d, p) in [("Hosting", "25"), ("Back-ups", "10")] {
            self.app
                .post(
                    &format!("/terugkerend/{id}/regels"),
                    &[
                        ("description", d),
                        ("quantity", "1"),
                        ("unit", "stuk"),
                        ("price", p),
                        ("vat_bp", "2100"),
                    ],
                )
                .await;
        }
        id
    }

    async fn drafts(&self) -> Vec<(String, String, String, i64)> {
        sqlx::query_as("SELECT period_from, period_to, intro_text, total_cents FROM invoices WHERE recurring_id IS NOT NULL AND number IS NULL ORDER BY period_from")
            .fetch_all(&self.app.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn generates_once_per_period_with_period_label() {
    let mut x = fx().await;
    let id = x.template("month", "2026-11-01", "").await;
    assert_eq!(
        generate_due(&x.app.pool, date!(2026 - 10 - 31))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        generate_due(&x.app.pool, date!(2026 - 11 - 01))
            .await
            .unwrap(),
        1
    );
    // a second run makes nothing
    assert_eq!(
        generate_due(&x.app.pool, date!(2026 - 11 - 25))
            .await
            .unwrap(),
        0
    );
    let d = x.drafts().await;
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].0, "2026-11-01");
    assert_eq!(d[0].1, "2026-11-30");
    assert_eq!(d[0].2, "Hierbij ontvangt u de factuur voor november 2026.");
    assert_eq!(d[0].3, 3500 + 735);
    assert_eq!(
        x.app
            .count("SELECT COUNT(*) FROM invoice_lines WHERE source = 'manual'")
            .await,
        2
    );
    let next: String = sqlx::query_scalar("SELECT next_date FROM recurring_invoices WHERE id = ?")
        .bind(id)
        .fetch_one(&x.app.pool)
        .await
        .unwrap();
    assert_eq!(next, "2026-12-01");
    // audited as the system (no user)
    assert!(
        x.app
            .count(
                "SELECT COUNT(*) FROM audit_log WHERE table_name = 'invoices' AND user_id IS NULL"
            )
            .await
            >= 1
    );
    let page = x.app.get(&format!("/terugkerend/{id}")).await;
    assert!(
        page.body.contains("01-11-2026 t/m 30-11-2026"),
        "{}",
        page.body
    );
    assert!(page.body.contains("december 2026"));
}

#[tokio::test]
async fn catches_up_missed_periods_and_respects_end_date() {
    let mut x = fx().await;
    x.template("quarter", "2026-01-01", "2026-09-30").await;
    assert_eq!(
        generate_due(&x.app.pool, date!(2026 - 11 - 25))
            .await
            .unwrap(),
        3
    );
    let d = x.drafts().await;
    let labels: Vec<&str> = d.iter().map(|r| r.2.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Hierbij ontvangt u de factuur voor Q1 2026.",
            "Hierbij ontvangt u de factuur voor Q2 2026.",
            "Hierbij ontvangt u de factuur voor Q3 2026."
        ]
    );
    assert_eq!(d[2].1, "2026-09-30");
    // the end date stops further drafts
    assert_eq!(
        generate_due(&x.app.pool, date!(2027 - 06 - 01))
            .await
            .unwrap(),
        0
    );
    assert!(x.app.get("/terugkerend").await.body.contains("afgelopen"));
}

#[tokio::test]
async fn archived_template_stops_generating() {
    let mut x = fx().await;
    let id = x.template("month", "2026-11-01", "").await;
    x.app.get(&format!("/terugkerend/{id}")).await;
    x.app
        .post(&format!("/terugkerend/{id}/archiveren"), &[])
        .await;
    assert_eq!(
        generate_due(&x.app.pool, date!(2026 - 12 - 15))
            .await
            .unwrap(),
        0
    );
    x.app.get(&format!("/terugkerend/{id}")).await;
    x.app
        .post(&format!("/terugkerend/{id}/archiveren"), &[])
        .await;
    assert_eq!(
        generate_due(&x.app.pool, date!(2026 - 12 - 15))
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn editing_template_and_lines() {
    let mut x = fx().await;
    let id = x.template("month", "2026-11-01", "").await;
    let line = x
        .app
        .count(&format!(
            "SELECT MAX(id) FROM recurring_lines WHERE recurring_id = {id}"
        ))
        .await;
    x.app.get(&format!("/terugkerend/{id}")).await;
    x.app
        .post(&format!("/terugkerend/{id}/regels/{line}/verwijderen"), &[])
        .await;
    let c = x.client.to_string();
    let r = x
        .app
        .post(
            &format!("/terugkerend/{id}"),
            &[
                ("name", "Hosting jaarlijks"),
                ("client_id", &c),
                ("interval", "year"),
                ("next_date", "2027-01-01"),
                ("end_date", ""),
                ("intro_text", "Jaarfactuur {periode}"),
                ("footer_text", ""),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    // bad end date
    let r = x
        .app
        .post(
            &format!("/terugkerend/{id}"),
            &[
                ("name", "X"),
                ("interval", "year"),
                ("next_date", "2027-01-01"),
                ("end_date", "2026-01-01"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let page = x.app.get(&format!("/terugkerend/{id}")).await;
    assert!(page
        .body
        .contains("De einddatum ligt voor de volgende periode."));
    assert!(page.body.contains("Hosting jaarlijks"));
    assert_eq!(
        generate_due(&x.app.pool, date!(2027 - 01 - 02))
            .await
            .unwrap(),
        1
    );
    let d = x.drafts().await;
    assert_eq!(d[0].2, "Jaarfactuur 2027");
    assert_eq!(d[0].1, "2027-12-31");
    assert_eq!(d[0].3, 3025);
}

#[tokio::test]
async fn another_companys_template_is_not_found() {
    let mut x = fx().await;
    let id = x.template("month", "2026-11-01", "").await;
    let beta = x.app.create_company("Beta", "BB").await;
    x.app.select_company(beta).await;
    assert_eq!(
        x.app.get(&format!("/terugkerend/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    x.app.get("/terugkerend").await;
    assert_eq!(
        x.app
            .post(&format!("/terugkerend/{id}/archiveren"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(!x.app.get("/terugkerend").await.body.contains("Hosting"));
}

#[tokio::test]
async fn dashboard_shows_drafts_quotes_btw_and_year_hours() {
    let mut x = fx().await;
    x.template("month", "2026-11-01", "").await;
    generate_due(&x.app.pool, date!(2026 - 11 - 02))
        .await
        .unwrap();
    let page = x.app.get("/").await;
    assert!(
        page.body
            .contains("1 uit terugkerende facturen, klaar om te controleren"),
        "{}",
        page.body
    );
    assert!(page.body.contains("Offertes zonder antwoord"));
    assert!(page.body.contains("(schatting)"));
    assert!(page.body.contains("per persoon"));
}
