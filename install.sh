#!/usr/bin/env bash
# Kwartje installer and updater.
#
#   curl -fsSL https://raw.githubusercontent.com/KJELD-LRNT/kwartje/main/install.sh | sudo bash
#   curl -fsSL …/install.sh | sudo bash -s -- --version v0.1.1
#
# Run it again on an installed machine to update. Options: see --help.
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail

SCRIPT_VERSION="1"
REPO="${KWARTJE_REPO:-KJELD-LRNT/kwartje}"
API="https://api.github.com/repos/${REPO}"
TYPST_VERSION="0.13.1"

BIN=/usr/local/bin/kwartje
LIB=/usr/local/lib/kwartje
DOC=/usr/local/share/doc/kwartje
ETC_DIR=/etc/kwartje
CONFIG=$ETC_DIR/config.toml
DATA=/var/lib/kwartje
BACKUPS=/var/backups/kwartje
UNITS=/etc/systemd/system
LOG=/var/log/kwartje-install.log
SVC_USER=kwartje

ASSUME_YES=0
WANT_VERSION=""
MODE=""
PURGE=0
ARCHIVE="${KWARTJE_ARCHIVE:-}"
INCLUDE_PRE=0
WORK=""
HAS_SYSTEMD=1

# ---------- output ----------

if [ -t 1 ]; then
  B=$'\e[1m'; DIM=$'\e[2m'; RED=$'\e[31m'; GRN=$'\e[32m'; YEL=$'\e[33m'; BLU=$'\e[34m'; R=$'\e[0m'
else
  B=""; DIM=""; RED=""; GRN=""; YEL=""; BLU=""; R=""
fi

log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >>"$LOG" 2>/dev/null || true; }
say() { printf '%s\n' "$*"; log "$*"; }
step() { printf '\n%s==> %s%s\n' "$B$BLU" "$*" "$R"; log "== $*"; }
ok() { printf '%s✓%s %s\n' "$GRN" "$R" "$*"; log "ok: $*"; }
warn() { printf '%s! %s%s\n' "$YEL" "$*" "$R"; log "warn: $*"; }
die() { printf '%s✗ %s%s\n' "$RED" "$*" "$R" >&2; log "error: $*"; exit 1; }

cleanup() { [ -n "$WORK" ] && rm -rf "$WORK"; return 0; }
trap cleanup EXIT
# Never stop silently: an unexpected failure names the line.
trap 'printf "%s✗ Onverwachte fout op regel %s: %s (zie %s)%s\n" "$RED" "$LINENO" "$BASH_COMMAND" "$LOG" "$R" >&2; log "unexpected failure at line $LINENO: $BASH_COMMAND"' ERR

# Questions are read from the terminal, also when the script itself comes
# from `curl | bash` (stdin is then the script). The terminal is opened once,
# on fd 3 (answers) and fd 4 (prompts). KWARTJE_TTY=<file> reads the answers
# from a file instead (tests).
TTY_OK=0
open_tty() {
  if [ -n "${KWARTJE_TTY:-}" ]; then
    exec 3<"$KWARTJE_TTY" 4>&2; TTY_OK=1
  elif { exec 3</dev/tty 4>/dev/tty; } 2>/dev/null; then
    TTY_OK=1
  fi
}
have_tty() { [ "$TTY_OK" = 1 ]; }

# ask "Question" "default" -> REPLY
ask() {
  local q="$1" def="${2:-}"
  if [ "$ASSUME_YES" = 1 ]; then REPLY="$def"; say "$q [$def] (--yes)"; return; fi
  have_tty || die "Geen terminal om vragen te stellen. Draai het script interactief, of gebruik --yes."
  if [ -n "$def" ]; then printf '%s [%s]: ' "$q" "$def" >&4; else printf '%s: ' "$q" >&4; fi
  IFS= read -r -u 3 REPLY || REPLY=""
  [ -z "$REPLY" ] && REPLY="$def"
  log "ask: $q -> $REPLY"
}

# confirm "Question" y|n  (default answer) -> 0 for yes
confirm() {
  local q="$1" def="${2:-n}" hint
  if [ "$def" = y ]; then hint="J/n"; else hint="j/N"; fi
  if [ "$ASSUME_YES" = 1 ]; then say "$q [$hint] (--yes: $def)"; [ "$def" = y ]; return; fi
  have_tty || die "Geen terminal om vragen te stellen. Draai het script interactief, of gebruik --yes."
  printf '%s [%s]: ' "$q" "$hint" >&4
  local a; IFS= read -r -u 3 a || a=""
  a="$(printf '%s' "$a" | tr '[:upper:]' '[:lower:]')"
  [ -z "$a" ] && a="$def"
  log "confirm: $q -> $a"
  case "$a" in j|ja|y|yes) return 0 ;; *) return 1 ;; esac
}

