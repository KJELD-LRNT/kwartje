-- Tariffs and tasks.
--
-- Tariff level: client_id/project_id both NULL = company level, client only =
-- client level, project (+ its client) = project level. `is_default` marks the
-- tariff picked automatically at its level. A price change creates a new row
-- with the same `lineage_id` (the id of the first version) and a later
-- `valid_from`; the old row gets `valid_to`.

CREATE TABLE tariffs (
    id            INTEGER PRIMARY KEY,
    company_id    INTEGER NOT NULL REFERENCES companies(id),
    lineage_id    INTEGER,
    name          TEXT    NOT NULL,
    unit          TEXT    NOT NULL CHECK (unit IN ('hour', 'km', 'piece', 'fixed')),
    price_cents   INTEGER NOT NULL CHECK (price_cents >= 0),
    vat_bp        INTEGER NOT NULL CHECK (vat_bp BETWEEN 0 AND 10000),
    client_id     INTEGER,
    project_id    INTEGER,
    is_default    INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    valid_from    TEXT    NOT NULL,
    valid_to      TEXT    CHECK (valid_to IS NULL OR valid_to >= valid_from),
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id),
    FOREIGN KEY (project_id, company_id) REFERENCES projects (id, company_id),
    FOREIGN KEY (project_id, client_id) REFERENCES projects (id, client_id),
    CHECK (project_id IS NULL OR client_id IS NOT NULL)
);
CREATE INDEX tariffs_company ON tariffs (company_id, unit, valid_from);
CREATE INDEX tariffs_lineage ON tariffs (lineage_id, valid_from);

CREATE TRIGGER tariffs_set_lineage AFTER INSERT ON tariffs
WHEN NEW.lineage_id IS NULL
BEGIN
    UPDATE tariffs SET lineage_id = NEW.id WHERE id = NEW.id;
END;

CREATE TABLE tasks (
    id            INTEGER PRIMARY KEY,
    company_id    INTEGER NOT NULL REFERENCES companies(id),
    name          TEXT    NOT NULL,
    client_id     INTEGER,
    project_id    INTEGER,
    tariff_id     INTEGER,
    billable      INTEGER NOT NULL DEFAULT 1 CHECK (billable IN (0, 1)),
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id),
    UNIQUE (id, company_id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id),
    FOREIGN KEY (project_id, company_id) REFERENCES projects (id, company_id),
    FOREIGN KEY (project_id, client_id) REFERENCES projects (id, client_id),
    FOREIGN KEY (tariff_id, company_id) REFERENCES tariffs (id, company_id),
    CHECK (project_id IS NULL OR client_id IS NOT NULL),
    CHECK (billable = 1 OR tariff_id IS NULL)
);
CREATE INDEX tasks_company ON tasks (company_id, name);
