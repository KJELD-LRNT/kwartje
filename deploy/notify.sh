#!/bin/sh
# Notifications for the backup jobs.
#   notify.sh failure <unit>   -> ntfy message with the last log lines
#   notify.sh success <job>    -> Uptime Kuma push heartbeat (if configured)
# URLs come from /etc/kwartje/notify.env (see notify.env.example).
# Never fails the calling unit: notification problems are only logged.
set -u
[ -r /etc/kwartje/notify.env ] && . /etc/kwartje/notify.env

kind="${1:-}"
name="${2:-onbekend}"
host="$(hostname)"

case "$kind" in
  failure)
    [ -n "${NTFY_URL:-}" ] || { echo "NTFY_URL niet ingesteld" >&2; exit 0; }
    log="$(journalctl -u "$name" -n 20 --no-pager -o cat 2>/dev/null || true)"
    curl -fsS -m 20 \
      -H "Title: kwartje: $name mislukt op $host" \
      -H "Priority: high" \
      -H "Tags: warning" \
      ${NTFY_TOKEN:+-H "Authorization: Bearer $NTFY_TOKEN"} \
      --data-binary "$log" "$NTFY_URL" >/dev/null \
      || echo "ntfy-melding versturen mislukt" >&2
    ;;
  success)
    case "$name" in
      kwartje-backup) url="${KUMA_PUSH_BACKUP:-}" ;;
      kwartje-restic) url="${KUMA_PUSH_RESTIC:-}" ;;
      kwartje-restore-test) url="${KUMA_PUSH_RESTORE_TEST:-}" ;;
      *) url="" ;;
    esac
    [ -n "$url" ] && { curl -fsS -m 20 "$url" >/dev/null || echo "Uptime Kuma-push mislukt" >&2; }
    ;;
  *)
    echo "gebruik: notify.sh failure|success <naam>" >&2
    ;;
esac
exit 0
