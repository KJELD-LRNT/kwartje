# Developing Kwartje

Everything you need to run Kwartje from source, run the tests and find your way
around the code. For how it is built and why, see [ARCHITECTURE.md](ARCHITECTURE.md);
for installing it on a server, see [deploy/INSTALL.md](../deploy/INSTALL.md).

## Running locally

Requirements: Rust stable ≥ 1.89 (`rustup update`). Nothing else is needed: SQLite is
bundled, and htmx and Pico.css are vendored in `static/` and embedded in the binary.

```sh
cp deploy/config.example.toml config.toml
# edit config.toml for local use:
#   data_dir      = "data"
#   backup_dir    = "backups"
#   cookie_secure = false            # plain http on localhost
#   session_secret = "<output of: openssl rand -base64 64 | tr -d '\n'>"

cargo run -- seed                    # 3 fake users + 2 fake companies
cargo run -- serve                   # http://127.0.0.1:8080
```

Log in as `anna@example.test` (beheerder), `bram@example.test` (medewerker)
or `boekhouder@example.test` (read-only, first company) with the password
`ontwikkelen-123`. The seed also creates clients, tariffs (€ 95 default, € 0
Kennismaking, a € 110 client tariff, km) and tasks, plus this week's and last
week's hours for the medewerker at the first company, a test car (TS-123-K),
two saved routes, last month's trips and this quarter's purchase documents. The first login asks you to scan a QR code with an authenticator
app; TOTP is mandatory.

Other commands (`cargo run -- --help`):

| Command | What it does |
| --- | --- |
| `serve` | Run migrations, start the web server |
| `migrate` | Run migrations only |
| `create-user --name N --email E [--role R] [--bedrijf ID…]` | Create an account (asks for the password, min. 12 characters). Role `beheerder`, `medewerker` (default) or `boekhouder` (with `--bedrijf`) |
| `set-role --email E --role R [--bedrijf ID…]` | Change a role; the last beheerder can't be demoted |
| `reset-user --email E [--password] [--totp]` | Recovery: new password and/or reset 2FA (both also forget all trusted devices) |
| `export --out DIR [--company ID] [--from D] [--to D]` | All tables as CSV + `manifest.json` |
| `backup` | `backup_dir/YYYY-MM-DD/{app.db, csv/, documents/, manifest.json}`, then prune to 30 daily + 12 monthly |
| `restore DIR [--yes]` | Verify and restore a backup folder (refuses while `serve` runs) |
| `verify-documents` | Check that every document file exists and matches its SHA-256 |
| `seed [--force]` | Fake dev data (refuses on a non-empty database) |

## Development

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- **Queries** use sqlx's compile-time checked macros. The offline metadata lives in `.sqlx/`
  (`SQLX_OFFLINE=true` is set in `.cargo/config.toml`). After changing a query or a
  migration, run `scripts/sqlx-prepare.sh` (needs
  `cargo install sqlx-cli --version 0.9.1 --no-default-features --features sqlite`) and commit `.sqlx/`.
- **New tables** go in `src/export.rs` (`TABLES`). A test fails when a table or column
  is neither exported nor explicitly excluded. Backups pick them up automatically.
- **Tests:**
  - unit tests are inline, covering money/VAT rounding, formatting, validation, TOTP,
    the rate limiter and backup pruning
  - integration tests in `tests/` run the real router against an in-memory SQLite DB
    (`auth.rs`, `roles.rs`, `trips.rs`, …) or a temp dir (`backup.rs`)
  - `tests/migrations.rs` migrates a filled schema-v10 database (`tests/fixtures/`) to the latest version

## Layout

```
migrations/         sqlx migrations
templates/          Askama templates (Dutch)
static/             vendored pico.min.css, htmx.min.js + app.css/app.js
src/main.rs, cli.rs CLI subcommands
src/config.rs       config.toml
src/db.rs           pool, migrations
src/audit.rs        audit log (same transaction as the write)
src/auth/           argon2 passwords, TOTP, trusted devices, rate limit, login routes, users
src/web/            router, CSRF + security headers, layout, filters, errors
src/modules/        dashboard, companies, clients, projects, tasks, tariffs,
                    hours (week/year), unbilled (te factureren), vehicles,
                    routes, mileage_rates, trips (km), invoices/, documents,
                    btw, quotes (offertes), recurring (terugkerend), logboek, export,
                    account, users_admin (gebruikers), security_view (beveiliging)
src/tariff.rs       tariff resolution (task → project → client → company → none)
src/mileage.rs      odometer continuity and the km year balance
src/invoice.rs      invoice lines, rounding, numbering, status
src/btw.rs          BTW boxes per rate and whole-euro rounding
src/recurring.rs    periods, month-end clamping, {periode} labels
src/restore_test.rs restore a backup into a temp folder and check it
src/documents.rs    document type sniffing, safe names, paths, integrity check
src/pdf.rs          Typst renderer (templates/invoice.typ)
src/mail.rs         SMTP mailer (lettre)
src/export.rs       CSV registry, zip, manifest
src/security_log.rs sign-ins and other security events
src/backup.rs       backup, prune, verify, restore
src/format.rs       Dutch dates/amounts/durations (always use these)
src/money.rs        i64 cents, basis points, half-up rounding
src/validate.rs     KvK, btw-id, IBAN, postcode, e-mail
deploy/             systemd units + timers, scripts, example configs, INSTALL.md
```