usage() {
  cat <<EOF
Kwartje installer (script $SCRIPT_VERSION)

Gebruik:  curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | sudo bash [-s -- opties]

Zonder opties: nieuwe installatie, of het updatemenu als Kwartje al geïnstalleerd is.

  --version TAG     deze release installeren of erheen bijwerken (ook een oudere)
  --list            beschikbare releases tonen
  --update          direct bijwerken (zonder menu)
  --uninstall       Kwartje verwijderen; data en back-ups blijven staan
  --purge           met --uninstall: ook alle data, back-ups, configuratie en de gebruiker
  --pre             ook test-releases (pre-releases) tonen
  --archive FILE    een lokaal releasepakket (.tar.gz) gebruiken in plaats van te downloaden
  --yes             geen vragen: standaardkeuzes (Tailscale, poort 8080, nieuwste release)

Zonder vragen een eerste beheerder aanmaken: zet KWARTJE_ADMIN_EMAIL, KWARTJE_PASSWORD
en eventueel KWARTJE_ADMIN_NAME (sudo -E). Een andere repo: KWARTJE_REPO=eigenaar/naam.
  -h, --help        deze hulp

Log: $LOG
EOF
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --version) WANT_VERSION="${2:-}"; [ -n "$WANT_VERSION" ] || die "--version verwacht een release, bijv. v0.1.2"; shift ;;
      --list) MODE=list ;;
      --update) MODE=update ;;
      --uninstall) MODE=uninstall ;;
      --purge) PURGE=1 ;;
      --pre) INCLUDE_PRE=1 ;;
      --archive) ARCHIVE="${2:-}"; [ -f "$ARCHIVE" ] || die "--archive: bestand niet gevonden: $ARCHIVE"; shift ;;
      --yes|-y) ASSUME_YES=1 ;;
      -h|--help) usage; exit 0 ;;
      *) die "Onbekende optie: $1 (zie --help)" ;;
    esac
    shift
  done
  if [ "$PURGE" = 1 ] && [ "$MODE" != uninstall ]; then die "--purge hoort bij --uninstall"; fi
}

# ---------- checks ----------

preflight() {
  [ "$(id -u)" = 0 ] || die "Draai dit script als root (sudo bash)."
  mkdir -p "$(dirname "$LOG")"; touch "$LOG"; chmod 0600 "$LOG"
  log "---- install.sh $SCRIPT_VERSION, args: ${ORIG_ARGS:-}"
  command -v apt-get >/dev/null || die "Dit script werkt met apt (Debian en afgeleiden). Zie deploy/INSTALL.md voor een handmatige installatie."
  local arch; arch="$(uname -m)"
  [ "$arch" = x86_64 ] || die "Er zijn alleen releases voor x86-64; deze machine is $arch. Bouw zelf vanaf de broncode (docs/DEVELOPMENT.md)."
  if [ -d /run/systemd/system ]; then HAS_SYSTEMD=1; else
    HAS_SYSTEMD=0
    warn "systemd draait niet (container zonder init?). Bestanden worden geïnstalleerd, maar de service wordt niet gestart."
  fi
}

# osr KEY -> value from /etc/os-release
osr() { sed -nE "s/^$1=\"?([^\"]*)\"?$/\1/p" /etc/os-release 2>/dev/null | head -1; }

check_os() {
  local id ver name like
  id="$(osr ID)"; ver="$(osr VERSION_ID)"; name="$(osr PRETTY_NAME)"; like="$(osr ID_LIKE)"
  if [ "$id" = debian ] && { [ "$ver" = 12 ] || [ "$ver" = 13 ]; }; then
    ok "Besturingssysteem: $name"
  elif [ "$id" = debian ]; then
    warn "Besturingssysteem: $name. Getest op Debian 12 en 13; andere versies kunnen werken."
    confirm "Toch doorgaan?" y || die "Gestopt."
  else
    warn "Besturingssysteem: ${name:-onbekend}${like:+ (lijkt op: $like)}. Kwartje is gemaakt en getest voor Debian 12 en 13."
    warn "Het kan werken, maar dat is niet getest. Problemen met andere systemen worden niet altijd opgelost."
    confirm "Toch doorgaan?" y || die "Gestopt."
  fi
  # The release binary is built on Debian 12 (glibc 2.36).
  local glibc; glibc="$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)"
  if [ -n "$glibc" ] && [ "$(printf '%s\n%s\n' 2.36 "$glibc" | sort -V | head -1)" != 2.36 ]; then
    die "glibc $glibc is te oud; de release heeft glibc 2.36 of nieuwer nodig (Debian 12 of nieuwer)."
  fi
}

apt_install() {
  local missing=() p
  for p in "$@"; do dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p"); done
  [ ${#missing[@]} -eq 0 ] && return 0
  say "Installeren: ${missing[*]}"
  DEBIAN_FRONTEND=noninteractive apt-get update -qq >>"$LOG" 2>&1 || die "apt-get update mislukt (zie $LOG)"
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${missing[@]}" >>"$LOG" 2>&1 \
    || die "Installeren van ${missing[*]} mislukt (zie $LOG)"
}

base_packages() {
  apt_install ca-certificates curl jq tar gzip xz-utils sqlite3 openssl tzdata iproute2
  ok "Basispakketten aanwezig"
}

# ---------- releases ----------

installed_version() {
  [ -x "$BIN" ] || { echo ""; return; }
  "$BIN" --version 2>/dev/null | awk '{print $2}'
}

norm() { printf '%s' "$1" | sed -E 's/^v\.?//'; }   # v0.1.2 / v.0.1.0 -> 0.1.2

# newer A B: true when A > B
newer() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]; }

