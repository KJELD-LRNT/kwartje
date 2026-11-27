-- One row per company. Almost every business table hangs off this.
CREATE TABLE companies (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL,
    kvk                TEXT    NOT NULL DEFAULT '',
    btw_id             TEXT    NOT NULL DEFAULT '',
    iban               TEXT    NOT NULL DEFAULT '',
    street             TEXT    NOT NULL DEFAULT '',
    postal_code        TEXT    NOT NULL DEFAULT '',
    city               TEXT    NOT NULL DEFAULT '',
    email              TEXT    NOT NULL DEFAULT '',
    phone              TEXT    NOT NULL DEFAULT '',
    invoice_prefix     TEXT    NOT NULL UNIQUE,
    payment_term_days  INTEGER NOT NULL DEFAULT 14 CHECK (payment_term_days BETWEEN 1 AND 365),
    default_vat_bp     INTEGER NOT NULL DEFAULT 2100 CHECK (default_vat_bp BETWEEN 0 AND 10000),
    rounding_minutes   INTEGER NOT NULL DEFAULT 0 CHECK (rounding_minutes BETWEEN 0 AND 60),
    logo_path          TEXT,
    archived_at        TEXT,
    created_at         TEXT    NOT NULL,
    updated_at         TEXT    NOT NULL,
    created_by         INTEGER REFERENCES users(id)
);
