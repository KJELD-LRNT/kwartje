-- Invoices, lines, number sequences, payments, reminders and documents.
-- Issued invoices are immutable (triggers); drafts may be deleted, so the
-- audit log gets a 'delete' action.

-- audit_log: allow 'delete' (rebuild; keep it append-only).
DROP TRIGGER audit_log_no_update;
DROP TRIGGER audit_log_no_delete;
CREATE TABLE audit_log_new (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER REFERENCES users(id),
    table_name  TEXT    NOT NULL,
    row_id      INTEGER NOT NULL,
    action      TEXT    NOT NULL CHECK (action IN ('insert', 'update', 'archive', 'unarchive', 'delete')),
    at          TEXT    NOT NULL,
    diff_json   TEXT    NOT NULL
);
INSERT INTO audit_log_new (id, user_id, table_name, row_id, action, at, diff_json)
    SELECT id, user_id, table_name, row_id, action, at, diff_json FROM audit_log;
DROP TABLE audit_log;
ALTER TABLE audit_log_new RENAME TO audit_log;
CREATE INDEX audit_log_row ON audit_log (table_name, row_id);
CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;
CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TABLE documents (
    id             INTEGER PRIMARY KEY,
    company_id     INTEGER NOT NULL REFERENCES companies(id),
    kind           TEXT    NOT NULL CHECK (kind IN ('sales_invoice', 'purchase_invoice', 'receipt', 'other')),
    file_path      TEXT    NOT NULL UNIQUE,
    sha256         TEXT    NOT NULL,
    original_name  TEXT    NOT NULL,
    mime           TEXT    NOT NULL,
    size           INTEGER NOT NULL CHECK (size >= 0),
    doc_date       TEXT    NOT NULL,
    supplier       TEXT,
    amount_cents   INTEGER,
    vat_cents      INTEGER,
    invoice_id     INTEGER,
    client_id      INTEGER,
    archived_at    TEXT,
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL,
    created_by     INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id)
);
CREATE INDEX documents_company ON documents (company_id, kind, doc_date);
CREATE INDEX documents_sha ON documents (company_id, sha256);

CREATE TABLE invoices (
    id                     INTEGER PRIMARY KEY,
    company_id             INTEGER NOT NULL REFERENCES companies(id),
    client_id              INTEGER NOT NULL,
    kind                   TEXT    NOT NULL DEFAULT 'invoice' CHECK (kind IN ('invoice', 'credit')),
    credit_for_invoice_id  INTEGER,
    release_entries        INTEGER NOT NULL DEFAULT 0 CHECK (release_entries IN (0, 1)),
    number                 TEXT,
    issue_date             TEXT,
    due_date               TEXT,
    period_from            TEXT    NOT NULL,
    period_to              TEXT    NOT NULL,
    line_style             TEXT    NOT NULL DEFAULT 'task' CHECK (line_style IN ('task', 'tariff', 'project', 'entry')),
    hide_zero_lines        INTEGER NOT NULL DEFAULT 0 CHECK (hide_zero_lines IN (0, 1)),
    include_hours_spec     INTEGER NOT NULL DEFAULT 0 CHECK (include_hours_spec IN (0, 1)),
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
    pdf_document_id        INTEGER,
    created_at             TEXT    NOT NULL,
    updated_at             TEXT    NOT NULL,
    created_by             INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    UNIQUE (company_id, number),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id),
    FOREIGN KEY (credit_for_invoice_id, company_id) REFERENCES invoices (id, company_id),
    FOREIGN KEY (pdf_document_id, company_id) REFERENCES documents (id, company_id),
    CHECK ((number IS NULL) = (issue_date IS NULL)),
    CHECK (kind = 'invoice' OR credit_for_invoice_id IS NOT NULL)
);
CREATE INDEX invoices_company ON invoices (company_id, issue_date);
CREATE INDEX invoices_client ON invoices (company_id, client_id);

CREATE TABLE invoice_lines (
    id                INTEGER PRIMARY KEY,
    invoice_id        INTEGER NOT NULL REFERENCES invoices(id),
    position          INTEGER NOT NULL,
    description       TEXT    NOT NULL,
    quantity_milli    INTEGER NOT NULL,
    unit              TEXT    NOT NULL CHECK (unit IN ('uur', 'km', 'stuk')),
    unit_price_cents  INTEGER NOT NULL,
    vat_bp            INTEGER NOT NULL CHECK (vat_bp BETWEEN 0 AND 10000),
    line_total_cents  INTEGER NOT NULL,
    source            TEXT    NOT NULL CHECK (source IN ('hours', 'km', 'manual')),
    is_zero           INTEGER NOT NULL DEFAULT 0 CHECK (is_zero IN (0, 1)),
    created_at        TEXT    NOT NULL,
    updated_at        TEXT    NOT NULL,
    created_by        INTEGER REFERENCES users(id)
);
CREATE INDEX invoice_lines_invoice ON invoice_lines (invoice_id, position);

