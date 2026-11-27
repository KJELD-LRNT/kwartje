-- Issued quotes store their PDF, like invoices: a new document kind 'quote'
-- and quotes.pdf_document_id.
--
-- Widening the kind CHECK rebuilds `documents` (same pattern as 0011: drop
-- with deferred foreign keys, re-insert, commit only when every reference
-- resolves). Dropping the table also drops its triggers from 0012, so they
-- are created again below, now treating quote PDFs like invoice PDFs.
PRAGMA defer_foreign_keys = ON;

CREATE TABLE documents_copy_0013 AS SELECT * FROM documents;
DROP TABLE documents;

CREATE TABLE documents (
    id             INTEGER PRIMARY KEY,
    company_id     INTEGER NOT NULL REFERENCES companies(id),
    kind           TEXT    NOT NULL CHECK (kind IN ('sales_invoice', 'quote', 'purchase_invoice', 'receipt', 'contract', 'other')),
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
FROM documents_copy_0013;

DROP TABLE documents_copy_0013;

CREATE INDEX documents_company ON documents (company_id, kind, doc_date);
CREATE INDEX documents_sha ON documents (company_id, sha256);
CREATE INDEX documents_supplier ON documents (company_id, supplier);

CREATE TRIGGER documents_no_delete BEFORE DELETE ON documents
BEGIN
    SELECT RAISE(ABORT, 'documents: rows are archived, not deleted');
END;

-- A stored file never changes: its path (except the one step from the
-- temporary "pending/<hash>" path at upload), checksum, size, type and
-- company are fixed. Generated PDFs (sales_invoice, quote) keep their kind,
-- and no upload can become one.
CREATE TRIGGER documents_file_immutable BEFORE UPDATE ON documents
WHEN (NEW.file_path IS NOT OLD.file_path AND OLD.file_path NOT LIKE 'pending/%')
    OR NEW.sha256 IS NOT OLD.sha256
    OR NEW.size IS NOT OLD.size
    OR NEW.mime IS NOT OLD.mime
    OR NEW.company_id IS NOT OLD.company_id
    OR (OLD.kind IN ('sales_invoice', 'quote') AND NEW.kind IS NOT OLD.kind)
    OR (NEW.kind IN ('sales_invoice', 'quote') AND NEW.kind IS NOT OLD.kind)
BEGIN
    SELECT RAISE(ABORT, 'documents: stored files are immutable');
END;

ALTER TABLE quotes ADD COLUMN pdf_document_id INTEGER REFERENCES documents(id);

-- Once an issued quote has its PDF, the link is fixed.
CREATE TRIGGER quotes_pdf_fixed BEFORE UPDATE ON quotes
WHEN OLD.pdf_document_id IS NOT NULL AND NEW.pdf_document_id IS NOT OLD.pdf_document_id
BEGIN
    SELECT RAISE(ABORT, 'issued quotes are immutable');
END;

-- Abort (and roll back) if any foreign key no longer resolves.
CREATE TEMP TABLE fk_check_0013 (problems INTEGER NOT NULL CHECK (problems = 0));
INSERT INTO fk_check_0013 SELECT COUNT(*) FROM pragma_foreign_key_check;
DROP TABLE fk_check_0013;
