mod common;

use axum::http::StatusCode;
use common::TestApp;

const PDF: &[u8] = b"%PDF-1.4\n% factuur KPN oktober\n%%EOF\n";
const JPG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 1, 2, 3,
];
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nbonnetje";

fn meta<'a>(
    supplier: &'a str,
    date: &'a str,
    amount: &'a str,
    vat: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("kind", "purchase_invoice"),
        ("doc_date", date),
        ("supplier", supplier),
        ("amount", amount),
        ("vat", vat),
        ("description", ""),
    ]
}

async fn up(app: &mut TestApp, fields: &[(&str, &str)], file: (&str, &str, &[u8])) -> common::Resp {
    app.get("/documenten/nieuw").await;
    app.upload("/documenten/nieuw", fields, Some(file)).await
}

#[tokio::test]
async fn upload_pdf_jpg_png_with_path_and_hash() {
    let (mut app, company) = TestApp::with_company().await;
    let r = up(
        &mut app,
        &meta("KPN", "2026-10-15", "121,00", "21,00"),
        ("file", "Factuur KPN (okt).pdf", PDF),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        r.location.as_deref(),
        Some("/documenten/nieuw?soort=purchase_invoice")
    );
    assert!(app
        .get("/documenten/nieuw")
        .await
        .body
        .contains("Inkoopfactuur opgeslagen. Nog een uploaden?"));
    let id = app.last_id("documents").await;
    let (path, sha, mime, amount): (String, String, String, Option<i64>) =
        sqlx::query_as("SELECT file_path, sha256, mime, amount_cents FROM documents WHERE id = ?")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        path,
        format!("documents/{company}/2026/purchase_invoice/{id}_Factuur_KPN_okt.pdf")
    );
    assert_eq!(mime, "application/pdf");
    assert_eq!(amount, Some(12_100));
    let stored = std::fs::read(app.cfg.data_dir.join(&path)).unwrap();
    assert_eq!(stored, PDF);
    assert_eq!(kwartje::export::sha256_hex(&stored), sha);

    let mut photo = vec![("kind", "receipt")];
    photo.extend(
        meta("Albert Heijn", "2026-11-02", "", "")
            .into_iter()
            .skip(1),
    );
    let r = up(&mut app, &photo, ("photo", "IMG_0001.JPG", JPG)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = up(
        &mut app,
        &meta("NS", "2026-11-03", "12,40", "0,70"),
        ("file", "bon.png", PNG),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let mimes: Vec<String> = sqlx::query_scalar("SELECT mime FROM documents ORDER BY id")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert_eq!(mimes, ["application/pdf", "image/jpeg", "image/png"]);

    // the file is served inline and can be framed by our own page only
    let f = app.get(&format!("/documenten/{id}/bestand")).await;
    assert_eq!(f.headers["content-type"], "application/pdf");
    assert_eq!(f.headers["x-frame-options"], "SAMEORIGIN");
    let page = app.get(&format!("/documenten/{id}")).await;
    assert!(page.body.contains("Bestand intact ✓"));
    assert!(page.body.contains("Bewaren tot 31-12-2033"));
}

#[tokio::test]
async fn upload_validation_type_size_and_duplicates() {
    let (mut app, _) = TestApp::with_company().await;
    let r = up(
        &mut app,
        &meta("", "", "abc", ""),
        ("file", "x.gif", b"GIF89a...."),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    for msg in [
        "Vul de leverancier of winkel in.",
        "Vul de datum van het document in.",
        "Vul een bedrag in zoals",
        "Alleen PDF, JPG of PNG",
    ] {
        assert!(r.body.contains(msg), "{msg}");
    }
    app.get("/documenten/nieuw").await;
    let r = app
        .upload(
            "/documenten/nieuw",
            &meta("KPN", "2026-10-15", "", ""),
            None,
        )
        .await;
    assert!(r.body.contains("Kies een bestand of maak een foto."));

    let big = {
        let mut v = b"%PDF-1.4\n".to_vec();
        v.resize(21 * 1024 * 1024, b'x');
        v
    };
    let r = up(
        &mut app,
        &meta("KPN", "2026-10-15", "", ""),
        ("file", "groot.pdf", &big),
    )
    .await;
    assert!(
        r.status == StatusCode::BAD_REQUEST
            || r.status == StatusCode::PAYLOAD_TOO_LARGE
            || r.status == StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.status
    );
    assert_eq!(app.count("SELECT COUNT(*) FROM documents").await, 0);

    up(
        &mut app,
        &meta("KPN", "2026-10-15", "", ""),
        ("file", "a.pdf", PDF),
    )
    .await;
    let first = app.last_id("documents").await;
    let r = up(
        &mut app,
        &meta("KPN", "2026-10-16", "", ""),
        ("file", "kopie.pdf", PDF),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Dit bestand is al opgeslagen."));
    assert!(r.body.contains(&format!("/documenten/{first}")));
    // also when the original is archived
    app.get(&format!("/documenten/{first}")).await;
    app.post(&format!("/documenten/{first}/archiveren"), &[])
        .await;
    let r = up(
        &mut app,
        &meta("KPN", "2026-10-16", "", ""),
        ("file", "kopie.pdf", PDF),
    )
    .await;
    assert!(r.body.contains("gearchiveerd; je kunt het terugzetten"));
    assert_eq!(app.count("SELECT COUNT(*) FROM documents").await, 1);
}

#[tokio::test]
async fn list_filters_totals_edit_archive_and_scope() {
    let (mut app, _) = TestApp::with_company().await;
    up(
        &mut app,
        &meta("KPN", "2026-10-15", "121,00", "21,00"),
        ("file", "kpn.pdf", PDF),
    )
    .await;
    let kpn = app.last_id("documents").await;
    up(
        &mut app,
        &meta("Shell", "2026-11-20", "60,50", "10,50"),
        ("file", "shell.png", PNG),
    )
    .await;
    up(
        &mut app,
        &meta("Bol.com", "2026-07-01", "", ""),
        ("file", "bol.jpg", JPG),
    )
    .await;
    let bol = app.last_id("documents").await;

    let q4 = app.get("/documenten?jaar=2026&kwartaal=4").await;
    assert!(q4.body.contains("KPN") && q4.body.contains("Shell") && !q4.body.contains("Bol.com"));
    assert!(
        q4.body.contains("€ 181,50") && q4.body.contains("waarvan btw € 31,50"),
        "{}",
        q4.body
    );
    let q3 = app.get("/documenten?jaar=2026&kwartaal=3").await;
    assert!(q3.body.contains("bedrag ontbreekt"));
    let search = app.get("/documenten?jaar=2026&kwartaal=&q=shell").await;
    assert!(search.body.contains("Shell") && !search.body.contains(">KPN<"));
    let missing = app.get("/documenten?jaar=2026&kwartaal=&zonder=1").await;
    assert!(missing.body.contains("Bol.com") && !missing.body.contains("Shell"));

    // fill in the amounts later
    app.get(&format!("/documenten/{bol}")).await;
    let r = app
        .post(
            &format!("/documenten/{bol}"),
            &[
                ("kind", "purchase_invoice"),
                ("doc_date", "2026-07-01"),
                ("supplier", "Bol.com"),
                ("amount", "24,20"),
                ("vat", "4,20"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(!app
        .get("/documenten?jaar=2026&kwartaal=&zonder=1")
        .await
        .body
        .contains("Bol.com"));

    // archive: hidden, file kept, audited
    app.get(&format!("/documenten/{kpn}")).await;
    app.post(&format!("/documenten/{kpn}/archiveren"), &[])
        .await;
    assert!(!app
        .get("/documenten?jaar=2026&kwartaal=4")
        .await
        .body
        .contains(">KPN<"));
    let path: String = sqlx::query_scalar("SELECT file_path FROM documents WHERE id = ?")
        .bind(kpn)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(app.cfg.data_dir.join(path).exists());
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE table_name = 'documents' ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["insert", "insert", "insert", "update", "archive"]);

    // another company sees nothing
    let beta = app.create_company("Beta", "BB").await;
    app.select_company(beta).await;
    assert_eq!(
        app.get(&format!("/documenten/{bol}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&format!("/documenten/{bol}/bestand")).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!app
        .get("/documenten?jaar=2026&kwartaal=")
        .await
        .body
        .contains("Bol.com"));
}

#[tokio::test]
async fn sales_invoice_documents_are_read_only() {
    let (mut app, company) = TestApp::with_company().await;
    sqlx::query("INSERT INTO documents (company_id, kind, file_path, sha256, original_name, mime, size, doc_date, created_at, updated_at) VALUES (?, 'sales_invoice', 'documents/x.pdf', 'abc', 'AA-2026-0001.pdf', 'application/pdf', 3, '2026-11-30', 't', 't')")
        .bind(company).execute(&app.pool).await.unwrap();
    let id = app.last_id("documents").await;
    let page = app.get(&format!("/documenten/{id}")).await;
    assert!(page.body.contains("door de app gemaakt"));
    assert!(page.body.contains("het bestand ontbreekt"));
    assert_eq!(
        app.post(
            &format!("/documenten/{id}"),
            &[("kind", "other"), ("doc_date", "2026-11-30")]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(&format!("/documenten/{id}/archiveren"), &[])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn contracts_need_no_supplier_or_amounts_and_stay_out_of_btw() {
    let (mut app, company) = TestApp::with_company().await;
    app.get("/klanten/nieuw").await;
    app.post("/klanten", &[("name", "Sportclub")]).await;
    let client = app.last_id("clients").await.to_string();
    let r = up(
        &mut app,
        &[
            ("kind", "contract"),
            ("doc_date", "2026-07-01"),
            ("supplier", ""),
            ("amount", ""),
            ("vat", ""),
            ("description", "Bemiddelingsovereenkomst 2026-2028"),
            ("client_id", &client),
        ],
        (
            "file",
            "Contract Sportclub.pdf",
            b"%PDF-1.4\n% contract\n%%EOF\n",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = app.last_id("documents").await;
    let (kind, path): (String, String) =
        sqlx::query_as("SELECT kind, file_path FROM documents WHERE id = ?")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(kind, "contract");
    assert_eq!(
        path,
        format!("documents/{company}/2026/contract/{id}_Contract_Sportclub.pdf")
    );
    // findable by kind, not flagged for missing amounts
    let list = app
        .get("/documenten?jaar=2026&kwartaal=3&soort=contract")
        .await;
    assert!(
        list.body.contains("Bemiddelingsovereenkomst"),
        "{}",
        list.body
    );
    assert!(!list.body.contains("bedrag ontbreekt"));
    let missing = app.get("/documenten?jaar=2026&kwartaal=3&zonder=1").await;
    assert!(!missing.body.contains("Bemiddelingsovereenkomst"));
    // not counted as a purchase document in the BTW overview
    let btw = app.get("/btw?jaar=2026&kwartaal=3").await;
    assert!(
        btw.body.contains("0 inkoopfactuur/facturen"),
        "{}",
        btw.body
    );
    // shown on the client's page
    let page = app.get(&format!("/klanten/{client}")).await;
    assert!(page.body.contains("Bemiddelingsovereenkomst") || page.body.contains("Contract"));
}
