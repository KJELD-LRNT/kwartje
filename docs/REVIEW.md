# Kwartje: code review and improvement list

Review of the whole codebase after phase 6 and the contract document kind
(schema version 11, 196 tests). How the app is built is described in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Status

**All P1 items are done** (26-11-2026, schema version 12, 206 tests):

| Item | Done in |
|---|---|
| A1 snapshot before migrations | `db::migrate_with_snapshot` → `data_dir/pre-migration/` |
| A2 no deletes, invoiced entries locked, files immutable | migration `0012_protect_business_data.sql` + tests in `db.rs` |
| A3 snapshot JSON stays readable | `serde(default)` + pinned-JSON test in `invoices/issue.rs` |
| A4 migrations tested on a filled database | `tests/migrations.rs` + `tests/fixtures/schema-v10.db`; dry-run step in INSTALL §6 |
| B1 `trusted_proxies` | `config.rs`, `web::layout::client_ip` (+ unit tests) |
| B2 SameSite=Lax | `web/mod.rs` (+ cookie test) |
| B3 7-day session lifetime | `auth::login_expired`, checked in `AuthUser` |
| F `zip` 2.6 → 9.8 | 7 advisories cleared; `cargo audit` now reports only the SQLite engine (P1 → moved to the sqlx 0.9 upgrade, P2) and two items not in the binary |

**P2: all done** (210 tests):

| Item | Done in |
|---|---|
| F sqlx 0.8 → 0.9 (bundled SQLite 3.46 → 3.54) with the session store 0.15 → 0.16 | `cargo audit`: 0 vulnerabilities (only `basic-toml` unmaintained, compile time). The RSA advisory went with the old sqlx. |
| C1 Typst off the async workers | `pdf::render_blocking` (invoices and quotes) |
| C2 no write lock while rendering | `invoices::issue`: render first, then reserve the number in `BEGIN IMMEDIATE` and store only if the PDF data is unchanged (retry ≤ 3); tested |
| C4 Typst stderr can't stall | written to a file; error reporting tested with real Typst |
| D1 + B4 CI with `cargo audit` | `.github/workflows/ci.yml`, on pull requests only (by choice, no schedule) |
| D2 one manual-line parser | `invoice::ManualLineForm` + `invoice::unit` |

**P3: done** (215 tests, schema version 13):

| Item | Done in |
|---|---|
| A5 quote PDFs stored at issuing | migration 0013 (kind `quote`, `quotes.pdf_document_id`), `quotes::issue_quote` |
| B7 `/offertes/nieuw` requires login | `quotes::new_page` |
| B8 CSV formula protection | `export::CsvMode` / `spreadsheet_text`: downloads only, backups and `kwartje export` stay exact (by choice) |
| B9 HSTS | added when `cookie_secure` is on |
| D3 split large modules | `hours/` and `trips/` → `model.rs`, `form.rs`, `pages.rs` |
| D4 per-row queries | quote and recurring lists: one query + client names |
| D5 swallowed serialisation | audit payloads `expect("row serialises")` |
| E CSRF edge-case tests | `tests/auth.rs::csrf_edge_cases` |

Not changed, by choice or because nothing is needed: B5 (TOTP enrolment stays in the browser), B6 (in-memory rate limiter is fine for now), C3 (covered by `BEGIN IMMEDIATE` since C2), C5 (mail sent but not recorded: rare and visible).

Also done: INSTALL §9 "Behind a reverse proxy with SSO", and the app refuses
`listen = "0.0.0.0:…"`. Open: nothing from this review.

The tables below are the original findings, kept as written.

**Scope:** all of `src/` (about 22,000 lines), the 11 migrations, the
templates, the deploy files and the tests.

**Method:**
- a read-through of the risky paths: auth, CSRF, issuing invoices,
  numbering, migrations, backup, file handling and the PDF renderer
- a probe of every route on a running instance without a session
- a scan for panics and for dynamic SQL
- a check of the dependencies against the RustSec advisory database

**Overall:** the codebase is in good shape. The important rules are mostly
enforced by the database and not only by the code:
- money is kept as whole cents, never as decimals
- each company's data is isolated by composite foreign keys
- issued invoices and quotes can't be changed
- the audit log can only be added to
- every query is checked against the schema at compile time

There are no blocking problems. The items below make it more robust for the
long term and prepare it for a reverse proxy with SSO.

Priorities: **P1** do soon · **P2** worthwhile · **P3** nice to have.

## A. Data safety during future updates

