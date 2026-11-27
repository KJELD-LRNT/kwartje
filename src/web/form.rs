//! Shared helpers for HTML forms: field errors, checkboxes, optional ids.

use std::collections::BTreeMap;

use crate::web::AppError;

/// Field name → Dutch error message.
pub type Errors = BTreeMap<&'static str, String>;

/// The message for a field, or "" (templates test `.is_empty()`).
pub fn err<'a>(errors: &'a Errors, field: &str) -> &'a str {
    errors.get(field).map(String::as_str).unwrap_or("")
}

/// HTML checkboxes send a value only when ticked.
pub fn checked(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.is_empty() && s != "0")
}

/// Optional id from a `<select>`: "" means none; garbage is a bad request.
pub fn opt_id(s: &str) -> Result<Option<i64>, AppError> {
    match s.trim() {
        "" => Ok(None),
        v => v
            .parse::<i64>()
            .map(Some)
            .map_err(|_| AppError::BadRequest("Ongeldige keuze in het formulier.".into())),
    }
}

pub fn is_unique_violation(err: &AppError) -> bool {
    match err {
        AppError::Internal(e) => e
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .is_some_and(|d| d.is_unique_violation()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        let mut e = Errors::new();
        e.insert("name", "Naam is verplicht.".into());
        assert_eq!(err(&e, "name"), "Naam is verplicht.");
        assert_eq!(err(&e, "kvk"), "");
        assert!(checked(&Some("on".into())));
        assert!(!checked(&None));
        assert!(!checked(&Some("0".into())));
        assert_eq!(opt_id("").unwrap(), None);
        assert_eq!(opt_id("12").unwrap(), Some(12));
        assert!(opt_id("x").is_err());
    }
}
