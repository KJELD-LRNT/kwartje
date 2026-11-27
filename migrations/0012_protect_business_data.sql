-- Business data is archived, never deleted. Until now the application code
-- enforced that; these triggers make the database refuse it too, so a bug in
-- a later change can't remove history. Drafts stay deletable: draft invoices
-- and quotes with their lines, and recurring template lines (those tables
-- have their own "issued" triggers).

CREATE TRIGGER users_no_delete BEFORE DELETE ON users
BEGIN
    SELECT RAISE(ABORT, 'users: rows are archived, not deleted');
END;

CREATE TRIGGER companies_no_delete BEFORE DELETE ON companies
BEGIN
    SELECT RAISE(ABORT, 'companies: rows are archived, not deleted');
END;

CREATE TRIGGER clients_no_delete BEFORE DELETE ON clients
BEGIN
    SELECT RAISE(ABORT, 'clients: rows are archived, not deleted');
END;

CREATE TRIGGER projects_no_delete BEFORE DELETE ON projects
BEGIN
    SELECT RAISE(ABORT, 'projects: rows are archived, not deleted');
END;

CREATE TRIGGER tasks_no_delete BEFORE DELETE ON tasks
BEGIN
    SELECT RAISE(ABORT, 'tasks: rows are archived, not deleted');
END;

CREATE TRIGGER tariffs_no_delete BEFORE DELETE ON tariffs
BEGIN
    SELECT RAISE(ABORT, 'tariffs: rows are archived, not deleted');
END;

CREATE TRIGGER time_entries_no_delete BEFORE DELETE ON time_entries
BEGIN
    SELECT RAISE(ABORT, 'time_entries: rows are archived, not deleted');
END;

CREATE TRIGGER vehicles_no_delete BEFORE DELETE ON vehicles
BEGIN
    SELECT RAISE(ABORT, 'vehicles: rows are archived, not deleted');
END;

CREATE TRIGGER mileage_rates_no_delete BEFORE DELETE ON mileage_rates
BEGIN
    SELECT RAISE(ABORT, 'mileage_rates: rows are archived, not deleted');
END;

CREATE TRIGGER saved_routes_no_delete BEFORE DELETE ON saved_routes
BEGIN
    SELECT RAISE(ABORT, 'saved_routes: rows are archived, not deleted');
END;

CREATE TRIGGER mileage_entries_no_delete BEFORE DELETE ON mileage_entries
BEGIN
    SELECT RAISE(ABORT, 'mileage_entries: rows are archived, not deleted');
END;

CREATE TRIGGER documents_no_delete BEFORE DELETE ON documents
BEGIN
    SELECT RAISE(ABORT, 'documents: rows are archived, not deleted');
END;

CREATE TRIGGER payments_no_delete BEFORE DELETE ON payments
BEGIN
    SELECT RAISE(ABORT, 'payments: rows are archived, not deleted');
END;

CREATE TRIGGER reminders_no_delete BEFORE DELETE ON reminders
BEGIN
    SELECT RAISE(ABORT, 'reminders: rows are archived, not deleted');
END;

CREATE TRIGGER invoice_sequences_no_delete BEFORE DELETE ON invoice_sequences
BEGIN
    SELECT RAISE(ABORT, 'invoice_sequences: rows are archived, not deleted');
END;

CREATE TRIGGER quote_sequences_no_delete BEFORE DELETE ON quote_sequences
BEGIN
    SELECT RAISE(ABORT, 'quote_sequences: rows are archived, not deleted');
END;

CREATE TRIGGER recurring_invoices_no_delete BEFORE DELETE ON recurring_invoices
BEGIN
    SELECT RAISE(ABORT, 'recurring_invoices: rows are archived, not deleted');
END;

-- An invoiced time entry can't change. The only allowed change is releasing it
-- (invoice_line_id back to NULL, by a deleted draft or a credit note).
CREATE TRIGGER time_entries_invoiced_locked BEFORE UPDATE ON time_entries
WHEN OLD.invoice_line_id IS NOT NULL AND (
       NEW.invoice_line_id IS NOT NULL AND NEW.invoice_line_id IS NOT OLD.invoice_line_id
    OR NEW.user_id IS NOT OLD.user_id
    OR NEW.task_id IS NOT OLD.task_id
    OR NEW.client_id IS NOT OLD.client_id
    OR NEW.project_id IS NOT OLD.project_id
    OR NEW.tariff_id IS NOT OLD.tariff_id
    OR NEW.date IS NOT OLD.date
    OR NEW.minutes IS NOT OLD.minutes
    OR NEW.description IS NOT OLD.description
    OR NEW.price_cents IS NOT OLD.price_cents
    OR NEW.vat_bp IS NOT OLD.vat_bp
    OR NEW.archived_at IS NOT OLD.archived_at
    OR NEW.company_id IS NOT OLD.company_id)
BEGIN
    SELECT RAISE(ABORT, 'time_entries: invoiced entries are locked');
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

-- A stored file never changes: its path (except the one step from the
-- temporary "pending/<hash>" path at upload), checksum, size, type and
-- company are fixed. Invoice PDFs (sales_invoice) can't be re-labelled, and
-- no other document can become one.
CREATE TRIGGER documents_file_immutable BEFORE UPDATE ON documents
WHEN (NEW.file_path IS NOT OLD.file_path AND OLD.file_path NOT LIKE 'pending/%')
    OR NEW.sha256 IS NOT OLD.sha256
    OR NEW.size IS NOT OLD.size
    OR NEW.mime IS NOT OLD.mime
    OR NEW.company_id IS NOT OLD.company_id
    OR (OLD.kind = 'sales_invoice') <> (NEW.kind = 'sales_invoice')
BEGIN
    SELECT RAISE(ABORT, 'documents: stored files are immutable');
END;
