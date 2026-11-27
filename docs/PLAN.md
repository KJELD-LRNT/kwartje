# Business administration app — setup & plan

Web app written in Rust, self-hosted (for example in a Proxmox LXC). A small team uses it to run the administration of one or more companies. Only the team logs in; there is no client-facing part.

## 1. Scope

**In scope**

- Basic administration: companies, settings, users
- Clients, per company (light CRM: contacts, notes, history)
- Hour registration on **tasks**, with **tariffs**
- Invoices generated from hours (and km), payment tracking, reminders, credit notes
- Kilometerregistratie (mileage log)
- Document store: generated sales invoices + uploaded purchase invoices and receipts
- CSV export of all data + a fixed backup path

**Out of scope:** double-entry bookkeeping, ledger, bank integration, BTW-aangifte filing, client portal, native mobile app, accountant login, foreign clients. Clients are Dutch only, so invoices are Dutch only, in euros, with no EU/ICP rules.

## 2. Technical setup

| Layer | Choice |
| --- | --- |
| Web framework | Axum + Tokio |
| UI | Askama templates + htmx + Pico.css (server-rendered, no JS build step, mobile-friendly) |
| Database | SQLite (WAL mode) via sqlx, migrations in `migrations/` |
| Money | Integer cents (`i64`), never floats. VAT rate in basis points (2100 = 21%) |
| PDF invoices | Typst template (`templates/invoice.typ`), rendered with the `typst` CLI |
| Auth | Local accounts, argon2 password hashes, `tower-sessions` (SQLite store), TOTP 2FA |
| CSV | `csv` + `serde` |
| Hosting | Debian 12 LXC on Proxmox (1 vCPU, 512 MB RAM, 8 GB disk), systemd service |
| Access | Tailscale only; app listens on the tailnet IP, HTTPS via `tailscale serve`. Never exposed to the internet |
| Backups | Nightly `kwartje backup` → `backup_dir` → restic offsite; Proxmox Backup Server snapshots the LXC |

**Language: fully Dutch.** All screens, menus, buttons, validation and error messages, emails, invoice and reminder texts, PDF templates and CSV column headers are in Dutch. Dutch formats: dates `25-11-2026`, amounts `€ 1.234,56`, decimal comma, weeks start on Monday, ISO week numbers. Only one language is needed, so the Dutch texts go straight into the templates (no i18n framework), with helpers for dates and amounts. Code, database tables and columns stay in English.

**Repository layout**

```
kwartje/
├── Cargo.toml
├── CLAUDE.md
├── docs/PLAN.md
├── migrations/          # sqlx SQL migrations
├── templates/           # Askama HTML + invoice.typ
├── static/              # htmx, pico.css (vendored, no CDN)
├── src/
│   ├── main.rs          # CLI (serve / backup / export / restore / create-user), config, router
│   ├── config.rs
│   ├── auth.rs
│   ├── db.rs            # pool, migrations, backup (VACUUM INTO)
│   ├── export.rs        # CSV per table + zip
│   ├── format.rs        # Dutch date/amount formatting
│   ├── modules/         # companies, clients, tasks, tariffs, hours, mileage, invoices, documents
│   └── pdf.rs           # Typst render
└── deploy/              # systemd units + timer, config.toml example, install notes
```

**Configuration** (`/etc/kwartje/config.toml`, local dev: `./config.toml`): listen address, `data_dir` (`app.db` + `documents/`), `backup_dir`, backup retention (30 daily / 12 monthly), session secret, SMTP settings (phase 4).

## 3. Data model

Almost every table hangs off `companies`, so one install serves several companies. Every table has `id`, `created_at`, `updated_at`, `created_by`. All writes go to `audit_log`. No hard deletes on business data: archive instead (`archived_at`).

