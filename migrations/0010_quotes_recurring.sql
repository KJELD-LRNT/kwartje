-- Quotes (offertes) with their own number series, and recurring invoice
-- templates that generate draft invoices.

CREATE TABLE quotes (
    id                     INTEGER PRIMARY KEY,
    company_id             INTEGER NOT NULL REFERENCES companies(id),
    client_id              INTEGER NOT NULL,
    number                 TEXT,
    issue_date             TEXT,
    valid_until            TEXT    NOT NULL,
    intro_text             TEXT    NOT NULL DEFAULT '',
    footer_text            TEXT    NOT NULL DEFAULT '',
    subtotal_cents         INTEGER NOT NULL DEFAULT 0,
    vat_cents              INTEGER NOT NULL DEFAULT 0,
    total_cents            INTEGER NOT NULL DEFAULT 0,
    company_snapshot_json  TEXT,
    client_snapshot_json   TEXT,
    issued_at              TEXT,
    sent_at                TEXT,
    sent_to                TEXT,
    accepted_at            TEXT,
    rejected_at            TEXT,
    invoice_id             INTEGER,
    created_at             TEXT    NOT NULL,
    updated_at             TEXT    NOT NULL,
    created_by             INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    UNIQUE (company_id, number),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id),
    FOREIGN KEY (invoice_id, company_id) REFERENCES invoices (id, company_id),
    CHECK ((number IS NULL) = (issue_date IS NULL)),
    CHECK (accepted_at IS NULL OR rejected_at IS NULL)
);
CREATE INDEX quotes_company ON quotes (company_id, issue_date);

CREATE TABLE quote_lines (
    id                INTEGER PRIMARY KEY,
    quote_id          INTEGER NOT NULL REFERENCES quotes(id),
    position          INTEGER NOT NULL,
    description       TEXT    NOT NULL,
    quantity_milli    INTEGER NOT NULL,
    unit              TEXT    NOT NULL CHECK (unit IN ('uur', 'km', 'stuk')),
    unit_price_cents  INTEGER NOT NULL,
    vat_bp            INTEGER NOT NULL CHECK (vat_bp BETWEEN 0 AND 10000),
    line_total_cents  INTEGER NOT NULL,
    created_at        TEXT    NOT NULL,
    updated_at        TEXT    NOT NULL,
    created_by        INTEGER REFERENCES users(id)
);
CREATE INDEX quote_lines_quote ON quote_lines (quote_id, position);

CREATE TABLE quote_sequences (
    id           INTEGER PRIMARY KEY,
    company_id   INTEGER NOT NULL REFERENCES companies(id),
    year         INTEGER NOT NULL,
    last_number  INTEGER NOT NULL CHECK (last_number >= 0),
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    UNIQUE (company_id, year)
);

CREATE TRIGGER quotes_issued_immutable BEFORE UPDATE ON quotes
WHEN OLD.number IS NOT NULL AND (
       NEW.number IS NOT OLD.number OR NEW.company_id IS NOT OLD.company_id
    OR NEW.client_id IS NOT OLD.client_id OR NEW.issue_date IS NOT OLD.issue_date
    OR NEW.valid_until IS NOT OLD.valid_until OR NEW.intro_text IS NOT OLD.intro_text
    OR NEW.footer_text IS NOT OLD.footer_text OR NEW.subtotal_cents IS NOT OLD.subtotal_cents
    OR NEW.vat_cents IS NOT OLD.vat_cents OR NEW.total_cents IS NOT OLD.total_cents
    OR NEW.company_snapshot_json IS NOT OLD.company_snapshot_json
    OR NEW.client_snapshot_json IS NOT OLD.client_snapshot_json
    OR NEW.issued_at IS NOT OLD.issued_at)
BEGIN
    SELECT RAISE(ABORT, 'issued quotes are immutable');
END;

CREATE TRIGGER quotes_issued_no_delete BEFORE DELETE ON quotes
WHEN OLD.number IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued quotes cannot be deleted');
END;

CREATE TRIGGER quote_lines_no_insert_issued BEFORE INSERT ON quote_lines
WHEN (SELECT number FROM quotes WHERE id = NEW.quote_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued quotes are immutable');
END;

CREATE TRIGGER quote_lines_no_update_issued BEFORE UPDATE ON quote_lines
WHEN (SELECT number FROM quotes WHERE id = OLD.quote_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued quotes are immutable');
END;

CREATE TRIGGER quote_lines_no_delete_issued BEFORE DELETE ON quote_lines
WHEN (SELECT number FROM quotes WHERE id = OLD.quote_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued quotes are immutable');
END;

CREATE TABLE recurring_invoices (
    id            INTEGER PRIMARY KEY,
    company_id    INTEGER NOT NULL REFERENCES companies(id),
    client_id     INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    interval      TEXT    NOT NULL CHECK (interval IN ('month', 'quarter', 'year')),
    next_date     TEXT    NOT NULL,
    end_date      TEXT,
    intro_text    TEXT    NOT NULL DEFAULT '',
    footer_text   TEXT    NOT NULL DEFAULT '',
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id)
);
CREATE INDEX recurring_company ON recurring_invoices (company_id, next_date);

CREATE TABLE recurring_lines (
    id                INTEGER PRIMARY KEY,
    recurring_id      INTEGER NOT NULL REFERENCES recurring_invoices(id),
    position          INTEGER NOT NULL,
    description       TEXT    NOT NULL,
    quantity_milli    INTEGER NOT NULL,
    unit              TEXT    NOT NULL CHECK (unit IN ('uur', 'km', 'stuk')),
    unit_price_cents  INTEGER NOT NULL,
    vat_bp            INTEGER NOT NULL CHECK (vat_bp BETWEEN 0 AND 10000),
    created_at        TEXT    NOT NULL,
    updated_at        TEXT    NOT NULL,
    created_by        INTEGER REFERENCES users(id)
);
CREATE INDEX recurring_lines_parent ON recurring_lines (recurring_id, position);

ALTER TABLE invoices ADD COLUMN recurring_id INTEGER REFERENCES recurring_invoices(id);
