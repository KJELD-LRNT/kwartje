//! Dutch formatting and parsing: dates `dd-mm-jjjj`, amounts `€ 1.234,56`,
//! VAT percentages, durations, ISO weeks (Monday first), Amsterdam local time.
//!
//! Always use these helpers for user-facing values; never format ad hoc.

use time::{Date, Duration, Month, OffsetDateTime, UtcOffset, Weekday};

// ---------- dates ----------

/// `25-11-2026`
pub fn date(d: Date) -> String {
    format!("{:02}-{:02}-{:04}", d.day(), u8::from(d.month()), d.year())
}

/// `2026-11-25` (ISO, for CSV and `<input type="date">`).
pub fn iso_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

/// Accepts `25-11-2026`, `5-1-2026`, `25/11/2026` and ISO `2026-11-25`.
pub fn parse_date(s: &str) -> Option<Date> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let (y, m, d) = if parts[0].len() == 4 {
        (parts[0], parts[1], parts[2])
    } else if parts[2].len() == 4 {
        (parts[2], parts[1], parts[0])
    } else {
        return None;
    };
    let month = Month::try_from(m.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(y.parse().ok()?, month, d.parse().ok()?).ok()
}

/// Parse a stored ISO date (`YYYY-MM-DD`).
pub fn parse_iso_date(s: &str) -> Option<Date> {
    let s = s.trim();
    if s.len() != 10 || s.as_bytes()[4] != b'-' {
        return None;
    }
    parse_date(s)
}

/// Monday of the week containing `d`.
pub fn week_start(d: Date) -> Date {
    d - Duration::days(i64::from(d.weekday().number_days_from_monday()))
}

/// ISO week: (ISO year, week number).
pub fn iso_week(d: Date) -> (i32, u8) {
    let (y, w, _) = d.to_iso_week_date();
    (y, w)
}

pub fn weekday_name(d: Weekday) -> &'static str {
    match d {
        Weekday::Monday => "maandag",
        Weekday::Tuesday => "dinsdag",
        Weekday::Wednesday => "woensdag",
        Weekday::Thursday => "donderdag",
        Weekday::Friday => "vrijdag",
        Weekday::Saturday => "zaterdag",
        Weekday::Sunday => "zondag",
    }
}

pub fn month_name(m: Month) -> &'static str {
    match m {
        Month::January => "januari",
        Month::February => "februari",
        Month::March => "maart",
        Month::April => "april",
        Month::May => "mei",
        Month::June => "juni",
        Month::July => "juli",
        Month::August => "augustus",
        Month::September => "september",
        Month::October => "oktober",
        Month::November => "november",
        Month::December => "december",
    }
}

// ---------- Amsterdam local time ----------

/// Last Sunday of a month, at 01:00 UTC (EU DST switch moment).
fn last_sunday_0100_utc(year: i32, month: Month) -> OffsetDateTime {
    let next_first = if month == Month::December {
        Date::from_calendar_date(year + 1, Month::January, 1)
    } else {
        Date::from_calendar_date(year, month.next(), 1)
    }
    .expect("valid date");
    let last = next_first - Duration::days(1);
    let back = i64::from(last.weekday().number_days_from_sunday());
    (last - Duration::days(back))
        .with_hms(1, 0, 0)
        .expect("valid time")
        .assume_utc()
}

/// UTC offset for Europe/Amsterdam at `t` (CET +1, CEST +2, EU rules).
pub fn amsterdam_offset(t: OffsetDateTime) -> UtcOffset {
    let t = t.to_offset(UtcOffset::UTC);
    let start = last_sunday_0100_utc(t.year(), Month::March);
    let end = last_sunday_0100_utc(t.year(), Month::October);
    let hours = if t >= start && t < end { 2 } else { 1 };
    UtcOffset::from_hms(hours, 0, 0).expect("valid offset")
}

pub fn to_amsterdam(t: OffsetDateTime) -> OffsetDateTime {
    t.to_offset(amsterdam_offset(t))
}

/// Today's date in the Netherlands.
pub fn today() -> Date {
    to_amsterdam(OffsetDateTime::now_utc()).date()
}

/// `25-11-2026 11:00` in Dutch local time, from a stored RFC 3339 timestamp.
/// Falls back to the raw string when it cannot be parsed.
pub fn datetime(ts: &str) -> String {
    match OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339) {
        Ok(t) => {
            let l = to_amsterdam(t);
            format!("{} {:02}:{:02}", date(l.date()), l.hour(), l.minute())
        }
        Err(_) => ts.to_string(),
    }
}

// ---------- amounts ----------

fn group_thousands(mut n: u64) -> String {
    let mut groups = Vec::new();
    loop {
        if n < 1000 {
            groups.push(n.to_string());
            break;
        }
        groups.push(format!("{:03}", n % 1000));
        n /= 1000;
    }
    groups.reverse();
    groups.join(".")
}

/// `€ 1.234,56`; negative `€ -1.234,56`.
pub fn amount(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("€ {sign}{},{:02}", group_thousands(abs / 100), abs % 100)
}