RELEASES_JSON=""
fetch_releases() {
  [ -n "$RELEASES_JSON" ] && return 0
  local out
  out="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$API/releases?per_page=30" 2>>"$LOG")" \
    || die "Releases ophalen van github.com/$REPO mislukt. Is de machine online? (zie $LOG)"
  RELEASES_JSON="$(printf '%s' "$out" | jq -c --argjson pre "$INCLUDE_PRE" \
    '[.[] | select(.draft == false) | select($pre == 1 or .prerelease == false)
      | {tag: .tag_name, name: .name, date: (.published_at // "" | .[0:10]), pre: .prerelease,
         body: (.body // ""),
         asset: ([.assets[] | select(.name | endswith("-x86_64-debian12.tar.gz"))][0].browser_download_url // ""),
         sha: ([.assets[] | select(.name | endswith("-x86_64-debian12.tar.gz.sha256"))][0].browser_download_url // "")}
      | select(.asset != "")]')"
  [ "$(printf '%s' "$RELEASES_JSON" | jq length)" -gt 0 ] || die "Geen releases met een Debian-pakket gevonden op github.com/$REPO."
}

list_releases() {
  fetch_releases
  local cur; cur="$(installed_version)"
  printf '%s' "$RELEASES_JSON" | jq -r '.[] | "\(.tag)\t\(.date)\t\(if .pre then "test-release" else "" end)"' |
    while IFS=$'\t' read -r tag date pre; do
      local mark=""; [ -n "$cur" ] && [ "$(norm "$tag")" = "$cur" ] && mark=" ← geïnstalleerd"
      printf '  %-12s %s %s%s\n' "$tag" "$date" "$pre" "$mark"
    done
}

# choose_release [only-newer-than] -> TAG
choose_release() {
  local floor="${1:-}"
  fetch_releases
  local tags=() t
  while IFS= read -r t; do
    if [ -z "$floor" ] || newer "$(norm "$t")" "$floor"; then tags+=("$t"); fi
  done < <(printf '%s' "$RELEASES_JSON" | jq -r '.[].tag')
  if [ -n "$WANT_VERSION" ]; then
    printf '%s' "$RELEASES_JSON" | jq -e --arg t "$WANT_VERSION" 'any(.[]; .tag == $t)' >/dev/null \
      || die "Release $WANT_VERSION niet gevonden. Beschikbaar: $(printf '%s' "$RELEASES_JSON" | jq -r '[.[].tag] | join(", ")')"
    TAG="$WANT_VERSION"; return
  fi
  if [ ${#tags[@]} -eq 0 ]; then TAG=""; return; fi
  say "Beschikbare releases:"
  local i=1
  for t in "${tags[@]:0:10}"; do
    local d; d="$(printf '%s' "$RELEASES_JSON" | jq -r --arg t "$t" '.[] | select(.tag == $t) | .date')"
    printf '  %2d) %-12s %s%s\n' "$i" "$t" "$d" "$([ $i = 1 ] && echo '  (nieuwste)')"
    i=$((i + 1))
  done
  ask "Kies een release (nummer of tag)" 1
  if [[ "$REPLY" =~ ^[0-9]+$ ]] && [ "$REPLY" -ge 1 ] && [ "$REPLY" -le "${#tags[@]}" ]; then
    TAG="${tags[$((REPLY - 1))]}"
  elif printf '%s' "$RELEASES_JSON" | jq -e --arg t "$REPLY" 'any(.[]; .tag == $t)' >/dev/null; then
    TAG="$REPLY"
  else
    die "Onbekende keuze: $REPLY"
  fi
}

show_notes() {
  local tag="$1" body
  body="$(printf '%s' "$RELEASES_JSON" | jq -r --arg t "$tag" '.[] | select(.tag == $t) | .body' | head -25)"
  [ -n "$body" ] && printf '%s\n%s\n%s\n' "${DIM}--- release notes $tag ---" "$body" "---${R}"
  return 0
}

# download TAG -> PKG (unpacked folder with kwartje + deploy/)
download() {
  local tag="$1" url sha
  WORK="$(mktemp -d /tmp/kwartje-install.XXXXXX)"
  chmod 0755 "$WORK"   # the kwartje user runs the new binary for the restore test
  if [ -n "$ARCHIVE" ]; then
    cp "$ARCHIVE" "$WORK/pkg.tar.gz"
    [ -f "$ARCHIVE.sha256" ] && cp "$ARCHIVE.sha256" "$WORK/pkg.tar.gz.sha256"
    say "Lokaal pakket: $ARCHIVE"
  else
    url="$(printf '%s' "$RELEASES_JSON" | jq -r --arg t "$tag" '.[] | select(.tag == $t) | .asset')"
    sha="$(printf '%s' "$RELEASES_JSON" | jq -r --arg t "$tag" '.[] | select(.tag == $t) | .sha')"
    say "Downloaden: $url"
    curl -fsSL --retry 3 -o "$WORK/pkg.tar.gz" "$url" || die "Downloaden mislukt"
    [ -n "$sha" ] && curl -fsSL --retry 3 -o "$WORK/pkg.tar.gz.sha256" "$sha"
  fi
  if [ -f "$WORK/pkg.tar.gz.sha256" ]; then
    local want got
    want="$(awk '{print $1}' "$WORK/pkg.tar.gz.sha256")"
    got="$(sha256sum "$WORK/pkg.tar.gz" | awk '{print $1}')"
    [ "$want" = "$got" ] || die "Controlegetal (SHA-256) klopt niet: het pakket is beschadigd of aangepast. Gestopt."
    ok "Controlegetal (SHA-256) klopt"
  else
    [ -n "$ARCHIVE" ] || die "Geen .sha256 bij deze release; niet veilig om te installeren."
    warn "Geen .sha256 naast het lokale pakket; controle overgeslagen"
  fi
  tar xzf "$WORK/pkg.tar.gz" -C "$WORK"
  PKG="$(find "$WORK" -mindepth 1 -maxdepth 1 -type d -name 'kwartje-*' | head -1)"
  [ -n "$PKG" ] && [ -x "$PKG/kwartje" ] && [ -d "$PKG/deploy" ] || die "Onverwachte inhoud van het pakket"
  NEW_VERSION="$("$PKG/kwartje" --version | awk '{print $2}')" || die "De nieuwe binary start niet op deze machine (glibc?)"
  ok "Kwartje $NEW_VERSION uitgepakt"
}

# ---------- typst ----------

install_typst() {
  if command -v typst >/dev/null && typst --version 2>/dev/null | grep -q "typst $TYPST_VERSION"; then
    ok "Typst $TYPST_VERSION aanwezig (voor factuur-PDF's)"; return
  fi
  say "Typst $TYPST_VERSION installeren (voor factuur-PDF's, van github.com/typst/typst, Apache-2.0)"
  local t="$WORK/typst"; mkdir -p "$t"
  curl -fsSL --retry 3 -o "$t/typst.tar.xz" \
    "https://github.com/typst/typst/releases/download/v$TYPST_VERSION/typst-x86_64-unknown-linux-musl.tar.xz" \
    || { warn "Typst downloaden mislukt; installeer het later (deploy/INSTALL.md §2b). Facturen maken werkt pas daarna."; return; }
  tar xJf "$t/typst.tar.xz" -C "$t"
  install -m 0755 "$t/typst-x86_64-unknown-linux-musl/typst" /usr/local/bin/typst
  ok "$(/usr/local/bin/typst --version)"
}

# ---------- access ----------

this_ip() {
  ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") {print $(i + 1); exit}}'
}

port_free() { ! ss -ltnH 2>/dev/null | awk '{print $4}' | grep -qE "[:.]$1\$"; }

valid_ip() { [[ "$1" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || [[ "$1" =~ : ]]; }

ask_port() {
  while :; do
    ask "Poort voor Kwartje" 8080
    if ! [[ "$REPLY" =~ ^[0-9]+$ ]] || [ "$REPLY" -lt 1024 ] || [ "$REPLY" -gt 65535 ]; then
      warn "Kies een poort tussen 1024 en 65535."; [ "$ASSUME_YES" = 1 ] && die "Ongeldige poort"; continue
    fi
    if ! port_free "$REPLY"; then
      warn "Poort $REPLY is al in gebruik."; [ "$ASSUME_YES" = 1 ] && die "Poort bezet"; continue
    fi
    PORT="$REPLY"; return
  done
}

explain_access() {
  cat <<EOF

${B}Hoe bereik je Kwartje?${R}
Kwartje zelf spreekt gewoon HTTP en doet geen HTTPS. Inloggen werkt normaal
alleen via HTTPS: de sessiecookie is beveiligd en wordt anders niet bewaard.
Er zijn drie manieren:

  ${B}A) Via Tailscale${R} (aanbevolen)
     Kwartje luistert alleen op deze machine (127.0.0.1). 'tailscale serve' geeft
     HTTPS op je eigen tailnet: https://<naam>.<tailnet>.ts.net. Niets staat open
     op je netwerk of internet.

  ${B}B) Achter een reverse proxy${R} (Pangolin/Newt, Caddy, Nginx Proxy Manager, …)
     De proxy regelt HTTPS en eventueel SSO. Draait de proxy (of Newt) op deze
     machine, dan luistert Kwartje alleen op 127.0.0.1. Staat de proxy elders,
     dan luistert Kwartje op het netwerkadres van deze machine en vertrouwt het
     alleen dat ene proxy-adres.

  ${B}C) Direct op het netwerk${R} (http://<ip>:<poort>)
     Geen HTTPS: wachtwoorden en gegevens gaan onversleuteld over het netwerk.
     Alleen voor een vertrouwd thuis- of kantoornetwerk.

EOF
}

choose_access() {
  explain_access
  ask "Kies A, B of C" A
  ACCESS="$(printf '%s' "$REPLY" | tr '[:lower:]' '[:upper:]')"
  TRUSTED=""; ALLOW_ALL=0; COOKIE_SECURE=true
  case "$ACCESS" in
    A)
      ask_port; LISTEN="127.0.0.1:$PORT" ;;
    B)
      ask_port
      if confirm "Draait de proxy (of Pangolin Newt) op deze machine?" y; then
        LISTEN="127.0.0.1:$PORT"
      else
        local me; me="$(this_ip)"
        ask "Netwerkadres van deze machine waarop Kwartje luistert" "$me"; valid_ip "$REPLY" || die "Ongeldig IP-adres: $REPLY"
        LISTEN="$REPLY:$PORT"
        ask "IP-adres van de proxy (alleen dat adres wordt vertrouwd)" ""
        valid_ip "$REPLY" || die "Ongeldig IP-adres: $REPLY"
        TRUSTED="$REPLY"
        warn "Leg het IP-adres van deze machine vast in je router (DHCP-reservering), anders kan Kwartje na een herstart onbereikbaar zijn."
      fi ;;
    C)
      ask_port
      local me; me="$(this_ip)"
      cat <<EOF

${YEL}${B}Let op: direct op het netwerk, zonder HTTPS.${R}
${YEL}- Iedereen op je netwerk ziet de inlogpagina.
- Wachtwoorden, codes en gegevens gaan onversleuteld over het netwerk en zijn
  af te luisteren, bijvoorbeeld op een gedeelde wifi.
- Zet deze poort nooit open in je router naar internet.${R}
Kies je toch hiervoor, dan wordt cookie_secure = false gezet.

  1) Alleen op dit adres: $me  (aanbevolen; leg het vast in je router)
  2) Op alle netwerkadressen (0.0.0.0)  ${YEL}— ook op netwerken die je misschien vergeet,
     zoals een tweede netwerkkaart of een VPN${R}

EOF
      ask "Kies 1 of 2" 1
      if [ "$REPLY" = 2 ]; then
        warn "Kwartje luistert dan op álle netwerkadressen (allow_all_interfaces = true)."
        confirm "Weet je zeker dat je dit wilt?" n || die "Gestopt. Draai het script opnieuw en kies een andere manier."
        LISTEN="0.0.0.0:$PORT"; ALLOW_ALL=1
      else
        ask "Netwerkadres" "$me"; valid_ip "$REPLY" || die "Ongeldig IP-adres: $REPLY"
        LISTEN="$REPLY:$PORT"
      fi
      confirm "Begrijp je dat dit onversleuteld is, en wil je doorgaan?" n || die "Gestopt. Kies A of B voor HTTPS."
      COOKIE_SECURE=false ;;
    *) die "Kies A, B of C" ;;
  esac
  ok "Kwartje luistert op $LISTEN"
}

