//! Money arithmetic. Amounts are `i64` cents, VAT rates `i64` basis points
//! (2100 = 21%), quantities `i64` thousandths. Never floats.
//!
//! Rounding is half-up (half away from zero, so a credit note mirrors its
//! invoice exactly), applied per line; totals are sums of rounded lines.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("bedrag te groot")]
pub struct Overflow;

/// `num / den`, rounded half away from zero. `den` must be positive.
fn div_round(num: i128, den: i128) -> i128 {
    debug_assert!(den > 0);
    let q = num / den;
    let r = num % den;
    if r.abs() * 2 >= den {
        q + num.signum()
    } else {
        q
    }
}

fn to_i64(v: i128) -> Result<i64, Overflow> {
    i64::try_from(v).map_err(|_| Overflow)
}

/// VAT on a (line) amount: `amount × bp / 10 000`, rounded half-up.
pub fn vat_cents(amount_cents: i64, vat_bp: i64) -> Result<i64, Overflow> {
    to_i64(div_round(
        i128::from(amount_cents) * i128::from(vat_bp),
        10_000,
    ))
}

/// Line total ex VAT: `quantity (thousandths) × unit price`, rounded half-up.
pub fn line_total_cents(quantity_milli: i64, unit_price_cents: i64) -> Result<i64, Overflow> {
    to_i64(div_round(
        i128::from(quantity_milli) * i128::from(unit_price_cents),
        1_000,
    ))
}

/// Amount for time worked: `minutes × hourly price / 60`, rounded half-up.
/// Computed from minutes directly so 20 minutes at € 90 is € 30,00, not € 29,97.
pub fn time_total_cents(minutes: i64, hourly_price_cents: i64) -> Result<i64, Overflow> {
    to_i64(div_round(
        i128::from(minutes) * i128::from(hourly_price_cents),
        60,
    ))
}

/// Hours as a quantity in thousandths (for display on an invoice line), half-up.
pub fn minutes_to_quantity_milli(minutes: i64) -> i64 {
    div_round(i128::from(minutes) * 1000, 60) as i64
}

/// Round minutes up to the company's rounding step (0 = no rounding).
pub fn round_up_minutes(minutes: i64, step: i64) -> i64 {
    if step <= 0 || minutes <= 0 {
        return minutes;
    }
    (minutes + step - 1) / step * step
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VatGroup {
    pub base_cents: i64,
    pub vat_cents: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub subtotal_cents: i64,
    pub vat_cents: i64,
    pub total_cents: i64,
    /// Per VAT rate (bp): base and VAT, for the "BTW per tarief" block on an invoice.
    pub per_rate: BTreeMap<i64, VatGroup>,
}

/// Totals for a set of lines `(line_total_cents, vat_bp)`: VAT is rounded per
/// line and then summed.
pub fn totals(lines: &[(i64, i64)]) -> Result<Totals, Overflow> {
    let mut t = Totals::default();
    for &(amount, bp) in lines {
        let vat = vat_cents(amount, bp)?;
        let g = t.per_rate.entry(bp).or_default();
        g.base_cents = g.base_cents.checked_add(amount).ok_or(Overflow)?;
        g.vat_cents = g.vat_cents.checked_add(vat).ok_or(Overflow)?;
        t.subtotal_cents = t.subtotal_cents.checked_add(amount).ok_or(Overflow)?;
        t.vat_cents = t.vat_cents.checked_add(vat).ok_or(Overflow)?;
    }
    t.total_cents = t.subtotal_cents.checked_add(t.vat_cents).ok_or(Overflow)?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vat_rounds_half_up() {
        assert_eq!(vat_cents(10_000, 2100), Ok(2100));
        // 0,50 × 21% = 0,105 → 0,11
        assert_eq!(vat_cents(50, 2100), Ok(11));
        // 0,02 × 21% = 0,0042 → 0,00
        assert_eq!(vat_cents(2, 2100), Ok(0));
        // 1,19 × 21% = 0,2499 → 0,25 ; 1,10 × 9% = 0,099 → 0,10
        assert_eq!(vat_cents(119, 2100), Ok(25));
        assert_eq!(vat_cents(110, 900), Ok(10));
        // exactly half: 2,50 × 1% = 0,025 → 0,03
        assert_eq!(vat_cents(250, 100), Ok(3));
        assert_eq!(vat_cents(12_345, 0), Ok(0));
    }

    #[test]
    fn negative_amounts_mirror_positive() {
        assert_eq!(vat_cents(-50, 2100), Ok(-11));
        assert_eq!(vat_cents(-250, 100), Ok(-3));
        assert_eq!(line_total_cents(-1500, 333), Ok(-500));
    }

    #[test]
    fn line_total() {
        // 1,5 × € 3,33 = 4,995 → 5,00
        assert_eq!(line_total_cents(1500, 333), Ok(500));
        assert_eq!(line_total_cents(1000, 0), Ok(0));
        assert_eq!(line_total_cents(2000, 9500), Ok(19_000));
        // 0,333 × 9000 = 2997
        assert_eq!(line_total_cents(333, 9000), Ok(2997));
    }

    #[test]
    fn time_total_from_minutes() {
        assert_eq!(time_total_cents(20, 9000), Ok(3000));
        assert_eq!(time_total_cents(60, 8550), Ok(8550));
        // 7 min × 100/60 = 11,67 → 12
        assert_eq!(time_total_cents(7, 100), Ok(12));
        assert_eq!(time_total_cents(90, 0), Ok(0));
    }

    #[test]
    fn quantity_from_minutes() {
        assert_eq!(minutes_to_quantity_milli(60), 1000);
        assert_eq!(minutes_to_quantity_milli(20), 333);
        assert_eq!(minutes_to_quantity_milli(40), 667);
        assert_eq!(minutes_to_quantity_milli(90), 1500);
    }

    #[test]
    fn rounding_minutes_up() {
        assert_eq!(round_up_minutes(1, 15), 15);
        assert_eq!(round_up_minutes(15, 15), 15);
        assert_eq!(round_up_minutes(16, 15), 30);
        assert_eq!(round_up_minutes(16, 0), 16);
        assert_eq!(round_up_minutes(0, 15), 0);
        assert_eq!(round_up_minutes(61, 6), 66);
    }

    #[test]
    fn totals_round_per_line_then_sum() {
        // Two lines of € 0,50 at 21%: per line 0,11 + 0,11 = 0,22
        // (rounding the sum would give 0,21).
        let t = totals(&[(50, 2100), (50, 2100)]).unwrap();
        assert_eq!(t.subtotal_cents, 100);
        assert_eq!(t.vat_cents, 22);
        assert_eq!(t.total_cents, 122);

        let t = totals(&[(10_000, 2100), (5_000, 900), (0, 2100)]).unwrap();
        assert_eq!(t.subtotal_cents, 15_000);
        assert_eq!(t.vat_cents, 2100 + 450);
        assert_eq!(
            t.per_rate[&2100],
            VatGroup {
                base_cents: 10_000,
                vat_cents: 2100
            }
        );
        assert_eq!(t.per_rate[&900].vat_cents, 450);
    }

    #[test]
    fn overflow_is_reported() {
        assert_eq!(line_total_cents(i64::MAX, i64::MAX), Err(Overflow));
        assert_eq!(totals(&[(i64::MAX, 0), (1, 0)]), Err(Overflow));
    }
}
