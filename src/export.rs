//! CSV export of every table: per table, as a zip with a manifest, and to a
//! directory (CLI and backup).
//!
//! Format: UTF-8 with BOM, `;` separator, CRLF, Dutch headers, ISO dates and
//! timestamps; money as euros with a decimal comma plus a cents column.
//!
//! **Every new table must be added to [`TABLES`]** (or, if it holds no
//! business data, to [`EXCLUDED`]). A test enforces this.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use time::Date;

use crate::{db, format};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Int,
    Bool,
    /// `YYYY-MM-DD`
    Date,
    /// RFC 3339 UTC
    Timestamp,
    /// cents → two columns: euros (`1234,56`) and cents
    Money,
    /// basis points → percentage (`21` / `5,5`)
    Bp,
    /// minutes → two columns: minutes and decimal hours (`1,50`)
    Minutes,
}

#[derive(Debug)]
pub struct Col {
    pub name: &'static str,
    pub header: &'static str,
    pub kind: Kind,
}

const fn col(name: &'static str, header: &'static str, kind: Kind) -> Col {
    Col { name, header, kind }
}

#[derive(Debug)]
pub struct ExportTable {
    pub name: &'static str,
    /// Dutch name for the export page.
    pub label: &'static str,
    pub columns: &'static [Col],
    /// Column (or SQL expression) holding the company id, for `?company=`.
    /// `None` = global table.
    pub company_col: Option<&'static str>,
    /// Column used for `?from=&to=` (date or timestamp; compared on its date part).
    pub date_col: &'static str,
}

use Kind::*;

