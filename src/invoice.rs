//! Invoice building (pure, unit-tested): lines from hours and km, rounding,
//! numbering and the derived status.
//!
//! Hours: each entry is rounded up to the company's rounding step, rounded
//! minutes are summed per group, the quantity is hours with two decimals
//! (half-up), and the line total is quantity × price (half-up), so the
//! printed quantity × price always equals the line amount.

use std::collections::BTreeMap;

use crate::format;
use crate::money::{self, Overflow};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    Task,
    Tariff,
    Project,
    Entry,
}

impl LineStyle {
    pub const ALL: [(LineStyle, &'static str, &'static str); 4] = [
        (LineStyle::Task, "task", "Eén regel per taak"),
        (LineStyle::Tariff, "tariff", "Eén regel per tarief"),
        (LineStyle::Project, "project", "Eén regel per project"),
        (LineStyle::Entry, "entry", "Eén regel per urenregel"),
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, k, _)| *k == s)
            .map(|(v, _, _)| *v)
    }

    pub fn key(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(v, _, _)| *v == self)
            .map(|(_, k, _)| *k)
            .unwrap_or("task")
    }
}

/// An unbilled hour entry (with its price/VAT snapshot).
#[derive(Debug, Clone)]
pub struct HourItem {
    pub entry_id: i64,
    pub date: String,
    pub task_id: i64,
    pub task_name: String,
    pub project_id: Option<i64>,
    pub project_name: Option<String>,
    pub tariff_id: i64,
    pub tariff_name: String,
    pub price_cents: i64,
    pub vat_bp: i64,
    pub minutes: i64,
    pub description: String,
}

