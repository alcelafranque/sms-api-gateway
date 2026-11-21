# sms-api-gateway

An HTTP gateway for sending and receiving SMS through a SIM card in a
TP-Link NE200-Outdoor 5G CPE. Applications queue SMS through a REST API and
receive inbound SMS and send outcomes through signed webhooks.
Messages are stored in PostgreSQL.

The NE200-Outdoor has no official API, no SSH and no AT command access: its
only interface is its web UI. The gateway drives the device the way that
web UI does, by speaking its encrypted `/cgi_gdpr` protocol, and turns it
into a reliable API: one serialized device session, a persistent queue,
retries only when they are safe, and inbound polling with deduplication.

## Features

- **REST API**: per-app API keys, `Idempotency-Key` support, message history
  with cursor pagination, OpenAPI document and Swagger UI.
- **Outbound queue** in PostgreSQL, with pacing between sends and retries
  only when nothing reached the device. A send whose outcome cannot be known
  ends as `unknown` and is never sent twice.
- **Inbound SMS**: polling, deduplication, deletion from the device once
  stored (its inbox is small), and routing to an app by keyword, by reply
  to a recent outbound SMS, or to a default app.
- **Webhooks** for received, sent, failed and unknown messages, signed with
  HMAC-SHA256, retried with backoff, every attempt logged.
- **Validation**: E.164 normalization, allowed/denied prefixes, GSM-7/UCS-2
  segment counting.
- **Operations**: pause/resume to free the device session for its web UI
  (persisted across restarts), Prometheus metrics, health and readiness
  probes, JSON logs, masked phone numbers, retention purge of texts and old
  records.
- **Mock modem** with fault injection, for development without hardware.
- [**`tplink-ex`**](crates/tplink-ex): a standalone async Rust driver for
  the device protocol, with a `probe` command-line tool.

## Status and compatibility

Version 0.1: usable, but the API may still change before 1.0. The device
protocol is unofficial, so a firmware update can break it.

| Device | Status |
|---|---|
| TP-Link NE200-Outdoor v1 | Verified on a live device: login, sending with send result, inbox listing, mark as read, delete, radio status. |
| Other TP-Link EX/MR-family CPEs and 5G/LTE routers (EX511, EX920, MR series...) | May work; untested. Reports are welcome. |

| Session cipher variant | Support |
|---|---|
| AES-128-CBC, raw RSA signature | Supported; used by the NE200-Outdoor v1 (verified). |
| AES-128-GCM, raw RSA signature | Supported; tested against a simulated device. |
| AES-128-GCM, RSA-OAEP signature | Supported; tested with known-answer vectors. |

The variant is detected at login (`modem.tplink.variant = "auto"`).

## Architecture

```text
      REST + API key                                  signed webhooks
  apps ──────────┐                                  ┌─────────▶ apps
                 ▼                                  │
┌─────────────────────────── sms-gateway ───────────────────────────┐
│                                                                   │
│   axum API ──────────▶ PostgreSQL ◀────────── webhook dispatcher  │
│                         ▲        ▲                                │
│           outbound worker        inbound poller                   │
│                         │        │                                │
│        ModemHandle: one serialized device session, pause          │
│                              │                                    │
└──────────────────────────────┼────────────────────────────────────┘
                               │ HTTP, /cgi_gdpr (AES + RSA)
                               ▼
                 NE200-Outdoor ── SIM ── mobile network
```

| Crate | Role |
|---|---|
| [`crates/tplink-ex`](crates/tplink-ex) | Device driver: session crypto, login, acts, SMS and status operations. |
| [`crates/modem`](crates/modem) | `SmsModem` trait, TP-Link backend and in-memory mock. |
| [`crates/gateway`](crates/gateway) | The `sms-gateway` binary: API, storage, workers, webhooks, CLI. |

[docs/architecture.md](docs/architecture.md) describes the components, the
outbound state machine, the inbound pipeline and the data model.

## Quick start

### With containers

[`compose.yaml`](compose.yaml) runs PostgreSQL and the gateway:

```sh
cp .env.example .env      # set POSTGRES_PASSWORD, MODEM_URL, MODEM_PASSWORD
docker compose up -d      # or: podman compose up -d
docker compose run --rm sms-gateway app create billing --webhook-url https://billing.example.com/hooks/sms
```

Set `MODEM_KIND=mock` in `.env` to try it without hardware. See
[docs/deployment.md](docs/deployment.md) for the image, Kubernetes, probes
and network requirements.

### From source

Requirements: a recent stable Rust toolchain (edition 2024) and PostgreSQL.

```sh
cargo build --release -p gateway                 # target/release/sms-gateway
cp config.example.toml config.toml               # then edit it

export SMS_GATEWAY__DATABASE__URL=postgres://sms:secret@localhost:5432/sms
export SMS_GATEWAY__MODEM__TPLINK__PASSWORD=secret
# export SMS_GATEWAY__MODEM__KIND=mock           # no hardware: nothing is sent

./target/release/sms-gateway migrate
./target/release/sms-gateway serve
```

### Send a first SMS

Create an app. Its API key is printed once; only a hash is stored:

```sh
sms-gateway app create billing --webhook-url https://billing.example.com/hooks/sms
# api key:        sgw_0123456789ab_...
# webhook secret: whsec_...
```

