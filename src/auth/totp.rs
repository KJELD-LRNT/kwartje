//! TOTP (RFC 6238): SHA1, 6 digits, 30 s steps, ±1 step tolerance.
//! A code is accepted at most once: the used step is stored and older or equal
//! steps are rejected.

use totp_rs::{Algorithm, Secret, TOTP};

pub const ISSUER: &str = "Kwartje";
const STEP: u64 = 30;

pub fn generate_secret() -> String {
    match Secret::generate_secret().to_encoded() {
        Secret::Encoded(s) => s,
        Secret::Raw(_) => unreachable!("to_encoded returns Encoded"),
    }
}

fn build(secret_b32: &str, account: &str) -> anyhow::Result<TOTP> {
    let bytes = Secret::Encoded(secret_b32.to_string())
        .to_bytes()
        .map_err(|e| anyhow::anyhow!("invalid TOTP secret: {e:?}"))?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        STEP,
        bytes,
        Some(ISSUER.to_string()),
        account.to_string(),
    )
    .map_err(|e| anyhow::anyhow!("invalid TOTP parameters: {e}"))
}

/// `otpauth://` URL for authenticator apps.
pub fn otpauth_url(secret_b32: &str, account: &str) -> anyhow::Result<String> {
    Ok(build(secret_b32, account)?.get_url())
}

/// QR code for the `otpauth://` URL as inline SVG.
pub fn qr_svg(url: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .build())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Code for a given time (tests and the seed command).
pub fn code_at(secret_b32: &str, unix: u64) -> anyhow::Result<String> {
    Ok(build(secret_b32, "test")?.generate(unix))
}

/// Check `code` at time `unix`. Returns the matched step when valid and newer
/// than `last_step`.
pub fn verify(secret_b32: &str, code: &str, unix: u64, last_step: Option<i64>) -> Option<i64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let totp = build(secret_b32, "check").ok()?;
    let current = unix / STEP;
    for step in [current.saturating_sub(1), current, current + 1] {
        let candidate = totp.generate(step * STEP);
        if constant_time_eq(candidate.as_bytes(), code.as_bytes()) {
            let step = i64::try_from(step).ok()?;
            if last_step.is_some_and(|last| step <= last) {
                return None;
            }
            return Some(step);
        }
    }
    None
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_current_and_adjacent_steps() {
        let s = generate_secret();
        let t = 1_800_000_000;
        assert!(verify(&s, &code_at(&s, t).unwrap(), t, None).is_some());
        assert!(verify(&s, &code_at(&s, t - 30).unwrap(), t, None).is_some());
        assert!(verify(&s, &code_at(&s, t + 30).unwrap(), t, None).is_some());
        assert!(verify(&s, &code_at(&s, t - 90).unwrap(), t, None).is_none());
        assert!(verify(&s, "12345", t, None).is_none());
        assert!(verify(&s, "abcdef", t, None).is_none());
    }

    #[test]
    fn rejects_replay() {
        let s = generate_secret();
        let t = 1_800_000_000;
        let code = code_at(&s, t).unwrap();
        let step = verify(&s, &code, t, None).unwrap();
        assert_eq!(verify(&s, &code, t, Some(step)), None);
        // the next step's code is still fine
        let next = code_at(&s, t + 30).unwrap();
        assert!(verify(&s, &next, t + 30, Some(step)).is_some());
    }

    #[test]
    fn url_and_qr() {
        let s = generate_secret();
        let url = otpauth_url(&s, "anna@example.test").unwrap();
        assert!(url.starts_with("otpauth://totp/"));
        assert!(url.contains(&s));
        assert!(qr_svg(&url).unwrap().contains("<svg"));
    }
}
