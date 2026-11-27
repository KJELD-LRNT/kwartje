-- Users who can log in.
CREATE TABLE users (
    id               INTEGER PRIMARY KEY,
    name             TEXT    NOT NULL,
    email            TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash    TEXT    NOT NULL,
    totp_secret      TEXT,
    totp_enabled_at  TEXT,
    totp_last_step   INTEGER,
    archived_at      TEXT,
    created_at       TEXT    NOT NULL,
    updated_at       TEXT    NOT NULL,
    created_by       INTEGER REFERENCES users(id)
);