Queue a message:

```sh
curl -sS -X POST http://localhost:8080/v1/messages \
  -H "Authorization: Bearer $SMS_API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: order-42-shipped' \
  -d '{"to": "+33600000000", "text": "Your order 42 has shipped.", "client_ref": "order-42"}'
```

The answer is `202 Accepted` with the message in status `queued`. Follow it
with `GET /v1/messages/{id}` or the webhooks: it ends as `sent` (the
device confirmed it left), `submitted` (accepted without confirmation),
`failed`, or `unknown` (outcome unknowable, never re-sent).

The API is described in [docs/api.md](docs/api.md), and served by the
gateway itself at `/openapi.json` with a Swagger UI at `/docs`.

### Receive webhooks

Events `message.received`, `message.sent`, `message.failed` and
`message.unknown` are POSTed as JSON to the app's webhook URL:

```text
{"id": "<event id>", "type": "message.received", "created_at": "<RFC 3339>", "data": <message object>}
```

Each request carries `X-Webhook-Id` (constant across retries, for
deduplication), `X-Event-Type`, `X-Timestamp` and
`X-Signature: sha256=<hex HMAC-SHA256(secret, "<X-Timestamp>.<raw body>")>`.
Any `2xx` acknowledges the event; anything else is retried for up to 24
hours. See [verifying the signature](docs/api.md#verifying-the-signature)
for a Python example.

Inbound SMS are routed to the app owning the first word as a keyword
(`--keyword PAY`), else to the app that last sent an SMS to that number
within 72 hours, else to `inbound.default_app`.

## Configuration

Copy [`config.example.toml`](config.example.toml), which lists every key
with its default. Any key can be overridden by an environment variable
`SMS_GATEWAY__<SECTION>__<KEY>`, which is the recommended way to pass
secrets. [docs/configuration.md](docs/configuration.md) documents every
setting and the SMS length limits (765 GSM-7 or 335 UCS-2 characters by
default).

## Testing a device with `probe`

The `probe` example of `tplink-ex` talks to a device directly, without the
gateway or a database. Stop or [pause](docs/api.md#pause-and-resume) the
gateway first: the device accepts a single session.

```sh
export TPLINK_URL=http://192.168.254.1 TPLINK_USER=user TPLINK_PASSWORD=...
cargo run -p tplink-ex --example probe -- login        # detect the cipher variant
cargo run -p tplink-ex --example probe -- status -v    # radio status, raw objects
cargo run -p tplink-ex --example probe -- inbox
cargo run -p tplink-ex --example probe -- send +33600000000 'Test from probe'
```

Other commands: `read <index>`, `delete <index>`, and
`raw <op> <oid> [attrs]` for any protocol act. What is known about the
protocol is written up in [docs/ne200-protocol.md](docs/ne200-protocol.md).

## Development

```sh
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude gateway && cargo test -p gateway --lib   # no database needed
```

The gateway's integration tests (`crates/gateway/tests/db`) use
`#[sqlx::test]`, which creates a temporary database per test. They need
`DATABASE_URL` pointing to a PostgreSQL role allowed to create databases:

```sh
podman run -d --rm --name sms-test-db -e POSTGRES_PASSWORD=postgres \
  -p 5432:5432 docker.io/library/postgres:18
DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test --workspace
```

The driver is tested against an in-process fake device, so no hardware is
needed. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security notes

- The device link is weak by design: plain HTTP and a 512-bit RSA key.
  Keep the device on an isolated network that only the gateway can reach,
  and never expose its web UI.
- The device password, the database URL and the webhook secrets (stored in
  clear in the database, since they are needed to sign) are sensitive. API
  keys are stored as SHA-256 hashes and shown once.
- The API speaks plain HTTP: put it behind a TLS reverse proxy when it
  crosses an untrusted network. The dedicated metrics listener is
  unauthenticated.
- SMS texts are not logged and phone numbers are masked, except at `trace`
  level for `tplink_ex`. A retention job erases texts after 30 days and
  deletes messages after a year by default.

Report vulnerabilities as described in [SECURITY.md](SECURITY.md).

## Acknowledgements

The device protocol (endpoints, object names, message formats, encryption
scheme, return codes) was reverse-engineered by Alex Erohin and the
contributors of [`tplinkrouterc6u`](https://github.com/AlexandrErohin/TP-Link-Archer-C6U)
(GPL-3.0). `tplink-ex` is a separate Rust implementation built on that work.

## Disclaimer

- This is an independent project. It is not affiliated with, endorsed by
  or supported by TP-Link.
- "TP-Link" and "NE200" are trademarks of their respective owners, used
  here only to identify compatible hardware.
- The driver talks to the device's local web management interface with the
  device owner's own credentials. It was built, for interoperability
  purposes, only by observing that interface's network traffic and the web
  UI served to the browser, and from the open-source `tplinkrouterc6u`
  project.
- No TP-Link firmware or web UI code is included in this repository.
- Use it at your own risk: it may stop working after a firmware update.
  You are responsible for complying with your mobile operator's terms,
  including any terms on automated or bulk SMS.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). The changelog is in
[CHANGELOG.md](CHANGELOG.md).
