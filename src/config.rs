//! Configuration loaded from `config.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the HTTP server binds to (behind `tailscale serve`: 127.0.0.1:8080).
    pub listen: SocketAddr,
    /// Holds `app.db` and `documents/`.
    pub data_dir: PathBuf,
    /// Root folder for `kwartje backup`.
    pub backup_dir: PathBuf,
    /// Secret used to sign session cookies. At least 64 characters.
    pub session_secret: String,
    /// Set the `Secure` flag on cookies. Only `false` for local development over plain HTTP.
    #[serde(default = "default_true")]
    pub cookie_secure: bool,
    #[serde(default)]
    pub backup: BackupConfig,
    /// Typst CLI used for invoice PDFs.
    #[serde(default = "default_typst")]
    pub typst_path: PathBuf,
    /// Outgoing mail for invoices and reminders. Optional: without it you
    /// download the PDF and mark the invoice as sent.
    #[serde(default)]
    pub smtp: Option<SmtpConfig>,
    /// Reverse proxies whose `X-Forwarded-For` is believed (exact IP
    /// addresses). Loopback is always trusted (`tailscale serve`, or a proxy
    /// on the same host). Needed when a proxy runs on another machine, so
    /// login rate limiting sees the real client instead of the proxy.
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Allow `listen = "0.0.0.0:…"` (every network interface). Off by
    /// default: anyone who can reach the machine can reach the login page,
    /// usually over plain HTTP. Only for a trusted network.
    #[serde(default)]
    pub allow_all_interfaces: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// `starttls` (default, port 587), `tls` (port 465) or `none` (local test only).
    #[serde(default = "default_security")]
    pub security: String,
    pub username: Option<String>,
    /// File holding the password (keeps it out of config.toml).
    pub password_file: Option<PathBuf>,
    /// Sender address, e.g. `facturen@example.nl`; the company name is used as display name.
    pub from: String,
}

fn default_smtp_port() -> u16 {
    587
}

fn default_security() -> String {
    "starttls".into()
}

fn default_typst() -> PathBuf {
    PathBuf::from("typst")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    #[serde(default = "default_daily")]
    pub keep_daily: usize,
    #[serde(default = "default_monthly")]
    pub keep_monthly: usize,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            keep_daily: default_daily(),
            keep_monthly: default_monthly(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_daily() -> usize {
    30
}
fn default_monthly() -> usize {
    12
}

pub const MIN_SECRET_LEN: usize = 64;

impl Config {
    /// Find the config file: explicit path, `$KWARTJE_CONFIG`, `./config.toml`,
    /// `/etc/kwartje/config.toml`.
    pub fn locate(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        if let Ok(p) = std::env::var("KWARTJE_CONFIG") {
            return Ok(PathBuf::from(p));
        }
        for candidate in ["config.toml", "/etc/kwartje/config.toml"] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return Ok(p);
            }
        }
        bail!("geen configuratiebestand gevonden (gezocht: ./config.toml en /etc/kwartje/config.toml); gebruik --config")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("lezen van configuratie {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("ongeldige configuratie {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.session_secret.len() < MIN_SECRET_LEN {
            bail!("session_secret moet minstens {MIN_SECRET_LEN} tekens lang zijn");
        }
        if self.session_secret.contains("CHANGE-ME") {
            bail!("session_secret heeft nog de voorbeeldwaarde; maak er een met `openssl rand -base64 64`");
        }
        if self.listen.ip().is_unspecified() && !self.allow_all_interfaces {
            bail!(
                "listen = \"{}\" luistert op alle netwerken; gebruik 127.0.0.1 (tailscale serve) of het eigen adres van de machine, of zet allow_all_interfaces = true als je dat bewust wilt",
                self.listen
            );
        }
        if self.backup.keep_daily == 0 {
            bail!("backup.keep_daily moet minstens 1 zijn");
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("app.db")
    }

    pub fn documents_dir(&self) -> PathBuf {
        self.data_dir.join("documents")
    }

    /// Config for tests and tools: everything under one directory.
    pub fn for_dir(dir: &Path) -> Self {
        Config {
            listen: "127.0.0.1:0".parse().expect("valid addr"),
            data_dir: dir.join("data"),
            backup_dir: dir.join("backups"),
            session_secret: "x".repeat(MIN_SECRET_LEN),
            cookie_secure: false,
            backup: BackupConfig::default(),
            typst_path: default_typst(),
            smtp: None,
            trusted_proxies: Vec::new(),
            allow_all_interfaces: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example must load as is (with a real secret), and with its
    /// optional lines switched on.
    #[test]
    fn example_config_parses() {
        let example = include_str!("../deploy/config.example.toml");
        let secret = "s".repeat(MIN_SECRET_LEN);
        let text = example.replace(
            "CHANGE-ME-CHANGE-ME-CHANGE-ME-CHANGE-ME-CHANGE-ME-CHANGE-ME-CHANGE-ME",
            &secret,
        );
        let cfg: Config = toml::from_str(&text).expect("example parses");
        cfg.validate().unwrap();
        assert_eq!(cfg.typst_path, PathBuf::from("/usr/local/bin/typst"));
        assert_eq!(cfg.backup.keep_daily, 30);
        assert!(cfg.smtp.is_none() && cfg.trusted_proxies.is_empty());

        // every commented-out setting switched on
        let all: String = text
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if rest.starts_with('[') || rest.contains(" = ") => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cfg: Config = toml::from_str(&all).expect("example with options parses");
        assert_eq!(cfg.trusted_proxies.len(), 1);
        assert_eq!(cfg.smtp.expect("smtp").port, 587);
    }

    #[test]
    fn listening_on_all_interfaces_is_refused() {
        let mut c = Config::for_dir(Path::new("/tmp/x"));
        c.listen = "0.0.0.0:8080".parse().unwrap();
        assert!(c.validate().is_err());
        c.listen = "[::]:8080".parse().unwrap();
        assert!(c.validate().is_err());
        c.listen = "10.0.0.20:8080".parse().unwrap();
        assert!(c.validate().is_ok());
        // only with the explicit setting
        c.listen = "0.0.0.0:8080".parse().unwrap();
        c.allow_all_interfaces = true;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn parses_example_config_shape() {
        let text = r#"
            listen = "127.0.0.1:8080"
            data_dir = "/var/lib/kwartje"
            backup_dir = "/var/backups/kwartje"
            session_secret = "0123456789012345678901234567890123456789012345678901234567890123"
            [backup]
            keep_daily = 30
            keep_monthly = 12
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.cookie_secure);
        assert_eq!(cfg.db_path(), PathBuf::from("/var/lib/kwartje/app.db"));
    }

    #[test]
    fn rejects_short_secret() {
        let mut cfg = Config::for_dir(Path::new("/tmp"));
        cfg.session_secret = "short".into();
        assert!(cfg.validate().is_err());
    }
}
