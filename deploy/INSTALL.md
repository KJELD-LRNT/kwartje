# Installing kwartje in a Debian 12 LXC behind Tailscale

Target: Proxmox VE, unprivileged Debian 12 LXC (1 vCPU, 512 MB RAM, 8 GB disk).
The app listens on `127.0.0.1:8080` only. `tailscale serve` provides HTTPS on
the tailnet. Nothing is reachable from the LAN or the internet.

```
 phone / laptop ──tailnet (WireGuard)──► tailscale serve :443 (HTTPS, *.ts.net cert)
                                              │
                                              ▼
                                       127.0.0.1:8080  kwartje serve
                                              │
                         /var/lib/kwartje/{app.db, documents/}
   02:00 kwartje backup ──► /var/backups/kwartje/YYYY-MM-DD/
   03:00 restic ──► offsite repository (encrypted)
```

## Quick install with `install.sh`

On a fresh Debian 12 or 13 machine (an LXC, a VM or bare metal), as root:

```sh
curl -fsSL https://raw.githubusercontent.com/KJELD-LRNT/kwartje/main/install.sh | sudo bash
```

The script walks you through every step and asks before it changes anything
important:

1. **Checks:** root, x86-64, `apt` and glibc 2.36 or newer. On anything other
   than Debian 12/13 it warns and asks whether to continue.
2. **Release:** pick one from the list of GitHub releases (the newest by
   default, or an older one). The download is verified against its `.sha256`.
3. **Prerequisites:** base packages, Typst for the PDFs, optionally restic, and
   the time zone.
4. **Access:**
   - **A** Tailscale (recommended). It can install Tailscale for you after you
     confirm; you need your own Tailscale account and accept their terms.
   - **B** behind a reverse proxy such as Pangolin, on this machine or elsewhere
     (that proxy IP becomes the only trusted one).
   - **C** directly on the network over plain `http://`. It warns you, and with
     your explicit consent it can listen on every interface
     (`allow_all_interfaces`).
   - The port can be changed (default 8080).
5. **Install:** the user, folders, binary, systemd units and a config with a
   fresh secret. An existing config is never overwritten.
6. **First admin:** name and e-mail; the app asks for the password itself.
7. **Start:** the service and timers, a health check and a first backup.

**Updating:** run the same command again. The script recognises the installed
version and offers the newer releases, with their release notes. It then:

1. makes a backup
2. runs a restore test with the new binary
3. installs the new version and restarts
4. checks that it runs, and automatically puts the previous version (and, if
   needed, the pre-migration database copy) back when it doesn't

| Option | |
| --- | --- |
| `--version v0.1.2` | install or update to this release (an older one too) |
| `--list` | show the releases |
| `--update` | update to the newest release without the menu |
| `--yes` | no questions, defaults only. Tailscale is never installed this way; first admin via `KWARTJE_ADMIN_EMAIL` and `KWARTJE_PASSWORD` |
| `--uninstall` | remove the program and services; data, backups and config stay |
| `--uninstall --purge` | also remove all data, local backups, config and the user. Asks twice, can't be undone |
| `--archive file.tar.gz` | use a downloaded package instead of GitHub |

Options go after `bash -s --`, for example `curl … | sudo bash -s -- --version v0.1.1`.
Everything is logged to `/var/log/kwartje-install.log`.

The rest of this guide describes the same installation by hand, plus Proxmox,
Tailscale, backups and proxies in more detail.

## 1. Create the LXC (on the Proxmox host)

```sh
pveam update
pveam available | grep debian-12          # pick the current template name
pveam download local debian-12-standard_12.12-1_amd64.tar.zst

pct create 120 local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst \
  --hostname kwartje --unprivileged 1 --features nesting=1 \
  --cores 1 --memory 512 --swap 512 --rootfs local-lvm:8 \
  --net0 name=eth0,bridge=vmbr0,ip=dhcp --onboot 1 --timezone Europe/Amsterdam
```

Tailscale needs `/dev/net/tun` inside the unprivileged container. Add these lines to
`/etc/pve/lxc/120.conf`:

```
lxc.cgroup2.devices.allow: c 10:200 rwm
lxc.mount.entry: /dev/net/tun dev/net/tun none bind,create=file
```

```sh
pct start 120
pct enter 120
```

Also add the container to a **Proxmox Backup Server** job (snapshot mode) as a second
layer next to restic.

## 2. Base packages, time zone and Tailscale (inside the LXC)

