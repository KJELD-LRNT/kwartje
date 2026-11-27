mod common;

use axum::http::StatusCode;
use common::TestApp;

pub struct Fx {
    pub app: TestApp,
    pub company: i64,
    pub client: i64,
    pub task: i64,
    pub user: i64,
}

/// Company (rounding 15 min) with full invoice data, a client with address,
/// a € 90 default tariff, a € 0 task and three unbilled entries.
pub async fn fx() -> Fx {
    fx_with(common::fake_services(false)).await
}

pub async fn fx_with(services: kwartje::web::Services) -> Fx {
    let (mut app, company) = TestApp::with_company_and(services).await;
    sqlx::query(
        "UPDATE companies SET kvk = '12345678', btw_id = 'NL123456789B01', iban = 'NL91 ABNA 0417 1643 00',
                street = 'Kantoorweg 1', postal_code = '3511 AB', city = 'Utrecht', rounding_minutes = 15,
                email = 'info@alfa.example' WHERE id = ?",
    )
    .bind(company)
    .execute(&app.pool)
    .await
    .unwrap();
    let user = app.count("SELECT MIN(id) FROM users").await;
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
    app.get("/tarieven/nieuw").await;
    for f in [
        vec![
            ("name", "Standaard"),
            ("unit", "hour"),
            ("price", "90"),
            ("vat_bp", "2100"),
            ("valid_from", "2025-01-01"),
            ("is_default", "1"),
        ],
        vec![
            ("name", "Gratis"),
            ("unit", "hour"),
            ("price", "0"),
            ("vat_bp", "2100"),
            ("valid_from", "2025-01-01"),
        ],
    ] {
        app.post("/tarieven", &f).await;
    }
    let free = app.last_id("tariffs").await.to_string();
    app.get("/taken/nieuw").await;
    app.post("/taken", &[("name", "Scouting")]).await;
    let task = app.last_id("tasks").await;
    app.post("/taken", &[("name", "Kennismaking"), ("tariff_id", &free)])
        .await;
    let intro = app.last_id("tasks").await;
    let mut x = Fx {
        app,
        company,
        client,
        task,
        user,
    };
    for (date, task, dur) in [
        ("2026-11-02", task, "0:50"),
        ("2026-11-03", task, "1:00"),
        ("2026-11-04", intro, "1:00"),
    ] {
        x.log(date, task, dur).await;
    }
    x
}