pub const TABLES: &[ExportTable] = &[
    ExportTable {
        name: "companies",
        label: "Bedrijven",
        company_col: Some("id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("name", "Naam", Text),
            col("kvk", "KvK-nummer", Text),
            col("btw_id", "Btw-id", Text),
            col("iban", "IBAN", Text),
            col("street", "Straat", Text),
            col("postal_code", "Postcode", Text),
            col("city", "Plaats", Text),
            col("email", "E-mailadres", Text),
            col("phone", "Telefoon", Text),
            col("invoice_prefix", "Factuurprefix", Text),
            col("payment_term_days", "Betaaltermijn (dagen)", Int),
            col("default_vat_bp", "Standaard btw", Bp),
            col("rounding_minutes", "Afronding (minuten)", Int),
            col("logo_path", "Logo", Text),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "users",
        label: "Gebruikers",
        company_col: None,
        date_col: "created_at",
        // password_hash and totp_secret are deliberately not exported; they
        // are only in app.db.
        columns: &[
            col("id", "ID", Int),
            col("name", "Naam", Text),
            col("email", "E-mailadres", Text),
            col("role", "Rol", Text),
            col("must_change_password", "Moet wachtwoord wijzigen", Bool),
            col("last_login_at", "Laatst ingelogd", Timestamp),
            col("totp_enabled_at", "Tweestapsverificatie sinds", Timestamp),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "user_companies",
        label: "Toegang boekhouders",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("user_id", "Gebruiker (ID)", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("created_at", "Aangemaakt op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "clients",
        label: "Klanten",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("name", "Naam", Text),
            col("contact_name", "Contactpersoon", Text),
            col("email", "E-mailadres", Text),
            col("phone", "Telefoon", Text),
            col("street", "Straat", Text),
            col("postal_code", "Postcode", Text),
            col("city", "Plaats", Text),
            col("kvk", "KvK-nummer", Text),
            col("btw_id", "Btw-id", Text),
            col("notes", "Notities", Text),
            col("invoice_prefix", "Factuurprefix", Text),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "projects",
        label: "Projecten",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("client_id", "Klant (ID)", Int),
            col("name", "Naam", Text),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "tariffs",
        label: "Tarieven",
        company_col: Some("company_id"),
        date_col: "valid_from",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("lineage_id", "Tariefreeks (ID)", Int),
            col("name", "Naam", Text),
            col("unit", "Eenheid", Text),
            col("price_cents", "Prijs", Money),
            col("vat_bp", "Btw", Bp),
            col("client_id", "Klant (ID)", Int),
            col("project_id", "Project (ID)", Int),
            col("is_default", "Standaard", Bool),
            col("valid_from", "Geldig vanaf", Date),
            col("valid_to", "Geldig tot en met", Date),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "tasks",
        label: "Taken",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("name", "Naam", Text),
            col("client_id", "Klant (ID)", Int),
            col("project_id", "Project (ID)", Int),
            col("tariff_id", "Tarief (ID)", Int),
            col("billable", "Declarabel", Bool),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "time_entries",
        label: "Uren",
        company_col: Some("company_id"),
        date_col: "date",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("user_id", "Gebruiker (ID)", Int),
            col("date", "Datum", Date),
            col("task_id", "Taak (ID)", Int),
            col("client_id", "Klant (ID)", Int),
            col("project_id", "Project (ID)", Int),
            col("tariff_id", "Tarief (ID)", Int),
            col("minutes", "Duur", Minutes),
            col("description", "Omschrijving", Text),
            col("price_cents", "Uurprijs", Money),
            col("vat_bp", "Btw", Bp),
            col("invoice_line_id", "Factuurregel (ID)", Int),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "vehicles",
        label: "Voertuigen",
        company_col: None,
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("name", "Naam", Text),
            col("plate", "Kenteken", Text),
            col("kind", "Soort", Text),
            col("odometer_mode", "Registratie", Text),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "mileage_rates",
        label: "Kilometervergoeding per jaar",
        company_col: None,
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("year", "Jaar", Int),
            col("rate_cents_per_km", "Vergoeding per km", Money),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "saved_routes",
        label: "Vaste ritten",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("name", "Naam", Text),
            col("from_address", "Van", Text),
            col("to_address", "Naar", Text),
            col("km", "Kilometers", Int),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "mileage_entries",
        label: "Ritten",
        company_col: Some("company_id"),
        date_col: "date",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("vehicle_id", "Voertuig (ID)", Int),
            col("user_id", "Bestuurder (ID)", Int),
            col("date", "Datum", Date),
            col("from_address", "Van", Text),
            col("to_address", "Naar", Text),
            col("purpose", "Doel", Text),
            col("client_id", "Klant (ID)", Int),
            col("odometer_start", "Beginstand", Int),
            col("odometer_end", "Eindstand", Int),
            col("km", "Kilometers", Int),
            col("business", "Zakelijk", Bool),
            col("tariff_id", "Tarief (ID)", Int),
            col("price_cents", "Prijs per km", Money),
            col("vat_bp", "Btw", Bp),
            col("route_id", "Vaste rit (ID)", Int),
            col("invoice_line_id", "Factuurregel (ID)", Int),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "invoices",
        label: "Facturen",
        company_col: Some("company_id"),
        date_col: "period_to",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("client_id", "Klant (ID)", Int),
            col("kind", "Soort", Text),
            col("credit_for_invoice_id", "Creditnota voor factuur (ID)", Int),
            col("release_entries", "Uren vrijgegeven", Bool),
            col("number", "Factuurnummer", Text),
            col("issue_date", "Factuurdatum", Date),
            col("due_date", "Vervaldatum", Date),
            col("period_from", "Periode vanaf", Date),
            col("period_to", "Periode tot en met", Date),
            col("line_style", "Regels per", Text),
            col("hide_zero_lines", "€ 0-regels verborgen", Bool),
            col("include_hours_spec", "Urenspecificatie", Bool),
            col("intro_text", "Inleiding", Text),
            col("footer_text", "Voettekst", Text),
            col("subtotal_cents", "Subtotaal", Money),
            col("vat_cents", "Btw", Money),
            col("total_cents", "Totaal", Money),
            col("company_snapshot_json", "Bedrijfsgegevens (JSON)", Text),
            col("client_snapshot_json", "Klantgegevens (JSON)", Text),
            col("issued_at", "Definitief op", Timestamp),
            col("sent_at", "Verzonden op", Timestamp),
            col("sent_to", "Verzonden aan", Text),
            col("pdf_document_id", "PDF-document (ID)", Int),
            col("recurring_id", "Terugkerende factuur (ID)", Int),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "invoice_lines",
        label: "Factuurregels",
        company_col: Some("(SELECT i.company_id FROM invoices i WHERE i.id = invoice_id)"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("invoice_id", "Factuur (ID)", Int),
            col("position", "Volgorde", Int),
            col("description", "Omschrijving", Text),
            col("quantity_milli", "Aantal (duizendsten)", Int),
            col("unit", "Eenheid", Text),
            col("unit_price_cents", "Prijs per eenheid", Money),
            col("vat_bp", "Btw", Bp),
            col("line_total_cents", "Regeltotaal", Money),
            col("source", "Bron", Text),
            col("is_zero", "€ 0-regel", Bool),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "invoice_sequences",
        label: "Factuurnummering",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("year", "Jaar", Int),
            col("last_number", "Laatste nummer", Int),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
        ],
    },
    ExportTable {
        name: "payments",
        label: "Betalingen",
        company_col: Some("(SELECT i.company_id FROM invoices i WHERE i.id = invoice_id)"),
        date_col: "date",
        columns: &[
            col("id", "ID", Int),
            col("invoice_id", "Factuur (ID)", Int),
            col("date", "Datum", Date),
            col("amount_cents", "Bedrag", Money),
            col("method", "Betaalwijze", Text),
            col("note", "Notitie", Text),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "reminders",
        label: "Herinneringen",
        company_col: Some("(SELECT i.company_id FROM invoices i WHERE i.id = invoice_id)"),
        date_col: "sent_at",
        columns: &[
            col("id", "ID", Int),
            col("invoice_id", "Factuur (ID)", Int),
            col("level", "Niveau", Text),
            col("sent_at", "Verstuurd op", Timestamp),
            col("sent_to", "Verstuurd aan", Text),
            col("via_email", "Per e-mail", Bool),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "documents",
        label: "Documenten",
        company_col: Some("company_id"),
        date_col: "doc_date",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("kind", "Soort", Text),
            col("file_path", "Bestand", Text),
            col("sha256", "SHA-256", Text),
            col("original_name", "Oorspronkelijke naam", Text),
            col("mime", "Type", Text),
            col("size", "Grootte (bytes)", Int),
            col("doc_date", "Documentdatum", Date),
            col("supplier", "Leverancier", Text),
            col("description", "Omschrijving", Text),
            col("amount_cents", "Bedrag", Money),
            col("vat_cents", "Btw", Money),
            col("invoice_id", "Factuur (ID)", Int),
            col("client_id", "Klant (ID)", Int),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "quotes",
        label: "Offertes",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("client_id", "Klant (ID)", Int),
            col("number", "Offertenummer", Text),
            col("issue_date", "Datum", Date),
            col("valid_until", "Geldig tot", Date),
            col("intro_text", "Inleiding", Text),
            col("footer_text", "Voettekst", Text),
            col("subtotal_cents", "Subtotaal", Money),
            col("vat_cents", "Btw", Money),
            col("total_cents", "Totaal", Money),
            col("company_snapshot_json", "Bedrijfsgegevens (JSON)", Text),
            col("client_snapshot_json", "Klantgegevens (JSON)", Text),
            col("issued_at", "Definitief op", Timestamp),
            col("sent_at", "Verzonden op", Timestamp),
            col("sent_to", "Verzonden aan", Text),
            col("accepted_at", "Geaccepteerd op", Timestamp),
            col("rejected_at", "Afgewezen op", Timestamp),
            col("invoice_id", "Factuur (ID)", Int),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
            col("pdf_document_id", "PDF-document (ID)", Int),
        ],
    },
    ExportTable {
        name: "quote_lines",
        label: "Offerteregels",
        company_col: Some("(SELECT q.company_id FROM quotes q WHERE q.id = quote_id)"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("quote_id", "Offerte (ID)", Int),
            col("position", "Volgorde", Int),
            col("description", "Omschrijving", Text),
            col("quantity_milli", "Aantal (duizendsten)", Int),
            col("unit", "Eenheid", Text),
            col("unit_price_cents", "Prijs per eenheid", Money),
            col("vat_bp", "Btw", Bp),
            col("line_total_cents", "Regeltotaal", Money),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "quote_sequences",
        label: "Offertenummering",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("year", "Jaar", Int),
            col("last_number", "Laatste nummer", Int),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
        ],
    },
    ExportTable {
        name: "recurring_invoices",
        label: "Terugkerende facturen",
        company_col: Some("company_id"),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("company_id", "Bedrijf (ID)", Int),
            col("client_id", "Klant (ID)", Int),
            col("name", "Naam", Text),
            col("interval", "Interval", Text),
            col("next_date", "Volgende datum", Date),
            col("end_date", "Einddatum", Date),
            col("intro_text", "Inleiding", Text),
            col("footer_text", "Voettekst", Text),
            col("archived_at", "Gearchiveerd op", Timestamp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "recurring_lines",
        label: "Regels terugkerende facturen",
        company_col: Some(
            "(SELECT r.company_id FROM recurring_invoices r WHERE r.id = recurring_id)",
        ),
        date_col: "created_at",
        columns: &[
            col("id", "ID", Int),
            col("recurring_id", "Terugkerende factuur (ID)", Int),
            col("position", "Volgorde", Int),
            col("description", "Omschrijving", Text),
            col("quantity_milli", "Aantal (duizendsten)", Int),
            col("unit", "Eenheid", Text),
            col("unit_price_cents", "Prijs per eenheid", Money),
            col("vat_bp", "Btw", Bp),
            col("created_at", "Aangemaakt op", Timestamp),
            col("updated_at", "Gewijzigd op", Timestamp),
            col("created_by", "Aangemaakt door (ID)", Int),
        ],
    },
    ExportTable {
        name: "audit_log",
        label: "Logboek",
        company_col: None,
        date_col: "at",
        columns: &[
            col("id", "ID", Int),
            col("user_id", "Gebruiker (ID)", Int),
            col("table_name", "Tabel", Text),
            col("row_id", "Rij (ID)", Int),
            col("action", "Actie", Text),
            col("at", "Tijdstip", Timestamp),
            col("diff_json", "Wijzigingen (JSON)", Text),
        ],
    },
];

/// Tables that are deliberately not exported (no business data).
/// `trusted_devices` holds login trust
/// (token hashes); like the sessions it is only in `app.db` (and so in backups).
/// `security_events` (the sign-in log) has its own CSV download for the
/// beheerder on /beveiliging.
pub const EXCLUDED: &[&str] = &[
    "_sqlx_migrations",
    "tower_sessions",
    "trusted_devices",
    "security_events",
];

pub fn find(name: &str) -> Option<&'static ExportTable> {
    TABLES.iter().find(|t| t.name == name)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Filter {
    pub company_id: Option<i64>,
    #[serde(serialize_with = "ser_date")]
    pub from: Option<Date>,
    #[serde(serialize_with = "ser_date")]
    pub to: Option<Date>,
}

fn ser_date<S: serde::Serializer>(d: &Option<Date>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_str(&format::iso_date(*d)),
        None => s.serialize_none(),
    }
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.company_id.is_none() && self.from.is_none() && self.to.is_none()
    }
}

fn select_sql(t: &ExportTable, f: &Filter) -> (String, Vec<SqlArg>) {
    let cols: Vec<&str> = t.columns.iter().map(|c| c.name).collect();
    let mut sql = format!("SELECT {} FROM {}", cols.join(", "), t.name);
    let mut wh = Vec::new();
    let mut args = Vec::new();
    if let (Some(cc), Some(id)) = (t.company_col, f.company_id) {
        wh.push(format!("{cc} = ?"));
        args.push(SqlArg::Int(id));
    }
    if let Some(from) = f.from {
        wh.push(format!("substr({}, 1, 10) >= ?", t.date_col));
        args.push(SqlArg::Text(format::iso_date(from)));
    }
    if let Some(to) = f.to {
        wh.push(format!("substr({}, 1, 10) <= ?", t.date_col));
        args.push(SqlArg::Text(format::iso_date(to)));
    }
    if !wh.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wh.join(" AND "));
    }
    sql.push_str(" ORDER BY id");
    (sql, args)
}

enum SqlArg {
    Int(i64),
    Text(String),
}

fn header_row(t: &ExportTable) -> Vec<String> {
    let mut out = Vec::new();
    for c in t.columns {
        match c.kind {
            Money => {
                out.push(format!("{} (€)", c.header));
                out.push(format!("{} (cent)", c.header));
            }
            Bp => out.push(format!("{} (%)", c.header)),
            Minutes => {
                out.push(format!("{} (minuten)", c.header));
                out.push(format!("{} (uren)", c.header));
            }
            _ => out.push(c.header.to_string()),
        }
    }
    out
}

/// `90` minutes → `1,50` (hours, two decimals, half-up).
pub fn hours_decimal(minutes: i64) -> String {
    let hundredths = (minutes.abs() * 100 + 30) / 60;
    format::decimal(if minutes < 0 { -hundredths } else { hundredths })
}

/// How CSV text is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvMode {
    /// Exactly the stored values: backups and `kwartje export`, which restore
    /// and checks rely on.
    Exact,
    /// For downloads that are opened in a spreadsheet: text that would be
    /// read as a formula gets a leading `'` (see `spreadsheet_text`).
    Spreadsheet,
}

/// Neutralise spreadsheet formulas: text starting with `=`, `+`, `-`, `@`,
/// a tab or a carriage return gets a leading `'`, so Excel/LibreOffice show
/// it as text instead of running it. Only for free-text columns: numbers
/// such as `-12,50` must stay numbers.
pub fn spreadsheet_text(s: &str) -> std::borrow::Cow<'_, str> {
    match s.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{s}").into(),
        _ => s.into(),
    }
}

