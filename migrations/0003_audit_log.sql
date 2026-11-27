-- Append-only log of every insert/update/archive on business data.
CREATE TABLE audit_log (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER REFERENCES users(id),
    table_name  TEXT    NOT NULL,
    row_id      INTEGER NOT NULL,
    action      TEXT    NOT NULL CHECK (action IN ('insert', 'update', 'archive', 'unarchive')),
    at          TEXT    NOT NULL,
    diff_json   TEXT    NOT NULL
);

CREATE INDEX audit_log_row ON audit_log (table_name, row_id);

CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;
