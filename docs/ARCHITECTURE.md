# Kwartje: technical design

How Kwartje is built: the parts, how a request flows through them, where the
data lives and which rules protect it. For *what* the app does see
[`PLAN.md`](PLAN.md) and the [README](../README.md); for installing it see
[`deploy/INSTALL.md`](../deploy/INSTALL.md). Known weak spots and planned
improvements are in [`REVIEW.md`](REVIEW.md).

State of this document: after phase 6, the contract document kind and the
review fixes (schema version 13).

## 1. Overview

```
 browser (desktop / phone)
        │ HTTPS (tailnet)
        ▼
 tailscale serve ──► 127.0.0.1:8080  kwartje serve  (one Rust binary)
                                      │
                ┌─────────────────────┼──────────────────────┐
                ▼                     ▼                      ▼
        /var/lib/kwartje/app.db   /var/lib/kwartje/      typst CLI (PDF)
        SQLite, WAL               documents/…            SMTP server (optional)
                │
   systemd timers: backup 02:00 ─► /var/backups/kwartje/YYYY-MM-DD/
                   restic 03:00 ─► offsite (encrypted)
                   restore-test (quarterly), failure ─► ntfy / Uptime Kuma
```

- **One process, one binary.** `kwartje` contains the web server, the CLI, the
  templates, the CSS/JS/font and the SQL migrations (all embedded at compile
  time). The only external programs are Typst (PDF rendering) and, when
  configured, an SMTP server.
- **Two users, several companies.** Users are global; almost all business data
  belongs to exactly one company (`company_id`). Vehicles are shared between
  companies on purpose (one car drives for several BVs).
- **No JavaScript build.** Pages are rendered on the server (Askama). htmx adds
  partial updates (tariff suggestion, project list, km proposal); `app.js` is a
  few small enhancements. Everything works without JavaScript except those
  conveniences.

## 2. Stack

| Concern | Choice | Notes |
|---|---|---|
| Language | Rust 2021, MSRV 1.89 | `std::fs::File::try_lock` for the serve lock |
| HTTP | Axum 0.8 on Tokio | `tower-http` trace, body limits |
| Database | SQLite (WAL, `synchronous=NORMAL`, foreign keys on) via sqlx 0.9, bundled SQLite 3.54 | 168 compile-time checked queries; dynamic SQL must be wrapped in `AssertSqlSafe` (`query!` macros, offline data in `.sqlx/`) |
| Templates | Askama 0.14 + askama_web | Compile-time checked; auto-escaping |
| Front end | Pico.css 2.1.1 + own `app.css`, htmx 2.0.11, Manrope font | All vendored in `static/`, served from the binary |
| Sessions | tower-sessions 0.15 + sqlx store 0.16, SQLite, signed cookie | 12 h idle, 7 days at most |
| Passwords / 2FA | argon2 (default params), totp-rs 5 | TOTP mandatory |
| PDF | Typst CLI 0.13 | Template `templates/invoice.typ`, data as JSON |
| Mail | lettre 0.11 (rustls) | Optional |
| Export | csv, zip, sha2 | Every table, with a manifest |

## 3. Source layout

```
src/
  main.rs, cli.rs      CLI (clap): serve, migrate, create-user, reset-user, export,
                       backup, restore, restore-test, verify-documents, recurring, seed
  config.rs            config.toml (listen, data_dir, backup_dir, session_secret,
                       cookie_secure, backup retention, typst_path, [smtp])
  db.rs                pool, migrations (sqlx::migrate! embeds migrations/), now()
  audit.rs             audit log writer (insert / update diff / archive / delete)
  web/                 router, middleware and shared web plumbing
    mod.rs             AppState, Services, app()/app_with(), serve()
    security.rs        security headers, CSRF middleware
    layout.rs          Layout (per-page context), AuthUser/Page/AnonPage/ClientIp extractors
    error.rs           AppError → Dutch error page / redirect
    flash.rs, form.rs  flash messages, form error helpers
    filters.rs         Askama filters (amount, date, duration, vat, …)
    assets.rs          embedded static files
    health.rs          GET /gezond
  auth/                users, argon2, TOTP, rate limiter, login routes
  modules/             one module per feature: routes + queries + page structs
    dashboard, companies, clients, projects, tasks, tariffs, hours, unbilled,
    vehicles, routes (favoriete ritten), mileage_rates, hours/ and trips/ (km)
    (each model.rs + form.rs + pages.rs), invoices/,
    quotes, recurring, documents, btw, audit_view, export_view
  — pure logic, no I/O, unit-tested —
  money.rs             cents, basis points, half-up rounding, per-rate totals
  format.rs            Dutch dates/amounts/durations, parsing, Amsterdam time
  validate.rs          KvK, btw-id, IBAN (mod 97), postcode, kenteken, e-mail
  tariff.rs            tariff resolution and price versions
  mileage.rs           odometer continuity, year balance
  invoice.rs           invoice line building, rounding, numbering, status
  recurring.rs         periods, month-end clamping, {periode} labels
  btw.rs               BTW boxes and whole-euro rounding
  documents.rs         file type sniffing, safe names, storage paths, retention
  — infrastructure —
  pdf.rs               PdfRenderer trait; TypstCli; FakePdf for tests
  mail.rs              Mailer (Disabled / Smtp / Memory for tests)
  export.rs            table registry → CSV, zip, manifest
  backup.rs            backup, prune, verify, restore, ServeLock
  restore_test.rs      restore a backup into a temp dir and check it
  seed.rs              fake development data
migrations/            0001…0011 SQL migrations (append-only)
templates/             Askama HTML (Dutch) + invoice.typ
static/                pico, htmx, app.css, app.js, manrope.woff2, favicon
tests/                 integration tests per area (in-memory SQLite)
deploy/                systemd units/timers, scripts, example configs, INSTALL.md
```

