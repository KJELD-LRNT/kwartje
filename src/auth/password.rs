//! argon2id password hashing.

use std::sync::LazyLock;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

pub const MIN_LEN: usize = 12;

pub fn hash(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

pub fn verify(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Verified against when the e-mail address is unknown, so a login attempt
/// takes the same time whether or not the account exists.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash("dummy password for timing").expect("hashing works"));

pub fn burn_time(password: &str) {
    let _ = verify(&DUMMY_HASH, password);
}

/// Dutch validation message, or `None` when the password is acceptable.
pub fn check_strength(password: &str) -> Option<String> {
    if password.chars().count() < MIN_LEN {
        return Some(format!(
            "Het wachtwoord moet minstens {MIN_LEN} tekens lang zijn."
        ));
    }
    None
}

/// A temporary password suggestion for a new or reset account: four groups
/// of letters and digits without look-alikes ("kq7m-2xra-9pfw-h3tc").
pub fn suggestion() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rngs::OsRng;
    (0..4)
        .map(|_| {
            (0..4)
                .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify(&h, "correct horse battery"));
        assert!(!verify(&h, "wrong"));
        assert!(!verify("not a hash", "x"));
    }

    #[test]
    fn suggestions_are_strong_and_differ() {
        let a = suggestion();
        assert_eq!(a.len(), 19);
        assert!(check_strength(&a).is_none());
        assert_ne!(a, suggestion());
    }

    #[test]
    fn strength() {
        assert!(check_strength("kort").is_some());
        assert!(check_strength("lang-genoeg-123").is_none());
    }
}
