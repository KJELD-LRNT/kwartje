-- Clients and projects, per company.
-- Composite foreign keys (id, company_id) make it impossible to link rows of
-- two different companies. (companies.id is the primary key, so no extra index.)


CREATE TABLE clients (
    id            INTEGER PRIMARY KEY,
    company_id    INTEGER NOT NULL REFERENCES companies(id),
    name          TEXT    NOT NULL,
    contact_name  TEXT    NOT NULL DEFAULT '',
    email         TEXT    NOT NULL DEFAULT '',
    phone         TEXT    NOT NULL DEFAULT '',
    street        TEXT    NOT NULL DEFAULT '',
    postal_code   TEXT    NOT NULL DEFAULT '',
    city          TEXT    NOT NULL DEFAULT '',
    kvk           TEXT    NOT NULL DEFAULT '',
    btw_id        TEXT    NOT NULL DEFAULT '',
    notes         TEXT    NOT NULL DEFAULT '',
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id),
    UNIQUE (id, company_id)
);
CREATE INDEX clients_company ON clients (company_id, name);

CREATE TABLE projects (
    id            INTEGER PRIMARY KEY,
    company_id    INTEGER NOT NULL REFERENCES companies(id),
    client_id     INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    UNIQUE (id, client_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id)
);
CREATE INDEX projects_client ON projects (company_id, client_id);
