//! Validation and normalisation of Dutch business identifiers.

/// Strip spaces and dots, uppercase.
fn compact(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '.' && *c != '-')
        .collect::<String>()
        .to_uppercase()
}

/// KvK number: exactly 8 digits. Returns the normalised value.
pub fn kvk(s: &str) -> Result<String, &'static str> {
    let v = compact(s);
    if v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit()) {
        Ok(v)
    } else {
        Err("Een KvK-nummer bestaat uit 8 cijfers.")
    }
}

/// Dutch BTW-id (omzetbelastingnummer): `NL` + 9 digits + `B` + 2 digits.
pub fn btw_id(s: &str) -> Result<String, &'static str> {
    let v = compact(s);
    let b = v.as_bytes();
    let ok = b.len() == 14
        && v.starts_with("NL")
        && b[2..11].iter().all(u8::is_ascii_digit)
        && b[11] == b'B'
        && b[12..].iter().all(u8::is_ascii_digit);
    if ok {
        Ok(v)
    } else {
        Err("Een btw-id ziet eruit als NL123456789B01.")
    }
}

/// IBAN with mod-97 check. Returns it in groups of four (`NL91 ABNA 0417 1643 00`).
pub fn iban(s: &str) -> Result<String, &'static str> {
    const MSG: &str = "Dit IBAN is ongeldig (controlegetal klopt niet).";
    let v = compact(s);
    if !(15..=34).contains(&v.len())
        || !v.bytes().all(|b| b.is_ascii_alphanumeric())
        || !v.as_bytes()[..2].iter().all(u8::is_ascii_alphabetic)
        || !v.as_bytes()[2..4].iter().all(u8::is_ascii_digit)
    {
        return Err(MSG);
    }
    if v.starts_with("NL") && v.len() != 18 {
        return Err("Een Nederlands IBAN heeft 18 tekens.");
    }
    let rearranged = format!("{}{}", &v[4..], &v[..4]);
    let mut rem: u32 = 0;
    for c in rearranged.chars() {
        let n = c.to_digit(36).expect("alphanumeric");
        for d in n.to_string().chars() {
            rem = (rem * 10 + d.to_digit(10).expect("digit")) % 97;
        }
    }
    if rem != 1 {
        return Err(MSG);
    }
    Ok(v.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Dutch postcode `1234 AB`.
pub fn postal_code(s: &str) -> Result<String, &'static str> {
    let v: String = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase();
    let b = v.as_bytes();
    if b.len() == 6
        && b[0] != b'0'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4..].iter().all(u8::is_ascii_uppercase)
    {
        Ok(format!("{} {}", &v[..4], &v[4..]))
    } else {
        Err("Een postcode ziet eruit als 1234 AB.")
    }
}

/// Dutch licence plate (6 letters/digits) in its sidecode form, e.g.
/// `ab123c` → `AB-123-C`, `1234AB` → `12-34-AB`.
pub fn plate(s: &str) -> Result<String, &'static str> {
    const MSG: &str = "Vul een Nederlands kenteken in, bijvoorbeeld AB-123-C.";
    let v: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    if v.len() != 6
        || s.chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == ' '))
    {
        return Err(MSG);
    }
    let mut groups: Vec<String> = Vec::new();
    for c in v.chars() {
        match groups.last_mut() {
            Some(g)
                if g.chars()
                    .last()
                    .is_some_and(|l| l.is_ascii_digit() == c.is_ascii_digit()) =>
            {
                g.push(c)
            }
            _ => groups.push(c.to_string()),
        }
    }
    let groups: Vec<String> = groups
        .into_iter()
        .flat_map(|g| {
            if g.len() == 4 {
                vec![g[..2].to_string(), g[2..].to_string()]
            } else {
                vec![g]
            }
        })
        .collect();
    if groups.len() != 3 {
        return Err(MSG);
    }
    Ok(groups.join("-"))
}

pub fn email(s: &str) -> Result<String, &'static str> {
    let v = s.trim();
    let ok = v.len() <= 254
        && !v.contains(char::is_whitespace)
        && v.split_once('@').is_some_and(|(l, d)| {
            !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
        });
    if ok {
        Ok(v.to_string())
    } else {
        Err("Dit e-mailadres is ongeldig.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvk_numbers() {
        assert_eq!(kvk("1234 5678"), Ok("12345678".into()));
        assert!(kvk("1234567").is_err());
        assert!(kvk("1234567a").is_err());
    }

    #[test]
    fn btw_ids() {
        assert_eq!(btw_id("nl 1234.56.789 b01"), Ok("NL123456789B01".into()));
        assert!(btw_id("NL123456789").is_err());
        assert!(btw_id("BE0123456789").is_err());
    }

    #[test]
    fn ibans() {
        // Well-known valid example IBANs.
        assert_eq!(
            iban("NL91ABNA0417164300"),
            Ok("NL91 ABNA 0417 1643 00".into())
        );
        assert_eq!(
            iban("nl91 abna 0417 1643 00"),
            Ok("NL91 ABNA 0417 1643 00".into())
        );
        assert!(iban("DE89370400440532013000").is_ok());
        assert!(iban("NL91ABNA0417164301").is_err());
        assert!(iban("NL91ABNA041716430").is_err());
        assert!(iban("hallo").is_err());
    }

    #[test]
    fn postal_codes() {
        assert_eq!(postal_code("1234ab"), Ok("1234 AB".into()));
        assert_eq!(postal_code("1234 AB"), Ok("1234 AB".into()));
        assert!(postal_code("0123 AB").is_err());
        assert!(postal_code("12345").is_err());
    }

    #[test]
    fn plates() {
        assert_eq!(plate("ab123c"), Ok("AB-123-C".into()));
        assert_eq!(plate("AB-123-C"), Ok("AB-123-C".into()));
        assert_eq!(plate("12 34 ab"), Ok("12-34-AB".into()));
        assert_eq!(plate("XX9999"), Ok("XX-99-99".into()));
        assert_eq!(plate("9XXX99"), Ok("9-XXX-99".into()));
        assert_eq!(plate("GZ123B"), Ok("GZ-123-B".into()));
        assert!(plate("ABC").is_err());
        assert!(plate("ABCDEF").is_err());
        assert!(plate("AB.123.C").is_err());
    }

    #[test]
    fn emails() {
        assert!(email("a@b.nl").is_ok());
        assert!(email("a@b").is_err());
        assert!(email("a b@c.nl").is_err());
    }
}