| # | P | Finding | Proposal |
|---|---|---|---|
| A1 | P1 | No automatic backup right before a migration. `serve` migrates at startup; the pre-update backup is a manual step in INSTALL. | Before applying pending migrations, write `VACUUM INTO backup_dir/pre-migration-<version>-<timestamp>.db`. |
| A2 | P1 | "Never hard-delete" and "invoiced entries are locked" are enforced only by the code. A bug in a later change could delete or edit hours, km, payments, documents or clients. | Triggers that refuse `DELETE` on business tables (drafts excepted), refuse edits of `time_entries` / `mileage_entries` while `invoice_line_id` is set (except clearing it, as credit notes do), and refuse changes to `documents.file_path` / `sha256`. Add tests in `db.rs` like the existing trigger tests. |
| A3 | P1 | The company/client snapshot structs (`CompanySnapshot`, `ClientSnapshot`) have no serde defaults. Adding a field later breaks reading the snapshots of existing invoices and quotes (send, reminders, quote PDF), with a misleading error. | Put `#[serde(default)]` on the structs and add a test that parses a fixed snapshot JSON from today's version. |
| A4 | P1 | Migrations are only tested on an empty database. The first versions of 0011 failed only on real data. | (1) In INSTALL's update steps, run the **new** binary's `restore-test --latest` first; it migrates a copy of the latest backup. (2) Add a test that fills a database with the seed at the previous schema version, migrates it and checks row counts and `foreign_key_check`. |
| A5 | P3 | Quote PDFs are rendered on request, so a later template change alters how old quotes look (the content is frozen). | Accept, or store the PDF at issuing like invoices. |

## B. Security

