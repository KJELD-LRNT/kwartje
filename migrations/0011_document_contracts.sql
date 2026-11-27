-- Documents: add the kind 'contract'. SQLite cannot change a CHECK
-- constraint, so the table is rebuilt; ids, files and links are unchanged.
-- sqlx runs SQLite migrations inside a transaction, so foreign keys cannot
-- be switched off. invoices.pdf_document_id references documents: with the
-- check deferred, dropping the old table counts those references as broken
-- and inserting the rows into the new "documents" table counts them as
-- fixed again. Commit fails if anything is left unresolved.
PRAGMA defer_foreign_keys = ON;

CREATE TABLE documents_copy_0011 AS SELECT * FROM documents;
DROP TABLE documents;

CREATE TABLE documents (
    id             INTEGER PRIMARY KEY,
    company_id     INTEGER NOT NULL REFERENCES companies(id),
    kind           TEXT    NOT NULL CHECK (kind IN ('sales_invoice', 'purchase_invoice', 'receipt', 'contract', 'other')),
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
    description    TEXT    NOT NULL DEFAULT '',
    UNIQUE (id, company_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id)
);

INSERT INTO documents (id, company_id, kind, file_path, sha256, original_name, mime, size,
                           doc_date, supplier, amount_cents, vat_cents, invoice_id, client_id,
                           archived_at, created_at, updated_at, created_by, description)
SELECT id, company_id, kind, file_path, sha256, original_name, mime, size,
       doc_date, supplier, amount_cents, vat_cents, invoice_id, client_id,
       archived_at, created_at, updated_at, created_by, description
FROM documents_copy_0011;

DROP TABLE documents_copy_0011;

CREATE INDEX documents_company ON documents (company_id, kind, doc_date);
CREATE INDEX documents_sha ON documents (company_id, sha256);
CREATE INDEX documents_supplier ON documents (company_id, supplier);

-- Abort (and roll back) if any foreign key no longer resolves.
CREATE TEMP TABLE fk_check_0011 (problems INTEGER NOT NULL CHECK (problems = 0));
INSERT INTO fk_check_0011 SELECT COUNT(*) FROM pragma_foreign_key_check;
DROP TABLE fk_check_0011;
