-- Document store: a free-text description and a supplier index for search.
ALTER TABLE documents ADD COLUMN description TEXT NOT NULL DEFAULT '';
CREATE INDEX documents_supplier ON documents (company_id, supplier);
