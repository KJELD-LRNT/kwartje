//! Audit log: every insert/update/archive on business data is recorded in the
//! same transaction as the write itself.

use anyhow::Result;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;

use crate::db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Insert,
    Update,
    Archive,
    Unarchive,
    /// Only for drafts (the one kind of business row that may be deleted).
    Delete,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Insert => "insert",
            Action::Update => "update",
            Action::Archive => "archive",
            Action::Unarchive => "unarchive",
            Action::Delete => "delete",
        }
    }

    pub fn label_nl(action: &str) -> &'static str {
        match action {
            "insert" => "aangemaakt",
            "update" => "gewijzigd",
            "archive" => "gearchiveerd",
            "unarchive" => "teruggezet",
            "delete" => "verwijderd",
            _ => "onbekend",
        }
    }
}

/// Columns whose values never appear in the audit log.
const SECRET_FIELDS: &[&str] = &["password_hash", "totp_secret"];

/// Bookkeeping columns that are not interesting in a diff.
const IGNORED_FIELDS: &[&str] = &["updated_at"];

fn redact(field: &str, v: &Value) -> Value {
    if SECRET_FIELDS.contains(&field) && !v.is_null() {
        Value::String("gewijzigd".into())
    } else {
        v.clone()
    }
}

/// Full row for an insert, with secrets redacted.
pub fn snapshot(row: &Value) -> Value {
    match row.as_object() {
        Some(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), redact(k, v)))
                .collect::<Map<_, _>>(),
        ),
        None => row.clone(),
    }
}

/// Changed fields between two row snapshots: `{field: [old, new]}`.
pub fn diff(old: &Value, new: &Value) -> Value {
    let empty = Map::new();
    let old = old.as_object().unwrap_or(&empty);
    let new = new.as_object().unwrap_or(&empty);
    let mut out = Map::new();
    for (k, nv) in new {
        if IGNORED_FIELDS.contains(&k.as_str()) {
            continue;
        }
        let ov = old.get(k).unwrap_or(&Value::Null);
        if ov != nv {
            out.insert(k.clone(), Value::Array(vec![redact(k, ov), redact(k, nv)]));
        }
    }
    Value::Object(out)
}

/// Write one audit row. Pass the transaction the business write runs in.
pub async fn record(
    conn: &mut SqliteConnection,
    user_id: Option<i64>,
    table: &str,
    row_id: i64,
    action: Action,
    diff: &Value,
) -> Result<()> {
    let at = db::now();
    let diff_json = diff.to_string();
    let action = action.as_str();
    sqlx::query!(
        "INSERT INTO audit_log (user_id, table_name, row_id, action, at, diff_json)
         VALUES (?, ?, ?, ?, ?, ?)",
        user_id,
        table,
        row_id,
        action,
        at,
        diff_json
    )
    .execute(conn)
    .await?;
    Ok(())
}

fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Audit an insert: the full row (secrets redacted).
pub async fn record_insert<T: serde::Serialize>(
    conn: &mut SqliteConnection,
    user_id: Option<i64>,
    table: &str,
    row_id: i64,
    row: &T,
) -> Result<()> {
    record(
        conn,
        user_id,
        table,
        row_id,
        Action::Insert,
        &snapshot(&to_json(row)),
    )
    .await
}

/// Audit an update: only the changed fields; nothing is written when nothing changed.
pub async fn record_update<T: serde::Serialize>(
    conn: &mut SqliteConnection,
    user_id: Option<i64>,
    table: &str,
    row_id: i64,
    old: &T,
    new: &T,
) -> Result<()> {
    let d = diff(&to_json(old), &to_json(new));
    if d.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(());
    }
    record(conn, user_id, table, row_id, Action::Update, &d).await
}

/// Audit archiving or un-archiving.
pub async fn record_archive(
    conn: &mut SqliteConnection,
    user_id: Option<i64>,
    table: &str,
    row_id: i64,
    old_archived_at: Option<&str>,
    new_archived_at: Option<&str>,
) -> Result<()> {
    let action = if new_archived_at.is_some() {
        Action::Archive
    } else {
        Action::Unarchive
    };
    record(
        conn,
        user_id,
        table,
        row_id,
        action,
        &serde_json::json!({ "archived_at": [old_archived_at, new_archived_at] }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_lists_only_changed_fields() {
        let old = json!({"name": "A", "kvk": "1", "updated_at": "t1"});
        let new = json!({"name": "B", "kvk": "1", "updated_at": "t2"});
        assert_eq!(diff(&old, &new), json!({"name": ["A", "B"]}));
    }

    #[test]
    fn secrets_are_redacted() {
        let old = json!({"password_hash": "old", "totp_secret": null});
        let new = json!({"password_hash": "new", "totp_secret": "ABC"});
        assert_eq!(
            diff(&old, &new),
            json!({
                "password_hash": ["gewijzigd", "gewijzigd"],
                "totp_secret": [null, "gewijzigd"]
            })
        );
        assert_eq!(
            snapshot(&json!({"name": "K", "password_hash": "x"})),
            json!({"name": "K", "password_hash": "gewijzigd"})
        );
    }
}
