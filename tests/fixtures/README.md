# Test fixtures

`schema-v10.db` is a SQLite database at schema version 10 with **fake** data
only: the `kwartje seed` data plus, made through the app itself, an issued
invoice `VA-2026-0001` with its stored PDF document and a payment, an issued
quote converted into a draft invoice, and a draft from a recurring invoice.
Document files are not included (the test checks the database only).

`tests/migrations.rs` copies it, runs every migration after version 10 and
checks that no table loses rows, foreign keys and integrity are clean, the
issued invoice is intact, and today's code can export and list everything.

Never edit the file. When a later schema version deserves a fixture of its own
(e.g. a migration that rewrites a table this one has little data in), add a
new `schema-vNN.db` built the same way with the binary of that version, and a
test next to the existing one.