# ---------- tailscale ----------

setup_tailscale() {
  local port="$1"
  if ! command -v tailscale >/dev/null; then
    cat <<EOF

${B}Tailscale installeren?${R}
- Het script downloadt en draait dan het officiële installatiescript van
  Tailscale (https://tailscale.com/install.sh). Dat voegt de pakketbron van
  Tailscale toe en installeert het pakket 'tailscale'. De Linux-client is open
  source (BSD-3-Clause); Kwartje heeft geen band met Tailscale Inc.
- Je hebt een eigen Tailscale-account nodig. Bij het inloggen ga je akkoord met
  de voorwaarden van Tailscale (https://tailscale.com/terms).
- Het gratis Personal-abonnement is volgens Tailscale alleen voor
  niet-commercieel gebruik. Gebruik je Kwartje voor een bedrijf, kijk dan op
  https://tailscale.com/pricing welk abonnement past.
EOF
    if [ ! -c /dev/net/tun ]; then
      warn "/dev/net/tun ontbreekt. In een Proxmox-LXC moet je die eerst doorgeven; zie deploy/INSTALL.md §1."
    fi
    # Never accept a third party's software and terms on someone's behalf.
    if [ "$ASSUME_YES" = 1 ]; then
      say "--yes: Tailscale wordt niet automatisch geïnstalleerd (dat vraagt jouw eigen akkoord)."
      say "Later: curl -fsSL https://tailscale.com/install.sh | sh && tailscale up && tailscale serve --bg $port"
      return
    fi
    if ! confirm "Tailscale nu installeren?" y; then
      say "Overgeslagen. Later: curl -fsSL https://tailscale.com/install.sh | sh && tailscale up && tailscale serve --bg $port"
      return
    fi
    curl -fsSL https://tailscale.com/install.sh | sh >>"$LOG" 2>&1 || { warn "Tailscale installeren mislukt (zie $LOG)"; return; }
    ok "Tailscale geïnstalleerd"
  fi
  if [ "$HAS_SYSTEMD" = 1 ] && ! tailscale status >/dev/null 2>&1; then
    say "Log in bij Tailscale: open de link die zo verschijnt en meld je aan."
    ask "Naam van deze machine in je tailnet" kwartje
    tailscale up --hostname "$REPLY" </dev/null || { warn "tailscale up mislukt; draai het later zelf"; return; }
  fi
  if tailscale serve --bg "$port" >>"$LOG" 2>&1; then
    ok "tailscale serve: HTTPS op je tailnet → 127.0.0.1:$port"
    TS_URL="$(tailscale serve status 2>/dev/null | grep -oE 'https://[^ ]+' | head -1 || true)"
  else
    warn "tailscale serve lukte niet. Zet in de Tailscale-console MagicDNS en HTTPS-certificaten aan en draai: tailscale serve --bg $port"
  fi
}

# ---------- install ----------

install_files() {
  local d="$PKG/deploy"
  id "$SVC_USER" >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin "$SVC_USER"
  install -d -o "$SVC_USER" -g "$SVC_USER" -m 0750 "$DATA" "$BACKUPS"
  install -d -o root -g "$SVC_USER" -m 0750 "$ETC_DIR"
  install -d -m 0755 "$LIB" "$DOC"
  install -m 0755 "$PKG/kwartje" "$BIN"
  install -m 0755 "$d"/notify.sh "$d"/restic-backup.sh "$d"/restore.sh "$LIB/"
  install -m 0644 "$d"/INSTALL.md "$d"/config.example.toml "$d"/*.env.example "$DOC/"
  local changed=0 f
  for f in "$d"/*.service "$d"/*.timer; do
    if ! cmp -s "$f" "$UNITS/$(basename "$f")"; then install -m 0644 "$f" "$UNITS/"; changed=1; fi
  done
  [ "$HAS_SYSTEMD" = 1 ] && [ "$changed" = 1 ] && systemctl daemon-reload
  ok "Programma, hulpscripts en systemd-units geïnstalleerd"
}

write_config() {
  if [ -f "$CONFIG" ]; then ok "Bestaande configuratie behouden: $CONFIG"; return; fi
  local secret tmp
  secret="$(openssl rand -base64 64 | tr -d '\n')"
  tmp="$(mktemp)"
  sed -e "s|^listen = .*|listen = \"$LISTEN\"|" \
      -e "s|^session_secret = .*|session_secret = \"$secret\"|" \
      -e "s|^cookie_secure = .*|cookie_secure = $COOKIE_SECURE|" \
      "$PKG/deploy/config.example.toml" >"$tmp"
  if [ -n "$TRUSTED" ]; then sed -i "s|^# trusted_proxies = .*|trusted_proxies = [\"$TRUSTED\"]|" "$tmp"; fi
  if [ "$ALLOW_ALL" = 1 ]; then sed -i "s|^# allow_all_interfaces = .*|allow_all_interfaces = true|" "$tmp"; fi
  install -o root -g "$SVC_USER" -m 0640 "$tmp" "$CONFIG"; rm -f "$tmp"
  grep -q "^listen = \"$LISTEN\"" "$CONFIG" || die "Configuratie schrijven mislukt"
  ok "Configuratie geschreven: $CONFIG (met een nieuw session_secret)"
}

as_svc() { runuser -u "$SVC_USER" -- "$@"; }

first_admin() {
  as_svc "$BIN" --config "$CONFIG" migrate >>"$LOG" 2>&1 || die "Database aanmaken mislukt (zie $LOG)"
  local n; n="$(sqlite3 "$DATA/app.db" 'SELECT COUNT(*) FROM users' 2>/dev/null || echo 0)"
  if [ "$n" != 0 ]; then ok "Database bevat al $n gebruiker(s)"; return; fi
  if [ -n "${KWARTJE_ADMIN_EMAIL:-}" ] && [ -n "${KWARTJE_PASSWORD:-}" ]; then
    as_svc env KWARTJE_PASSWORD="$KWARTJE_PASSWORD" "$BIN" --config "$CONFIG" create-user \
      --name "${KWARTJE_ADMIN_NAME:-Beheerder}" --email "$KWARTJE_ADMIN_EMAIL" --role beheerder >>"$LOG" 2>&1 \
      || die "Gebruiker aanmaken mislukt (zie $LOG)"
    ok "Beheerder $KWARTJE_ADMIN_EMAIL aangemaakt"
    return
  fi
  if [ "$ASSUME_YES" = 1 ]; then
    warn "Nog geen gebruiker. Maak de eerste beheerder aan met:"
    say "  runuser -u $SVC_USER -- $BIN --config $CONFIG create-user --name <naam> --email <e-mail> --role beheerder"
    return
  fi
  say "Maak de eerste gebruiker aan. Dit wordt de ${B}beheerder${R}: die beheert gebruikers en kan uren en ritten namens medewerkers schrijven."
  local name email
  while :; do ask "Naam" ""; name="$REPLY"; [ -n "$name" ] && break; done
  while :; do ask "E-mailadres (om mee in te loggen)" ""; email="$REPLY"; [[ "$email" == *@*.* ]] && break; warn "Vul een geldig e-mailadres in."; done
  say "Kies een wachtwoord van minstens 12 tekens (het wordt niet getoond):"
  local tries=0
  until as_svc env ${KWARTJE_PASSWORD:+KWARTJE_PASSWORD="$KWARTJE_PASSWORD"} "$BIN" --config "$CONFIG" \
      create-user --name "$name" --email "$email" --role beheerder; do
    tries=$((tries + 1)); [ $tries -ge 3 ] && die "Gebruiker aanmaken mislukt"
    warn "Probeer het nog eens."
  done
  ok "Beheerder $email aangemaakt; tweestapsverificatie stel je in bij de eerste login"
}

health() {
  local addr="$1" host port url i
  port="${addr##*:}"; host="${addr%:*}"
  [ "$host" = 0.0.0.0 ] && host=127.0.0.1
  url="http://$host:$port/inloggen"
  for i in $(seq 1 20); do
    if [ "$(curl -s -o /dev/null -w '%{http_code}' "$url" || true)" = 200 ]; then return 0; fi
    sleep 1
  done
  return 1
}

current_listen() { sed -nE 's/^listen = "([^"]+)".*/\1/p' "$CONFIG" | head -1; }

start_services() {
  if [ "$HAS_SYSTEMD" = 0 ]; then warn "Geen systemd: start Kwartje zelf met: runuser -u $SVC_USER -- $BIN --config $CONFIG serve"; return; fi
  systemctl daemon-reload
  systemctl enable --now kwartje.service >>"$LOG" 2>&1
  systemctl enable --now kwartje-backup.timer kwartje-restore-test.timer >>"$LOG" 2>&1
  if health "$(current_listen)"; then ok "Kwartje draait"; else
    journalctl -u kwartje.service -n 30 --no-pager || true
    die "Kwartje start niet (zie hierboven en $LOG)"
  fi
  # A first backup, so /gezond is green from the start.
  if systemctl start kwartje-backup.service >>"$LOG" 2>&1; then
    ok "Eerste back-up gemaakt in $BACKUPS"
  else
    warn "Eerste back-up mislukt (journalctl -u kwartje-backup)"
  fi
}

do_install() {
  step "1/8 Controles"
  check_os
  base_packages
  step "2/8 Release kiezen"
  if [ -n "$ARCHIVE" ]; then TAG="(lokaal)"; else choose_release; fi
  [ -n "$TAG" ] || die "Geen release gevonden"
  [ -z "$ARCHIVE" ] && show_notes "$TAG"
  download "$TAG"
  step "3/8 Benodigdheden"
  install_typst
  if confirm "Offsite back-ups met restic gebruiken? (instellen doe je daarna, zie INSTALL.md §5)" n; then
    apt_install restic; ok "restic geïnstalleerd"
  fi
  if [ "$(cat /etc/timezone 2>/dev/null || timedatectl show -p Timezone --value 2>/dev/null || true)" != Europe/Amsterdam ]; then
    if confirm "Tijdzone op Europe/Amsterdam zetten? (de nachtelijke back-up draait op lokale tijd)" y; then
      if [ "$HAS_SYSTEMD" = 1 ]; then timedatectl set-timezone Europe/Amsterdam; else ln -sf /usr/share/zoneinfo/Europe/Amsterdam /etc/localtime; echo Europe/Amsterdam >/etc/timezone; fi
      ok "Tijdzone Europe/Amsterdam"
    fi
  fi
  step "4/8 Toegang"
  choose_access
  step "5/8 Installeren"
  install_files
  write_config
  step "6/8 Database en eerste beheerder"
  first_admin
  step "7/8 Starten"
  start_services
  TS_URL=""
  if [ "$ACCESS" = A ]; then step "8/8 Tailscale"; setup_tailscale "$PORT"; else step "8/8 Klaar"; fi
  summary
}

summary() {
  local listen url; listen="$(current_listen)"
  case "${ACCESS:-}" in
    A) url="${TS_URL:-https://<naam>.<tailnet>.ts.net (na tailscale serve)}" ;;
    B) url="het adres dat je in je proxy instelt → http://$listen" ;;
    C) url="http://${listen/0.0.0.0/$(this_ip)}" ;;
    *) url="http://$listen" ;;
  esac
  cat <<EOF