The rule of thumb: **pure rules live in top-level modules** (`money`,
`invoice`, `tariff`, …) and are unit-tested without a database; **feature
modules** under `modules/` do I/O (queries, pages) and call into them.

## 4. Request pipeline

Layers, outermost first (as built in `web::app_with`):

1. `TraceLayer` – request logging.
2. `security::headers` – sets CSP (`default-src 'self'`, no inline script,
   `frame-ancestors 'none'`), `X-Frame-Options: DENY`, `nosniff`,
   `Referrer-Policy`, `COOP`, `Permissions-Policy`, and `Cache-Control:
   no-store` unless a handler set its own.
3. `DefaultBodyLimit` 256 KB (upload routes raise it to 20 MB + margin).
4. Session layer – signed cookie `kwartje_sid` (HttpOnly, SameSite=Lax,
   Secure in production), data in the SQLite `tower_sessions` table.
5. `security::csrf` – for every non-GET: rejects `Sec-Fetch-Site: cross-site`
   and a foreign `Origin` (compared with `Host` or `X-Forwarded-Host`), then
   requires the session token as form field `csrf_token`, header
   `X-CSRF-Token` (htmx, set on `<body hx-headers>`) or `?csrf_token=`
   (multipart uploads). Constant-time comparison.
6. Router → handler.

Extractors do the access control:

- `AuthUser` – session has a user id, the login is at most 7 days old, the
  user exists, is active and has TOTP. Otherwise the session is flushed and
  the user is sent to `/inloggen`.
- `Page` – `AuthUser` + the `Layout` (CSRF token, flashes, company switcher).
- `CurrentCompany` – the company selected in the session; without one, a
  redirect to `/bedrijven/nieuw`.
- `ClientIp` – the peer address, or, when the peer is loopback or listed in
  `trusted_proxies`, the right-most `X-Forwarded-For` entry that isn't a
  trusted proxy (`web::layout::client_ip`).