/// `1234,56` without currency or thousands separator (CSV, form fields).
pub fn decimal(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{},{:02}", abs / 100, abs % 100)
}

/// Parse an amount typed by a user into cents.
///
/// Accepts `1.234,56`, `1234,56`, `1234`, `€ 12,5`, `-3,00`, and `12.50`
/// (a single dot followed by one or two digits is taken as a decimal point).
pub fn parse_amount(input: &str) -> Option<i64> {
    let mut s: String = input
        .trim()
        .trim_start_matches('€')
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let negative = s.starts_with('-');
    if negative {
        s.remove(0);
    }
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = if let Some((i, f)) = s.split_once(',') {
        (i.to_string(), f.to_string())
    } else if s.matches('.').count() == 1 {
        let (i, f) = s.split_once('.').expect("one dot");
        if f.len() <= 2 {
            (i.to_string(), f.to_string())
        } else {
            (s.clone(), String::new())
        }
    } else {
        (s.clone(), String::new())
    };
    // Thousands separators: groups of exactly 3 digits after the first.
    let int_digits = if int_part.contains('.') {
        let groups: Vec<&str> = int_part.split('.').collect();
        if groups[0].is_empty() || groups[0].len() > 3 || groups[1..].iter().any(|g| g.len() != 3) {
            return None;
        }
        groups.concat()
    } else {
        int_part
    };
    if int_digits.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_digits.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || frac_part.len() > 2
    {
        return None;
    }
    let euros: i64 = if int_digits.is_empty() {
        0
    } else {
        int_digits.parse().ok()?
    };
    let cents: i64 = match frac_part.len() {
        0 => 0,
        1 => frac_part.parse::<i64>().ok()? * 10,
        _ => frac_part.parse().ok()?,
    };
    let total = euros.checked_mul(100)?.checked_add(cents)?;
    Some(if negative { -total } else { total })
}

// ---------- VAT ----------

/// `21%`, `9%`, `5,5%`, `12,34%`.
pub fn vat_pct(bp: i64) -> String {
    let whole = bp / 100;
    let frac = (bp % 100).abs();
    if frac == 0 {
        format!("{whole}%")
    } else if frac % 10 == 0 {
        format!("{whole},{}%", frac / 10)
    } else {
        format!("{whole},{frac:02}%")
    }
}

/// `21` / `21%` / `5,5` → basis points.
pub fn parse_vat_pct(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('%').trim();
    let bp = parse_amount(s)?; // same shape: up to 2 decimals
    (0..=10_000).contains(&bp).then_some(bp)
}

// ---------- durations ----------

/// `1:30` for 90 minutes.
pub fn duration(minutes: i64) -> String {
    let sign = if minutes < 0 { "-" } else { "" };
    let m = minutes.unsigned_abs();
    format!("{sign}{}:{:02}", group_thousands(m / 60), m % 60)
}

