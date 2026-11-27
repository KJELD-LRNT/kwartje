#!/bin/sh
# Push backup_dir offsite with restic (encrypted). Run by kwartje-restic.service.
# Fails (non-zero) when today's backup is missing, so a broken nightly backup
# is noticed even if the backup unit itself did not report it.
set -eu
BACKUP_DIR="${BACKUP_DIR:-/var/backups/kwartje}"
today="$(date +%F)"

if [ ! -f "$BACKUP_DIR/$today/manifest.json" ]; then
  echo "Geen backup van vandaag ($today) in $BACKUP_DIR" >&2
  exit 1
fi

restic backup --no-scan --tag kwartje --host kwartje "$BACKUP_DIR" \
  --exclude "$BACKUP_DIR/.tmp-*" --exclude "$BACKUP_DIR/.old-*" --exclude "$BACKUP_DIR/.lock"

restic forget --tag kwartje --host kwartje \
  --keep-daily 30 --keep-monthly 12 --keep-yearly 7 --prune

# Verify a sample of the repository once a week (Sunday).
if [ "$(date +%u)" = "7" ]; then
  restic check --read-data-subset=5%
fi