impl Fx {
    pub async fn log(&mut self, date: &str, task: i64, duration: &str) {
        self.app.get("/uren").await;
        let (u, t, c) = (
            self.user.to_string(),
            task.to_string(),
            self.client.to_string(),
        );
        let r = self
            .app
            .post(
                "/uren",
                &[
                    ("date", date),
                    ("user_id", &u),
                    ("task_id", &t),
                    ("client_id", &c),
                    ("tariff", "auto"),
                    ("duration", duration),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }

    pub async fn entry_ids(&self) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM time_entries WHERE invoice_line_id IS NULL ORDER BY id")
            .fetch_all(&self.app.pool)
            .await
            .unwrap()
    }

    /// Create a draft with all unbilled entries; returns the invoice id.
    pub async fn draft(&mut self, extra: &[(&str, &str)]) -> i64 {
        self.app
            .get(&format!("/facturen/nieuw?klant={}", self.client))
            .await;
        let ids: Vec<String> = self
            .entry_ids()
            .await
            .iter()
            .map(|i| i.to_string())
            .collect();
        let c = self.client.to_string();
        let mut f: Vec<(&str, &str)> = vec![("client_id", &c)];
        for i in &ids {
            f.push(("uren", i));
        }
        f.extend_from_slice(extra);
        let r = self.app.post("/facturen", &f).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let id = self.app.last_id("invoices").await;
        assert_eq!(r.location, Some(format!("/facturen/{id}")));
        id
    }
}

#[tokio::test]
async fn draft_takes_entries_and_deleting_releases_them() {
    let mut x = fx().await;
    assert!(x
        .app
        .get("/te-factureren")
        .await
        .body
        .contains("Factuur maken"));
    let page = x
        .app
        .get(&format!("/facturen/nieuw?klant={}", x.client))
        .await;
    assert!(page.body.contains("Scouting") && page.body.contains("afgerond op 15 minuten"));

    let id = x.draft(&[]).await;
    assert!(x.entry_ids().await.is_empty(), "all entries linked");
    let r = x.app.get(&format!("/facturen/{id}")).await;
    assert!(r.body.contains("Factuur (concept)"));
    // 0:50 → 1:00 (rounded up to 15 min) + 1:00 = 2 h × € 90 = € 180,00; € 0 line separately
    assert!(r.body.contains("2 uur"), "{}", r.body);
    assert!(r.body.contains("€ 180,00"));
    assert!(r.body.contains("€ 37,80"), "21% of 180");
    assert!(r.body.contains("€ 217,80"));
    assert!(r.body.contains("Kennismaking"));
    let (sub, total): (i64, i64) =
        sqlx::query_as("SELECT subtotal_cents, total_cents FROM invoices WHERE id = ?")
            .bind(id)
            .fetch_one(&x.app.pool)
            .await
            .unwrap();
    assert_eq!((sub, total), (18_000, 21_780));

    // linked hours are locked for editing
    let entry: i64 = x.app.count("SELECT MIN(id) FROM time_entries").await;
    x.app.get(&format!("/uren/{entry}")).await;
    let r = x
        .app
        .post(&format!("/uren/{entry}"), &[("date", "2026-11-02")])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    x.app.get(&format!("/facturen/{id}")).await;
    let r = x
        .app
        .post(&format!("/facturen/{id}/verwijderen"), &[])
        .await;
    assert_eq!(r.location.as_deref(), Some("/te-factureren"));
    assert_eq!(x.entry_ids().await.len(), 3, "entries released");
    assert_eq!(x.app.count("SELECT COUNT(*) FROM invoices").await, 0);
    assert_eq!(x.app.count("SELECT COUNT(*) FROM invoice_lines").await, 0);
    assert_eq!(x.app.count("SELECT COUNT(*) FROM audit_log WHERE table_name = 'invoices' AND action = 'delete'").await, 1);
}

#[tokio::test]
async fn edit_draft_lines_options_and_rebuild() {
    let mut x = fx().await;
    let id = x.draft(&[("hide_zero_lines", "1")]).await;
    let line: i64 = x
        .app
        .count(&format!(
            "SELECT MIN(id) FROM invoice_lines WHERE invoice_id = {id}"
        ))
        .await;
    let page = x.app.get(&format!("/facturen/{id}")).await;
    assert!(page.body.contains("verborgen op de factuur"));

    // manual line: 2 × € 12,50 at 9%
    let r = x
        .app
        .post(
            &format!("/facturen/{id}/regels"),
            &[
                ("description", "Boek"),
                ("quantity", "2"),
                ("unit", "stuk"),
                ("price", "12,50"),
                ("vat_bp", "900"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let page = x.app.get(&format!("/facturen/{id}")).await;
    assert!(page.body.contains("Btw 9% over € 25,00"), "{}", page.body);

    // descriptions, texts, line style "per entry" rebuild
    let key = format!("desc_{line}");
    x.app
        .post(
            &format!("/facturen/{id}"),
            &[
                (key.as_str(), "Scouting november"),
                ("intro_text", "Hierbij de factuur."),
                ("line_style", "task"),
            ],
        )
        .await;
    let d: String = sqlx::query_scalar("SELECT description FROM invoice_lines WHERE id = ?")
        .bind(line)
        .fetch_one(&x.app.pool)
        .await
        .unwrap();
    assert_eq!(d, "Scouting november");
    x.app.get(&format!("/facturen/{id}")).await;
    x.app
        .post(&format!("/facturen/{id}"), &[("line_style", "entry")])
        .await;
    let n = x
        .app
        .count(&format!(
            "SELECT COUNT(*) FROM invoice_lines WHERE invoice_id = {id}"
        ))
        .await;
    assert_eq!(n, 3 + 1, "one line per entry + the manual line");
    assert!(
        x.entry_ids().await.is_empty(),
        "still all linked after rebuild"
    );

    // removing an hours line releases its entry
    let hours_line: i64 = x
        .app
        .count(&format!(
            "SELECT MIN(id) FROM invoice_lines WHERE invoice_id = {id} AND source = 'hours'"
        ))
        .await;
    x.app.get(&format!("/facturen/{id}")).await;
    x.app
        .post(
            &format!("/facturen/{id}/regels/{hours_line}/verwijderen"),
            &[],
        )
        .await;
    assert_eq!(x.entry_ids().await.len(), 1);
}

#[tokio::test]
async fn another_companys_invoice_is_not_found() {
    let mut x = fx().await;
    let id = x.draft(&[]).await;
    let beta = x.app.create_company("Beta", "BB").await;
    x.app.select_company(beta).await;
    assert_eq!(
        x.app.get(&format!("/facturen/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    x.app.get("/").await;
    assert_eq!(
        x.app
            .post(&format!("/facturen/{id}/verwijderen"), &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // and Beta cannot draft from company A's client
    x.app.get("/").await;
    let c = x.client.to_string();
    let r = x.app.post("/facturen", &[("client_id", &c)]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let _ = x.company;
    let _ = x.task;
}

impl Fx {
    pub async fn issue(&mut self, id: i64, date: &str) -> common::Resp {
        self.app.get(&format!("/facturen/{id}")).await;
        self.app
            .post(
                &format!("/facturen/{id}/definitief"),
                &[("issue_date", date)],
            )
            .await
    }

    pub async fn number(&self, id: i64) -> Option<String> {
        sqlx::query_scalar("SELECT number FROM invoices WHERE id = ?")
            .bind(id)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn issuing_numbers_sequentially_and_stores_the_pdf() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    let r = x.issue(a, "2026-11-30").await;
    assert_eq!(r.location, Some(format!("/facturen/{a}")));
    assert_eq!(x.number(a).await.as_deref(), Some("AA-2026-0001"));
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(page.body.contains("Factuur AA-2026-0001 is definitief."));
    assert!(page.body.contains("definitief"));
    assert!(
        page.body.contains("14-12-2026"),
        "due date = +14 days: {}",
        page.body
    );

    // the PDF document: stored, hash matches, downloadable
    let (path, sha): (String, String) = sqlx::query_as(
        "SELECT d.file_path, d.sha256 FROM documents d JOIN invoices i ON i.pdf_document_id = d.id WHERE i.id = ?",
    )
    .bind(a)
    .fetch_one(&x.app.pool)
    .await
    .unwrap();
    assert!(path.starts_with(&format!(
        "documents/{}/2026/sales_invoice/{a}_AA-2026-0001",
        x.company
    )));
    let bytes = std::fs::read(x.app.cfg.data_dir.join(&path)).unwrap();
    assert_eq!(kwartje::export::sha256_hex(&bytes), sha);
    let pdf = x.app.get(&format!("/facturen/{a}/pdf")).await;
    assert_eq!(pdf.headers["content-type"], "application/pdf");
    assert!(pdf.body.contains("AA-2026-0001") && pdf.body.contains("Sportclub"));
    assert!(pdf.body.contains("\"total\":\"€ 217,80\"") || pdf.body.contains("€ 217,80"));

    // second invoice: next number
    x.log("2026-12-01", x.task, "1:00").await;
    let b = x.draft(&[]).await;
    x.issue(b, "2026-12-01").await;
    assert_eq!(x.number(b).await.as_deref(), Some("AA-2026-0002"));

    // a date before the previous invoice is refused
    x.log("2026-12-02", x.task, "1:00").await;
    let c = x.draft(&[]).await;
    x.issue(c, "2026-11-01").await;
    assert_eq!(x.number(c).await, None);
    assert!(x
        .app
        .get(&format!("/facturen/{c}"))
        .await
        .body
        .contains("mag niet vóór die van de vorige factuur"));
}

#[tokio::test]
async fn issued_invoice_cannot_be_edited() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    x.app.get(&format!("/facturen/{a}")).await;
    for path in [
        format!("/facturen/{a}"),
        format!("/facturen/{a}/regels"),
        format!("/facturen/{a}/verwijderen"),
        format!("/facturen/{a}/definitief"),
    ] {
        let r = x
            .app
            .post(
                &path,
                &[
                    ("description", "x"),
                    ("quantity", "1"),
                    ("price", "1"),
                    ("vat_bp", "2100"),
                    ("issue_date", "2026-12-01"),
                ],
            )
            .await;
        assert!(
            r.status == StatusCode::BAD_REQUEST || r.status == StatusCode::SEE_OTHER,
            "{path}: {}",
            r.status
        );
    }
    assert_eq!(x.number(a).await.as_deref(), Some("AA-2026-0001"));
    assert_eq!(
        x.app
            .count("SELECT last_number FROM invoice_sequences")
            .await,
        1
    );
}

#[tokio::test]
async fn missing_company_data_blocks_issuing() {
    let mut x = fx().await;
    sqlx::query("UPDATE companies SET kvk = '', iban = ''")
        .execute(&x.app.pool)
        .await
        .unwrap();
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(
        page.body
            .contains("Ontbreekt: KvK-nummer van het bedrijf, IBAN van het bedrijf."),
        "{}",
        page.body
    );
    assert_eq!(x.number(a).await, None);
}

#[tokio::test]
async fn failed_pdf_uses_no_number() {
    let mut x = fx_with(common::fake_services(true)).await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    assert_eq!(x.number(a).await, None);
    assert_eq!(
        x.app.count("SELECT COUNT(*) FROM invoice_sequences").await,
        0
    );
    assert_eq!(x.app.count("SELECT COUNT(*) FROM documents").await, 0);
    assert!(x
        .app
        .get(&format!("/facturen/{a}"))
        .await
        .body
        .contains("De PDF kon niet worden gemaakt"));
}

#[tokio::test]
async fn real_typst_renders_a_pdf_with_hours_specification() {
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
    let a = x
        .draft(&[("include_hours_spec", "1"), ("hide_zero_lines", "1")])
        .await;
    let r = x.issue(a, "2026-11-30").await;
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(
        page.body.contains("is definitief"),
        "{:?} {}",
        r.location,
        page.body
    );
    let path: String = sqlx::query_scalar("SELECT d.file_path FROM documents d JOIN invoices i ON i.pdf_document_id = d.id WHERE i.id = ?")
        .bind(a).fetch_one(&x.app.pool).await.unwrap();
    let bytes = std::fs::read(x.app.cfg.data_dir.join(&path)).unwrap();
    assert!(bytes.starts_with(b"%PDF"));
    if let Ok(out) = std::env::var("KWARTJE_KEEP_PDF") {
        std::fs::write(out, &bytes).unwrap();
    }
}

#[tokio::test]
async fn send_by_email_attaches_the_pdf_and_marks_sent() {
    let (mailer, outbox) = kwartje::mail::Mailer::memory();
    let services = kwartje::web::Services {
        pdf: std::sync::Arc::new(kwartje::pdf::FakePdf { fail: false }),
        mailer,
    };
    let mut x = fx_with(services).await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    let page = x.app.get(&format!("/facturen/{a}/versturen")).await;
    assert!(page.body.contains("penning@sportclub.example"));
    assert!(page.body.contains("Factuur AA-2026-0001 van Alfa B.V."));
    assert!(page.body.contains("uiterlijk 14-12-2026"));
    // invalid address
    let r = x
        .app
        .post(
            &format!("/facturen/{a}/versturen"),
            &[("to", "geen"), ("subject", "S"), ("body", "B")],
        )
        .await;
    assert!(r.body.contains("Dit e-mailadres is ongeldig."));
    let r = x
        .app
        .post(
            &format!("/facturen/{a}/versturen"),
            &[
                ("to", "penning@sportclub.example"),
                ("subject", "Factuur AA-2026-0001"),
                ("body", "Beste,\r\n\r\nZie bijlage."),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let sent = outbox.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "penning@sportclub.example");
    assert_eq!(sent[0].from_name, "Alfa B.V.");
    assert_eq!(sent[0].reply_to.as_deref(), Some("info@alfa.example"));
    let (name, bytes) = sent[0].attachment.as_ref().unwrap();
    assert_eq!(name, "AA-2026-0001.pdf");
    assert!(bytes.starts_with(b"%PDF"));
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(page
        .body
        .contains("verstuurd naar penning@sportclub.example"));
    assert!(
        page.body
            .contains("<span class=\"status info\">verzonden</span>"),
        "{}",
        page.body
    );
}

#[tokio::test]
async fn without_smtp_mark_as_sent_by_hand() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    let page = x.app.get(&format!("/facturen/{a}/versturen")).await;
    assert!(page.body.contains("E-mail is nog niet ingesteld"));
    let r = x
        .app
        .post(
            &format!("/facturen/{a}/versturen"),
            &[("to", "a@b.nl"), ("subject", "S"), ("body", "B")],
        )
        .await;
    assert!(r.body.contains("Versturen is mislukt"));
    x.app.post(&format!("/facturen/{a}/verzonden"), &[]).await;
    let to: Option<String> = sqlx::query_scalar("SELECT sent_to FROM invoices WHERE id = ?")
        .bind(a)
        .fetch_one(&x.app.pool)
        .await
        .unwrap();
    assert_eq!(to.as_deref(), Some("handmatig"));
    // a draft cannot be marked as sent
    x.log("2026-12-01", x.task, "1:00").await;
    let b = x.draft(&[]).await;
    x.app.get(&format!("/facturen/{b}")).await;
    assert_eq!(
        x.app
            .post(&format!("/facturen/{b}/verzonden"), &[])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

async fn status_text(x: &mut Fx, id: i64) -> String {
    let page = x.app.get(&format!("/facturen/{id}")).await;
    let i = page.body.find("<span class=\"status").expect("status");
    let rest = &page.body[i..];
    let s = rest.find('>').unwrap() + 1;
    let e = rest.find("</span>").unwrap();
    rest[s..e].to_string()
}

#[tokio::test]
async fn payments_drive_the_status_and_overdue_is_detected() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    // issued long ago: due date in the past
    x.issue(a, "2026-01-02").await;
    assert_eq!(status_text(&mut x, a).await, "definitief");
    x.app.post(&format!("/facturen/{a}/verzonden"), &[]).await;
    assert_eq!(status_text(&mut x, a).await, "vervallen");

    x.app
        .post(
            &format!("/facturen/{a}/betalingen"),
            &[
                ("date", "2026-02-01"),
                ("amount", "100"),
                ("method", "bank"),
            ],
        )
        .await;
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(page.body.contains("Betaling van € 100,00 geregistreerd."));
    assert!(page.body.contains("Openstaand: <strong>€ 117,80</strong>"));
    assert_eq!(
        status_text(&mut x, a).await,
        "vervallen",
        "partly paid but overdue"
    );
    x.app
        .post(
            &format!("/facturen/{a}/betalingen"),
            &[
                ("date", "2026-02-10"),
                ("amount", "117,80"),
                ("method", "bank"),
            ],
        )
        .await;
    assert_eq!(status_text(&mut x, a).await, "betaald");

    // archiving a payment reopens; it is kept, not deleted
    let pid = x.app.last_id("payments").await;
    x.app
        .post(&format!("/facturen/{a}/betalingen/{pid}/archiveren"), &[])
        .await;
    assert_eq!(status_text(&mut x, a).await, "vervallen");
    assert_eq!(x.app.count("SELECT COUNT(*) FROM payments").await, 2);

    // overpayment warning
    x.app
        .post(
            &format!("/facturen/{a}/betalingen"),
            &[
                ("date", "2026-02-10"),
                ("amount", "500"),
                ("method", "bank"),
            ],
        )
        .await;
    assert!(x
        .app
        .get(&format!("/facturen/{a}"))
        .await
        .body
        .contains("meer dan het factuurbedrag"));

    // not on a draft
    x.log("2026-12-01", x.task, "1:00").await;
    let b = x.draft(&[]).await;
    x.app.get(&format!("/facturen/{b}")).await;
    let r = x
        .app
        .post(
            &format!("/facturen/{b}/betalingen"),
            &[("date", "2026-02-10"), ("amount", "1")],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn partly_paid_before_due_date() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    let today = kwartje::format::iso_date(kwartje::format::today());
    x.issue(a, &today).await;
    x.app.post(&format!("/facturen/{a}/verzonden"), &[]).await;
    assert_eq!(status_text(&mut x, a).await, "verzonden");
    x.app
        .post(
            &format!("/facturen/{a}/betalingen"),
            &[("date", &today), ("amount", "10"), ("method", "contant")],
        )
        .await;
    assert_eq!(status_text(&mut x, a).await, "deels betaald");
}

#[tokio::test]
async fn no_reminder_before_the_due_date() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    let today = kwartje::format::iso_date(kwartje::format::today());
    x.issue(a, &today).await;
    x.app.post(&format!("/facturen/{a}/verzonden"), &[]).await;
    assert_eq!(
        x.app
            .get(&format!("/facturen/{a}/herinnering"))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn reminder_levels_on_an_overdue_invoice() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-01-02").await;
    x.app.post(&format!("/facturen/{a}/verzonden"), &[]).await;
    for (level, subject) in [
        ("1", "Herinnering: factuur AA-2026-0001"),
        ("2", "Tweede herinnering: factuur AA-2026-0001"),
        ("final", "Laatste aanmaning: factuur AA-2026-0001"),
    ] {
        let page = x.app.get(&format!("/facturen/{a}/herinnering")).await;
        assert!(page.body.contains(subject), "{level}: {}", page.body);
        assert!(page.body.contains("€ 217,80"));
        let r = x
            .app
            .post(
                &format!("/facturen/{a}/herinnering"),
                &[
                    ("level", level),
                    ("subject", subject),
                    ("body", "tekst"),
                    ("how", "manual"),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
    }
    assert_eq!(x.app.count("SELECT COUNT(*) FROM reminders").await, 3);
    assert_eq!(
        x.app
            .get(&format!("/facturen/{a}/herinnering"))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(page.body.contains("Laatste aanmaning:"));
}

#[tokio::test]
async fn credit_note_takes_next_number_and_releases_entries() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    assert!(x.entry_ids().await.is_empty());
    x.app.get(&format!("/facturen/{a}")).await;
    let r = x
        .app
        .post(
            &format!("/facturen/{a}/crediteren"),
            &[("release_entries", "1")],
        )
        .await;
    let c = x.app.last_id("invoices").await;
    assert_eq!(r.location, Some(format!("/facturen/{c}")));
    let page = x.app.get(&format!("/facturen/{c}")).await;
    assert!(page.body.contains("Creditfactuur (concept)"));
    assert!(page.body.contains("credit voor AA-2026-0001"));
    assert!(page.body.contains("€ -217,80"), "{}", page.body);
    // a second credit note is not created
    x.app.get(&format!("/facturen/{a}")).await;
    x.app.post(&format!("/facturen/{a}/crediteren"), &[]).await;
    assert_eq!(
        x.app
            .count("SELECT COUNT(*) FROM invoices WHERE kind = 'credit'")
            .await,
        1
    );

    x.issue(c, "2026-11-30").await;
    assert_eq!(x.number(c).await.as_deref(), Some("AA-2026-0002"));
    assert_eq!(status_text(&mut x, a).await, "gecrediteerd");
    assert_eq!(
        x.entry_ids().await.len(),
        3,
        "entries back under Te factureren"
    );
    assert!(x.app.get("/te-factureren").await.body.contains("Sportclub"));
    // the credit note's PDF mentions the original
    let pdf = x.app.get(&format!("/facturen/{c}/pdf")).await;
    assert!(pdf.body.contains("Creditfactuur") && pdf.body.contains("AA-2026-0001"));
    // a refund on the credit note
    x.app.get(&format!("/facturen/{c}")).await;
    x.app
        .post(
            &format!("/facturen/{c}/betalingen"),
            &[
                ("date", "2026-12-05"),
                ("amount", "217,80"),
                ("method", "bank"),
            ],
        )
        .await;
    assert_eq!(status_text(&mut x, c).await, "betaald");
    let refund: i64 = x.app.count("SELECT amount_cents FROM payments").await;
    assert_eq!(refund, -21_780);
}

#[tokio::test]
async fn overview_ageing_client_page_and_dashboard() {
    let mut x = fx().await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-01-02").await; // due 16-01-2026: long overdue
    x.app.post(&format!("/facturen/{a}/verzonden"), &[]).await;
    x.log("2026-12-01", x.task, "1:00").await;
    let _draft = x.draft(&[]).await;

    let list = x.app.get("/facturen").await;
    assert!(list.body.contains("AA-2026-0001") && list.body.contains("concept"));
    assert!(list.body.contains("vervallen"));
    let open = x.app.get("/facturen?status=open").await;
    assert!(open.body.contains("AA-2026-0001") && !open.body.contains(">concept</a>"));
    let drafts = x.app.get("/facturen?status=concept").await;
    assert!(!drafts.body.contains("AA-2026-0001"));

    let ageing = x.app.get("/facturen/ouderdom").await;
    assert!(ageing.body.contains("Sportclub"));
    assert!(ageing.body.contains("€ 217,80"), "{}", ageing.body);

    let client = x.app.get(&format!("/klanten/{}", x.client)).await;
    assert!(client.body.contains("AA-2026-0001"));

    let dash = x.app.get("/").await;
    assert!(dash.body.contains("Openstaande facturen"));
    assert!(dash.body.contains("Vervallen"));
}

#[tokio::test]
async fn btw_summary_for_the_quarter() {
    let mut x = fx().await;
    // invoice in Q4: € 180,00 + € 37,80 (21%) and a € 0 line
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    // a 9% manual-line invoice in Q4
    x.log("2026-12-01", x.task, "1:00").await;
    let b = x.draft(&[]).await;
    let l: i64 = x
        .app
        .count(&format!(
            "SELECT MIN(id) FROM invoice_lines WHERE invoice_id = {b}"
        ))
        .await;
    x.app.get(&format!("/facturen/{b}")).await;
    x.app
        .post(&format!("/facturen/{b}/regels/{l}/verwijderen"), &[])
        .await;
    x.app
        .post(
            &format!("/facturen/{b}/regels"),
            &[
                ("description", "Boek"),
                ("quantity", "1"),
                ("unit", "stuk"),
                ("price", "100"),
                ("vat_bp", "900"),
            ],
        )
        .await;
    x.issue(b, "2026-12-01").await;
    // purchase documents in Q4: one with € 21,00 VAT, one without amounts
    x.app.get("/documenten/nieuw").await;
    x.app
        .upload(
            "/documenten/nieuw",
            &[
                ("kind", "purchase_invoice"),
                ("doc_date", "2026-10-15"),
                ("supplier", "KPN"),
                ("amount", "121,00"),
                ("vat", "21,00"),
            ],
            Some(("file", "kpn.pdf", b"%PDF-1.4 kpn")),
        )
        .await;
    x.app.get("/documenten/nieuw").await;
    x.app
        .upload(
            "/documenten/nieuw",
            &[
                ("kind", "receipt"),
                ("doc_date", "2026-11-02"),
                ("supplier", "AH"),
            ],
            Some(("file", "ah.pdf", b"%PDF-1.4 ah")),
        )
        .await;
    // a draft in the quarter
    x.log("2026-12-02", x.task, "1:00").await;
    x.draft(&[]).await;

    let r = x.app.get("/btw?jaar=2026&kwartaal=4").await;
    assert_eq!(r.status, StatusCode::OK);
    let b = &r.body;
    assert!(b.contains("Btw-overzicht Q4 2026"));
    assert!(b.contains("€ 180,00") && b.contains("€ 37,80"), "1a");
    assert!(b.contains("€ 100,00") && b.contains("€ 9,00"), "1b");
    assert!(
        b.contains("<td class=\"num\">€ 46,80</td>"),
        "5a = 37,80 + 9,00: {b}"
    );
    assert!(b.contains("<td class=\"num\">€ 21,00</td>"), "5b");
    assert!(b.contains("Te betalen"));
    assert!(b.contains("€ 25,80"), "46,80 - 21,00");
    assert!(b.contains("€ 25<"), "rounded: 37 + 9 - 21 = 25");
    assert!(b.contains("1 inkoopdocument(en) zonder btw-bedrag"));
    assert!(b.contains("1 conceptfactuur/facturen"));

    // a credit note in the quarter lowers it
    x.app.get(&format!("/facturen/{a}")).await;
    x.app.post(&format!("/facturen/{a}/crediteren"), &[]).await;
    let c = x.app.last_id("invoices").await;
    x.issue(c, "2026-12-03").await;
    let r = x.app.get("/btw?jaar=2026&kwartaal=4").await;
    assert!(r.body.contains("Terug te vragen"), "9,00 - 21,00 < 0");

    let csv = x.app.get("/btw.csv?jaar=2026&kwartaal=4").await;
    assert!(csv.body.starts_with("\u{feff}Rubriek;Omschrijving"));
    assert!(csv.body.contains("5b;Voorbelasting;;21,00;;21"));
    // Q3 is empty
    assert!(x
        .app
        .get("/btw?jaar=2026&kwartaal=3")
        .await
        .body
        .contains("€ 0,00"));
}

/// Renderer that, during its first render, lets "a colleague" issue an
/// invoice (simulated by bumping the number series), as can happen while
/// Typst runs outside the write transaction.
struct InterruptingPdf {
    pool: std::sync::OnceLock<sqlx::SqlitePool>,
    calls: std::sync::atomic::AtomicUsize,
}

impl kwartje::pdf::PdfRenderer for InterruptingPdf {
    fn render(&self, t: &str, data: &str, a: &[(String, Vec<u8>)]) -> anyhow::Result<Vec<u8>> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            let pool = self.pool.get().unwrap().clone();
            tokio::runtime::Handle::current().block_on(async move {
                sqlx::query(
                    "INSERT INTO invoice_sequences (company_id, year, last_number, created_at, updated_at)
                     SELECT id, 2026, 5, 't', 't' FROM companies",
                )
                .execute(&pool)
                .await
                .unwrap();
            });
        }
        kwartje::pdf::FakePdf { fail: false }.render(t, data, a)
    }
}

#[tokio::test]
async fn a_number_taken_while_rendering_is_noticed_and_the_pdf_redone() {
    let pdf = std::sync::Arc::new(InterruptingPdf {
        pool: std::sync::OnceLock::new(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let services = kwartje::web::Services {
        pdf: pdf.clone(),
        mailer: kwartje::mail::Mailer::Disabled,
    };
    let mut x = fx_with(services).await;
    pdf.pool.set(x.app.pool.clone()).unwrap();
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    // rendered twice: once for 0001 (taken meanwhile), then for 0006
    assert_eq!(pdf.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(x.number(a).await.as_deref(), Some("AA-2026-0006"));
    let path: String = sqlx::query_scalar("SELECT d.file_path FROM documents d JOIN invoices i ON i.pdf_document_id = d.id WHERE i.id = ?")
        .bind(a).fetch_one(&x.app.pool).await.unwrap();
    let stored = std::fs::read_to_string(x.app.cfg.data_dir.join(&path)).unwrap();
    assert!(stored.contains("AA-2026-0006"), "{stored}");
    assert!(!stored.contains("AA-2026-0001"));
}

#[tokio::test]
async fn numbers_carry_the_client_prefix_in_one_company_series() {
    let mut x = fx().await;
    // the fixture client has prefix AA; a second client gets VH
    x.app.get("/klanten/nieuw").await;
    x.app
        .post(
            "/klanten",
            &[
                ("name", "Vliegende Hollander"),
                ("street", "Dijk 1"),
                ("postal_code", "1234AB"),
                ("city", "Hoorn"),
                ("invoice_prefix", "vh"),
            ],
        )
        .await;
    let vh = x.app.last_id("clients").await;
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    assert_eq!(x.number(a).await.as_deref(), Some("AA-2026-0001"));
    // hours for the second client: its invoice takes the next number in
    // the same series, with its own prefix
    let task = x.task;
    x.client = vh;
    x.log("2026-11-10", task, "1:00").await;
    let b = x.draft(&[]).await;
    assert_ne!(a, b);
    x.issue(b, "2026-11-30").await;
    assert_eq!(x.number(b).await.as_deref(), Some("VH-2026-0002"));
}

#[tokio::test]
async fn client_prefix_is_required_to_issue_and_unique_per_company() {
    let mut x = fx().await;
    // remove the fixture client's prefix: issuing is refused with a clear reason
    sqlx::query("UPDATE clients SET invoice_prefix = NULL")
        .execute(&x.app.pool)
        .await
        .unwrap();
    let a = x.draft(&[]).await;
    x.issue(a, "2026-11-30").await;
    assert_eq!(x.number(a).await, None);
    let page = x.app.get(&format!("/facturen/{a}")).await;
    assert!(
        page.body.contains("factuurprefix van klant Sportclub"),
        "{}",
        page.body
    );
    // a second client can't take a prefix another client already has
    sqlx::query("UPDATE clients SET invoice_prefix = 'SC'")
        .execute(&x.app.pool)
        .await
        .unwrap();
    x.app.get("/klanten/nieuw").await;
    let r = x
        .app
        .post("/klanten", &[("name", "Tweede"), ("invoice_prefix", "sc")])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        r.body
            .contains("Prefix SC wordt al gebruikt door klant Sportclub"),
        "{}",
        r.body
    );
    let r = x
        .app
        .post("/klanten", &[("name", "Derde"), ("invoice_prefix", "A-B")])
        .await;
    assert!(r.body.contains("1 tot 10 hoofdletters of cijfers"));
    // the database refuses a duplicate too
    let company: i64 = sqlx::query_scalar("SELECT company_id FROM clients LIMIT 1")
        .fetch_one(&x.app.pool)
        .await
        .unwrap();
    assert!(sqlx::query("INSERT INTO clients (company_id, name, invoice_prefix, created_at, updated_at) VALUES (?, 'X', 'SC', 't', 't')")
        .bind(company)
        .execute(&x.app.pool)
        .await
        .is_err());
}