| Table | Key fields | Notes |
| --- | --- | --- |
| `users` | name, email, password_hash, totp_secret | the team (roles: beheerder, medewerker, boekhouder) |
| `companies` | name, kvk, btw_id, iban, address, invoice_prefix, payment_term_days, default_vat_bp, rounding_minutes, logo_path | One row per company |
| `clients` | company_id, name, contact_name, email, phone, address, kvk, btw_id, notes, archived_at | |
| `projects` | company_id, client_id, name, archived_at | Optional grouping |
| `tasks` | company_id, name, client_id?, project_id?, tariff_id?, archived_at | What hours are logged on (e.g. Scouting, Contractonderhandeling, Administratie) |
| `tariffs` | company_id, name, unit (hour / km / piece / fixed), price_cents, vat_bp, client_id?, project_id?, valid_from, valid_to?, archived_at | `price_cents` may be 0 |
| `time_entries` | company_id, user_id, client_id?, project_id?, task_id, tariff_id?, date, minutes, description, price_cents?, vat_bp? (snapshot), invoice_line_id? | Locked once invoiced |
| `vehicles` | company_id, name, plate, kind (private / business) | |
| `mileage_entries` | company_id, vehicle_id, user_id, date, from_address, to_address, purpose, client_id?, odometer_start, odometer_end, km, business (bool), tariff_id?, price_cents?, invoice_line_id? | Valid rittenregistratie |
| `saved_routes` | company_id, name, from_address, to_address, km | One-tap trip entry |
| `mileage_rates` | year, rate_cents_per_km | Tax-free rate per year, entered by hand |
| `invoices` | company_id, client_id, number?, issue_date?, due_date?, period_from, period_to, status, subtotal_cents, vat_cents, total_cents, hide_zero_lines, include_hours_spec, pdf_document_id?, credit_for_invoice_id? | Number unique per company; immutable once issued |
| `invoice_lines` | invoice_id, description, quantity_milli, unit_price_cents, vat_bp, line_total_cents, source (hours / km / manual) | |
| `payments` | invoice_id, date, amount_cents, method, note | Partial payments allowed |
| `reminders` | invoice_id, level (1 / 2 / final), sent_at | |
| `documents` | company_id, kind (sales_invoice / purchase_invoice / receipt / other), file_path, sha256, original_name, mime, size, doc_date, supplier?, amount_cents?, vat_cents?, invoice_id?, client_id? | Files on disk, metadata in DB |
| `audit_log` | user_id, table_name, row_id, action, at, diff_json | |

Invoice status is derived: `concept` → `verzonden` → `deels betaald` / `betaald`; `vervallen` (overdue) = sent + past due date + not fully paid. Mistakes on an issued invoice are fixed with a credit note (`creditfactuur`), never by editing.

## 4. Tasks and tariffs

Hours are always logged on a **task** (`taak`). A task can be linked to a client and/or project and can carry its own tariff.

**Tariff resolution** when logging an hour, most specific first, and overridable per entry:

1. Tariff on the task
2. Tariff for the project
3. Tariff for the client
4. Company default tariff
5. None

When an entry is saved, the tariff's price and VAT are **copied** onto the entry (snapshot). Changing a tariff later never alters past entries or invoices.

**No tariff vs. € 0 tariff**

- **No tariff** (`tariff_id` NULL): internal, not billable. Never appears under *Te factureren* or on an invoice. Still counts in hour totals.
- **€ 0 tariff**: billable at € 0,00. Appears on the invoice as a line (e.g. *Kennismakingsgesprek — 1 uur × € 0,00*) to show work done free of charge. Per invoice option `hide_zero_lines`: hide € 0 lines from the invoice and show them only on the hours specification page.

Km work the same way: a tariff with unit `km` makes trips billable.

## 5. Hours → invoice flow

1. *Te factureren* per client, or *Nieuwe factuur → vanuit uren*, pick a period
2. List unbilled hours and km (entries with a tariff, not yet invoiced), grouped by task and tariff, with totals; tick/untick entries
3. Line style: one line per task, per tariff, per project, or per entry
4. Optional hours specification (date, task, description, duration) as page 2 of the PDF
5. Draft opens with lines, VAT per rate and totals; text editable, manual lines can be added
6. Issue (`Definitief maken`): next sequential number (e.g. `JB-2026-0001`), PDF rendered and stored, included entries linked and locked
7. Deleting a draft releases its entries back to *Te factureren*

Rounding per company (e.g. each entry rounded up to 15 minutes), applied when building the invoice; raw minutes stay stored.

