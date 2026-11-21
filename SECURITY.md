# Security policy

## Supported versions

Security fixes go to the latest release and to the `main` branch.

| Version | Supported |
|---|---|
| 0.1.x | Yes |

## Reporting a vulnerability

Please do not open a public issue. Report vulnerabilities privately through
GitHub: on the repository page, open the **Security** tab and choose
**Report a vulnerability**
(<https://github.com/alcelafranque/sms-api-gateway/security/advisories/new>).

Include the affected version or commit, the component (`gateway`,
`tplink-ex`, `modem`), the configuration involved (with secrets removed),
and steps to reproduce. Reports are acknowledged as soon as possible. Fixes
are released and disclosed through a GitHub security advisory.

Vulnerabilities of the device firmware itself should be reported to
TP-Link. Issues in how this project uses the device protocol are in scope.

## Sensitive data

| Data | Where | Notes |
|---|---|---|
| Device web UI password | Configuration / environment | Gives full control of the device. Pass it through the environment or a secret store. |
| Database URL | Configuration / environment | Contains the database password. |
| API keys | Printed once by the CLI | Only their SHA-256 is stored. Rotate with `sms-gateway app rotate-key`. |
| Webhook secrets | Database (`apps.webhook_secret`), in clear | Needed to sign webhooks, so anyone with database access can forge webhooks. Rotate with `app update --rotate-webhook-secret`. |
| SMS texts and phone numbers | Database, webhook payloads | Personal data, often including one-time codes. Purged by the retention job (`retention.*`). |

Protect the database and its backups accordingly, and restrict who can run
the `sms-gateway` CLI against it.

## Deployment recommendations

- **Isolate the device.** Its web protocol uses plain HTTP, an MD5 hash of
  the credentials and a 512-bit RSA key, which do not resist an attacker on
  the same network. Put the device on a dedicated network segment (VLAN or
  direct link) that only the gateway can reach, never forward or expose its
  web UI, and change its default password.
- **Protect the API.** The gateway speaks plain HTTP: put it behind a TLS
  reverse proxy when the API crosses an untrusted network, and bind it to
  an internal address otherwise. Give admin keys (pause, metrics, dev
  routes) to operators only.
- **Metrics.** The dedicated `metrics.listen` listener has no
  authentication; bind it to an internal address.
- **Webhooks.** Receivers should verify `X-Signature`, reject stale
  `X-Timestamp` values and deduplicate on `X-Webhook-Id` (see
  [docs/api.md](docs/api.md#verifying-the-signature)). Webhook URLs are set
  by operators through the CLI, never by API clients. Redirects are not
  followed.
- **Logs.** SMS texts are not logged and phone numbers are masked, but the
  `tplink_ex` driver logs decrypted device replies, which contain texts and
  numbers, at `trace` level. Do not enable it in production.
- **Mock modem.** `modem.kind = "mock"` enables development routes that
  inject messages and faults. Use it for development only.