| # | P | Finding | Proposal |
|---|---|---|---|
| B1 | P1 (with proxy) | `X-Forwarded-For` is trusted only from loopback. Behind a proxy on another host every request has the proxy's IP, so 5 failed logins by anyone lock the login for everyone for 15 minutes. | A `trusted_proxies = ["10.0.0.5"]` setting. Take the right-most address that isn't a trusted proxy. |
| B2 | P1 (with SSO) | The session cookie is `SameSite=Strict`. After an SSO redirect, the first request back is cross-site, so the cookie isn't sent and the app shows its login page despite a valid session. | Switch to `Lax`. The CSRF token, Origin and Sec-Fetch-Site checks protect all POSTs regardless. |
| B3 | P2 | Sessions only expire after 12 h of inactivity. A session used daily lives forever. | An absolute lifetime too (e.g. 7 days): store the login time in the session and check it in `AuthUser`. |
| B4 | P2 | No dependency audit in the workflow (see the audit result below). | Run `cargo audit` in CI and before each release. |
| B5 | P3 | TOTP is enrolled at the first login, so whoever knows the password first enrols. This applies after `create-user` and `reset-user --totp`. | Acceptable for two users; optionally `create-user --totp` prints the QR code in the terminal. |
| B6 | P3 | The rate limiter is in memory and resets when the app restarts. | Fine for now; with internet exposure, add fail2ban or the proxy's own rate limit. |
| B7 | P3 | `GET /offertes/nieuw` redirects without a login check and copies `?klant=` into the redirect. It stays inside the app, so this is harmless. | Use the `Page` extractor and URL-encode the parameter. |
| B8 | P3 | CSV exports don't neutralise cell values starting with `=`, `+`, `-` or `@`. A client name like `=HYPERLINK(…)` becomes a formula in Excel. All data comes from the two users themselves. | Prefix such values with `'` in the CSV writer. Not in the backups' CSV, which must stay exact, or behind an option. |
| B9 | P3 | No HSTS header. | Let the reverse proxy set it, or set it when `cookie_secure = true`. |

Checked and found in order:
- **Passwords and 2FA:** argon2id; TOTP with replay protection; the session ID is renewed at login; a session ends as soon as its user is archived or has 2FA reset.
- **Forms:** the CSRF token check is constant-time, plus the Origin and Sec-Fetch-Site checks.
- **Browser hardening:** a strict CSP with no inline scripts, and `frame-ancestors 'none'`.
- **Uploads:** checked by content, size-limited and stored under generated names; previews get their own CSP.
- **SQL:** dynamic SQL only with table names from the static export registry.
- **Redirects:** local paths only.
- **Login required:** all 93 routes require a login except the login pages, `/static`, `/gezond` and B7.
- **PDF rendering:** Typst runs in a temporary directory with `--root`, and only receives JSON and the logo.
- **Server:** the systemd unit is sandboxed; the app listens on loopback only; no secrets in git.

### Reverse proxy with an SSO gate

This is supported once B1 and B2 are done. Keep the app's own login plus
TOTP behind the SSO as a second layer. Proxy requirements:
- a (sub)domain of its own: the app lives at `/`
- pass `Host` or `X-Forwarded-Host`
- allow request bodies of at least 21 MB
- exempt `/gezond` from SSO, or monitor it over the tailnet
- an SSO session of at least 12 h; htmx requests that get redirected to the
  identity provider fail silently
- only the proxy can reach the app

Replacing the app login with a trusted `Remote-User` header or native OIDC
is possible, but only worth it for single sign-on everywhere, and only with
the app firewalled to the proxy.

## C. Correctness and robustness

| # | P | Finding | Proposal |
|---|---|---|---|
| C1 | P2 | `TypstCli::render` is synchronous: it busy-waits with `thread::sleep` and is called directly from async handlers. Each render blocks a Tokio worker thread, which matters on a 1–2 core LXC. | Wrap it in `tokio::task::spawn_blocking`. |
| C2 | P2 | Issuing an invoice holds SQLite's write lock while Typst renders, because the number is taken first. That is usually under a second. If it takes longer than the 5 s busy timeout, the other user's writes fail with "database is locked". | Keep the number and the PDF atomic, but render outside the transaction: build the data, render, then open the transaction, take the number, render again only if the number changed, and commit. Alternatively, raise `busy_timeout` to 30 s as a cheap first step. |
| C3 | P3 | Number reservation reads `last_number`, then updates it in a deferred transaction. Two simultaneous issues can't produce a duplicate (the UNIQUE constraint and SQLite's locking prevent it), but one of them gets an error page. | `UPDATE … SET last_number = last_number + 1 … RETURNING last_number`, or `BEGIN IMMEDIATE` for issuing. |
| C4 | P3 | Typst's stderr is read only after it exits. Very large warning output (over 64 KB) would fill the pipe and stall it until the 60 s timeout. | Read stderr in a thread, or redirect it to a temp file. |
| C5 | P3 | Sending mail and then recording "sent" aren't atomic. If the database write fails after a successful send, the invoice shows as unsent. | Accept: it is rare and visible. Optionally log the message id. |

## D. Maintainability

| # | P | Finding | Proposal |
|---|---|---|---|
| D1 | P2 | No CI. fmt, clippy and tests run only locally. | A GitHub Actions workflow running `cargo fmt --check`, `clippy -D warnings`, `cargo test` (with `SQLX_OFFLINE=true`) and `cargo audit`, on pushes and pull requests. |
| D2 | P2 | Manual-line handling is duplicated in invoices, quotes and recurring templates: the `LineForm` struct, quantity/price/VAT parsing and unit mapping (`"uur" => "uur"` appears six times). | One `invoice::parse_manual_line(form) -> Result<DraftLine, &str>` and an `Unit` enum with `as_str` / `parse`. |
| D3 | P3 | `hours.rs` (1,200 lines) and `trips.rs` (1,400 lines) mix queries, validation and pages. | Split each into `model.rs` / `form.rs` / `pages.rs` like `invoices/`. |
| D4 | P3 | Some list pages do one query per row (quotes, recurring templates) and the dashboard loads the full quote list to count open ones. Irrelevant at today's volumes. | Join in one query when a list grows past a few hundred rows. |
| D5 | P3 | Some serialisation results are swallowed with `unwrap_or_default()`: snapshots in `issue.rs` and `quotes.rs`, and audit payloads. It can't fail for these types, but a silent empty snapshot would be bad. | Use `expect("serialisable")` or propagate the error. |

## E. Tests

Good coverage of the money/VAT/rounding rules, numbering, triggers, company
isolation and every user flow. Gaps:
- migrations on a filled database (A4);
- the new triggers from A2;
- the snapshot compatibility test from A3;
- the CSRF middleware edge cases (foreign Origin, `X-Forwarded-Host`) as
  explicit tests. They are exercised indirectly today.

## F. Dependency audit

`cargo audit` (RustSec database, 26-11-2026) reports 8 advisories in
`Cargo.lock`. What each one means for Kwartje:

| Crate | Advisories | In the running app? | Assessment | Action |
|---|---|---|---|---|
| `zip` 2.6.1 | 7 (RUSTSEC-2026-0367/0368/0369/0429/0545/0548, up to 9.1 critical) | Yes, but only to **write** export zips | All of them concern *reading or extracting* untrusted archives (symlinks, permissions, AES checks). Kwartje never extracts a zip (one unit test reads its own output). Not exploitable today. | **P1:** upgrade to `zip` 9.x anyway (small API change in the writer), so a future "import" can't inherit them and the audit is clean. |
| `libsqlite3-sys` 0.30.1 (bundled SQLite 3.46.0) | RUSTSEC-2026-0459, medium | **Yes**, it is the database engine | An SQLite engine CVE; the impact depends on the bug, but only the two logged-in users can make the app run queries, and every query is fixed SQL. Real but low exposure. | **P1:** move to `sqlx` 0.9 (bundles a patched SQLite; needs Rust ≥ 1.94; the build image `rust:1-bookworm` in `deploy/build.sh` already tracks the latest stable) together with `tower-sessions-sqlx-store` 0.16. That is an upgrade project of its own: re-run `sqlx-prepare.sh`, the full test suite and a migration test (A4). |
| `rsa` 0.9.10 | RUSTSEC-2023-0071 (Marvin), medium | **No** | Only in `Cargo.lock` through sqlx's MySQL driver, which is never compiled (`cargo tree` shows it in neither the normal nor the build dependencies). | None; it disappears with the sqlx upgrade or can be ignored in `audit.toml` with this reason. |
| `basic-toml` 0.1.10 | unmaintained (RUSTSEC-2026-0374) | **No**, compile time only | Used by Askama's derive macro to read its config at build time. | None; follow Askama releases. |

After A1–A4, B1–B3 and the two upgrades above, `cargo audit` should run in CI
(D1) so new advisories show up without a manual review.