/// An unbilled, billable trip.
#[derive(Debug, Clone)]
pub struct KmItem {
    pub entry_id: i64,
    pub date: String,
    pub tariff_id: i64,
    pub tariff_name: String,
    pub price_cents: i64,
    pub vat_bp: i64,
    pub km: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftLine {
    pub description: String,
    pub quantity_milli: i64,
    pub unit: &'static str,
    pub unit_price_cents: i64,
    pub vat_bp: i64,
    pub line_total_cents: i64,
    pub source: &'static str,
    pub is_zero: bool,
    pub hour_entries: Vec<i64>,
    pub km_entries: Vec<i64>,
}

/// Hours as a quantity: two decimals, half-up, in thousandths (`90` → `1500`).
pub fn hours_quantity_milli(minutes: i64) -> i64 {
    let hundredths = (minutes.abs() * 100 * 2 + 60) / 120;
    let h = if minutes < 0 { -hundredths } else { hundredths };
    h * 10
}

/// `1500` → `1,5`; `1250` → `1,25`; `3000` → `3`.
pub fn quantity_label(milli: i64) -> String {
    let sign = if milli < 0 { "-" } else { "" };
    let m = milli.unsigned_abs();
    let whole = m / 1000;
    let frac = m % 1000;
    if frac == 0 {
        return format!("{sign}{whole}");
    }
    let s = format!("{frac:03}");
    format!("{sign}{whole},{}", s.trim_end_matches('0'))
}

#[derive(Default)]
struct Acc {
    first_date: String,
    description: String,
    minutes: i64,
    km: i64,
    price: i64,
    vat: i64,
    entries: Vec<i64>,
}

fn finish(acc: Acc, unit: &'static str, source: &'static str) -> Result<DraftLine, Overflow> {
    let quantity_milli = if unit == "km" {
        acc.km.checked_mul(1000).ok_or(Overflow)?
    } else {
        hours_quantity_milli(acc.minutes)
    };
    let total = money::line_total_cents(quantity_milli, acc.price)?;
    let (hour_entries, km_entries) = if source == "km" {
        (Vec::new(), acc.entries)
    } else {
        (acc.entries, Vec::new())
    };
    Ok(DraftLine {
        description: acc.description,
        quantity_milli,
        unit,
        unit_price_cents: acc.price,
        vat_bp: acc.vat,
        line_total_cents: total,
        source,
        is_zero: acc.price == 0,
        hour_entries,
        km_entries,
    })
}

/// Build invoice lines from unbilled hours and km.
pub fn build_lines(
    hours: &[HourItem],
    km: &[KmItem],
    style: LineStyle,
    rounding_minutes: i64,
) -> Result<Vec<DraftLine>, Overflow> {
    let mut groups: BTreeMap<String, Acc> = BTreeMap::new();
    for h in hours {
        let minutes = money::round_up_minutes(h.minutes, rounding_minutes);
        let (key, description) = match style {
            LineStyle::Task => (
                format!("t{}:{}:{}", h.task_id, h.price_cents, h.vat_bp),
                h.task_name.clone(),
            ),
            LineStyle::Tariff => (
                format!("r{}:{}:{}", h.tariff_id, h.price_cents, h.vat_bp),
                h.tariff_name.clone(),
            ),
            LineStyle::Project => (
                format!(
                    "p{}:{}:{}",
                    h.project_id.unwrap_or(0),
                    h.price_cents,
                    h.vat_bp
                ),
                h.project_name
                    .clone()
                    .unwrap_or_else(|| "Werkzaamheden".into()),
            ),
            LineStyle::Entry => {
                let date = format::parse_iso_date(&h.date)
                    .map(format::date)
                    .unwrap_or_default();
                let mut d = format!("{date} {}", h.task_name);
                if !h.description.is_empty() {
                    d.push_str(": ");
                    d.push_str(&h.description);
                }
                (format!("e{}", h.entry_id), d)
            }
        };
        let acc = groups.entry(key).or_insert_with(|| Acc {
            first_date: h.date.clone(),
            description,
            price: h.price_cents,
            vat: h.vat_bp,
            ..Default::default()
        });
        acc.minutes += minutes;
        if h.date < acc.first_date {
            acc.first_date = h.date.clone();
        }
        acc.entries.push(h.entry_id);
    }
    let mut hour_lines: Vec<Acc> = groups.into_values().collect();
    hour_lines
        .sort_by(|a, b| (&a.first_date, &a.description).cmp(&(&b.first_date, &b.description)));

    let mut km_groups: BTreeMap<(i64, i64, i64), Acc> = BTreeMap::new();
    for k in km {
        let acc = km_groups
            .entry((k.tariff_id, k.price_cents, k.vat_bp))
            .or_insert_with(|| Acc {
                first_date: k.date.clone(),
                description: if k.tariff_name.to_lowercase().contains("kilomet") {
                    k.tariff_name.clone()
                } else {
                    format!("Kilometers ({})", k.tariff_name)
                },
                price: k.price_cents,
                vat: k.vat_bp,
                ..Default::default()
            });
        acc.km += k.km;
        acc.entries.push(k.entry_id);
    }

    let mut out = Vec::new();
    for acc in hour_lines {
        out.push(finish(acc, "uur", "hours")?);
    }
    for acc in km_groups.into_values() {
        out.push(finish(acc, "km", "km")?);
    }
    Ok(out)
}

/// Line units: `uur`, `km` or `stuk` (anything else counts as `stuk`).
pub fn unit(s: &str) -> &'static str {
    match s {
        "uur" => "uur",
        "km" => "km",
        _ => "stuk",
    }
}

/// The "regel toevoegen" form of draft invoices, quotes and recurring
/// templates.
#[derive(Debug, Default, Deserialize)]
pub struct ManualLineForm {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub quantity: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub vat_bp: String,
}

impl ManualLineForm {
    /// Validate and build the line; the error is a Dutch message for a flash.
    pub fn parse(&self, vat_choices: &[i64]) -> Result<DraftLine, String> {
        let quantity = format::parse_amount(&self.quantity)
            .map(|h| h * 10)
            .filter(|q| *q != 0);
        let price = format::parse_amount(&self.price);
        let vat = self
            .vat_bp
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|v| vat_choices.contains(v));
        let (Some(q), Some(p), Some(v)) = (quantity, price, vat) else {
            return Err("Vul omschrijving, aantal (bijv. 1 of 2,5), prijs en btw in.".into());
        };
        let description = self.description.trim();
        if description.is_empty() {
            return Err("Vul een omschrijving in.".into());
        }
        manual_line(description, q, unit(&self.unit), p, v)
            .map_err(|e| format!("Bedrag te groot: {e}"))
    }
}