**Required on every invoice:** company name, address, KvK and BTW-id; client name and address; invoice date; unique sequential number; delivery date or period; description and quantity; price ex VAT; VAT rate and VAT amount per rate; total. If reverse charge is ever used: "btw verlegd" + client BTW-id.

## 6. Other modules

**Basic administration:** companies (details, logo, prefix, payment term, default VAT, rounding), two users with TOTP, company switcher on every screen, dashboard per company (open and overdue invoices, hours and km this month, yearly hour total per user).

**Clients:** add/edit/archive, search, client page with hours, km, invoices and documents.

**Hours:** quick entry form (date, client, task, tariff prefilled, duration, description), week view per user with totals, unbilled list, yearly total per user per month.

**Invoices & payments:** status overview, ageing per client, payments (full or partial) by hand, email invoice PDF to client via SMTP, reminders (1st, 2nd, final notice) with Dutch templates, credit notes.

**Kilometerregistratie:** trips with odometer readings, saved routes, year overview per vehicle (business vs private km, tax-free allowance = business km × that year's rate).

**Document store:** sales invoice PDFs stored automatically; upload purchase invoices and receipts (PDF/JPG/PNG, mobile-friendly upload page with camera); path `documents/<company_id>/<year>/<kind>/<id>_<name>`; SHA-256 for integrity and duplicates; 7-year retention, no hard deletes.

**Later (phase 6):** quotes (`offertes`) → invoice, recurring invoices, quarterly BTW summary (VAT charged minus VAT on purchase documents) as a helper for the aangifte.

## 7. CSV export and backup

**CSV export**

- `GET /export/<table>.csv?company=&from=&to=` for every table + an export button on every list
- `GET /export/all.zip`: one CSV per table + `manifest.json` (app version, schema version, timestamp, row counts)
- UTF-8 with BOM, `;` separator (Dutch Excel), Dutch column headers, ISO dates, amounts as euros with 2 decimals plus a cents column
- CLI: `kwartje export --out <dir>`

**Backup** — `kwartje backup`, run daily at 02:00 by a systemd timer:

1. `VACUUM INTO` → consistent copy of `app.db` while the app runs
2. Full CSV export into the same folder
3. Copy/sync `documents/`
4. Result: `backup_dir/YYYY-MM-DD/{app.db, csv/, documents/, manifest.json}`; prune to 30 daily + 12 monthly
5. restic pushes `backup_dir` offsite, encrypted (separate timer/script in `deploy/`)
6. Non-zero exit on failure → notification (ntfy / Uptime Kuma push)

**Restore:** `kwartje restore <backup-folder>` — stop service, swap in `app.db` + `documents/`, start. Test quarterly in a throwaway LXC.

## 8. Build plan

1. **Foundation.** Gate: login works and `kwartje backup` produces a correct backup folder.
   - Cargo project, config, CLI subcommands, sqlx migrations, base layout (Dutch), Dutch formatting helpers
   - Auth (argon2 + TOTP), `create-user` CLI, sessions, audit log
   - Companies CRUD + company switcher
   - Generic CSV export (per table + zip), `backup` and `restore` commands
   - `deploy/`: systemd service, backup timer, example config, LXC + Tailscale install notes
2. **Clients, tasks, tariffs, hours.** Gate: real hours are logged for a full week.
3. **Kilometerregistratie.** Gate: one month of trips; year overview matches the odometer.
4. **Invoices + payments.** Gate: first real invoice sent and its payment registered.
5. **Document store.** Gate: one quarter of purchase invoices uploaded and findable.
6. **Hardening and extras.** Restore test, dashboard, quotes, recurring invoices, BTW summary.

## 9. Open questions (settled before phase 4)

- Which companies, legal forms (eenmanszaak / BV), BTW-plichtig or KOR?
- Invoice numbering: one series per company; preferred format?
- Any clients with reverse charge (btw verlegd)?
- Car: private car used for business, or company car (full rittenregistratie to prove < 500 private km)?
- Who does bookkeeping/aangifte (accountant, Moneybird, e-Boekhouden)? Decides the export format.
- Offsite backup target, and who holds the restic key besides the beheerder?
- Import existing data (clients, hours, open invoices) at the start?