-- Last issued number per company and year (gapless; updated in the issue transaction).
CREATE TABLE invoice_sequences (
    id           INTEGER PRIMARY KEY,
    company_id   INTEGER NOT NULL REFERENCES companies(id),
    year         INTEGER NOT NULL,
    last_number  INTEGER NOT NULL CHECK (last_number >= 0),
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    UNIQUE (company_id, year)
);

CREATE TABLE payments (
    id            INTEGER PRIMARY KEY,
    invoice_id    INTEGER NOT NULL REFERENCES invoices(id),
    date          TEXT    NOT NULL,
    amount_cents  INTEGER NOT NULL CHECK (amount_cents <> 0),
    method        TEXT    NOT NULL CHECK (method IN ('bank', 'contant', 'anders')),
    note          TEXT    NOT NULL DEFAULT '',
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id)
);
CREATE INDEX payments_invoice ON payments (invoice_id);

CREATE TABLE reminders (
    id          INTEGER PRIMARY KEY,
    invoice_id  INTEGER NOT NULL REFERENCES invoices(id),
    level       TEXT    NOT NULL CHECK (level IN ('1', '2', 'final')),
    sent_at     TEXT    NOT NULL,
    sent_to     TEXT    NOT NULL DEFAULT '',
    via_email   INTEGER NOT NULL DEFAULT 0 CHECK (via_email IN (0, 1)),
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL,
    created_by  INTEGER REFERENCES users(id)
);
CREATE INDEX reminders_invoice ON reminders (invoice_id);

-- Immutability of issued invoices.
CREATE TRIGGER invoices_issued_immutable BEFORE UPDATE ON invoices
WHEN OLD.number IS NOT NULL AND (
       NEW.number IS NOT OLD.number OR NEW.company_id IS NOT OLD.company_id
    OR NEW.client_id IS NOT OLD.client_id OR NEW.kind IS NOT OLD.kind
    OR NEW.credit_for_invoice_id IS NOT OLD.credit_for_invoice_id
    OR NEW.issue_date IS NOT OLD.issue_date OR NEW.due_date IS NOT OLD.due_date
    OR NEW.period_from IS NOT OLD.period_from OR NEW.period_to IS NOT OLD.period_to
    OR NEW.line_style IS NOT OLD.line_style OR NEW.hide_zero_lines IS NOT OLD.hide_zero_lines
    OR NEW.include_hours_spec IS NOT OLD.include_hours_spec
    OR NEW.intro_text IS NOT OLD.intro_text OR NEW.footer_text IS NOT OLD.footer_text
    OR NEW.subtotal_cents IS NOT OLD.subtotal_cents OR NEW.vat_cents IS NOT OLD.vat_cents
    OR NEW.total_cents IS NOT OLD.total_cents
    OR NEW.company_snapshot_json IS NOT OLD.company_snapshot_json
    OR NEW.client_snapshot_json IS NOT OLD.client_snapshot_json
    OR NEW.issued_at IS NOT OLD.issued_at OR NEW.pdf_document_id IS NOT OLD.pdf_document_id
    OR NEW.release_entries IS NOT OLD.release_entries)
BEGIN
    SELECT RAISE(ABORT, 'issued invoices are immutable');
END;

CREATE TRIGGER invoices_issued_no_delete BEFORE DELETE ON invoices
WHEN OLD.number IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued invoices cannot be deleted');
END;

CREATE TRIGGER invoice_lines_no_insert_issued BEFORE INSERT ON invoice_lines
WHEN (SELECT number FROM invoices WHERE id = NEW.invoice_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued invoices are immutable');
END;

CREATE TRIGGER invoice_lines_no_update_issued BEFORE UPDATE ON invoice_lines
WHEN (SELECT number FROM invoices WHERE id = OLD.invoice_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued invoices are immutable');
END;

CREATE TRIGGER invoice_lines_no_delete_issued BEFORE DELETE ON invoice_lines
WHEN (SELECT number FROM invoices WHERE id = OLD.invoice_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'issued invoices are immutable');
END;
