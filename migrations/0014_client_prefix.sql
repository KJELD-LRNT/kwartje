-- Invoice and quote numbers carry the client's prefix (VH-2026-0041), in the
-- company's single series per year (invoice_sequences / quote_sequences are
-- unchanged). Issuing for a client without a prefix is refused by the app.
-- Numbers already issued keep the company prefix they got.
ALTER TABLE clients ADD COLUMN invoice_prefix TEXT
    CHECK (invoice_prefix IS NULL OR (length(invoice_prefix) BETWEEN 1 AND 10
                                      AND invoice_prefix NOT GLOB '*[^A-Z0-9]*'));

-- One client per prefix within a company.
CREATE UNIQUE INDEX clients_invoice_prefix ON clients (company_id, invoice_prefix)
    WHERE invoice_prefix IS NOT NULL;