```sh
apt update && apt full-upgrade -y
apt install -y ca-certificates curl sqlite3 restic unattended-upgrades
timedatectl set-timezone Europe/Amsterdam     # the 02:00 timer runs on local time

curl -fsSL https://tailscale.com/install.sh | sh
tailscale up --hostname kwartje
```

In the Tailscale admin console:

- Enable **MagicDNS** and **HTTPS certificates** (DNS page).
- Restrict access in the ACL policy so only your own users' devices can reach
  the container, for example:

```json
{
  "tagOwners": { "tag:kwartje": ["autogroup:admin"] },
  "acls": [
    { "action": "accept", "src": ["anna@example.com", "bram@example.com"], "dst": ["tag:kwartje:443"] }
  ]
}
```

Then run `tailscale up --hostname kwartje --advertise-tags=tag:kwartje`.

## 2b. Typst (invoice PDFs)

Invoices are rendered with the Typst CLI. Install a pinned version as a static binary:

```sh
cd /tmp
curl -fsSLO https://github.com/typst/typst/releases/download/v0.13.1/typst-x86_64-unknown-linux-musl.tar.xz
tar xJf typst-x86_64-unknown-linux-musl.tar.xz
install -m 0755 typst-x86_64-unknown-linux-musl/typst /usr/local/bin/typst
typst --version          # typst 0.13.1
```

Typst brings its own fonts, so no system fonts are needed. Set `typst_path` in
`config.toml` if you install it somewhere else.

## 3. Get the binary

**From GitHub Releases (normal route).** Every version tag (`v0.1.0`, …) makes
the *Release* workflow build Kwartje for Debian 12 and publish
`kwartje-<version>-x86_64-debian12.tar.gz` (binary + this `deploy/` folder) with
a `.sha256` file. The repository is private, so download both files from the
**Releases** page in your browser (logged in to GitHub), then copy them to the
LXC, e.g. from the Proxmox host:

```sh
# on the machine where you downloaded them → Proxmox host
scp kwartje-v0.1.0-x86_64-debian12.tar.gz* root@proxmox:/tmp/
# on the Proxmox host → into the container (120 = the container id)
pct push 120 /tmp/kwartje-v0.1.0-x86_64-debian12.tar.gz /tmp/kwartje.tar.gz
pct push 120 /tmp/kwartje-v0.1.0-x86_64-debian12.tar.gz.sha256 /tmp/kwartje.tar.gz.sha256
```

Inside the LXC:

```sh
cd /tmp
sed -i 's/kwartje-v[^ ]*\.tar\.gz$/kwartje.tar.gz/' kwartje.tar.gz.sha256
sha256sum -c kwartje.tar.gz.sha256          # kwartje.tar.gz: OK
tar xzf kwartje.tar.gz
mv kwartje-v*-x86_64-debian12/kwartje /tmp/kwartje
mv kwartje-v*-x86_64-debian12/deploy /tmp/kwartje-deploy
/tmp/kwartje --version
```

To publish a new version: bump `version` in `Cargo.toml` (via a pull request),
then tag the merged commit on `main`: `git tag v0.2.0 && git push origin v0.2.0`.
The workflow can also rebuild an existing tag from the Actions tab.

**Alternative: build yourself with Docker.**

```sh
git clone https://github.com/KJELD-LRNT/kwartje.git && cd kwartje
deploy/build.sh                # builds in rust:1-bookworm → target/release/kwartje
scp target/release/kwartje root@kwartje:/tmp/
scp -r deploy root@kwartje:/tmp/kwartje-deploy
```

## 4. Install (inside the LXC)

```sh
# system user and directories
useradd --system --home-dir /var/lib/kwartje --shell /usr/sbin/nologin kwartje
install -d -o kwartje -g kwartje -m 0750 /var/lib/kwartje /var/backups/kwartje
install -d -o root -g kwartje -m 0750 /etc/kwartje
install -d -m 0755 /usr/local/lib/kwartje /usr/local/share/doc/kwartje

# binary, scripts, units
install -m 0755 /tmp/kwartje /usr/local/bin/kwartje
cd /tmp/kwartje-deploy
install -m 0755 notify.sh restic-backup.sh restore.sh /usr/local/lib/kwartje/
install -m 0644 *.service *.timer /etc/systemd/system/
install -m 0644 INSTALL.md /usr/local/share/doc/kwartje/

# configuration
install -o root -g kwartje -m 0640 config.example.toml /etc/kwartje/config.toml
secret="$(openssl rand -base64 64 | tr -d '\n')"
sed -i "s|^session_secret = .*|session_secret = \"$secret\"|" /etc/kwartje/config.toml
grep -n session_secret /etc/kwartje/config.toml    # check it no longer says CHANGE-ME
```

