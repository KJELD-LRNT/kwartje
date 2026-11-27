# Kwartje

Self-hosted business administration app (hours on tasks with tariffs, clients, invoices + payments, kilometerregistratie, document store) for small Dutch businesses (one install serves several companies; roles beheerder, medewerker, boekhouder). Full spec and phases: `docs/PLAN.md`. Read it before starting any phase.

## Stack

- Rust (stable, edition 2021), Axum + Tokio
- SQLite (WAL) via sqlx, migrations in `migrations/` (`sqlx migrate`), compile-time checked queries where practical
- Askama templates + htmx + Pico.css, vendored in `static/` (no CDN, no JS build step)
- argon2 passwords, `tower-sessions`, TOTP 2FA
- `csv` + `serde` for export; Typst CLI for invoice PDFs
- Single binary with subcommands: `serve`, `backup`, `restore`, `export`, `create-user`, `migrate`

## Hard rules

- **UI fully in Dutch**: every user-facing string (pages, buttons, flash and validation messages, emails, PDFs, CSV headers). Code, identifiers, tables and columns in English.
- **Dutch formatting**: dates `dd-mm-jjjj`, amounts `€ 1.234,56`, weeks start Monday. Use the helpers in `src/format.rs`, never ad-hoc formatting.
- **Money is `i64` cents**, VAT rate is `i64` basis points (2100 = 21%). Never `f32`/`f64` for money. Round half-up per line, then sum.
- **Multi-company**: every business query is scoped by `company_id`. Never return another company's data.
- **No hard deletes** of business data: use `archived_at`. Drafts are the only exception.
- **Issued invoices are immutable**; corrections go through credit notes. Invoice numbers are sequential per company and never reused.
- **Tariff snapshot**: a time or mileage entry stores `price_cents` + `vat_bp` when it is saved. Tariff changes never alter history.
- **No tariff = not billable** (never on invoices). **€ 0 tariff = billable at € 0,00.**
- **Audit log** on every insert/update/archive.
- **Everything exportable to CSV**: when you add a table, add it to `export.rs` and the backup manifest in the same change.
- Security: internal app behind Tailscale, but still CSRF protection on forms, secure session cookies, login rate limiting, no secrets in the repo (`config.toml` is git-ignored; commit `deploy/config.example.toml`).

## Workflow

- Work **one phase at a time** from `docs/PLAN.md`. Start each phase in plan mode and list the steps before writing code.
- Small commits with clear messages; keep `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` clean before each commit.
- Tests: unit tests for money/VAT/rounding, tariff resolution and invoice numbering; integration tests for routes using an in-memory SQLite DB.
- Seed command or fixture with fake data for local dev. Never commit real client data.
- If something in the plan is unclear or conflicts, ask instead of guessing.
