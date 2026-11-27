//! Document store helpers (pure, unit-tested): type sniffing, safe file
//! names, storage paths and the retention date.

use time::Date;

pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

pub const KINDS: &[(&str, &str)] = &[
    ("purchase_invoice", "Inkoopfactuur"),
    ("receipt", "Bon"),
    ("contract", "Contract"),
    ("other", "Overig"),
    ("sales_invoice", "Verkoopfactuur"),
    ("quote", "Offerte"),
];

/// Kinds a user may upload (sales invoices are created by the app).
pub const UPLOAD_KINDS: &[&str] = &["purchase_invoice", "receipt", "contract", "other"];

/// Purchase invoices and receipts: they need a supplier, count for the BTW
/// (5b) and are flagged when their amounts are missing. Contracts and other
/// documents need neither.
/// PDFs the app makes itself (issued invoices and quotes): read-only.
pub fn is_generated(kind: &str) -> bool {
    matches!(kind, "sales_invoice" | "quote")
}

pub fn is_purchase(kind: &str) -> bool {
    matches!(kind, "purchase_invoice" | "receipt")
}

pub fn kind_label(kind: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, l)| *l)
        .unwrap_or("Overig")
}

/// `(extension, mime)` from the file's first bytes; `None` for anything
/// other than PDF, JPEG or PNG.
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"%PDF-") {
        Some(("pdf", "application/pdf"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "image/jpeg"))
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else {
        None
    }
}

/// A file-system safe name with the sniffed extension, e.g.
/// `"Factuur KPN (okt).PDF"` → `Factuur_KPN_okt.pdf`.
pub fn safe_name(original: &str, ext: &str) -> String {
    let base = original.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    let mut out = String::new();
    let mut last_underscore = false;
    for c in stem.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if keep {
            out.push(c);
            last_underscore = c == '_';
        } else if !last_underscore && !out.is_empty() {
            out.push('_');
            last_underscore = true;
        }
    }
    let mut out: String = out.trim_matches('_').chars().take(80).collect();
    if out.is_empty() {
        out = "document".into();
    }
    format!("{out}.{ext}")
}

/// `documents/<company>/<year>/<kind>/<id>_<name>` (relative to data_dir).
pub fn path(company_id: i64, doc_date: Date, kind: &str, id: i64, name: &str) -> String {
    format!(
        "documents/{company_id}/{}/{kind}/{id}_{name}",
        doc_date.year()
    )
}

/// Keep until the end of the 7th year after the document date (fiscal
/// retention), e.g. a document of 15-03-2026 → 31-12-2033.
pub fn retain_until(doc_date: Date) -> Date {
    Date::from_calendar_date(doc_date.year() + 7, time::Month::December, 31).expect("valid date")
}

/// Quarter (1–4) of a date, and the first/last day of a quarter.
pub fn quarter_of(d: Date) -> u8 {
    (u8::from(d.month()) - 1) / 3 + 1
}

pub fn quarter_range(year: i32, q: u8) -> (Date, Date) {
    let first_month = time::Month::try_from((q.clamp(1, 4) - 1) * 3 + 1).expect("valid month");
    let from = Date::from_calendar_date(year, first_month, 1).expect("valid date");
    let last_month = time::Month::try_from((q.clamp(1, 4) - 1) * 3 + 3).expect("valid month");
    let next = if last_month == time::Month::December {
        Date::from_calendar_date(year + 1, time::Month::January, 1)
    } else {
        Date::from_calendar_date(year, last_month.next(), 1)
    }
    .expect("valid date");
    (from, next.previous_day().expect("valid date"))
}

/// A document whose file is missing or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub id: i64,
    pub file_path: String,
    pub missing: bool,
}

impl Problem {
    pub fn message(&self) -> String {
        if self.missing {
            format!(
                "document {}: bestand ontbreekt ({})",
                self.id, self.file_path
            )
        } else {
            format!(
                "document {}: controlesom klopt niet ({})",
                self.id, self.file_path
            )
        }
    }
}

/// Check every document (all companies, archived included) against the
/// files under `root` (data_dir, or a backup folder). Returns the number of
/// files checked and the problems.
pub async fn verify_all(
    pool: &sqlx::SqlitePool,
    root: &std::path::Path,
) -> anyhow::Result<(u64, Vec<Problem>)> {
    use sha2::{Digest, Sha256};
    let rows: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, file_path, sha256 FROM documents ORDER BY id")
            .fetch_all(pool)
            .await?;
    let mut problems = Vec::new();
    let mut checked = 0;
    for (id, file_path, sha) in rows {
        checked += 1;
        match std::fs::File::open(root.join(&file_path)) {
            Ok(mut f) => {
                let mut h = Sha256::new();
                std::io::copy(&mut f, &mut h)?;
                let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
                if got != sha {
                    problems.push(Problem {
                        id,
                        file_path,
                        missing: false,
                    });
                }
            }
            Err(_) => problems.push(Problem {
                id,
                file_path,
                missing: true,
            }),
        }
    }
    Ok((checked, problems))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"%PDF-1.7 ...").unwrap().0, "pdf");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE1]).unwrap().1, "image/jpeg");
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n....").unwrap().0, "png");
        assert!(sniff(b"GIF89a").is_none());
        assert!(sniff(b"<html>").is_none());
        assert!(sniff(b"").is_none());
    }

    #[test]
    fn safe_names() {
        assert_eq!(
            safe_name("Factuur KPN (okt).PDF", "pdf"),
            "Factuur_KPN_okt.pdf"
        );
        assert_eq!(safe_name("../../etc/passwd", "pdf"), "passwd.pdf");
        assert_eq!(safe_name("C:\\Users\\Bram\\bon.jpeg", "jpg"), "bon.jpg");
        assert_eq!(safe_name("bonnetje café.png", "png"), "bonnetje_caf.png");
        assert_eq!(safe_name("", "pdf"), "document.pdf");
        assert_eq!(safe_name("???", "pdf"), "document.pdf");
        assert_eq!(safe_name(&"a".repeat(200), "pdf").len(), 80 + 4);
    }

    #[test]
    fn paths_and_retention() {
        assert_eq!(
            path(3, date!(2026 - 10 - 05), "purchase_invoice", 17, "KPN.pdf"),
            "documents/3/2026/purchase_invoice/17_KPN.pdf"
        );
        assert_eq!(retain_until(date!(2026 - 03 - 15)), date!(2033 - 12 - 31));
        assert_eq!(kind_label("receipt"), "Bon");
        assert_eq!(kind_label("contract"), "Contract");
        assert!(is_purchase("receipt") && !is_purchase("contract") && !is_purchase("other"));
    }

    #[test]
    fn quarters() {
        assert_eq!(quarter_of(date!(2026 - 01 - 01)), 1);
        assert_eq!(quarter_of(date!(2026 - 06 - 30)), 2);
        assert_eq!(quarter_of(date!(2026 - 11 - 25)), 4);
        assert_eq!(
            quarter_range(2026, 1),
            (date!(2026 - 01 - 01), date!(2026 - 03 - 31))
        );
        assert_eq!(
            quarter_range(2026, 4),
            (date!(2026 - 10 - 01), date!(2026 - 12 - 31))
        );
        assert_eq!(quarter_range(2024, 1).1, date!(2024 - 03 - 31));
    }
}
