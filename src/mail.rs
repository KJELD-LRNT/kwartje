//! Outgoing e-mail (invoices, reminders). SMTP via lettre when configured;
//! tests capture messages in memory; without config sending is unavailable
//! and the user marks invoices as sent by hand.

use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::SmtpConfig;

#[derive(Debug, Clone)]
pub struct Email {
    pub to: String,
    pub reply_to: Option<String>,
    pub from_name: String,
    pub subject: String,
    pub body: String,
    pub attachment: Option<(String, Vec<u8>)>,
}

#[derive(Clone)]
pub enum Mailer {
    /// No `[smtp]` in the config.
    Disabled,
    Smtp {
        transport: Box<AsyncSmtpTransport<Tokio1Executor>>,
        from: Mailbox,
    },
    /// Tests: keeps every message.
    Memory(Arc<Mutex<Vec<Email>>>),
}

impl Mailer {
    pub fn from_config(cfg: Option<&SmtpConfig>) -> Result<Self> {
        let Some(c) = cfg else {
            return Ok(Mailer::Disabled);
        };
        let builder = match c.security.as_str() {
            "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&c.host)?,
            "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&c.host)?,
            "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&c.host),
            other => bail!("smtp.security moet 'starttls', 'tls' of 'none' zijn, niet '{other}'"),
        };
        let mut builder = builder.port(c.port);
        if let Some(user) = &c.username {
            let password = match &c.password_file {
                Some(p) => std::fs::read_to_string(p)
                    .with_context(|| format!("lezen van {}", p.display()))?
                    .trim()
                    .to_string(),
                None => String::new(),
            };
            builder = builder.credentials(Credentials::new(user.clone(), password));
        }
        let from: Mailbox = c
            .from
            .parse()
            .context("smtp.from is geen geldig e-mailadres")?;
        Ok(Mailer::Smtp {
            transport: Box::new(builder.build()),
            from,
        })
    }

    pub fn memory() -> (Self, Arc<Mutex<Vec<Email>>>) {
        let store = Arc::new(Mutex::new(Vec::new()));
        (Mailer::Memory(store.clone()), store)
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, Mailer::Disabled)
    }

    pub async fn send(&self, e: Email) -> Result<()> {
        match self {
            Mailer::Disabled => bail!("e-mail is niet ingesteld"),
            Mailer::Memory(store) => {
                store.lock().unwrap_or_else(|p| p.into_inner()).push(e);
                Ok(())
            }
            Mailer::Smtp { transport, from } => {
                let from = Mailbox::new(Some(e.from_name.clone()), from.email.clone());
                let mut b = Message::builder()
                    .from(from)
                    .to(e
                        .to
                        .parse()
                        .context("ongeldig e-mailadres van de ontvanger")?)
                    .subject(e.subject.clone());
                if let Some(r) = &e.reply_to {
                    if let Ok(mb) = r.parse::<Mailbox>() {
                        b = b.reply_to(mb);
                    }
                }
                let text = SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(e.body.clone());
                let msg = match &e.attachment {
                    Some((name, bytes)) => b.multipart(
                        MultiPart::mixed().singlepart(text).singlepart(
                            Attachment::new(name.clone())
                                .body(bytes.clone(), ContentType::parse("application/pdf")?),
                        ),
                    )?,
                    None => b.singlepart(text)?,
                };
                transport
                    .send(msg)
                    .await
                    .context("versturen via SMTP mislukt")?;
                Ok(())
            }
        }
    }
}
