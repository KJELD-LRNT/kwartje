-- Private cars can be registered with just the number of km per trip
-- ("Aantal km", the default) instead of odometer readings. Business cars
-- always use readings (rittenregistratie for the bijtelling).

ALTER TABLE vehicles ADD COLUMN odometer_mode TEXT NOT NULL DEFAULT 'odometer'
    CHECK (odometer_mode IN ('km', 'odometer') AND (kind = 'private' OR odometer_mode = 'odometer'));
UPDATE vehicles SET odometer_mode = 'km' WHERE kind = 'private';

-- Readings become optional: both set (km = end - start) or both empty.
-- SQLite can't drop NOT NULL in place, so the table is rebuilt. No other
-- table references mileage_entries.
CREATE TABLE mileage_entries_new (
    id               INTEGER PRIMARY KEY,
    company_id       INTEGER NOT NULL REFERENCES companies(id),
    vehicle_id       INTEGER NOT NULL REFERENCES vehicles(id),
    user_id          INTEGER NOT NULL REFERENCES users(id),
    date             TEXT    NOT NULL,
    from_address     TEXT    NOT NULL,
    to_address       TEXT    NOT NULL,
    purpose          TEXT    NOT NULL DEFAULT '',
    client_id        INTEGER,
    odometer_start   INTEGER CHECK (odometer_start IS NULL OR odometer_start >= 0),
    odometer_end     INTEGER,
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
    CHECK ((odometer_start IS NULL AND odometer_end IS NULL)
        OR (odometer_start IS NOT NULL AND odometer_end IS NOT NULL
            AND km = odometer_end - odometer_start)),
    CHECK ((tariff_id IS NULL) = (price_cents IS NULL) AND (tariff_id IS NULL) = (vat_bp IS NULL)),
    -- only business trips with a client can be billed
    CHECK (tariff_id IS NULL OR (business = 1 AND client_id IS NOT NULL))
);

INSERT INTO mileage_entries_new
    (id, company_id, vehicle_id, user_id, date, from_address, to_address, purpose, client_id,
     odometer_start, odometer_end, km, business, tariff_id, price_cents, vat_bp, route_id,
     invoice_line_id, archived_at, created_at, updated_at, created_by)
SELECT id, company_id, vehicle_id, user_id, date, from_address, to_address, purpose, client_id,
       odometer_start, odometer_end, km, business, tariff_id, price_cents, vat_bp, route_id,
       invoice_line_id, archived_at, created_at, updated_at, created_by
FROM mileage_entries;

DROP TABLE mileage_entries;
ALTER TABLE mileage_entries_new RENAME TO mileage_entries;

CREATE INDEX mileage_entries_vehicle ON mileage_entries (vehicle_id, odometer_start);
CREATE INDEX mileage_entries_company ON mileage_entries (company_id, date);

CREATE TRIGGER mileage_entries_no_delete BEFORE DELETE ON mileage_entries
BEGIN
    SELECT RAISE(ABORT, 'mileage_entries: rows are archived, not deleted');
END;

-- An invoiced km entry can't change. The only allowed change is releasing it
-- (invoice_line_id back to NULL, by a deleted draft or a credit note).
CREATE TRIGGER mileage_entries_invoiced_locked BEFORE UPDATE ON mileage_entries
WHEN OLD.invoice_line_id IS NOT NULL AND (
       NEW.invoice_line_id IS NOT NULL AND NEW.invoice_line_id IS NOT OLD.invoice_line_id
    OR NEW.company_id IS NOT OLD.company_id
    OR NEW.vehicle_id IS NOT OLD.vehicle_id
    OR NEW.user_id IS NOT OLD.user_id
    OR NEW.date IS NOT OLD.date
    OR NEW.from_address IS NOT OLD.from_address
    OR NEW.to_address IS NOT OLD.to_address
    OR NEW.purpose IS NOT OLD.purpose
    OR NEW.client_id IS NOT OLD.client_id
    OR NEW.odometer_start IS NOT OLD.odometer_start
    OR NEW.odometer_end IS NOT OLD.odometer_end
    OR NEW.km IS NOT OLD.km
    OR NEW.business IS NOT OLD.business
    OR NEW.tariff_id IS NOT OLD.tariff_id
    OR NEW.price_cents IS NOT OLD.price_cents
    OR NEW.vat_bp IS NOT OLD.vat_bp
    OR NEW.route_id IS NOT OLD.route_id
    OR NEW.archived_at IS NOT OLD.archived_at)
BEGIN
    SELECT RAISE(ABORT, 'mileage_entries: invoiced entries are locked');
END;