fn cell_values(
    c: &Col,
    row: &sqlx::sqlite::SqliteRow,
    i: usize,
    mode: CsvMode,
) -> Result<Vec<String>> {
    let s = match c.kind {
        Text => {
            let v = row.try_get::<Option<String>, _>(i)?.unwrap_or_default();
            vec![match mode {
                CsvMode::Exact => v,
                CsvMode::Spreadsheet => spreadsheet_text(&v).into_owned(),
            }]
        }
        Date | Timestamp => {
            vec![row.try_get::<Option<String>, _>(i)?.unwrap_or_default()]
        }
        Int => vec![row
            .try_get::<Option<i64>, _>(i)?
            .map(|v| v.to_string())
            .unwrap_or_default()],
        Bool => vec![match row.try_get::<Option<i64>, _>(i)? {
            Some(0) => "nee".into(),
            Some(_) => "ja".into(),
            None => String::new(),
        }],
        Money => match row.try_get::<Option<i64>, _>(i)? {
            Some(v) => vec![format::decimal(v), v.to_string()],
            None => vec![String::new(), String::new()],
        },
        Minutes => match row.try_get::<Option<i64>, _>(i)? {
            Some(m) => vec![m.to_string(), hours_decimal(m)],
            None => vec![String::new(), String::new()],
        },
        Bp => vec![row
            .try_get::<Option<i64>, _>(i)?
            .map(|v| format::vat_pct(v).trim_end_matches('%').to_string())
            .unwrap_or_default()],
    };
    Ok(s)
}