Database and the first accounts (a beheerder and a medewerker). Run as `kwartje` so the files get the right owner:

```sh
runuser -u kwartje -- kwartje --config /etc/kwartje/config.toml migrate
runuser -u kwartje -- kwartje --config /etc/kwartje/config.toml create-user --name Anna --email anna@example.com --role beheerder
runuser -u kwartje -- kwartje --config /etc/kwartje/config.toml create-user --name Bram --email bram@example.com
```

Each user sets up TOTP (QR code) at their first login. Never run `seed` in production.
Further users (a boekhouder, for example) are easiest to create in the app:
*Instellingen → Beheer → Gebruikers*.

Start the app and publish it on the tailnet:

```sh
systemctl daemon-reload
systemctl enable --now kwartje.service
systemctl status kwartje.service
curl -sI http://127.0.0.1:8080/inloggen | head -1     # HTTP/1.1 200 OK

tailscale serve --bg 8080       # https://kwartje.<tailnet>.ts.net → http://127.0.0.1:8080
tailscale serve status
```

Open `https://kwartje.<tailnet>.ts.net` from a device on the tailnet and log in.

## 5. Backups

Local nightly backup at 02:00:

```sh
systemctl enable --now kwartje-backup.timer
systemctl start kwartje-backup.service        # run one now
ls /var/backups/kwartje/$(date +%F)            # app.db  csv/  documents/  manifest.json
journalctl -u kwartje-backup.service -n 20
```

Notifications: copy `notify.env.example` to `/etc/kwartje/notify.env` (mode 0600,
owner root) and fill in the ntfy topic. You can also add Uptime Kuma push URLs, using a
"Push" monitor with a heartbeat interval of about 25 h. Then a backup that silently
stops running also raises an alert.

```sh
install -m 0600 /tmp/kwartje-deploy/notify.env.example /etc/kwartje/notify.env
editor /etc/kwartje/notify.env
systemctl start kwartje-notify@test.service    # sends a test failure message
```

Offsite with restic at 03:00:

```sh
install -o root -g kwartje -m 0640 /tmp/kwartje-deploy/restic.env.example /etc/kwartje/restic.env
editor /etc/kwartje/restic.env
openssl rand -base64 32 > /etc/kwartje/restic-password
chown root:kwartje /etc/kwartje/restic-password && chmod 0640 /etc/kwartje/restic-password
# store this password in the password manager too: without it the offsite copy is useless
set -a; . /etc/kwartje/restic.env; set +a
runuser -u kwartje --preserve-environment -- restic init
systemctl enable --now kwartje-restic.timer
systemctl start kwartje-restic.service && journalctl -u kwartje-restic.service -n 20
```

## 5b. E-mail (optional)

To send invoices and reminders from the app, add an `[smtp]` section to
`/etc/kwartje/config.toml` (see `config.example.toml`). Put the password in a
separate file:

```sh
install -o root -g kwartje -m 0640 /dev/null /etc/kwartje/smtp-password
editor /etc/kwartje/smtp-password
systemctl restart kwartje.service
```

Without `[smtp]` everything still works: download the PDF, send it yourself and
click "Markeer als verzonden".

## 6. Updating

Get the new version into `/tmp/kwartje` (and `/tmp/kwartje-deploy`) as in
section 3: download the release, `pct push`, check the `.sha256`, unpack.

```sh
# in the LXC:
systemctl start kwartje-backup.service          # 1. fresh backup
# 2. dry run: the NEW binary restores that backup into a temporary folder,
#    applies its migrations there and checks the result. Live data untouched.
runuser -u kwartje -- /tmp/kwartje --config /etc/kwartje/config.toml restore-test --latest
# 3. only if that said "Hersteltest geslaagd":
install -m 0755 /tmp/kwartje /usr/local/bin/kwartje
systemctl restart kwartje.service                # migrations run at startup
journalctl -u kwartje.service -n 20             # look for "kopie gemaakt vóór databasemigratie"
```

**Once, when updating to the version with roles:** every existing user becomes a
medewerker. Make yourself beheerder, then manage the rest in the app:

```sh
runuser -u kwartje -- kwartje --config /etc/kwartje/config.toml set-role --email anna@example.com --role beheerder
```

