//! Date logic for recurring invoices: periods, advancing with clamped month
//! ends, and the `{periode}` label.

use time::{Date, Month};

use crate::format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interval {
    Month,
    Quarter,
    Year,
}

impl Interval {
    pub const ALL: [Interval; 3] = [Interval::Month, Interval::Quarter, Interval::Year];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "month" => Some(Interval::Month),
            "quarter" => Some(Interval::Quarter),
            "year" => Some(Interval::Year),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Interval::Month => "month",
            Interval::Quarter => "quarter",
            Interval::Year => "year",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Interval::Month => "elke maand",
            Interval::Quarter => "elk kwartaal",
            Interval::Year => "elk jaar",
        }
    }
    fn months(self) -> i32 {
        match self {
            Interval::Month => 1,
            Interval::Quarter => 3,
            Interval::Year => 12,
        }
    }
}

fn is_month_end(d: Date) -> bool {
    d.day() == d.month().length(d.year())
}

/// Add `n` months. The day is clamped to the month's length, and a date on
/// the last day of a month stays on month ends: 31-01 → 28-02 → 31-03.
pub fn add_months(d: Date, n: i32) -> Date {
    let idx = d.year() * 12 + (u8::from(d.month()) as i32 - 1) + n;
    let (year, month) = (idx.div_euclid(12), idx.rem_euclid(12) as u8 + 1);
    let month = Month::try_from(month).expect("valid month");
    let len = month.length(year);
    let day = if is_month_end(d) {
        len
    } else {
        d.day().min(len)
    };
    Date::from_calendar_date(year, month, day).expect("valid date")
}

/// The start of the period after the one starting on `d`.
pub fn advance(d: Date, i: Interval) -> Date {
    add_months(d, i.months())
}

/// The period that starts on `d`: `(d, the day before the next period)`.
pub fn period(d: Date, i: Interval) -> (Date, Date) {
    (d, advance(d, i).previous_day().expect("valid date"))
}

/// "november 2026", "Q4 2026", "2026", or "01-11-2026 t/m 30-11-2026" when
/// the period does not start at the beginning of a month/quarter/year.
pub fn period_label(d: Date, i: Interval) -> String {
    let (from, to) = period(d, i);
    let m = u8::from(d.month());
    match i {
        Interval::Month if d.day() == 1 => {
            format!("{} {}", format::month_name(d.month()), d.year())
        }
        Interval::Quarter if d.day() == 1 && m % 3 == 1 => {
            format!("Q{} {}", m.div_ceil(3), d.year())
        }
        Interval::Year if d.day() == 1 && m == 1 => d.year().to_string(),
        _ => format!("{} t/m {}", format::date(from), format::date(to)),
    }
}

/// Every period start from `next` up to and including `today`, stopping after
/// `end` (inclusive) and after `cap` periods.
pub fn due_periods(
    next: Date,
    i: Interval,
    today: Date,
    end: Option<Date>,
    cap: usize,
) -> Vec<Date> {
    let mut out = Vec::new();
    let mut d = next;
    while d <= today && end.is_none_or(|e| d <= e) && out.len() < cap {
        out.push(d);
        d = advance(d, i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn month_ends_are_clamped_and_kept() {
        assert_eq!(add_months(date!(2026 - 01 - 31), 1), date!(2026 - 02 - 28));
        assert_eq!(add_months(date!(2026 - 02 - 28), 1), date!(2026 - 03 - 31));
        assert_eq!(add_months(date!(2028 - 01 - 31), 1), date!(2028 - 02 - 29));
        assert_eq!(add_months(date!(2026 - 01 - 15), 1), date!(2026 - 02 - 15));
        assert_eq!(add_months(date!(2026 - 11 - 30), 3), date!(2027 - 02 - 28));
        assert_eq!(add_months(date!(2026 - 12 - 01), 1), date!(2027 - 01 - 01));
        assert_eq!(add_months(date!(2026 - 03 - 10), 12), date!(2027 - 03 - 10));
    }

    #[test]
    fn periods_and_labels() {
        assert_eq!(
            period(date!(2026 - 11 - 01), Interval::Month),
            (date!(2026 - 11 - 01), date!(2026 - 11 - 30))
        );
        assert_eq!(
            period(date!(2026 - 10 - 01), Interval::Quarter).1,
            date!(2026 - 12 - 31)
        );
        assert_eq!(
            period_label(date!(2026 - 11 - 01), Interval::Month),
            "november 2026"
        );
        assert_eq!(
            period_label(date!(2026 - 10 - 01), Interval::Quarter),
            "Q4 2026"
        );
        assert_eq!(
            period_label(date!(2026 - 01 - 01), Interval::Quarter),
            "Q1 2026"
        );
        assert_eq!(period_label(date!(2026 - 01 - 01), Interval::Year), "2026");
        assert_eq!(
            period_label(date!(2026 - 11 - 15), Interval::Month),
            "15-11-2026 t/m 14-12-2026"
        );
        assert_eq!(
            period_label(date!(2026 - 11 - 01), Interval::Quarter),
            "01-11-2026 t/m 31-01-2027"
        );
    }

    #[test]
    fn catch_up_and_end_date() {
        let d = due_periods(
            date!(2026 - 08 - 01),
            Interval::Month,
            date!(2026 - 11 - 25),
            None,
            100,
        );
        assert_eq!(
            d,
            vec![
                date!(2026 - 08 - 01),
                date!(2026 - 09 - 01),
                date!(2026 - 10 - 01),
                date!(2026 - 11 - 01)
            ]
        );
        let d = due_periods(
            date!(2026 - 08 - 01),
            Interval::Month,
            date!(2026 - 11 - 25),
            Some(date!(2026 - 09 - 30)),
            100,
        );
        assert_eq!(d.len(), 2);
        assert!(due_periods(
            date!(2026 - 12 - 01),
            Interval::Month,
            date!(2026 - 11 - 25),
            None,
            100
        )
        .is_empty());
        assert_eq!(
            due_periods(
                date!(2020 - 01 - 01),
                Interval::Month,
                date!(2026 - 11 - 25),
                None,
                24
            )
            .len(),
            24
        );
    }
}
