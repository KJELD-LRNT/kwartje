mod common;

use axum::http::StatusCode;
use common::TestApp;

struct Fx {
    app: TestApp,
    client: i64,
}

async fn fx_with(services: kwartje::web::Services) -> Fx {
    let (mut app, company) = TestApp::with_company_and(services).await;
    sqlx::query(
        "UPDATE companies SET kvk = '12345678', btw_id = 'NL123456789B01', iban = 'NL91 ABNA 0417 1643 00',
                street = 'Kantoorweg 1', postal_code = '3511 AB', city = 'Utrecht',
                email = 'info@alfa.example' WHERE id = ?",
    )
    .bind(company)
    .execute(&app.pool)
    .await
    .unwrap();
    app.get("/klanten/nieuw").await;
    app.post(
        "/klanten",
        &[
            ("name", "Sportclub"),
            ("street", "Sportlaan 1"),
            ("postal_code", "3584AA"),
            ("city", "Utrecht"),
            ("email", "penning@sportclub.example"),
            // same as the company prefix, so the numbers below stay AA-…
            ("invoice_prefix", "AA"),
        ],
    )
    .await;
    let client = app.last_id("clients").await;
    Fx { app, client }
}

async fn fx() -> Fx {
    fx_with(common::fake_services(false)).await
}

fn year() -> i32 {
    kwartje::format::today().year()
}

