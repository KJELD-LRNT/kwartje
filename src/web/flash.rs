//! One-shot messages shown on the next page ("Bedrijf opgeslagen.").

use serde::{Deserialize, Serialize};
use tower_sessions::Session;

const KEY: &str = "flash";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flash {
    pub ok: bool,
    pub message: String,
}

async fn push(
    session: &Session,
    ok: bool,
    message: &str,
) -> Result<(), tower_sessions::session::Error> {
    let mut list: Vec<Flash> = session.get(KEY).await?.unwrap_or_default();
    list.push(Flash {
        ok,
        message: message.to_string(),
    });
    session.insert(KEY, list).await
}

pub async fn ok(session: &Session, message: &str) -> Result<(), tower_sessions::session::Error> {
    push(session, true, message).await
}

pub async fn error(session: &Session, message: &str) -> Result<(), tower_sessions::session::Error> {
    push(session, false, message).await
}

pub async fn take(session: &Session) -> Result<Vec<Flash>, tower_sessions::session::Error> {
    Ok(session.remove(KEY).await?.unwrap_or_default())
}