When the new version has migrations, the app first writes a copy of the
database to `/var/lib/kwartje/pre-migration/app-<time>-v<old>-naar-v<new>.db`
(the newest three are kept). A failed migration rolls back by itself and the
service doesn't start; start the old binary again. A problem found later can be
undone by stopping the service and copying that file back to
`/var/lib/kwartje/app.db`. That loses changes made since the update, so for
anything older use the normal restore (section 7).

Installing an **older** binary over a newer database doesn't work: the app
refuses to start because the database has migrations it doesn't know. To go
back a version, restore the pre-migration copy together with the old binary.

## 7. Restore

```sh
/usr/local/lib/kwartje/restore.sh /var/backups/kwartje/2026-11-25
```

The script stops the service and checks the backup (manifest, SHA-256 sums,
`integrity_check`, schema version). It then moves the current `app.db` and `documents/`
to `/var/lib/kwartje/pre-restore-<timestamp>/` (nothing is deleted), copies the backup
in, and starts the service again. To fetch a backup from offsite first:

```sh
set -a; . /etc/kwartje/restic.env; set +a
restic snapshots --tag kwartje
restic restore latest --tag kwartje --target /tmp/restore --include /var/backups/kwartje/2026-11-25
/usr/local/lib/kwartje/restore.sh /tmp/restore/var/backups/kwartje/2026-11-25
```

**Automatic restore test:** every quarter (the 5th of January, April, July and
October at 04:00) `kwartje restore-test --latest` restores the newest backup into a
temporary folder, checks the checksums, row counts and documents, and boots the app
against it. The live data is not touched and the service keeps running. A failure
sends the usual notification; `KUMA_PUSH_RESTORE_TEST` in `notify.env` adds a
heartbeat.

```sh
systemctl enable --now kwartje-restore-test.timer
systemctl start kwartje-restore-test.service && journalctl -u kwartje-restore-test.service -n 20
```

Once a year, also restore the latest **offsite** (restic) snapshot this way, to
prove the offsite copy and its password work:

```sh
set -a; . /etc/kwartje/restic.env; set +a
restic restore latest --tag kwartje --target /tmp/restore
runuser -u kwartje -- kwartje --config /etc/kwartje/config.toml restore-test /tmp/restore/var/backups/kwartje/<datum>
rm -rf /tmp/restore
```

## 8. Monitoring

`https://kwartje.<tailnet>.ts.net/gezond` needs no login and shows no data. It
returns 200 (`ok`, plus the date of the last backup), or 503 when the database
doesn't answer or the newest backup is older than 36 hours. Add an Uptime Kuma
**HTTP** monitor on it (interval 5 min, from a machine on the tailnet).

## 9. Behind a reverse proxy with SSO (optional)

Instead of (or next to) `tailscale serve`, Kwartje can sit behind a reverse
proxy with an SSO gate (Authelia, Authentik, oauth2-proxy, Cloudflare Access,
Pomerium, …). Keep Kwartje's own login with TOTP behind it: the SSO gate is
an extra layer, so a proxy mistake never exposes the data.

Requirements:

- **Own (sub)domain**, e.g. `kwartje.example.nl`. The app lives at `/`; a
  path prefix like `/kwartje/` is not supported.
- **Only the proxy reaches the app.** With the proxy on the same host keep
  `listen = "127.0.0.1:8080"`. With the proxy elsewhere, bind to the LXC's own
  address (`listen = "10.0.0.20:8080"`, never `0.0.0.0`) and allow only the
  proxy in the firewall:
  ```sh
  nft add rule inet filter input ip saddr != 10.0.0.5 tcp dport 8080 drop
  ```
- **Tell Kwartje which proxy to believe**, so the login rate limit sees real
  clients (otherwise five wrong passwords by anyone lock everyone out):
  ```toml
  trusted_proxies = ["10.0.0.5"]   # every proxy hop on another host
  ```
- **Pass the original host**: keep `Host` or set `X-Forwarded-Host`, and set
  `X-Forwarded-For`. Kwartje compares the browser's `Origin` with them.
- **Uploads up to 20 MB**: allow request bodies of at least 25 MB (nginx's
  default is 1 MB).
- **`/gezond` without SSO**, for Uptime Kuma. It needs no login and shows no
  data. Or monitor it over the tailnet instead.
- **SSO session at least 12 hours.** When the SSO session expires, the app's
  background requests (tariff suggestion, km proposal) get redirected to the
  identity provider and silently fail until the page is reloaded.
- **HSTS** from the proxy (`Strict-Transport-Security: max-age=31536000`).
- Keep `cookie_secure = true`.

Example with Caddy and Authelia (forward auth):

