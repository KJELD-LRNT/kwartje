-- Hour registration. price_cents/vat_bp are a snapshot of the tariff at the
-- moment the entry was saved; tariff changes never alter them.
-- invoice_line_id gets its foreign key in phase 4 (invoices).

CREATE TABLE time_entries (
    id               INTEGER PRIMARY KEY,
    company_id       INTEGER NOT NULL REFERENCES companies(id),
    user_id          INTEGER NOT NULL REFERENCES users(id),
    task_id          INTEGER NOT NULL,
    client_id        INTEGER,
    project_id       INTEGER,
    tariff_id        INTEGER,
    date             TEXT    NOT NULL,
    minutes          INTEGER NOT NULL CHECK (minutes BETWEEN 1 AND 1440),
    description      TEXT    NOT NULL DEFAULT '',
    price_cents      INTEGER CHECK (price_cents IS NULL OR price_cents >= 0),
    vat_bp           INTEGER CHECK (vat_bp IS NULL OR vat_bp BETWEEN 0 AND 10000),
    invoice_line_id  INTEGER,
    archived_at      TEXT,
    created_at       TEXT    NOT NULL,
    updated_at       TEXT    NOT NULL,
    created_by       INTEGER REFERENCES users(id),
    FOREIGN KEY (task_id, company_id) REFERENCES tasks (id, company_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id),
    FOREIGN KEY (project_id, company_id) REFERENCES projects (id, company_id),
    FOREIGN KEY (project_id, client_id) REFERENCES projects (id, client_id),
    FOREIGN KEY (tariff_id, company_id) REFERENCES tariffs (id, company_id),
    CHECK (project_id IS NULL OR client_id IS NOT NULL),
    -- a tariff snapshot exists exactly when a tariff is set
    CHECK ((tariff_id IS NULL) = (price_cents IS NULL) AND (tariff_id IS NULL) = (vat_bp IS NULL)),
    -- billable hours need a client
    CHECK (tariff_id IS NULL OR client_id IS NOT NULL)
);
CREATE INDEX time_entries_date ON time_entries (company_id, date);
CREATE INDEX time_entries_user ON time_entries (company_id, user_id, date);
CREATE INDEX time_entries_client ON time_entries (company_id, client_id);
