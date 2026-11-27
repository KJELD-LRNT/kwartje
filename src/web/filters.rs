//! Askama filters wrapping `format.rs`, so templates format exactly like the
//! rest of the app. Bring into scope with `use crate::web::filters;`.

use crate::format;

/// Integer-ish template values (i64 and references to them).
pub trait AsI64 {
    fn as_i64(&self) -> i64;
}

impl AsI64 for i64 {
    fn as_i64(&self) -> i64 {
        *self
    }
}

impl<T: AsI64 + ?Sized> AsI64 for &T {
    fn as_i64(&self) -> i64 {
        (**self).as_i64()
    }
}

/// `{{ cents|amount }}` → `€ 1.234,56`
pub fn amount<T: AsI64>(v: T, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(format::amount(v.as_i64()))
}

/// `{{ bp|vat }}` → `21%`
pub fn vat<T: AsI64>(v: T, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(format::vat_pct(v.as_i64()))
}

/// `{{ minutes|duration }}` → `1:30`
pub fn duration<T: AsI64>(v: T, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(format::duration(v.as_i64()))
}

/// `{{ "2026-11-25"|date }}` → `25-11-2026` (stored ISO date).
pub fn date<T: AsRef<str>>(v: T, _: &dyn askama::Values) -> askama::Result<String> {
    let s = v.as_ref();
    Ok(format::parse_iso_date(s)
        .map(format::date)
        .unwrap_or_else(|| s.to_string()))
}

/// `{{ created_at|datetime }}` → `25-11-2026 11:00` (Amsterdam time).
pub fn datetime<T: AsRef<str>>(v: T, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(format::datetime(v.as_ref()))
}

/// `{{ cents|decimal_amount }}` → `1234,56` (for form fields).
pub fn decimal_amount<T: AsI64>(v: T, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(format::decimal(v.as_i64()))
}
