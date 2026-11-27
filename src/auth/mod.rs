//! Authentication: argon2 passwords, mandatory TOTP, sessions, rate limiting.

pub mod devices;
pub mod password;
pub mod ratelimit;
pub mod routes;
pub mod totp;
pub mod users;

pub mod session_keys {
    /// Set only after password **and** TOTP are verified.
    pub const USER_ID: &str = "user_id";
    /// Set after the password step; `(user_id, unix time)`.
    pub const PENDING: &str = "pending_user";
    /// TOTP secret shown during enrolment, not yet confirmed.
    pub const TOTP_SETUP: &str = "totp_setup_secret";
    /// Selected company.
    pub const COMPANY_ID: &str = "company_id";
    /// Unix time of the completed login (password + TOTP).
    pub const LOGIN_AT: &str = "login_at";
    /// Set when "Ingelogd blijven" was ticked: unix time the session ends.
    pub const REMEMBER_UNTIL: &str = "remember_until";
    /// "Ingelogd blijven" choice carried from the password step to the TOTP step.
    pub const PENDING_REMEMBER: &str = "pending_remember";
}

/// A session ends this long after login, however active it is. (Inactivity
/// ends it sooner: `web::SESSION_IDLE`.)
pub const SESSION_MAX_SECS: u64 = 7 * 24 * 3600;

/// True when a login made at `login_at` (unix seconds) is too old at `now`.
/// A session without a login time (made before this rule) counts as expired.
pub fn login_expired(login_at: Option<u64>, now: u64) -> bool {
    match login_at {
        Some(t) => now.saturating_sub(t) > SESSION_MAX_SECS,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_end_seven_days_after_login() {
        let t = 1_800_000_000;
        assert!(!login_expired(Some(t), t));
        assert!(!login_expired(Some(t), t + SESSION_MAX_SECS));
        assert!(login_expired(Some(t), t + SESSION_MAX_SECS + 1));
        assert!(login_expired(None, t));
    }
}