/// Write one table as CSV. Returns the number of data rows.
pub async fn write_table<W: Write>(
    pool: &SqlitePool,
    t: &ExportTable,
    f: &Filter,
    mode: CsvMode,
    out: W,
) -> Result<usize> {
    let (sql, args) = select_sql(t, f);
    // Safe: table and column names come from the static TABLES registry;
    // filter values are bound parameters.
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for a in args {
        q = match a {
            SqlArg::Int(v) => q.bind(v),
            SqlArg::Text(v) => q.bind(v),
        };
    }
    let rows = q
        .fetch_all(pool)
        .await
        .with_context(|| format!("lezen van {}", t.name))?;

    let mut out = out;
    out.write_all("\u{feff}".as_bytes())?;
    let mut w = csv::WriterBuilder::new()
        .delimiter(b';')
        .terminator(csv::Terminator::CRLF)
        .from_writer(out);
    w.write_record(header_row(t))?;
    for row in &rows {
        let mut rec = Vec::with_capacity(t.columns.len() + 2);
        for (i, c) in t.columns.iter().enumerate() {
            rec.extend(cell_values(c, row, i, mode)?);
        }
        w.write_record(&rec)?;
    }
    w.flush()?;
    Ok(rows.len())
}

/// One table as a CSV download (spreadsheet-safe).
pub async fn table_csv(pool: &SqlitePool, t: &ExportTable, f: &Filter) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    write_table(pool, t, f, CsvMode::Spreadsheet, &mut buf).await?;
    Ok(buf)
}