impl Fx {
    /// A draft quote with two lines (10 uur à € 95 at 21%, 1 stuk € 250 at 9%).
    async fn draft(&mut self) -> i64 {
        self.app.get("/offertes").await;
        let c = self.client.to_string();
        let r = self.app.post("/offertes", &[("client_id", &c)]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let id = self.app.last_id("quotes").await;
        assert_eq!(r.location, Some(format!("/offertes/{id}")));
        self.app.get(&format!("/offertes/{id}")).await;
        for (d, q, u, p, v) in [
            ("Ontwikkeling website", "10", "uur", "95", "2100"),
            ("Drukwerk", "1", "stuk", "250", "900"),
        ] {
            let r = self
                .app
                .post(
                    &format!("/offertes/{id}/regels"),
                    &[
                        ("description", d),
                        ("quantity", q),
                        ("unit", u),
                        ("price", p),
                        ("vat_bp", v),
                    ],
                )
                .await;
            assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        }
        id
    }

    async fn issue(&mut self, id: i64) -> common::Resp {
        self.app.get(&format!("/offertes/{id}")).await;
        self.app
            .post(&format!("/offertes/{id}/definitief"), &[])
            .await
    }

    async fn number(&self, id: i64) -> Option<String> {
        sqlx::query_scalar("SELECT number FROM quotes WHERE id = ?")
            .bind(id)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn draft_totals_and_editing() {
    let mut x = fx().await;
    let id = x.draft().await;
    // 950 + 199,50 VAT; 250 + 22,50 VAT
    let total = x
        .app
        .count(&format!("SELECT total_cents FROM quotes WHERE id = {id}"))
        .await;
    assert_eq!(total, 95000 + 19950 + 25000 + 2250);
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(page.body.contains("Ontwikkeling website"));
    assert!(page.body.contains("€ 1.422,00"), "{}", page.body);
    let r = x
        .app
        .post(
            &format!("/offertes/{id}"),
            &[
                ("valid_until", "2027-01-31"),
                ("intro_text", "Naar aanleiding van ons gesprek."),
                ("footer_text", ""),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let v: String = sqlx::query_scalar("SELECT valid_until FROM quotes WHERE id = ?")
        .bind(id)
        .fetch_one(&x.app.pool)
        .await
        .unwrap();
    assert_eq!(v, "2027-01-31");
    // remove a line
    let line = x
        .app
        .count(&format!(
            "SELECT MAX(id) FROM quote_lines WHERE quote_id = {id}"
        ))
        .await;
    x.app.get(&format!("/offertes/{id}")).await;
    x.app
        .post(&format!("/offertes/{id}/regels/{line}/verwijderen"), &[])
        .await;
    assert_eq!(
        x.app
            .count(&format!("SELECT total_cents FROM quotes WHERE id = {id}"))
            .await,
        114950
    );
    assert!(
        x.app
            .count("SELECT COUNT(*) FROM audit_log WHERE table_name = 'quote_lines'")
            .await
            >= 3
    );
}

#[tokio::test]
async fn issuing_uses_its_own_series_and_freezes_the_quote() {
    let mut x = fx().await;
    let a = x.draft().await;
    let b = x.draft().await;
    let r = x.issue(a).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    x.issue(b).await;
    let y = year();
    assert_eq!(x.number(a).await, Some(format!("AA-OFF-{y}-0001")));
    assert_eq!(x.number(b).await, Some(format!("AA-OFF-{y}-0002")));
    // the invoice series is untouched
    assert_eq!(
        x.app.count("SELECT COUNT(*) FROM invoice_sequences").await,
        0
    );
    // content can no longer change
    x.app.get(&format!("/offertes/{a}")).await;
    let r = x
        .app
        .post(
            &format!("/offertes/{a}/regels"),
            &[
                ("description", "x"),
                ("quantity", "1"),
                ("unit", "stuk"),
                ("price", "1"),
                ("vat_bp", "2100"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    x.app.get(&format!("/offertes/{a}")).await;
    let r = x.app.post(&format!("/offertes/{a}/verwijderen"), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // and the database refuses it too
    let res = sqlx::query("UPDATE quotes SET total_cents = 1 WHERE id = ?")
        .bind(a)
        .execute(&x.app.pool)
        .await;
    assert!(res.is_err());
    let res = sqlx::query("DELETE FROM quote_lines WHERE quote_id = ?")
        .bind(a)
        .execute(&x.app.pool)
        .await;
    assert!(res.is_err());
    // PDF on demand
    let pdf = x.app.get(&format!("/offertes/{a}/pdf")).await;
    assert_eq!(pdf.status, StatusCode::OK);
    let page = x.app.get("/offertes").await;
    assert!(page.body.contains(&format!("AA-OFF-{y}-0002")));
    assert!(page.body.contains("definitief"));
}

#[tokio::test]
async fn issuing_needs_lines() {
    let mut x = fx().await;
    x.app.get("/offertes").await;
    let c = x.client.to_string();
    x.app.post("/offertes", &[("client_id", &c)]).await;
    let id = x.app.last_id("quotes").await;
    x.issue(id).await;
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(
        page.body.contains("minstens één offerteregel"),
        "{}",
        page.body
    );
    assert_eq!(x.number(id).await, None);
}

#[tokio::test]
async fn send_accept_and_convert_to_invoice() {
    let (mailer, outbox) = kwartje::mail::Mailer::memory();
    let services = kwartje::web::Services {
        pdf: std::sync::Arc::new(kwartje::pdf::FakePdf { fail: false }),
        mailer,
    };
    let mut x = fx_with(services).await;
    let id = x.draft().await;
    x.issue(id).await;
    let n = format!("AA-OFF-{}-0001", year());
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(page.body.contains(&format!("Offerte {n} van Alfa B.V.")));
    let r = x
        .app
        .post(
            &format!("/offertes/{id}/versturen"),
            &[
                ("to", "penning@sportclub.example"),
                ("subject", "Offerte"),
                ("body", "Beste,\r\n\r\nZie bijlage."),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let sent = outbox.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].attachment.as_ref().unwrap().0, format!("{n}.pdf"));
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(page.body.contains("verzonden"));

    x.app
        .post(&format!("/offertes/{id}/geaccepteerd"), &[])
        .await;
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(page.body.contains("geaccepteerd"), "{}", page.body);

    let r = x.app.post(&format!("/offertes/{id}/factuur"), &[]).await;
    let inv = x.app.last_id("invoices").await;
    assert_eq!(r.location, Some(format!("/facturen/{inv}")));
    assert_eq!(
        x.app
            .count(&format!(
                "SELECT COUNT(*) FROM invoice_lines WHERE invoice_id = {inv} AND source = 'manual'"
            ))
            .await,
        2
    );
    assert_eq!(
        x.app
            .count(&format!(
                "SELECT total_cents FROM invoices WHERE id = {inv} AND number IS NULL"
            ))
            .await,
        95000 + 19950 + 25000 + 2250
    );
    assert_eq!(
        x.app
            .count(&format!("SELECT invoice_id FROM quotes WHERE id = {id}"))
            .await,
        inv
    );
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(page.body.contains("gefactureerd"));
    assert!(page.body.contains("conceptfactuur"));
    // converting twice is refused
    x.app.post(&format!("/offertes/{id}/factuur"), &[]).await;
    assert_eq!(x.app.count("SELECT COUNT(*) FROM invoices").await, 1);
}

#[tokio::test]
async fn rejected_quote_cannot_be_invoiced_and_drafts_cannot_be_sent() {
    let mut x = fx().await;
    let id = x.draft().await;
    x.app.get(&format!("/offertes/{id}")).await;
    let r = x.app.post(&format!("/offertes/{id}/verzonden"), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    x.issue(id).await;
    x.app.get(&format!("/offertes/{id}")).await;
    x.app.post(&format!("/offertes/{id}/verzonden"), &[]).await;
    x.app.post(&format!("/offertes/{id}/afgewezen"), &[]).await;
    x.app.post(&format!("/offertes/{id}/factuur"), &[]).await;
    assert_eq!(x.app.count("SELECT COUNT(*) FROM invoices").await, 0);
    let page = x.app.get(&format!("/offertes/{id}")).await;
    assert!(page.body.contains("afgewezen"));
}

#[tokio::test]
async fn another_companys_quote_is_not_found() {
    let mut x = fx().await;
    let id = x.draft().await;
    let beta = x.app.create_company("Beta", "BB").await;
    x.app.select_company(beta).await;
    assert_eq!(
        x.app.get(&format!("/offertes/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        x.app.get(&format!("/offertes/{id}/pdf")).await.status,
        StatusCode::NOT_FOUND
    );
    x.app.get("/offertes").await;
    assert_eq!(
        x.app
            .post(&format!("/offertes/{id}/definitief"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let c = x.client.to_string();
    assert_eq!(
        x.app.post("/offertes", &[("client_id", &c)]).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!x.app.get("/offertes").await.body.contains("Sportclub"));
}

#[tokio::test]
async fn real_typst_renders_a_quote() {
    if std::process::Command::new("typst")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("typst not installed; skipping");
        return;
    }
    let services = kwartje::web::Services {
        pdf: std::sync::Arc::new(kwartje::pdf::TypstCli {
            binary: "typst".into(),
            timeout: std::time::Duration::from_secs(60),
        }),
        mailer: kwartje::mail::Mailer::Disabled,
    };
    let mut x = fx_with(services).await;
    let id = x.draft().await;
    x.issue(id).await;
    let r = x.app.get(&format!("/offertes/{id}/pdf")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

#[tokio::test]
async fn issued_quote_pdf_is_stored_and_stays_as_sent() {
    let mut x = fx().await;
    let id = x.draft().await;
    x.issue(id).await;
    let n = format!("AA-OFF-{}-0001", year());
    let (doc, path, kind): (i64, String, String) = sqlx::query_as(
        "SELECT d.id, d.file_path, d.kind FROM quotes q JOIN documents d ON d.id = q.pdf_document_id WHERE q.id = ?",
    )
    .bind(id)
    .fetch_one(&x.app.pool)
    .await
    .unwrap();
    assert_eq!(kind, "quote");
    assert!(path.ends_with(&format!("/quote/{id}_{n}.pdf")), "{path}");
    let stored = std::fs::read(x.app.cfg.data_dir.join(&path)).unwrap();
    assert!(String::from_utf8_lossy(&stored).contains(&n));

    // the company changes its name afterwards: the quote PDF doesn't
    sqlx::query("UPDATE companies SET name = 'Nieuwe Naam B.V.'")
        .execute(&x.app.pool)
        .await
        .unwrap();
    let r = x.app.get(&format!("/offertes/{id}/pdf")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("Nieuwe Naam"));
    assert!(r.body.contains("Alfa B.V."));

    // read-only in the document store, and the database agrees
    let page = x.app.get(&format!("/documenten/{doc}")).await;
    assert!(page.body.contains("offerte-PDF is door de app gemaakt"));
    let r = x
        .app
        .post(&format!("/documenten/{doc}/archiveren"), &[])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    for sql in [
        "UPDATE documents SET kind = 'other' WHERE kind = 'quote'",
        "UPDATE documents SET sha256 = 'x' WHERE kind = 'quote'",
        "DELETE FROM documents WHERE kind = 'quote'",
        "UPDATE quotes SET pdf_document_id = NULL",
    ] {
        assert!(
            sqlx::query(sql).execute(&x.app.pool).await.is_err(),
            "{sql}"
        );
    }
}

#[tokio::test]
async fn quote_numbers_use_the_client_prefix() {
    let mut x = fx().await;
    sqlx::query("UPDATE clients SET invoice_prefix = 'SC'")
        .execute(&x.app.pool)
        .await
        .unwrap();
    let id = x.draft().await;
    x.issue(id).await;
    assert_eq!(x.number(id).await, Some(format!("SC-OFF-{}-0001", year())));
    // without a prefix the quote can't be issued
    sqlx::query("UPDATE clients SET invoice_prefix = NULL")
        .execute(&x.app.pool)
        .await
        .unwrap();
    let b = x.draft().await;
    x.issue(b).await;
    assert_eq!(x.number(b).await, None);
    assert!(x
        .app
        .get(&format!("/offertes/{b}"))
        .await
        .body
        .contains("factuurprefix van klant Sportclub"));
}
