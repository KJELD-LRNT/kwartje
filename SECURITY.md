# Security policy

Kwartje holds business data: clients, hours, invoices, documents. Security
reports are taken seriously and handled privately.

## Reporting a vulnerability

**Please don't open a public issue.** Report it privately through GitHub:

1. Go to the [Security tab](https://github.com/KJELD-LRNT/kwartje/security) of the repository.
2. Choose **Report a vulnerability**. Only the maintainers can see what you write.

Please include:

- the version (`kwartje --version`) and how it is installed
- what an attacker can do, and the steps to reproduce it
- anything you already know about a fix

You can expect:

- an acknowledgement within a week
- an assessment and a plan after that
- a fix in a new release

You will be credited in the release notes unless you prefer not to be.

## Supported versions

Only the **latest release** gets security fixes. Upgrading is described in
[deploy/INSTALL.md](deploy/INSTALL.md#6-updating).

## Scope and design

Kwartje is designed to run **inside a private network**: a Tailscale tailnet, or
behind a reverse proxy with its own sign-in such as Pangolin SSO. The following
are by design, not vulnerabilities:

- the web server only listens on `127.0.0.1`
- `GET /gezond` answers without a login (it shows only health and the date of the
  last backup)

Defences you can rely on:

- argon2 password hashes and mandatory TOTP
- login rate limiting
- CSRF tokens plus Origin and Sec-Fetch checks
- secure session cookies and a strict Content Security Policy
- an append-only audit log and security log
- server-side role checks, so a read-only account can't write

Reports about any of these are very welcome.