${GRN}${B}Kwartje $(installed_version) is geïnstalleerd.${R}

  Openen:          $url
  Configuratie:    $CONFIG
  Data:            $DATA   (back-ups: $BACKUPS, elke nacht 02:00)
  Logboek:         journalctl -u kwartje.service -f
  Bijwerken:       draai dit script opnieuw
  Handleiding:     $DOC/INSTALL.md

Bij de eerste login stel je tweestapsverificatie in met een authenticator-app.
EOF
  [ "${ACCESS:-}" = B ] && say "Proxy: stuur het verkeer naar http://$listen en laat de Host-header ongemoeid. Pangolin: zie INSTALL.md §9b."
  return 0
}

# ---------- update ----------

do_update() {
  local cur="$1"
  step "Bijwerken"
  base_packages
  say "Geïnstalleerd: Kwartje $cur"
  if [ -n "$ARCHIVE" ]; then TAG="(lokaal)"; else
    fetch_releases
    local latest; latest="$(printf '%s' "$RELEASES_JSON" | jq -r '.[0].tag')"
    say "Nieuwste release: $latest"
    if [ -n "$WANT_VERSION" ]; then choose_release
    elif newer "$(norm "$latest")" "$cur"; then
      if [ "$MODE" = update ]; then TAG="$latest"; else choose_release "$cur"; fi
    else
      ok "Je hebt de nieuwste versie."
      say "Een andere versie kiezen kan met --version <tag> (zie --list)."
      return 0
    fi
    show_notes "$TAG"
  fi
  download "$TAG"
  if [ "$NEW_VERSION" = "$cur" ]; then
    confirm "Versie $cur is al geïnstalleerd. Opnieuw installeren?" n || return 0
  elif newer "$cur" "$NEW_VERSION"; then
    warn "Je gaat TERUG van $cur naar $NEW_VERSION."
    warn "Heeft de nieuwere versie de database al bijgewerkt, dan weigert de oudere te starten."
    warn "Teruggaan kan dan alleen met de kopie van vóór de migratie in $DATA/pre-migration/ (zie INSTALL.md §6)."
    confirm "Toch teruggaan?" n || die "Gestopt."
  fi
  confirm "Bijwerken naar $NEW_VERSION?" y || die "Gestopt."
  install_typst

  if [ "$HAS_SYSTEMD" = 1 ]; then
    say "Verse back-up maken…"
    systemctl start kwartje-backup.service >>"$LOG" 2>&1 || die "Back-up mislukt; er is niets veranderd (journalctl -u kwartje-backup)"
    say "Hersteltest met de nieuwe versie (je echte gegevens blijven onaangeroerd)…"
    as_svc "$PKG/kwartje" --config "$CONFIG" restore-test --latest >>"$LOG" 2>&1 \
      || die "Hersteltest mislukt met $NEW_VERSION; er is niets veranderd. Zie $LOG."
    ok "Hersteltest geslaagd"
  fi

  local snaps_before; snaps_before="$(snapshots)"
  cp -p "$BIN" "$BIN.vorige"
  install_files
  [ -f "$DOC/config.example.toml" ] && new_settings
  if [ "$HAS_SYSTEMD" = 0 ]; then ok "Bijgewerkt naar $NEW_VERSION (start de service zelf)"; return; fi
  systemctl restart kwartje.service
  if health "$(current_listen)"; then
    ok "Kwartje $NEW_VERSION draait. De vorige versie staat als $BIN.vorige."
    return 0
  fi
  warn "Kwartje $NEW_VERSION start niet. Terugzetten…"
  journalctl -u kwartje.service -n 30 --no-pager || true
  systemctl stop kwartje.service || true
  local snap; snap="$(comm -13 <(printf '%s\n' "$snaps_before") <(snapshots) | tail -1)"
  if [ -n "$snap" ]; then
    local aside; aside="$DATA/app.db.mislukt-$(date +%Y%m%d-%H%M%S)"
    mv "$DATA/app.db" "$aside"; rm -f "$DATA/app.db-wal" "$DATA/app.db-shm"
    install -o "$SVC_USER" -g "$SVC_USER" -m 0640 "$DATA/pre-migration/$snap" "$DATA/app.db"
    warn "Database teruggezet van $snap (de bijgewerkte staat bewaard als $aside)"
  fi
  install -m 0755 "$BIN.vorige" "$BIN"
  systemctl start kwartje.service
  if health "$(current_listen)"; then die "Teruggezet naar $cur; Kwartje draait weer. Meld het probleem met de uitvoer hierboven."; fi
  die "Terugzetten lukte niet. Zie journalctl -u kwartje.service en INSTALL.md §6/§7."
}

