-- Roles, accountant company access, and the security log.
--
-- admin      (Beheerder):  everything, user management, security log; books
--                          hours and trips on behalf of others, never on itself
-- employee   (Medewerker): everything except user management and the security log
-- accountant (Boekhouder): read-only, only the companies in user_companies
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'employee'
    CHECK (role IN ('admin', 'employee', 'accountant'));
-- Set for a temporary password chosen by an admin; cleared by the user's own change.
ALTER TABLE users ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0
    CHECK (must_change_password IN (0, 1));
ALTER TABLE users ADD COLUMN last_login_at TEXT;

-- Which companies an accountant may see. A link, not business data: removing
-- access deletes the row (audited).
CREATE TABLE user_companies (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    company_id  INTEGER NOT NULL REFERENCES companies(id),
    created_at  TEXT    NOT NULL,
    created_by  INTEGER REFERENCES users(id),
    UNIQUE (user_id, company_id)
);

-- Sign-ins and other security events. Append-only; rows can only be removed
-- once they are older than the retention of 365 days.
CREATE TABLE security_events (
    id           INTEGER PRIMARY KEY,
    at           TEXT    NOT NULL,
    event        TEXT    NOT NULL,
    user_id      INTEGER REFERENCES users(id),
    email        TEXT    NOT NULL DEFAULT '',
    ip           TEXT    NOT NULL DEFAULT '',
    via          TEXT    NOT NULL DEFAULT '',
    user_agent   TEXT    NOT NULL DEFAULT '',
    detail_json  TEXT    NOT NULL DEFAULT '{}'
);
CREATE INDEX security_events_at ON security_events (at);
CREATE INDEX security_events_user ON security_events (user_id, at);

CREATE TRIGGER security_events_no_update BEFORE UPDATE ON security_events
BEGIN
    SELECT RAISE(ABORT, 'security_events: append-only');
END;

CREATE TRIGGER security_events_retention BEFORE DELETE ON security_events
WHEN OLD.at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-365 days')
BEGIN
    SELECT RAISE(ABORT, 'security_events: kept for 365 days');
END;

-- Last address a trusted device was used from (an IP change is logged).
ALTER TABLE trusted_devices ADD COLUMN last_ip TEXT;
