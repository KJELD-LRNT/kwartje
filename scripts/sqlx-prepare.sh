#!/usr/bin/env bash
# Regenerate .sqlx/ (offline query metadata) from a scratch database built
# from migrations/. Needs the CLI matching the sqlx version in Cargo.toml:
#   cargo install sqlx-cli --version 0.9.1 --no-default-features --features sqlite
set -euo pipefail
cd "$(dirname "$0")/.."
db="target/sqlx-prepare.db"
mkdir -p target
rm -f "$db" "$db-wal" "$db-shm"
export DATABASE_URL="sqlite:$db"
export SQLX_OFFLINE=false
sqlx database create
sqlx migrate run
cargo sqlx prepare -- --all-targets
