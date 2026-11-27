## What does this change?

<!-- A short description. Link the issue it closes, e.g. "Closes #12". -->

## How was it tested?

<!-- Tests added or changed, and what you checked in the browser. -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] Changed queries or migrations: ran `scripts/sqlx-prepare.sh` and committed `.sqlx/`
- [ ] New migrations only (existing migration files are never edited)
- [ ] New tables are in `src/export.rs` (or explicitly excluded, with a reason)
- [ ] All user-facing text is in Dutch and uses the helpers in `src/format.rs`
- [ ] No real client or personal data in code, tests, fixtures or screenshots
