# Contributing to Kwartje

Thanks for your interest in Kwartje! This page explains how issues and pull
requests are handled, so you know what to expect.

## Ground rules

- **Kwartje stays small.** It records hours and trips, makes invoices and quotes,
  and hands clean records to an accountant. It is not bookkeeping software. If an
  idea is a big step outside that scope, open an issue to discuss it before you
  write code.
- **The UI is Dutch, the project is English.** User-facing text (pages, buttons,
  messages, e-mails, PDFs, CSV headers) is Dutch. Code, comments, commits,
  issues and documentation are in English. Issues in Dutch are fine too.
- **Never share real data.** Use the demo data (`kwartje seed`) for screenshots,
  logs and tests. No client names, amounts, IBANs or e-mail addresses.
- Be kind. See the [code of conduct](CODE_OF_CONDUCT.md).

## Reporting bugs and suggesting features

Use the forms on the [Issues page](https://github.com/KJELD-LRNT/kwartje/issues/new/choose):

| You have… | Use |
| --- | --- |
| something that doesn't work | **🐞 Bug report** |
| an idea or a missing feature | **💡 Feature request** |
| a question about installing or using Kwartje | **💬 Discussions** |
| a security problem | a **private report**, see [SECURITY.md](SECURITY.md) |

### What happens to an issue

1. **New**: every new issue gets the `needs-triage` label.
2. **Triage**: a maintainer reads it, usually within a week. They may ask
   questions (`needs-info`), mark it as a duplicate, or close it when it is out of
   scope (`wontfix`, with an explanation).
3. **Accepted**: `needs-triage` is removed and the issue keeps `bug` or
   `enhancement`. Small, well-described issues get `good first issue`. Issues
   where help is wanted get `help wanted`.
4. **In progress**: someone says "I'll take this" in a comment, so work isn't
   done twice.
5. **Done**: a pull request with `Closes #<number>` closes the issue when it is
   merged. The fix ships in the next release.

Issues waiting on `needs-info` without an answer for 30 days may be closed. You
can reopen them at any time.

## Pull requests

1. For anything bigger than a small fix, comment on the issue first, or open
   one, so we can agree on the approach.
2. Fork, create a branch, and keep the change focused. One topic per pull
   request.
3. Run the same checks as CI:
   ```sh
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. Fill in the pull request template. CI runs the checks and `cargo audit`.
5. A maintainer reviews. Expect questions; that is normal.

### Rules the code relies on

These are checked in review, and many of them by tests:

- **Money** is `i64` cents and VAT rates are `i64` basis points (2100 = 21%).
  Never use floats. Round half-up per line, then sum.
- **Companies**: every business query is scoped by `company_id`.
- **No hard deletes** of business data: archive with `archived_at`. Drafts are
  the only exception.
- **Issued invoices are immutable.** Corrections go through credit notes. Invoice
  numbers are sequential per company and never reused.
- **Tariff snapshot**: an hour or trip entry stores its price and VAT when it is
  saved.
- **Audit log** on every insert, update and archive.
- **Export**: a new table goes into `src/export.rs` (`TABLES`) in the same
  change. A test fails otherwise.
- **Migrations** are append-only. Never edit a released migration file; add a
  new one.
- **Formatting**: use `src/format.rs` for dates (`dd-mm-jjjj`) and amounts
  (`€ 1.234,56`).
- **Queries** use sqlx's checked macros. After changing a query or a migration,
  run `scripts/sqlx-prepare.sh` and commit `.sqlx/`.

How to run the app and find your way around the code is described in
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## License

By contributing you agree that your contribution is licensed under the
[GNU AGPL v3.0 or later](LICENSE), the license of the project.
