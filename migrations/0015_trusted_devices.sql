-- Devices on which a user chose "MFA 30 dagen onthouden": after the password
-- step the TOTP code is skipped while the device cookie matches an active row.
-- Only the SHA-256 hash of the cookie token is stored. Rows are never deleted:
-- revoking sets revoked_at, expiry is expires_at.
CREATE TABLE trusted_devices (
    id            INTEGER PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id),
    token_hash    TEXT    NOT NULL UNIQUE,
    label         TEXT    NOT NULL,
    created_at    TEXT    NOT NULL,
    expires_at    TEXT    NOT NULL,
    last_used_at  TEXT,
    revoked_at    TEXT
);

CREATE INDEX trusted_devices_user ON trusted_devices (user_id);