/// A manual line: quantity (thousandths) × price.
pub fn manual_line(
    description: &str,
    quantity_milli: i64,
    unit: &'static str,
    unit_price_cents: i64,
    vat_bp: i64,
) -> Result<DraftLine, Overflow> {
    Ok(DraftLine {
        description: description.to_string(),
        quantity_milli,
        unit,
        unit_price_cents,
        vat_bp,
        line_total_cents: money::line_total_cents(quantity_milli, unit_price_cents)?,
        source: "manual",
        is_zero: unit_price_cents == 0,
        hour_entries: Vec::new(),
        km_entries: Vec::new(),
    })
}

/// Lines of a credit note: same lines, negative quantities.
pub fn negate(lines: &[DraftLine]) -> Result<Vec<DraftLine>, Overflow> {
    lines
        .iter()
        .map(|l| {
            let q = -l.quantity_milli;
            Ok(DraftLine {
                quantity_milli: q,
                line_total_cents: money::line_total_cents(q, l.unit_price_cents)?,
                hour_entries: Vec::new(),
                km_entries: Vec::new(),
                source: "manual",
                ..l.clone()
            })
        })
        .collect()
}

/// `VA-2026-0001`
pub fn number(prefix: &str, year: i32, n: i64) -> String {
    format!("{prefix}-{year:04}-{n:04}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Concept,
    Definitief,
    Verzonden,
    DeelsBetaald,
    Betaald,
    Vervallen,
    Gecrediteerd,
}

impl Status {
    /// CSS class of the status badge.
    pub fn css(self) -> &'static str {
        match self {
            Status::Concept => "draft",
            Status::Definitief => "",
            Status::Verzonden => "info",
            Status::DeelsBetaald => "warn",
            Status::Betaald => "ok",
            Status::Vervallen => "bad",
            Status::Gecrediteerd => "",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Concept => "concept",
            Status::Definitief => "definitief",
            Status::Verzonden => "verzonden",
            Status::DeelsBetaald => "deels betaald",
            Status::Betaald => "betaald",
            Status::Vervallen => "vervallen",
            Status::Gecrediteerd => "gecrediteerd",
        }
    }

    /// Still money to receive.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Status::Definitief | Status::Verzonden | Status::DeelsBetaald | Status::Vervallen
        )
    }
}

pub struct StatusInput<'a> {
    pub issued: bool,
    pub sent: bool,
    pub total_cents: i64,
    pub paid_cents: i64,
    pub due_date: Option<&'a str>,
    pub today: &'a str,
    pub credited: bool,
}

