-- Kilometerregistratie.
--
-- Vehicles are global: one physical car can drive for several companies.
-- Each trip belongs to one company; odometer continuity is checked per car
-- across all companies (in code). mileage_rates holds the tax-free allowance
-- per km per year (entered by hand).

CREATE TABLE vehicles (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL,
    plate         TEXT    NOT NULL UNIQUE,
    kind          TEXT    NOT NULL CHECK (kind IN ('private', 'business')),
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id)
);

CREATE TABLE mileage_rates (
    id                 INTEGER PRIMARY KEY,
    year               INTEGER NOT NULL UNIQUE CHECK (year BETWEEN 2000 AND 2100),
    rate_cents_per_km  INTEGER NOT NULL CHECK (rate_cents_per_km BETWEEN 0 AND 1000),
    created_at         TEXT    NOT NULL,
    updated_at         TEXT    NOT NULL,
    created_by         INTEGER REFERENCES users(id)
);

CREATE TABLE saved_routes (
    id            INTEGER PRIMARY KEY,
    company_id    INTEGER NOT NULL REFERENCES companies(id),
    name          TEXT    NOT NULL,
    from_address  TEXT    NOT NULL,
    to_address    TEXT    NOT NULL,
    km            INTEGER NOT NULL CHECK (km BETWEEN 1 AND 2000),
    archived_at   TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    created_by    INTEGER REFERENCES users(id),
    UNIQUE (id, company_id)
);
CREATE INDEX saved_routes_company ON saved_routes (company_id, name);

CREATE TABLE mileage_entries (
    id               INTEGER PRIMARY KEY,
    company_id       INTEGER NOT NULL REFERENCES companies(id),
    vehicle_id       INTEGER NOT NULL REFERENCES vehicles(id),
    user_id          INTEGER NOT NULL REFERENCES users(id),
    date             TEXT    NOT NULL,
    from_address     TEXT    NOT NULL,
    to_address       TEXT    NOT NULL,
    purpose          TEXT    NOT NULL DEFAULT '',
    client_id        INTEGER,
    odometer_start   INTEGER NOT NULL CHECK (odometer_start >= 0),
    odometer_end     INTEGER NOT NULL,
    km               INTEGER NOT NULL CHECK (km BETWEEN 1 AND 2000),
    business         INTEGER NOT NULL DEFAULT 1 CHECK (business IN (0, 1)),
    tariff_id        INTEGER,
    price_cents      INTEGER CHECK (price_cents IS NULL OR price_cents >= 0),
    vat_bp           INTEGER CHECK (vat_bp IS NULL OR vat_bp BETWEEN 0 AND 10000),
    route_id         INTEGER,
    invoice_line_id  INTEGER,
    archived_at      TEXT,
    created_at       TEXT    NOT NULL,
    updated_at       TEXT    NOT NULL,
    created_by       INTEGER REFERENCES users(id),
    FOREIGN KEY (client_id, company_id) REFERENCES clients (id, company_id),
    FOREIGN KEY (tariff_id, company_id) REFERENCES tariffs (id, company_id),
    FOREIGN KEY (route_id, company_id) REFERENCES saved_routes (id, company_id),
    CHECK (km = odometer_end - odometer_start),
    CHECK ((tariff_id IS NULL) = (price_cents IS NULL) AND (tariff_id IS NULL) = (vat_bp IS NULL)),
    -- only business trips with a client can be billed
    CHECK (tariff_id IS NULL OR (business = 1 AND client_id IS NOT NULL))
);
CREATE INDEX mileage_entries_vehicle ON mileage_entries (vehicle_id, odometer_start);
CREATE INDEX mileage_entries_company ON mileage_entries (company_id, date);
