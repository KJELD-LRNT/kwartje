//! Quarterly BTW summary (factuurstelsel), a helper for the aangifte — not a
//! filing. Sales VAT per rate from issued invoices and credit notes (by
//! invoice date), input VAT from purchase documents (by document date).

use std::collections::BTreeMap;

use crate::money::Totals;

/// Aangifte box for a VAT rate.
pub fn box_for(bp: i64) -> &'static str {
    match bp {
        2100 => "1a",
        900 => "1b",
        0 => "1e",
        _ => "?",
    }
}

pub fn box_label(b: &str) -> &'static str {
    match b {
        "1a" => "Leveringen/diensten belast met hoog tarief (21%)",
        "1b" => "Leveringen/diensten belast met laag tarief (9%)",
        "1e" => "Leveringen/diensten belast met 0% of niet bij u belast",
        _ => "Ander tarief (controleren)",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rate {
    pub base_cents: i64,
    pub vat_cents: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Per VAT rate (bp): turnover and VAT.
    pub sales: BTreeMap<i64, Rate>,
    /// 5b: input VAT on purchase invoices and receipts.
    pub input_vat_cents: i64,
}

/// Whole euros rounded down (towards minus infinity).
pub fn euros_down(cents: i64) -> i64 {
    cents.div_euclid(100)
}

/// Whole euros rounded up.
pub fn euros_up(cents: i64) -> i64 {
    -((-cents).div_euclid(100))
}

impl Summary {
    pub fn add_invoice(&mut self, t: &Totals) {
        for (bp, g) in &t.per_rate {
            let r = self.sales.entry(*bp).or_default();
            r.base_cents += g.base_cents;
            r.vat_cents += g.vat_cents;
        }
    }

    /// 5a: VAT due on sales.
    pub fn sales_vat_cents(&self) -> i64 {
        self.sales.values().map(|r| r.vat_cents).sum()
    }

    /// 5c / 5g: positive = to pay, negative = to reclaim.
    pub fn balance_cents(&self) -> i64 {
        self.sales_vat_cents() - self.input_vat_cents
    }

    /// The same in whole euros, rounded in your favour: VAT due down, input VAT up.
    pub fn balance_euros(&self) -> i64 {
        let due: i64 = self.sales.values().map(|r| euros_down(r.vat_cents)).sum();
        due - euros_up(self.input_vat_cents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money;

    #[test]
    fn rounding_in_your_favour() {
        assert_eq!(euros_down(123_499), 1234);
        assert_eq!(euros_down(-150), -2);
        assert_eq!(euros_up(123_401), 1235);
        assert_eq!(euros_up(123_400), 1234);
        assert_eq!(euros_up(0), 0);
    }

    #[test]
    fn sums_invoices_and_credit_notes_per_rate() {
        let mut s = Summary::default();
        s.add_invoice(&money::totals(&[(100_000, 2100), (20_000, 900), (5_000, 0)]).unwrap());
        s.add_invoice(&money::totals(&[(50_050, 2100)]).unwrap());
        // credit note
        s.add_invoice(&money::totals(&[(-10_000, 2100)]).unwrap());
        assert_eq!(
            s.sales[&2100],
            Rate {
                base_cents: 140_050,
                vat_cents: 21_000 + 10_511 - 2_100
            }
        );
        assert_eq!(
            s.sales[&900],
            Rate {
                base_cents: 20_000,
                vat_cents: 1_800
            }
        );
        assert_eq!(s.sales[&0].vat_cents, 0);
        s.input_vat_cents = 6_972;
        assert_eq!(s.sales_vat_cents(), 29_411 + 1_800);
        assert_eq!(s.balance_cents(), 31_211 - 6_972);
        // whole euros: 294 + 18 - 70 = 242
        assert_eq!(s.balance_euros(), 294 + 18 - 70);
        assert_eq!(box_for(2100), "1a");
        assert_eq!(box_for(550), "?");
    }
}