/// `1:30`, `1,5` / `1.5` / `2` / `2u` (hours), `90m` (minutes) → minutes.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    if let Some((h, m)) = s.split_once(':') {
        let h: i64 = h.trim().parse().ok()?;
        let m: i64 = m.trim().parse().ok()?;
        if !(0..60).contains(&m) || h < 0 {
            return None;
        }
        return Some(h * 60 + m);
    }
    if let Some(m) = s.strip_suffix('m') {
        return m.trim().parse().ok().filter(|v: &i64| *v >= 0);
    }
    let s = s.strip_suffix('u').map(str::trim).unwrap_or(&s);
    if s.contains(',') || s.contains('.') {
        let hundredths = parse_amount(&s.replace('.', ","))?;
        if s.contains('.') && s.matches('.').count() > 1 {
            return None;
        }
        if hundredths < 0 {
            return None;
        }
        // hours with up to 2 decimals → minutes, half-up
        return Some((hundredths * 60 + 50) / 100);
    }
    s.parse::<i64>()
        .ok()
        .filter(|v| *v >= 0)
        .and_then(|h| h.checked_mul(60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    #[test]
    fn formats_dates() {
        assert_eq!(date(date!(2026 - 11 - 25)), "25-11-2026");
        assert_eq!(date(date!(2026 - 01 - 05)), "05-01-2026");
        assert_eq!(iso_date(date!(2026 - 01 - 05)), "2026-01-05");
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("25-11-2026"), Some(date!(2026 - 11 - 25)));
        assert_eq!(parse_date("5-1-2026"), Some(date!(2026 - 01 - 05)));
        assert_eq!(parse_date("25/11/2026"), Some(date!(2026 - 11 - 25)));
        assert_eq!(parse_date("2026-11-25"), Some(date!(2026 - 11 - 25)));
        assert_eq!(parse_date("31-02-2026"), None);
        assert_eq!(parse_date("25-11-26"), None);
        assert_eq!(parse_date("gisteren"), None);
        assert_eq!(parse_iso_date("25-11-2026"), None);
    }

    #[test]
    fn weeks_start_on_monday() {
        // 25-11-2026 is a Wednesday
        assert_eq!(week_start(date!(2026 - 11 - 25)), date!(2026 - 11 - 23));
        assert_eq!(week_start(date!(2026 - 11 - 23)), date!(2026 - 11 - 23));
        assert_eq!(week_start(date!(2026 - 11 - 29)), date!(2026 - 11 - 23));
        assert_eq!(iso_week(date!(2026 - 11 - 25)), (2026, 48));
        // 1 Jan 2027 is a Friday → ISO week 53 of 2026
        assert_eq!(iso_week(date!(2027 - 01 - 01)), (2026, 53));
        assert_eq!(weekday_name(date!(2026 - 11 - 25).weekday()), "woensdag");
    }

    #[test]
    fn amsterdam_time() {
        // Winter: +1
        assert_eq!(datetime("2026-11-25T10:00:00Z"), "25-11-2026 11:00");
        // Summer: +2
        assert_eq!(datetime("2026-07-01T10:00:00Z"), "01-07-2026 12:00");
        // DST starts 29-03-2026 01:00 UTC
        assert_eq!(
            amsterdam_offset(datetime!(2026-03-29 00:59 UTC)).whole_hours(),
            1
        );
        assert_eq!(
            amsterdam_offset(datetime!(2026-03-29 01:00 UTC)).whole_hours(),
            2
        );
        // DST ends 25-10-2026 01:00 UTC
        assert_eq!(
            amsterdam_offset(datetime!(2026-10-25 00:59 UTC)).whole_hours(),
            2
        );
        assert_eq!(
            amsterdam_offset(datetime!(2026-10-25 01:00 UTC)).whole_hours(),
            1
        );
        // Late evening UTC on 31-12 is already the next day in NL
        assert_eq!(
            to_amsterdam(datetime!(2026-12-31 23:30 UTC)).date(),
            date!(2027 - 01 - 01)
        );
        assert_eq!(datetime("rommel"), "rommel");
    }

    #[test]
    fn formats_amounts() {
        assert_eq!(amount(0), "€ 0,00");
        assert_eq!(amount(5), "€ 0,05");
        assert_eq!(amount(123_456), "€ 1.234,56");
        assert_eq!(amount(100_000_000), "€ 1.000.000,00");
        assert_eq!(amount(-1250), "€ -12,50");
        assert_eq!(amount(-123_456), "€ -1.234,56");
        assert_eq!(decimal(123_456), "1234,56");
        assert_eq!(decimal(-5), "-0,05");
    }

    #[test]
    fn parses_amounts() {
        assert_eq!(parse_amount("1.234,56"), Some(123_456));
        assert_eq!(parse_amount("1234,56"), Some(123_456));
        assert_eq!(parse_amount("€ 1.234,56"), Some(123_456));
        assert_eq!(parse_amount("12,5"), Some(1250));
        assert_eq!(parse_amount("12"), Some(1200));
        assert_eq!(parse_amount("12.50"), Some(1250));
        assert_eq!(parse_amount("1.234"), Some(123_400));
        assert_eq!(parse_amount("1.234.567,89"), Some(123_456_789));
        assert_eq!(parse_amount("-3,00"), Some(-300));
        assert_eq!(parse_amount("0"), Some(0));
        assert_eq!(parse_amount(",5"), Some(50));
        assert_eq!(parse_amount(""), None);
        assert_eq!(parse_amount("12,345"), None);
        assert_eq!(parse_amount("1.23.4"), None);
        assert_eq!(parse_amount("abc"), None);
        assert_eq!(parse_amount("1,2,3"), None);
    }

    #[test]
    fn vat_percentages() {
        assert_eq!(vat_pct(2100), "21%");
        assert_eq!(vat_pct(900), "9%");
        assert_eq!(vat_pct(0), "0%");
        assert_eq!(vat_pct(550), "5,5%");
        assert_eq!(vat_pct(1234), "12,34%");
        assert_eq!(parse_vat_pct("21"), Some(2100));
        assert_eq!(parse_vat_pct("21%"), Some(2100));
        assert_eq!(parse_vat_pct("5,5"), Some(550));
        assert_eq!(parse_vat_pct("101"), None);
        assert_eq!(parse_vat_pct("-1"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(90), "1:30");
        assert_eq!(duration(5), "0:05");
        assert_eq!(duration(600), "10:00");
        assert_eq!(duration(1225 * 60), "1.225:00");
        assert_eq!(parse_duration("1:30"), Some(90));
        assert_eq!(parse_duration("1,5"), Some(90));
        assert_eq!(parse_duration("1.25"), Some(75));
        assert_eq!(parse_duration("0,33"), Some(20));
        assert_eq!(parse_duration("45m"), Some(45));
        assert_eq!(parse_duration("2"), Some(120));
        assert_eq!(parse_duration("2u"), Some(120));
        assert_eq!(parse_duration("1,5 u"), Some(90));
        assert_eq!(parse_duration("1:75"), None);
        assert_eq!(parse_duration("-1"), None);
        assert_eq!(parse_duration("lang"), None);
    }
}
