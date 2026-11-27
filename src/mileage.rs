//! Kilometerregistratie logic (pure, unit-tested): odometer continuity per
//! vehicle across all companies, and the year balance.
//!
//! A trip covers the odometer range `[start, end)`. Ranges of one vehicle may
//! not overlap, and a later date may not have a lower odometer reading than an
//! earlier date. Gaps between trips are km that were not registered; for a
//! private car they are private km.
//!
//! A trip of a private car in "Aantal km" mode may have no readings at all:
//! it counts for the km totals but never in the continuity checks.

/// One trip as seen by the checks (any company).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    pub id: i64,
    pub company_id: i64,
    /// ISO date `YYYY-MM-DD`.
    pub date: String,
    /// Odometer readings; `None` for a km-only trip.
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub km: i64,
    pub business: bool,
}

impl Leg {
    /// `(start, end)` when the trip has readings.
    pub fn range(&self) -> Option<(i64, i64)> {
        self.start.zip(self.end)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Odometer range overlaps another trip.
    Overlap { date: String, other_company: bool },
    /// Odometer order does not match the date order.
    Order { date: String, other_company: bool },
}

impl Conflict {
    /// Dutch message; never names the other company.
    pub fn message(&self) -> String {
        let (what, date, other) = match self {
            Conflict::Overlap {
                date,
                other_company,
            } => (
                "De kilometerstand overlapt met een rit van",
                date,
                *other_company,
            ),
            Conflict::Order {
                date,
                other_company,
            } => (
                "De kilometerstand past niet bij de datum: vergelijk met de rit van",
                date,
                *other_company,
            ),
        };
        let d = crate::format::parse_iso_date(date)
            .map(crate::format::date)
            .unwrap_or_else(|| date.clone());
        format!(
            "{what} {d}{}. Controleer begin- en eindstand.",
            if other { " (ander bedrijf)" } else { "" }
        )
    }
}

/// Check a new or edited trip (`exclude` = its own id) against the other
/// trips of the same vehicle.
pub fn check(
    company_id: i64,
    date: &str,
    start: i64,
    end: i64,
    exclude: Option<i64>,
    others: &[Leg],
) -> Option<Conflict> {
    for o in others.iter().filter(|o| Some(o.id) != exclude) {
        let Some((o_start, o_end)) = o.range() else {
            continue;
        };
        let other_company = o.company_id != company_id;
        if start < o_end && o_start < end {
            return Some(Conflict::Overlap {
                date: o.date.clone(),
                other_company,
            });
        }
        let earlier_but_higher = o.date.as_str() < date && o_start >= end;
        let later_but_lower = o.date.as_str() > date && o_end <= start;
        if earlier_but_higher || later_but_lower {
            return Some(Conflict::Order {
                date: o.date.clone(),
                other_company,
            });
        }
    }
    None
}

/// The vehicle's last end reading (any company), to prefill the next trip.
pub fn last_reading(legs: &[Leg]) -> Option<i64> {
    legs.iter().filter_map(|l| l.end).max()
}

/// Unregistered km just before `start` (gap after the previous trip).
pub fn gap_before(start: i64, legs: &[Leg]) -> i64 {
    legs.iter()
        .filter_map(|l| l.end)
        .filter(|e| *e <= start)
        .max()
        .map(|prev| start - prev)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Balance {
    pub business_this: i64,
    pub business_by_month: [i64; 12],
    pub private_this: i64,
    pub other_companies: i64,
    pub unregistered: i64,
    pub first_reading: Option<i64>,
    pub last_reading: Option<i64>,
    /// Km of trips without readings (already in the totals above).
    pub km_only: i64,
}

impl Balance {
    /// Odometer km in the year: first start reading to last end reading.
    pub fn odometer_km(&self) -> i64 {
        match (self.first_reading, self.last_reading) {
            (Some(a), Some(b)) => b - a,
            _ => 0,
        }
    }

    pub fn accounted(&self) -> i64 {
        self.business_this + self.private_this + self.other_companies + self.unregistered
    }

    /// The gate: every odometer km is accounted for exactly once. Km-only
    /// trips have no readings and are left out of the comparison.
    pub fn matches(&self) -> bool {
        self.accounted() - self.km_only == self.odometer_km()
    }

    /// The vehicle's km in the year came only from km-only trips.
    pub fn has_readings(&self) -> bool {
        self.first_reading.is_some()
    }

    /// Private km for a private car: logged private trips + unregistered km.
    pub fn private_total(&self) -> i64 {
        self.private_this + self.unregistered
    }

    /// Tax-free allowance for this company's business km, if the rate is known.
    pub fn allowance_cents(&self, rate_cents_per_km: Option<i64>) -> Option<i64> {
        rate_cents_per_km.map(|r| self.business_this * r)
    }
}

/// Year balance of one vehicle from the point of view of `company_id`.
/// `legs` are all non-archived trips of the vehicle dated in that year.
pub fn year_balance(company_id: i64, legs: &[Leg]) -> Balance {
    let mut sorted: Vec<&Leg> = legs.iter().collect();
    sorted.sort_by_key(|l| (l.start.is_none(), l.start, l.id));
    let mut b = Balance {
        first_reading: sorted.iter().find_map(|l| l.start),
        last_reading: sorted.iter().filter_map(|l| l.end).max(),
        ..Default::default()
    };
    let mut prev_end: Option<i64> = None;
    for l in sorted {
        let km = l.km;
        match l.range() {
            Some((start, end)) => {
                if let Some(p) = prev_end {
                    if start > p {
                        b.unregistered += start - p;
                    }
                }
                prev_end = Some(prev_end.map_or(end, |p| p.max(end)));
            }
            None => b.km_only += km,
        }
        if l.company_id != company_id {
            b.other_companies += km;
        } else if l.business {
            b.business_this += km;
            let month: usize = l.date.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(1);
            b.business_by_month[month.clamp(1, 12) - 1] += km;
        } else {
            b.private_this += km;
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(id: i64, company: i64, date: &str, start: i64, end: i64, business: bool) -> Leg {
        Leg {
            id,
            company_id: company,
            date: date.into(),
            start: Some(start),
            end: Some(end),
            km: end - start,
            business,
        }
    }

    fn km_leg(id: i64, company: i64, date: &str, km: i64, business: bool) -> Leg {
        Leg {
            id,
            company_id: company,
            date: date.into(),
            start: None,
            end: None,
            km,
            business,
        }
    }

    #[test]
    fn km_only_trips_count_but_are_never_in_the_way() {
        let legs = vec![
            leg(1, 1, "2026-03-01", 1000, 1050, true),
            km_leg(2, 1, "2026-03-02", 30, true),
            leg(3, 1, "2026-03-03", 1100, 1120, false),
            km_leg(4, 2, "2026-03-04", 7, true),
        ];
        // a km-only trip never conflicts, whatever the readings around it
        assert_eq!(check(1, "2026-03-02", 1050, 1060, None, &legs), None);
        assert_eq!(last_reading(&legs), Some(1120));
        assert_eq!(gap_before(1100, &legs), 50);
        let b = year_balance(1, &legs);
        assert_eq!(b.business_this, 80);
        assert_eq!(b.business_by_month[2], 80);
        assert_eq!(b.private_this, 20);
        assert_eq!(b.other_companies, 7);
        assert_eq!(b.km_only, 37);
        assert_eq!(b.unregistered, 50);
        assert_eq!(b.odometer_km(), 120);
        assert!(b.matches(), "{b:?}");
        let only_km = year_balance(1, &[km_leg(1, 1, "2026-05-01", 12, true)]);
        assert!(!only_km.has_readings());
        assert!(only_km.matches());
        assert_eq!(only_km.business_this, 12);
    }

    #[test]
    fn overlap_is_detected_also_across_companies() {
        let others = vec![leg(1, 2, "2026-11-12", 1000, 1064, true)];
        assert_eq!(
            check(1, "2026-11-13", 1050, 1100, None, &others),
            Some(Conflict::Overlap {
                date: "2026-11-12".into(),
                other_company: true
            })
        );
        // touching ranges are fine
        assert_eq!(check(1, "2026-11-13", 1064, 1100, None, &others), None);
        assert_eq!(check(1, "2026-11-11", 900, 1000, None, &others), None);
        // editing the trip itself does not conflict with itself
        assert_eq!(check(2, "2026-11-12", 1000, 1070, Some(1), &others), None);
        let msg = check(1, "2026-11-13", 1050, 1100, None, &others)
            .unwrap()
            .message();
        assert!(msg.contains("12-11-2026 (ander bedrijf)"), "{msg}");
    }

    #[test]
    fn date_order_must_match_odometer_order() {
        let others = vec![leg(1, 1, "2026-11-12", 1000, 1064, true)];
        // dated later but lower odometer
        assert!(matches!(
            check(1, "2026-11-20", 500, 600, None, &others),
            Some(Conflict::Order {
                other_company: false,
                ..
            })
        ));
        // dated earlier but higher odometer
        assert!(matches!(
            check(1, "2026-11-01", 2000, 2100, None, &others),
            Some(Conflict::Order { .. })
        ));
        // same day, either order
        assert_eq!(check(1, "2026-11-12", 900, 1000, None, &others), None);
        assert_eq!(check(1, "2026-11-12", 1064, 1100, None, &others), None);
    }

    #[test]
    fn readings_and_gaps() {
        let legs = vec![
            leg(1, 1, "2026-11-02", 1000, 1064, true),
            leg(2, 1, "2026-11-02", 1064, 1128, true),
            leg(3, 1, "2026-11-05", 1300, 1350, true),
        ];
        assert_eq!(last_reading(&legs), Some(1350));
        assert_eq!(last_reading(&[]), None);
        assert_eq!(gap_before(1300, &legs), 172);
        assert_eq!(gap_before(1064, &legs), 0);
        assert_eq!(gap_before(500, &legs), 0);
    }

    #[test]
    fn year_balance_adds_up_to_the_odometer() {
        let legs = vec![
            leg(1, 1, "2026-01-10", 10_000, 10_100, true), // business, this company
            leg(2, 2, "2026-02-01", 10_100, 10_150, true), // other company
            leg(3, 1, "2026-02-03", 10_400, 10_420, false), // private logged; gap 250 before
            leg(4, 1, "2026-11-20", 10_420, 10_484, true),
        ];
        let b = year_balance(1, &legs);
        assert_eq!(b.business_this, 164);
        assert_eq!(b.business_by_month[0], 100);
        assert_eq!(b.business_by_month[10], 64);
        assert_eq!(b.other_companies, 50);
        assert_eq!(b.private_this, 20);
        assert_eq!(b.unregistered, 250);
        assert_eq!(b.odometer_km(), 484);
        assert!(b.matches());
        assert_eq!(b.private_total(), 270);
        assert_eq!(b.allowance_cents(Some(23)), Some(164 * 23));
        assert_eq!(b.allowance_cents(None), None);
        // from the other company's point of view
        let b2 = year_balance(2, &legs);
        assert_eq!(b2.business_this, 50);
        assert_eq!(b2.other_companies, 184);
        assert!(b2.matches());
    }

    #[test]
    fn empty_year() {
        let b = year_balance(1, &[]);
        assert_eq!(b.odometer_km(), 0);
        assert!(b.matches());
    }
}