```caddy
kwartje.example.nl {
    header Strict-Transport-Security "max-age=31536000"
    request_body {
        max_size 25MB
    }
    @health path /gezond
    handle @health {
        reverse_proxy 10.0.0.20:8080
    }
    handle {
        forward_auth authelia:9091 {
            uri /api/authz/forward-auth
            copy_headers Remote-User Remote-Groups Remote-Email Remote-Name
        }
        reverse_proxy 10.0.0.20:8080
    }
}
```

Caddy keeps `Host` and sets `X-Forwarded-For` by itself. With nginx, set them
explicitly:

```nginx
client_max_body_size 25m;
proxy_set_header Host $host;
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
proxy_set_header X-Forwarded-Proto $scheme;
```

Check afterwards: log in through the proxy and upload a document. Then, from
one device, try to log in five times with a made-up e-mail address (the limit
also counts per account, so don't use a real one), and confirm that a second
device on another network can still log in normally. That proves
`trusted_proxies` works: without it, both devices would share the proxy's
address and the second one would be locked out too.

## 9b. Example: Pangolin (next to Tailscale)

[Pangolin](https://docs.pangolin.net) is a tunnelled reverse proxy: the
Pangolin server (usually on a VPS) has the public domain, TLS and the login
(Pangolin SSO, an external identity provider, PIN, e-mail code, …), and a small
connector called **Newt** in your network dials out to it. Nothing in your
network has to accept incoming connections. Tailscale keeps working next to it.

Recommended layout: **run Newt inside the Kwartje LXC**. Then Pangolin reaches
the app on `127.0.0.1:8080`, the app keeps listening on loopback only, and no
`trusted_proxies` entry or firewall rule is needed (loopback is always trusted).

1. In Pangolin, create a **site** for the LXC (type *Newt*). Pangolin shows the
   install command with the site's id and secret; run it **inside the Kwartje
   LXC** (see Pangolin's *Install Sites* page for the Linux/systemd variant) and
   check in Pangolin that the site shows as online. Treat the Newt secret like a
   password: it stays on the LXC, never in git.
2. Create a public **HTTP resource**, e.g. `kwartje.example.nl`, on that site,
   with one target: method `http`, address `127.0.0.1` (or `localhost`), port
   `8080`. Keep the Host header as is (don't set a custom one).
3. Under the resource's **authentication**, keep Pangolin SSO on and allow only
   your own users (users/roles or an e-mail whitelist). Kwartje's own login
   with TOTP stays on behind it.
4. Leave `listen = "127.0.0.1:8080"` and `cookie_secure = true` in
   `/etc/kwartje/config.toml`. No other change is needed.
5. Monitoring: point Uptime Kuma at `/gezond` **over the tailnet**
   (`https://kwartje.<tailnet>.ts.net/gezond`), so Pangolin needs no exception
   for it.
6. Check (as in section 9): log in through the Pangolin URL, upload a
   document, then look at `journalctl -u kwartje.service | grep "failed login"`
   after one deliberate wrong password: the `ip=` must be your public IP, not
   `127.0.0.1`. If it shows `127.0.0.1`, the proxy isn't passing
   `X-Forwarded-For`; then all users share one rate-limit bucket. Tell the
   maintainer before relying on it.

If Newt has to run on another machine instead, use section 9 as written: bind
Kwartje to the LXC's own address, allow only the Newt machine in the firewall
and list its address in `trusted_proxies`.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Service won't start | `journalctl -u kwartje -n 50`. Look for a placeholder `session_secret`, wrong permissions on `/etc/kwartje/config.toml` (must be readable by group `kwartje`), or `data_dir` not owned by `kwartje`. |
| "Geen toegang" on every form | Clear the browser cookies for the site. Make sure you use the `https://….ts.net` URL, not `http://` (the session cookie is `Secure`). |
| Locked out after failed logins | 5 failures within 15 minutes lock that account and that IP. It clears by itself 15 minutes after the failures, or at once with `systemctl restart kwartje` (the rate limiter is in memory). |
| Lost phone / TOTP | `runuser -u kwartje -- kwartje --config /etc/kwartje/config.toml reset-user --email … --totp`, then set it up again at the next login. |
| Forgotten password | `… reset-user --email … --password` |
| Lost or stolen device that was trusted ("MFA 30 dagen onthouden") | On another device: *Mijn account* → *Vergeten* (or *Alle apparaten vergeten*). Without access: `… reset-user --email … --password`; a new password forgets all trusted devices. The password is still needed on a trusted device. |
| Backup fails | `journalctl -u kwartje-backup -n 50`. Check disk space with `df -h /var/backups`. |
