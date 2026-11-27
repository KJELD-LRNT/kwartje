#!/bin/sh
# Restore kwartje from a backup folder:
#   sudo /usr/local/lib/kwartje/restore.sh /var/backups/kwartje/2026-11-25
# Stops the service, restores (current data is moved to
# /var/lib/kwartje/pre-restore-<ts>/, never deleted) and starts it again.
# If the restore fails, the service stays stopped so you can inspect.
set -eu
dir="${1:?gebruik: restore.sh <backupmap>}"
systemctl stop kwartje.service
if runuser -u kwartje -- /usr/local/bin/kwartje --config /etc/kwartje/config.toml restore "$dir"; then
  systemctl start kwartje.service
  echo "kwartje draait weer."
else
  echo "Herstel mislukt; kwartje blijft gestopt. Eerdere gegevens staan in /var/lib/kwartje/pre-restore-*." >&2
  exit 1
fi