#[derive(Debug, Clone, Serialize)]
pub struct TableEntry {
    pub name: String,
    pub file: String,
    pub rows: usize,
    pub sha256: String,
    /// False for global tables (e.g. users) when a company filter was given.
    pub company_filter_applied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub app: &'static str,
    pub app_version: &'static str,
    pub schema_version: i64,
    pub created_at: String,
    pub filter: Filter,
    pub tables: Vec<TableEntry>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every table as `(file name, CSV bytes)` plus the manifest.
pub async fn all_tables(
    pool: &SqlitePool,
    f: &Filter,
    mode: CsvMode,
) -> Result<(Vec<(String, Vec<u8>)>, Manifest)> {
    tables_where(pool, f, mode, |_| true).await
}

/// What a read-only boekhouder may download: per-company tables (with a
/// company filter) plus the shared vehicles and allowance rates. Users and
/// the audit log span every company and stay with the beheerder.
pub fn accountant_may_export(t: &ExportTable) -> bool {
    (t.company_col.is_some() && t.name != "user_companies")
        || matches!(t.name, "vehicles" | "mileage_rates")
}

async fn tables_where(
    pool: &SqlitePool,
    f: &Filter,
    mode: CsvMode,
    keep: impl Fn(&ExportTable) -> bool,
) -> Result<(Vec<(String, Vec<u8>)>, Manifest)> {
    let mut files = Vec::new();
    let mut entries = Vec::new();
    for t in TABLES.iter().filter(|t| keep(t)) {
        let mut buf = Vec::new();
        let rows = write_table(pool, t, f, mode, &mut buf).await?;
        let file = format!("{}.csv", t.name);
        entries.push(TableEntry {
            name: t.name.to_string(),
            file: file.clone(),
            rows,
            sha256: sha256_hex(&buf),
            company_filter_applied: f.company_id.is_some() && t.company_col.is_some(),
        });
        files.push((file, buf));
    }
    let manifest = Manifest {
        app: "kwartje",
        app_version: env!("CARGO_PKG_VERSION"),
        schema_version: db::schema_version(pool).await?,
        created_at: db::now(),
        filter: f.clone(),
        tables: entries,
    };
    Ok((files, manifest))
}

/// `all.zip` download: one CSV per table + `manifest.json` (spreadsheet-safe).
pub async fn zip_all(pool: &SqlitePool, f: &Filter) -> Result<Vec<u8>> {
    zip_where(pool, f, |_| true).await
}

/// Like [`zip_all`], for the tables `keep` selects.
pub async fn zip_where(
    pool: &SqlitePool,
    f: &Filter,
    keep: impl Fn(&ExportTable) -> bool,
) -> Result<Vec<u8>> {
    let (files, manifest) = tables_where(pool, f, CsvMode::Spreadsheet, keep).await?;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &files {
        zip.start_file(name.as_str(), opts)?;
        zip.write_all(bytes)?;
    }
    zip.start_file("manifest.json", opts)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    Ok(zip.finish()?.into_inner())
}

/// Write every table plus `manifest.json` into `dir` (created if needed).
/// Exact values: used by `kwartje export` and the backups.
pub async fn export_to_dir(pool: &SqlitePool, dir: &Path, f: &Filter) -> Result<Manifest> {
    std::fs::create_dir_all(dir).with_context(|| format!("aanmaken van {}", dir.display()))?;
    let (files, manifest) = all_tables(pool, f, CsvMode::Exact).await?;
    for (name, bytes) in &files {
        let p = dir.join(name);
        std::fs::write(&p, bytes).with_context(|| format!("schrijven van {}", p.display()))?;
    }
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

/// Tables in the database that are neither exported nor explicitly excluded.
pub async fn unexported_tables(pool: &SqlitePool) -> Result<Vec<String>> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await?;
    Ok(names
        .into_iter()
        .filter(|n| find(n).is_none() && !EXCLUDED.contains(&n.as_str()))
        .collect())
}

/// Columns in exported tables that the registry does not mention, except
/// deliberately omitted secrets.
pub async fn unexported_columns(pool: &SqlitePool) -> Result<Vec<String>> {
    const SECRET: &[(&str, &str)] = &[
        ("users", "password_hash"),
        ("users", "totp_secret"),
        ("users", "totp_last_step"),
    ];
    let mut missing = Vec::new();
    for t in TABLES {
        // Safe: t.name comes from the static TABLES registry.
        let cols: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA table_info({})",
            t.name
        )))
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
        if cols.is_empty() {
            bail!("exported table {} does not exist", t.name);
        }
        for c in cols {
            if !t.columns.iter().any(|x| x.name == c) && !SECRET.contains(&(t.name, c.as_str())) {
                missing.push(format!("{}.{}", t.name, c));
            }
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreadsheet_text_neutralises_formulas() {
        for (input, out) in [
            ("=HYPERLINK(\"x\")", "'=HYPERLINK(\"x\")"),
            ("+31 6 1234", "'+31 6 1234"),
            ("-korting", "'-korting"),
            ("@SUM(A1)", "'@SUM(A1)"),
            ("\tx", "'\tx"),
            ("Sportclub", "Sportclub"),
            ("", ""),
            ("a=b", "a=b"),
        ] {
            assert_eq!(spreadsheet_text(input), out, "{input}");
        }
    }

    async fn pool() -> SqlitePool {
        let p = db::connect_memory().await.unwrap();
        db::migrate(&p).await.unwrap();
        // the session table, as created by the web layer
        tower_sessions_sqlx_store::SqliteStore::new(p.clone())
            .migrate()
            .await
            .unwrap();
        p
    }

    #[tokio::test]
    async fn every_table_is_exported_or_explicitly_excluded() {
        let p = pool().await;
        assert_eq!(unexported_tables(&p).await.unwrap(), Vec::<String>::new());
        assert_eq!(unexported_columns(&p).await.unwrap(), Vec::<String>::new());
    }

    #[test]
    fn hours_as_decimal() {
        assert_eq!(hours_decimal(90), "1,50");
        assert_eq!(hours_decimal(20), "0,33");
        assert_eq!(hours_decimal(40), "0,67");
        assert_eq!(hours_decimal(0), "0,00");
    }

    #[test]
    fn money_columns_get_euro_and_cent_headers() {
        static T: ExportTable = ExportTable {
            name: "x",
            label: "X",
            company_col: None,
            date_col: "d",
            columns: &[col("id", "ID", Int), col("amount", "Bedrag", Money)],
        };
        assert_eq!(header_row(&T), ["ID", "Bedrag (€)", "Bedrag (cent)"]);
    }

    #[tokio::test]
    async fn csv_has_bom_semicolons_crlf_and_dutch_headers() {
        let p = pool().await;
        sqlx::query(
            "INSERT INTO companies (name, invoice_prefix, created_at, updated_at, default_vat_bp)
             VALUES ('Bakker; Zn', 'BZ', '2026-01-02T10:00:00Z', '2026-01-02T10:00:00Z', 900)",
        )
        .execute(&p)
        .await
        .unwrap();
        let bytes = table_csv(&p, find("companies").unwrap(), &Filter::default())
            .await
            .unwrap();
        assert!(bytes.starts_with("\u{feff}".as_bytes()));
        let text = String::from_utf8(bytes).unwrap();
        let mut lines = text.trim_start_matches('\u{feff}').split("\r\n");
        let header = lines.next().unwrap();
        assert!(header.starts_with("ID;Naam;KvK-nummer;"));
        assert!(header.contains("Standaard btw (%)"));
        let row = lines.next().unwrap();
        // the ; in the name is quoted
        assert!(row.starts_with("1;\"Bakker; Zn\";"), "{row}");
        assert!(row.contains(";9;"));
    }

    #[tokio::test]
    async fn filters_by_company_and_date() {
        let p = pool().await;
        for (name, prefix, at) in [
            ("A", "A", "2026-01-15T10:00:00Z"),
            ("B", "B", "2026-03-15T10:00:00Z"),
        ] {
            sqlx::query("INSERT INTO companies (name, invoice_prefix, created_at, updated_at) VALUES (?, ?, ?, ?)")
                .bind(name).bind(prefix).bind(at).bind(at)
                .execute(&p).await.unwrap();
        }
        let t = find("companies").unwrap();
        let count = |f: Filter| {
            let p = p.clone();
            async move {
                write_table(&p, t, &f, CsvMode::Exact, std::io::sink())
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count(Filter::default()).await, 2);
        assert_eq!(
            count(Filter {
                company_id: Some(2),
                ..Default::default()
            })
            .await,
            1
        );
        assert_eq!(
            count(Filter {
                from: format::parse_date("01-02-2026"),
                ..Default::default()
            })
            .await,
            1
        );
        assert_eq!(
            count(Filter {
                to: format::parse_date("15-01-2026"),
                ..Default::default()
            })
            .await,
            1
        );
    }

    #[tokio::test]
    async fn child_tables_filter_by_their_invoices_company() {
        let p = pool().await;
        for sql in [
            "INSERT INTO companies (id, name, invoice_prefix, created_at, updated_at) VALUES (1, 'A', 'A', 't', 't'), (2, 'B', 'B', 't', 't')",
            "INSERT INTO clients (id, company_id, name, created_at, updated_at) VALUES (1, 1, 'K', 't', 't'), (2, 2, 'L', 't', 't')",
            "INSERT INTO invoices (id, company_id, client_id, period_from, period_to, created_at, updated_at) VALUES (1, 1, 1, 'a', 'b', 't', 't'), (2, 2, 2, 'a', 'b', 't', 't')",
            "INSERT INTO payments (invoice_id, date, amount_cents, method, created_at, updated_at) VALUES (1, '2026-01-01', 100, 'bank', 't', 't'), (2, '2026-01-01', 200, 'bank', 't', 't')",
        ] {
            sqlx::query(sql).execute(&p).await.unwrap();
        }
        let f = Filter {
            company_id: Some(2),
            ..Default::default()
        };
        let n = write_table(
            &p,
            find("payments").unwrap(),
            &f,
            CsvMode::Exact,
            std::io::sink(),
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn zip_contains_every_table_and_a_manifest() {
        let p = pool().await;
        let bytes = zip_all(&p, &Filter::default()).await.unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = z.file_names().map(|n| n.unwrap().to_string()).collect();
        for t in TABLES {
            assert!(names.contains(&format!("{}.csv", t.name)));
        }
        let mut m = String::new();
        std::io::Read::read_to_string(&mut z.by_name("manifest.json").unwrap(), &mut m).unwrap();
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["schema_version"], db::binary_schema_version());
        assert_eq!(v["tables"].as_array().unwrap().len(), TABLES.len());
    }
}