# Pre-migration snapshots, sorted by name (= by time).
snapshots() {
  [ -d "$DATA/pre-migration" ] || return 0
  find "$DATA/pre-migration" -maxdepth 1 -name '*.db' -printf '%f\n' | sort
}

new_settings() {
  local k missing=()
  while IFS= read -r k; do
    grep -qE "^#? ?$k = " "$CONFIG" || missing+=("$k")
  done < <(sed -nE 's/^#? ?([a-z_]+) = .*/\1/p' "$DOC/config.example.toml" | sort -u)
  [ ${#missing[@]} -gt 0 ] && say "Nieuwe instellingen in deze versie (optioneel): ${missing[*]} — zie $DOC/config.example.toml"
  return 0
}

# ---------- uninstall ----------

do_uninstall() {
  step "Kwartje verwijderen"
  cat <<EOF
Dit verwijdert:
  - de service en timers (kwartje*.service, kwartje*.timer)
  - $BIN, $LIB, $DOC
EOF
  if [ "$PURGE" = 1 ]; then
    cat <<EOF
${RED}${B}en met --purge ook ALLES hieronder, definitief:${R}
${RED}  - $DATA      (database, documenten, bonnetjes, facturen)
  - $BACKUPS   (alle lokale back-ups)
  - $ETC_DIR          (configuratie)
  - de systeemgebruiker '$SVC_USER'${R}
Offsite back-ups (restic) worden niet aangeraakt.
EOF
  else
    say "Blijft staan: $DATA (data), $BACKUPS (back-ups) en $ETC_DIR (configuratie)."
  fi
  say "${B}Dit kan niet ongedaan worden gemaakt.${R}"
  [ "$ASSUME_YES" = 1 ] && die "Verwijderen vraagt altijd om bevestiging; --yes werkt hier niet."
  confirm "Weet je zeker dat je Kwartje wilt verwijderen?" n || die "Gestopt; er is niets verwijderd."
  local word=verwijderen; [ "$PURGE" = 1 ] && word="ALLES VERWIJDEREN"
  ask "Typ '$word' om te bevestigen" ""
  [ "$REPLY" = "$word" ] || die "Niet bevestigd; er is niets verwijderd."

  if [ "$HAS_SYSTEMD" = 1 ]; then
    local u
    for u in $(systemctl list-unit-files 'kwartje*' --no-legend 2>/dev/null | awk '{print $1}'); do
      systemctl disable --now "$u" >>"$LOG" 2>&1 || true
    done
  fi
  rm -f "$UNITS"/kwartje*.service "$UNITS"/kwartje*.timer
  [ "$HAS_SYSTEMD" = 1 ] && systemctl daemon-reload
  rm -f "$BIN" "$BIN.vorige"; rm -rf "$LIB" "$DOC"
  ok "Programma en services verwijderd"
  if [ "$PURGE" = 1 ]; then
    rm -rf "$DATA" "$BACKUPS" "$ETC_DIR"
    id "$SVC_USER" >/dev/null 2>&1 && userdel "$SVC_USER" 2>/dev/null || true
    ok "Data, back-ups, configuratie en gebruiker verwijderd"
  fi
  command -v tailscale >/dev/null && say "Tailscale blijft geïnstalleerd. 'tailscale serve' stoppen: tailscale serve reset"
  command -v typst >/dev/null && say "Typst blijft staan in $(command -v typst)."
  return 0
}

# ---------- main ----------

main() {
  ORIG_ARGS="$*"
  parse_args "$@"
  [ "$ASSUME_YES" = 1 ] || open_tty
  printf '%sKwartje installer%s %s(script %s, github.com/%s)%s\n' "$B" "$R" "$DIM" "$SCRIPT_VERSION" "$REPO" "$R"
  preflight
  case "$MODE" in
    list) apt_install ca-certificates curl jq; list_releases; exit 0 ;;
    uninstall) do_uninstall; exit 0 ;;
  esac
  local cur; cur="$(installed_version)"
  if [ -n "$cur" ]; then
    [ -f "$CONFIG" ] || die "$BIN bestaat maar $CONFIG niet; los dat eerst op of verwijder Kwartje (--uninstall)."
    do_update "$cur"
  else
    [ "$MODE" = update ] && die "Kwartje is nog niet geïnstalleerd; draai het script zonder --update."
    do_install
  fi
}

main "$@"