pub fn status(s: &StatusInput<'_>) -> Status {
    if !s.issued {
        return Status::Concept;
    }
    if s.credited {
        return Status::Gecrediteerd;
    }
    let fully_paid = if s.total_cents >= 0 {
        s.paid_cents >= s.total_cents
    } else {
        s.paid_cents <= s.total_cents
    };
    if fully_paid {
        return Status::Betaald;
    }
    let overdue = s.due_date.is_some_and(|d| d < s.today);
    if s.sent && overdue {
        return Status::Vervallen;
    }
    if s.paid_cents != 0 {
        return Status::DeelsBetaald;
    }
    if s.sent {
        Status::Verzonden
    } else {
        Status::Definitief
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_line_form() {
        let f = |d: &str, q: &str, u: &str, p: &str, v: &str| ManualLineForm {
            description: d.into(),
            quantity: q.into(),
            unit: u.into(),
            price: p.into(),
            vat_bp: v.into(),
        };
        let l = f(" Drukwerk ", "2,5", "uur", "95", "2100")
            .parse(&[2100, 900, 0])
            .unwrap();
        assert_eq!(
            (l.description.as_str(), l.quantity_milli, l.unit),
            ("Drukwerk", 2500, "uur")
        );
        assert_eq!(
            (l.unit_price_cents, l.vat_bp, l.line_total_cents),
            (9500, 2100, 23750)
        );
        assert_eq!(
            f("X", "1", "vreemd", "1", "0").parse(&[0]).unwrap().unit,
            "stuk"
        );
        let msg = "Vul omschrijving, aantal (bijv. 1 of 2,5), prijs en btw in.";
        assert_eq!(f("X", "0", "stuk", "1", "0").parse(&[0]).unwrap_err(), msg);
        assert_eq!(f("X", "1", "stuk", "", "0").parse(&[0]).unwrap_err(), msg);
        assert_eq!(
            f("X", "1", "stuk", "1", "1234").parse(&[0]).unwrap_err(),
            msg
        );
        assert_eq!(
            f(" ", "1", "stuk", "1", "0").parse(&[0]).unwrap_err(),
            "Vul een omschrijving in."
        );
    }

    fn h(id: i64, date: &str, task: i64, minutes: i64, price: i64) -> HourItem {
        HourItem {
            entry_id: id,
            date: date.into(),
            task_id: task,
            task_name: format!("Taak {task}"),
            project_id: (task == 2).then_some(9),
            project_name: (task == 2).then(|| "Transfer".into()),
            tariff_id: 100 + price,
            tariff_name: format!("Tarief {price}"),
            price_cents: price,
            vat_bp: 2100,
            minutes,
            description: format!("omschrijving {id}"),
        }
    }

    #[test]
    fn quantities() {
        assert_eq!(hours_quantity_milli(90), 1500);
        assert_eq!(hours_quantity_milli(20), 330);
        assert_eq!(hours_quantity_milli(40), 670);
        assert_eq!(hours_quantity_milli(45), 750);
        assert_eq!(hours_quantity_milli(-90), -1500);
        assert_eq!(quantity_label(1500), "1,5");
        assert_eq!(quantity_label(1250), "1,25");
        assert_eq!(quantity_label(3000), "3");
        assert_eq!(quantity_label(-1500), "-1,5");
        assert_eq!(quantity_label(64_000), "64");
    }

    #[test]
    fn rounding_per_entry_then_sum_per_task() {
        // 10 + 20 min, rounded up to 15 each → 15 + 30 = 45 min = 0,75 h
        let hours = vec![
            h(1, "2026-11-02", 1, 10, 9000),
            h(2, "2026-11-03", 1, 20, 9000),
        ];
        let lines = build_lines(&hours, &[], LineStyle::Task, 15).unwrap();
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!(l.quantity_milli, 750);
        assert_eq!(l.line_total_cents, 6750);
        assert_eq!(l.hour_entries, vec![1, 2]);
        assert_eq!(l.unit, "uur");
        // without rounding: 30 min
        let lines = build_lines(&hours, &[], LineStyle::Task, 0).unwrap();
        assert_eq!(lines[0].quantity_milli, 500);
    }

    #[test]
    fn quantity_times_price_equals_line_total() {
        // 20 min at € 90: 0,33 h × € 90 = € 29,70 (consistent on paper)
        let lines =
            build_lines(&[h(1, "2026-11-02", 1, 20, 9000)], &[], LineStyle::Task, 0).unwrap();
        assert_eq!(lines[0].quantity_milli, 330);
        assert_eq!(lines[0].line_total_cents, 2970);
    }

    #[test]
    fn line_styles() {
        let hours = vec![
            h(1, "2026-11-02", 1, 60, 9000),
            h(2, "2026-11-03", 2, 60, 9000),
            h(3, "2026-11-04", 1, 60, 11_000),
            h(4, "2026-11-05", 1, 30, 9000),
        ];
        let task = build_lines(&hours, &[], LineStyle::Task, 0).unwrap();
        assert_eq!(task.len(), 3, "task 1 @90, task 2 @90, task 1 @110");
        assert_eq!(task[0].description, "Taak 1");
        assert_eq!(task[0].quantity_milli, 1500);
        let tariff = build_lines(&hours, &[], LineStyle::Tariff, 0).unwrap();
        assert_eq!(tariff.len(), 2);
        let project = build_lines(&hours, &[], LineStyle::Project, 0).unwrap();
        assert!(project.iter().any(|l| l.description == "Transfer"));
        assert!(project.iter().any(|l| l.description == "Werkzaamheden"));
        let entry = build_lines(&hours, &[], LineStyle::Entry, 0).unwrap();
        assert_eq!(entry.len(), 4);
        assert_eq!(entry[0].description, "02-11-2026 Taak 1: omschrijving 1");
    }

    #[test]
    fn zero_lines_and_km() {
        let hours = vec![h(1, "2026-11-02", 3, 60, 0)];
        let km = vec![
            KmItem {
                entry_id: 7,
                date: "2026-11-02".into(),
                tariff_id: 5,
                tariff_name: "Kilometervergoeding".into(),
                price_cents: 23,
                vat_bp: 2100,
                km: 64,
            },
            KmItem {
                entry_id: 8,
                date: "2026-11-03".into(),
                tariff_id: 5,
                tariff_name: "Kilometervergoeding".into(),
                price_cents: 23,
                vat_bp: 2100,
                km: 64,
            },
        ];
        let lines = build_lines(&hours, &km, LineStyle::Task, 0).unwrap();
        assert!(lines[0].is_zero);
        assert_eq!(lines[0].line_total_cents, 0);
        let k = &lines[1];
        assert_eq!(
            (k.unit, k.quantity_milli, k.line_total_cents),
            ("km", 128_000, 2944)
        );
        assert_eq!(k.km_entries, vec![7, 8]);
        assert_eq!(k.description, "Kilometervergoeding");
    }

    #[test]
    fn vat_totals_over_lines() {
        let lines = [
            manual_line("A", 1000, "stuk", 50, 2100).unwrap(),
            manual_line("B", 1000, "stuk", 50, 2100).unwrap(),
            manual_line("C", 2000, "stuk", 1000, 900).unwrap(),
        ];
        let t = money::totals(
            &lines
                .iter()
                .map(|l| (l.line_total_cents, l.vat_bp))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(t.subtotal_cents, 2100);
        assert_eq!(t.vat_cents, 11 + 11 + 180);
        assert_eq!(t.total_cents, 2100 + 202);
    }

    #[test]
    fn credit_note_negates() {
        let lines = vec![manual_line("A", 1500, "uur", 9000, 2100).unwrap()];
        let c = negate(&lines).unwrap();
        assert_eq!(c[0].quantity_milli, -1500);
        assert_eq!(c[0].line_total_cents, -13_500);
        let t = money::totals(&[(c[0].line_total_cents, 2100)]).unwrap();
        assert_eq!(t.total_cents, -16_335);
    }

    #[test]
    fn numbers() {
        assert_eq!(number("VA", 2026, 1), "VA-2026-0001");
        assert_eq!(number("JB", 2027, 123), "JB-2027-0123");
        assert_eq!(LineStyle::parse("project"), Some(LineStyle::Project));
        assert_eq!(LineStyle::Entry.key(), "entry");
    }

    #[test]
    fn statuses() {
        let base = StatusInput {
            issued: true,
            sent: false,
            total_cents: 10_000,
            paid_cents: 0,
            due_date: Some("2026-11-20"),
            today: "2026-11-10",
            credited: false,
        };
        assert_eq!(
            status(&StatusInput {
                issued: false,
                ..base
            }),
            Status::Concept
        );
        assert_eq!(status(&base), Status::Definitief);
        assert_eq!(
            status(&StatusInput { sent: true, ..base }),
            Status::Verzonden
        );
        assert_eq!(
            status(&StatusInput {
                sent: true,
                paid_cents: 4000,
                ..base
            }),
            Status::DeelsBetaald
        );
        assert_eq!(
            status(&StatusInput {
                sent: true,
                paid_cents: 10_000,
                ..base
            }),
            Status::Betaald
        );
        assert_eq!(
            status(&StatusInput {
                sent: true,
                today: "2026-11-21",
                ..base
            }),
            Status::Vervallen
        );
        assert_eq!(
            status(&StatusInput {
                sent: true,
                today: "2026-11-21",
                paid_cents: 10_000,
                ..base
            }),
            Status::Betaald
        );
        assert_eq!(
            status(&StatusInput {
                credited: true,
                ..base
            }),
            Status::Gecrediteerd
        );
        // credit note: negative total, refunded
        assert_eq!(
            status(&StatusInput {
                total_cents: -500,
                paid_cents: -500,
                ..base
            }),
            Status::Betaald
        );
        assert!(Status::Vervallen.is_open() && !Status::Betaald.is_open());
    }
}