Handlers return `AppResult<T>`. `AppError` maps to: `NotFound` (404 page, also
for another company's ids), `BadRequest(msg)` (Dutch message), `Forbidden`,
`LoginRequired` (redirect), `NoCompany` (redirect), `Internal` (logged, generic
page). Form validation errors re-render the form with per-field messages;
successful POSTs redirect (POST/redirect/GET) with a flash message.

Public routes: `/inloggen`, `/2fa`, `/2fa/instellen` (only with a pending
password step), `/static/*`, `/gezond`. Everything else requires login.

## 5. Authentication

- **Password** (argon2id, min 12 chars) → session gets a *pending* user (5 min).
- **TOTP**: first login shows a QR code to enrol; afterwards a 6-digit code.
  The last accepted time step is stored (`totp_last_step`), so a code can't be
  replayed.
- The session id is cycled at each step (no fixation).
- **Rate limiting** (in memory): 5 failures per 15 minutes, counted per account
  and per client IP, for both steps.
- `kwartje reset-user --password / --totp` is the recovery path (CLI only).

## 6. Data model

Fifteen business tables plus the session table. All money is `INTEGER` cents,
VAT rates are `INTEGER` basis points (2100 = 21%), dates are ISO `TEXT`
(`YYYY-MM-DD`), timestamps UTC RFC 3339 `TEXT`.

| Area | Tables | Key points |
|---|---|---|
| Accounts | `users`, `audit_log`, `tower_sessions` | audit log is append-only (triggers); users are never deleted |
| Company | `companies` | invoice prefix, payment term, rounding, logo |
| Relations | `clients`, `projects` | project belongs to client and company |
| Work | `tasks`, `tariffs`, `time_entries` | tariff price versions via `lineage_id`; entry stores `price_cents` + `vat_bp` snapshot |
| Km | `vehicles` (global), `mileage_rates`, `saved_routes`, `mileage_entries` | odometer readings per trip |
| Sales | `invoices`, `invoice_lines`, `invoice_sequences`, `payments`, `reminders` | entries link to `invoice_line_id`; credit notes are invoices with `kind = 'credit'` |
| Quotes | `quotes`, `quote_lines`, `quote_sequences` | own number series |
| Recurring | `recurring_invoices`, `recurring_lines` | generated drafts carry `invoices.recurring_id` |
| Documents | `documents` | file on disk + SHA-256; kinds sales invoice, quote (both generated, read-only), purchase invoice, receipt, contract, other |

**Multi-company isolation** is enforced twice:
1. every business query filters on `company_id` (taken from the session, never
   from the request), and fetching another company's id returns 404;
2. the schema has `UNIQUE (id, company_id)` on parent tables and *composite*
   foreign keys `(x_id, company_id)` on children, so a row of company A can't
   reference a row of company B even through a bug.

**Lifecycle.** Business rows are archived (`archived_at`), not deleted; since
migration 0012 the database refuses `DELETE` on the business tables. The only
hard deletes are drafts: draft invoices and quotes with their lines, and lines
of recurring templates. Each is written to the audit log first.

## 7. Business rules and where they are enforced

| Rule | Enforced by |
|---|---|
| Money is integer cents; half-up rounding per line, then sum | `money.rs` (unit-tested); no floats in money paths |
| Tariff resolution task → project → client → company default → none | `tariff.rs` |
| No tariff = not billable; € 0 tariff = billable at € 0 | `tariff.rs`, `link_entry` (only entries with a tariff), DB CHECK on billable entries needing a client |
| Price/VAT snapshot on every entry | stored on `time_entries` / `mileage_entries` at save |
| Hours rounded per entry to the company setting, then summed | `invoice.rs` |
| Invoice numbers `PREFIX-YYYY-NNNN`, gapless, never reused | `invoice_sequences` in the issuing transaction; `UNIQUE (company_id, number)`; trigger forbids changing a number |
| Issued invoices/quotes immutable | DB triggers on `invoices`, `invoice_lines`, `quotes`, `quote_lines` (only send/payment metadata may change) |
| Corrections via credit notes | `invoices/credit.rs`; can release the linked hours/km |
| Invoiced entries can't change | application code (`hours`, `trips`) and the `*_invoiced_locked` triggers (only releasing them is allowed) |
| Business rows are never deleted | `*_no_delete` triggers (0012); drafts excepted |
| Stored files never change | `documents_file_immutable` trigger: path (after upload), checksum, size, type, company |
| Odometer continuity per vehicle across companies | `mileage.rs` + checks on save |
| Audit log on insert/update/archive/delete | `audit::record_*` in the same transaction as the write |
| Everything exportable | `export::TABLES` registry; a test fails when a table is missing |

## 8. Invoices, quotes and PDFs

1. **Draft** – created from *Te factureren* (hours and km grouped per task,
   tariff, project or entry) or from a quote or recurring template. Draft lines
   are editable; manual lines can be added.
2. **Issue** ("Definitief maken"), one transaction:
   - check required company/client data;
   - take the next number from `invoice_sequences`;
   - freeze company and client data as JSON snapshots;
   - the PDF (Typst) rendered from those snapshots, see below;
   - store it as a document (`sales_invoice`, SHA-256) and link it.
   If rendering fails, the transaction rolls back and no number is used.
3. **After issuing** only metadata changes: sent, payments, reminders, credit
   notes. The stored PDF is what was sent; it is never re-rendered.

`templates/invoice.typ` reads `data.json` (company, client, lines, VAT groups,
`meta` rows, `closing` text, optional hours specification). Quotes use the same
template with other meta rows, and are issued the same way: since schema 13
"Definitief maken" stores the PDF as a document of kind `quote`
(`quotes.pdf_document_id`); quotes issued earlier are rendered on request from
their snapshots. Typst runs in a temporary directory with `--root` set to it, a
60-second timeout, and only our own JSON and logo as inputs. It runs on a
blocking thread (`pdf::render_blocking`). Issuing renders *before* taking
SQLite's write lock: the PDF is made for the number that would come next, then
one `BEGIN IMMEDIATE` transaction reserves the number and rebuilds the data;
only if it is identical is the PDF stored, otherwise it renders again.

## 9. Documents

Uploads (PDF/JPG/PNG, max 20 MB) are sniffed by content, not by name. The file
is written to `documents/<company>/<year>/<kind>/<id>_<safe name>` via a
temporary file + rename, with its SHA-256 in the row. The same file twice
(same hash, same company) is refused. Files are never deleted or overwritten;
`verify-documents`, `backup` and `restore-test` check every hash. Previews are
served with their own CSP and `frame-ancestors 'self'`.

## 10. Background work

`serve` starts two background tasks:
- **Recurring invoices** – at startup and hourly, `recurring::generate_due`
  creates one draft per due period (catch-up capped at 36), advancing
  `next_date` in the same transaction, so a run is idempotent.
- **Session cleanup** – hourly removal of expired sessions.

Backups, offsite copies and the restore test are separate systemd units, not
part of the server process.

## 11. Backup, restore and monitoring

- `kwartje backup` → `backup_dir/YYYY-MM-DD/`: `app.db` via `VACUUM INTO`
  (consistent while serving), a full CSV export made from that copy, the
  documents (hard-linked to the previous backup when unchanged) and
  `manifest.json` with checksums, row counts and the schema version. Retention
  30 daily + 12 monthly.
- `kwartje restore <dir>` refuses while `serve` holds the lock, verifies the
  backup, moves the current data aside (nothing deleted) and copies it in.
- `kwartje restore-test` restores into a temporary folder, migrates, compares
  row counts with the manifest, verifies documents and boots the app.
- `GET /gezond` returns 503 if the database fails or the last backup is older
  than 36 hours (for Uptime Kuma).

## 12. Schema changes

- Migrations in `migrations/` are append-only; sqlx stores a checksum per
  applied migration. Editing an applied migration, or starting an older binary
  on a newer database, makes the app refuse to start.
- `serve` and `migrate` apply pending migrations at startup, each in a
  transaction; a failure rolls back and the app does not start.
- Before applying anything to an existing database, they write a copy to
  `data_dir/pre-migration/app-<time>-v<old>-naar-v<new>.db` (newest three
  kept; `db::migrate_with_snapshot`).
- `tests/migrations.rs` runs every migration against
  `tests/fixtures/schema-v10.db`, a database with fake data made through the
  app, and checks that nothing is lost. INSTALL's update steps run the new
  binary's `restore-test` against the latest backup before installing it.
- Snapshot JSON stored on issued invoices and quotes must stay readable:
  `CompanySnapshot` / `ClientSnapshot` use `serde(default)`, and a test pins
  today's JSON.
- SQLite cannot alter constraints, so widening a CHECK means rebuilding the
  table (see `0011_document_contracts.sql` for the pattern that keeps foreign
  keys valid inside sqlx's transaction).
- After changing queries or migrations: `scripts/sqlx-prepare.sh` regenerates
  `.sqlx/` so the build works offline.

## 13. Testing

215 tests, against in-memory SQLite with the real migrations (plus the
fixture database for the migration test):
- **unit tests** for money/VAT/rounding, tariff resolution, numbering, date
  logic, validation, document helpers;
- **database tests** for triggers and cross-company foreign keys (`db.rs`);
- **integration tests** per area in `tests/` driving the full router
  (`tests/common` logs in with TOTP, handles CSRF and multipart), with
  `FakePdf` and an in-memory mailer; one test per area uses real Typst when it
  is installed.

`cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` must be clean
before each commit. On pull requests, `.github/workflows/ci.yml` runs them
plus `cargo audit` (no Typst there; the real-PDF tests skip themselves).

## 14. Deployment

Debian 12 LXC, user `kwartje`, binary in `/usr/local/bin`, config in
`/etc/kwartje/config.toml` (0640, root:kwartje), data in `/var/lib/kwartje`.
The systemd unit runs with `NoNewPrivileges`, `ProtectSystem=strict`,
`ProtectHome`, `PrivateTmp` and only `/var/lib/kwartje` writable. The app binds
to loopback (it refuses `0.0.0.0`); `tailscale serve` provides HTTPS on the
tailnet. Behind a reverse proxy with an SSO gate it binds to the container's
own address and lists the proxy in `trusted_proxies` (INSTALL section 9).
Details and the update procedure: [`deploy/INSTALL.md`](../deploy/INSTALL.md).

## 15. Conventions

- UI text Dutch; code, identifiers and tables English.
- Use `format.rs` for every date/amount/duration shown or parsed.
- Every business query takes `company_id`; every write is audited in the same
  transaction.
- New table ⇒ add it to `export::TABLES` (and so to backups) in the same change.
- Pure rules in top-level modules with unit tests; handlers stay thin.
