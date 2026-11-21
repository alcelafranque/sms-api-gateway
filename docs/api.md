# HTTP API

The API is versioned under `/v1`. Requests and responses are JSON. The
gateway serves its OpenAPI 3 document at `/openapi.json` and, unless
`server.swagger_ui = false`, a Swagger UI at `/docs`. The development
routes are not part of the OpenAPI document.

- [Authentication](#authentication)
- [Managing apps (CLI)](#managing-apps-cli)
- [Errors](#errors)
- [Messages](#messages)
  - [The message object](#the-message-object)
  - [Send an SMS](#send-an-sms)
  - [Get a message](#get-a-message)
  - [List messages](#list-messages)
- [Modem](#modem)
- [Health and metrics](#health-and-metrics)
- [Webhooks](#webhooks)
  - [Verifying the signature](#verifying-the-signature)
- [Development routes](#development-routes)

Examples assume the gateway listens on `http://localhost:8080` and that
`$SMS_API_KEY` holds an API key. Phone numbers are placeholders.

## Authentication

Every `/v1` route requires a per-app API key:

```http
Authorization: Bearer sgw_0123456789ab_<secret>
```

Keys look like `sgw_<12 hex characters>_<43 base64url characters>`. The
first part is a public lookup id; the gateway stores only the SHA-256 of
the whole key and compares it in constant time.

| Case | Response |
|---|---|
| Missing, malformed or unknown key | `401 unauthorized`, with `WWW-Authenticate: Bearer` |
| Key of a disabled app | `403 forbidden` (`app disabled`) |
| Admin route called by a non-admin app | `403 forbidden` (`admin privileges required`) |

Each app only sees its own messages: the ones it sent and the inbound ones
routed to it. Admin apps can additionally pause and resume the modem, read
`/metrics` on the main listener and use the development routes.

Every response carries an `X-Request-Id` header (the one sent by the
client, or a generated UUID), also present in the gateway logs.

## Managing apps (CLI)

Apps are managed with the `sms-gateway` binary, which needs the database
configuration (`database.url`) but not the modem:

```sh
sms-gateway app create billing \
    --webhook-url https://billing.example.com/hooks/sms --keyword PAY
sms-gateway app create ops --admin
sms-gateway app list
sms-gateway app update billing --keywords PAY,INVOICE      # replace keywords ("" clears them)
sms-gateway app update billing --webhook-url https://billing.example.com/v2/sms
sms-gateway app update billing --rotate-webhook-secret
sms-gateway app update billing --no-webhook
sms-gateway app update ops --admin false
sms-gateway app rotate-key billing
sms-gateway app disable billing                            # app enable billing
```

`app create` prints the API key, and the webhook secret when
`--webhook-url` is given:

```text
app:            billing
id:             019a8c5e-7f3a-7c21-9b4e-2f1d6c0a9e11
admin:          false
api key:        sgw_0123456789ab_<secret>
webhook secret: whsec_<64 hex characters>

Store these secrets now: the API key cannot be shown again.
```

The API key is shown once and cannot be recovered; `rotate-key` replaces
it and the previous key stops working immediately. `update` prints a new
webhook secret when it generates one (`--rotate-webhook-secret`, or a
webhook URL set on an app that had none). Keywords are case-insensitive
and stored upper-case; `--keyword` is repeatable and accepts
comma-separated values. Webhook URLs must be `http` or `https`.

## Errors

Every error has the same shape:

```json
{"error": {"code": "validation_failed", "message": "to: not a valid phone number"}}
```

`code` is stable and meant for programs; `message` is for humans and may
change.

| Status | `code` | When |
|---|---|---|
| 400 | `bad_request` | Malformed query string or path, unknown query parameter, invalid `cursor`, `limit` out of range, invalid `Idempotency-Key`. |
| 401 | `unauthorized` | Missing or invalid API key. |
| 403 | `forbidden` | App disabled, or admin privileges required. |
| 404 | `not_found` | Unknown route, or a message that does not exist or belongs to another app. |
| 405 | `method_not_allowed` | Wrong method for the route. |
| 408 | `timeout` | The request took longer than `server.request_timeout_secs`. |
| 409 | `idempotency_conflict` | `Idempotency-Key` already used with a different request. |
| 413 | `payload_too_large` | Body larger than `server.body_limit_bytes` (64 KiB by default). |
| 415 | `unsupported_media_type` | Body without `Content-Type: application/json`. |
| 422 | `validation_failed` | Invalid JSON body (syntax, missing or unknown field), invalid recipient, empty or too long text, invalid `client_ref`. |
| 422 | `destination_not_allowed` | Recipient refused by `validation.allowed_prefixes` / `denied_prefixes`. |
| 500 | `internal_error` | Unexpected server error (details are in the logs only). |

## Messages

### The message object

Outbound and inbound messages share one representation, also used as the
`data` of webhook events:

```json
{
  "id": "019a8c5e-7f3a-7c21-9b4e-2f1d6c0a9e11",
  "direction": "outbound",
  "status": "sent",
  "to": "+33600000000",
  "from": null,
  "text": "Your order 42 has shipped.",
  "text_purged_at": null,
  "encoding": "gsm7",
  "segments": 1,
  "client_ref": "order-42",
  "attempts": 1,
  "error": null,
  "next_attempt_at": null,
  "sent_at": "2026-11-21T14:03:15.104233Z",
  "received_at": null,
  "routed_by": null,
  "created_at": "2026-11-21T14:03:12.512870Z",
  "updated_at": "2026-11-21T14:03:15.104233Z"
}
```

| Field | Type | Description |
|---|---|---|
| `id` | UUID | Message id (UUIDv7, so ids sort by creation time). |
| `direction` | string | `outbound` or `inbound`. |
| `status` | string | Outbound: `queued`, `sending`, `sent`, `submitted`, `failed`, `unknown`. Inbound: `received`. |
| `to` | string or null | Recipient, E.164 (outbound only). |
| `from` | string or null | Sender (inbound only): E.164 when the device reports a phone number, otherwise as reported (alphanumeric sender, short code). |
| `text` | string or null | Text; `null` once purged by the retention policy. |
| `text_purged_at` | timestamp or null | When the text was purged. |
| `encoding` | string or null | `gsm7` or `ucs2` (outbound, while the text is kept). |
| `segments` | integer or null | Number of SMS segments the text is sent in (outbound, while the text is kept). |
| `client_ref` | string or null | The reference given at creation. |
| `attempts` | integer | Send attempts so far (outbound; `0` for inbound). |
| `error` | string or null | Last error (outbound). Informational, not a stable format. |
| `next_attempt_at` | timestamp or null | Next attempt while a retry is scheduled (status `queued` only). |
| `sent_at` | timestamp or null | When the device sent or accepted the message (outbound). |
| `received_at` | timestamp or null | Reception time reported by the device, converted to UTC with `modem.timezone` (inbound). |
| `routed_by` | string or null | Inbound routing rule: `keyword`, `reply`, `default` (or `none`, never visible through the API). |
| `created_at`, `updated_at` | timestamp | RFC 3339, UTC. |

Outbound statuses:

| Status | Meaning |
|---|---|
| `queued` | Waiting to be sent, or waiting for a retry. |
| `sending` | Being sent. |
| `sent` | The device confirmed the message left it. |
| `submitted` | The device accepted the message without confirming it left. |
| `failed` | The device refused it, or it stayed unreachable for `outbound.max_attempts` attempts. |
| `unknown` | The outcome cannot be known (for example a timeout after the request reached the device). The gateway never re-sends such a message. |

`sent`, `submitted`, `failed` and `unknown` are final. Neither `sent` nor
`submitted` means the handset received the message. See
[architecture.md](architecture.md#outbound-messages) for when retries
happen.

### Send an SMS

```http
POST /v1/messages
```

| Body field | Required | Description |
|---|---|---|
| `to` | yes | Recipient: E.164 (`+33600000000`), `00`-prefixed international, or a national number in `validation.default_region` (`06 00 00 00 00`). Spaces and punctuation are accepted. |
| `text` | yes | Message text, not blank, without NUL characters, at most `validation.max_segments` SMS segments (see [SMS length](configuration.md#sms-length)). |
| `client_ref` | no | Free-form reference, at most 128 characters, echoed in the message and its webhooks. |

| Header | Description |
|---|---|
| `Idempotency-Key` | Optional. 1 to 255 visible ASCII characters, unique per app. |

```sh
curl -sS -X POST http://localhost:8080/v1/messages \
  -H "Authorization: Bearer $SMS_API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: order-42-shipped' \
  -d '{"to": "+33600000000", "text": "Your order 42 has shipped.", "client_ref": "order-42"}'
```

Response: `202 Accepted` with the message (status `queued`) and a
`Location: /v1/messages/{id}` header. The send is asynchronous: follow it
with `GET /v1/messages/{id}` or the `message.*` webhooks. Messages are
accepted while the modem is paused or unreachable; they wait in the queue.

The recipient is normalized to E.164 before the prefix policy is applied,
so `0899...` and `+33899...` are treated alike.

#### Idempotency

With an `Idempotency-Key`, a client can safely retry a request whose
response it did not get:

- First request: the message is created (`202`).
- Same key, same `to` (after normalization), `text` and `client_ref`: no new
  message; the response is `202` with the existing message in its current
  state and the header `Idempotent-Replayed: true`.
- Same key, different payload: `409 idempotency_conflict`.

Keys are scoped to the calling app and kept as long as the message exists
(see `retention.message_days`).

### Get a message

```http
GET /v1/messages/{id}
```

```sh
curl -sS http://localhost:8080/v1/messages/019a8c5e-7f3a-7c21-9b4e-2f1d6c0a9e11 \
  -H "Authorization: Bearer $SMS_API_KEY"
```

Returns `200` with the message, or `404 not_found` when it does not exist
or belongs to another app. A malformed id is a `400`.

### List messages

```http
GET /v1/messages
```

| Query parameter | Description |
|---|---|
| `direction` | `outbound` or `inbound`. |
| `status` | One status. |
| `since` | Only messages created at or after this time (RFC 3339). |
| `until` | Only messages created before this time (RFC 3339). |
| `order` | `desc` (default, newest first) or `asc`, by creation time. |
| `limit` | Page size, 1 to 500 (default 50). |
| `cursor` | `next_cursor` of the previous page. |

Unknown parameters are rejected with `400`.

```sh
curl -sS -G http://localhost:8080/v1/messages \
  -H "Authorization: Bearer $SMS_API_KEY" \
  --data-urlencode direction=inbound \
  --data-urlencode since=2026-11-01T00:00:00Z \
  --data-urlencode limit=100
```

```json
{
  "items": [
    {
      "id": "019a8c61-02b7-7d40-a1c3-5e8f0b2d4c6a",
      "direction": "inbound",
      "status": "received",
      "to": null,
      "from": "+33600000000",
      "text": "PAY 42",
      "text_purged_at": null,
      "encoding": null,
      "segments": null,
      "client_ref": null,
      "attempts": 0,
      "error": null,
      "next_attempt_at": null,
      "sent_at": null,
      "received_at": "2026-11-21T14:05:40Z",
      "routed_by": "keyword",
      "created_at": "2026-11-21T14:05:52.208113Z",
      "updated_at": "2026-11-21T14:05:52.208113Z"
    }
  ],
  "next_cursor": "eyJ0IjoiMjAyNi0xMS0yMVQxNDowNTo1Mi4yMDgxMTNaIiwiaWQiOiIwMTlhOGM2MS0wMmI3LTdkNDAtYTFjMy01ZThmMGIyZDRjNmEifQ"
}
```

Pagination is keyset-based on `(created_at, id)`, so it is stable while new
messages arrive. `next_cursor` is `null` on the last page. Pass it back
unchanged as `cursor`, with the same filters and `order` as the first
request: the cursor only encodes the position.

## Modem

### Status

```http
GET /v1/modem
GET /v1/modem?refresh=true
```

Any authenticated app. Returns the cached radio status, the session state,
the pause state and the queue depth. `refresh=true` reads the radio status
from the device first, unless the device is busy for more than 5 s (the
cached value is returned then) or paused.

```json
{
  "backend": "tplink",
  "paused": false,
  "paused_until": null,
  "session": {
    "state": "ok",
    "last_ok_at": "2026-11-21T14:06:01.330112Z",
    "last_error": null,
    "last_error_at": null,
    "consecutive_failures": 0
  },
  "status": {
    "operator": "Operator",
    "signal_level": 4,
    "rsrp_dbm": -95,
    "rsrq_db": -11,
    "snr_db": 12.0,
    "unread_count": 0,
    "inbox_total": 0,
    "updated_at": "2026-11-21T14:05:45.018724Z"
  },
  "last_poll_at": "2026-11-21T14:06:01.330090Z",
  "queue": {"queued": 0, "sending": 0, "webhooks_pending": 0}
}
```

| Field | Description |
|---|---|
| `backend` | `tplink` or `mock`. |
| `paused`, `paused_until` | Pause state; `paused_until` is set for a timed pause. |
| `session.state` | `idle` (no device call yet), `ok` (last call succeeded), `error` (last call failed), `paused`. |
| `session.last_error` | Last device error, with `last_error_at` and the number of consecutive failures. |
| `status` | Last radio status read, `null` until the first read. Every field except `unread_count` and `updated_at` may be `null`. `snr_db` assumes the device reports tenths of dB, which is not verified. |
| `last_poll_at` | Last successful inbox poll. |
| `queue` | Outbound messages `queued` (including scheduled retries) and `sending`, and pending webhook deliveries (all apps). |

### Pause and resume

```http
POST /v1/modem/pause
POST /v1/modem/pause?duration_secs=900
POST /v1/modem/resume
```

Admin apps only. Pausing waits for the in-flight device call, stops all
device access and logs out from the device, so that someone can use its web
UI. Queued messages stay queued; inbound SMS stay on the device until
resume. The pause survives restarts. With `duration_secs`, the modem resumes
on its own after that many seconds. Both return the [status](#status)
object.

```sh
curl -sS -X POST 'http://localhost:8080/v1/modem/pause?duration_secs=900' \
  -H "Authorization: Bearer $SMS_ADMIN_KEY"
```

## Health and metrics

No authentication, except `/metrics` on the main listener.

| Route | Response |
|---|---|
| `GET /healthz` | Liveness: always `200 {"status": "ok"}` while the process runs. |
| `GET /readyz` | Readiness: `200` when the database answers and the last device call succeeded or the modem is paused, `503` otherwise. Body: `{"status": "ok" \| "unavailable", "database": "ok" \| "error", "modem": "<session state>"}`. Right after start-up the session is `idle` and the gateway is not ready until its first successful device call. |
| `GET /metrics` | Prometheus text format. Served unauthenticated on `metrics.listen` when set; otherwise on the main listener to admin apps only. |

Metrics:

| Name | Type | Labels | Description |
|---|---|---|---|
| `sms_outbound_queue_depth` | gauge | | Outbound messages waiting to be sent. |
| `sms_outbound_sends_total` | counter | `outcome` (`sent`, `submitted`, `retry`, `failed`, `unknown`) | Send attempts by outcome. |
| `sms_inbound_messages_total` | counter | `routed_by` | Inbound messages stored. |
| `sms_inbound_last_successful_poll_timestamp_seconds` | gauge | | Unix time of the last successful inbox poll. |
| `sms_webhook_deliveries_total` | counter | `outcome` (`delivered`, `retry`, `failed`, `dropped`) | Webhook delivery attempts by outcome. |
| `sms_webhook_queue_depth` | gauge | | Pending webhook deliveries. |
| `sms_modem_signal_level` | gauge | | Signal level reported by the device. |
| `sms_modem_rsrp_dbm` | gauge | | RSRP reported by the device. |
| `sms_modem_inbox_total` | gauge | | Messages stored in the device inbox. |
| `sms_modem_errors_total` | counter | `op`, `kind` | Device call errors. |
| `sms_modem_paused` | gauge | | `1` while the modem is paused. |
| `sms_retention_purged_total` | counter | `kind` | Rows touched by the retention job. |

Useful alerts: a stale `sms_inbound_last_successful_poll_timestamp_seconds`,
a growing `sms_outbound_queue_depth`, any increase of
`sms_outbound_sends_total{outcome="unknown"}`, and `sms_modem_inbox_total`
close to the device capacity.

## Webhooks

An app created with `--webhook-url` receives events as HTTP `POST`
requests.

| Event | Sent when |
|---|---|
| `message.received` | An inbound SMS was stored and routed to the app. |
| `message.sent` | An outbound SMS of the app reached status `sent` or `submitted` (check `data.status`). |
| `message.failed` | An outbound SMS ended in `failed`. |
| `message.unknown` | An outbound SMS ended in `unknown`. |

No event is sent for `queued`, `sending` or scheduled retries.

Request:

```http
POST /hooks/sms HTTP/1.1
Content-Type: application/json
User-Agent: sms-gateway/0.1.0
X-Webhook-Id: 019a8c5e-8c01-7b55-8d2e-61a7f4b39c02
X-Event-Type: message.sent
X-Timestamp: 1795269795
X-Signature: sha256=5d0c3e4f...
```

```json
{
  "id": "019a8c5e-8c01-7b55-8d2e-61a7f4b39c02",
  "type": "message.sent",
  "created_at": "2026-11-21T14:03:15.108410Z",
  "data": {
    "id": "019a8c5e-7f3a-7c21-9b4e-2f1d6c0a9e11",
    "direction": "outbound",
    "status": "sent",
    "to": "+33600000000",
    "from": null,
    "text": "Your order 42 has shipped.",
    "text_purged_at": null,
    "encoding": "gsm7",
    "segments": 1,
    "client_ref": "order-42",
    "attempts": 1,
    "error": null,
    "next_attempt_at": null,
    "sent_at": "2026-11-21T14:03:15.104233Z",
    "received_at": null,
    "routed_by": null,
    "created_at": "2026-11-21T14:03:12.512870Z",
    "updated_at": "2026-11-21T14:03:15.104233Z"
  }
}
```

| Header / field | Description |
|---|---|
| `X-Webhook-Id`, `id` | Event id, identical on every retry. Use it to discard duplicates. |
| `X-Event-Type`, `type` | Event type. |
| `X-Timestamp` | Unix time (seconds) of this attempt. Each retry has a new timestamp and signature. |
| `X-Signature` | `sha256=` followed by the lowercase hex HMAC-SHA256 of `<X-Timestamp>.<raw body>`. |
| `created_at` | When the event was created. |
| `data` | The [message object](#the-message-object) as it was when the event was created. |

Delivery rules:

- Any `2xx` response acknowledges the event. Anything else (other status,
  timeout, connection error) is a failure. Redirects are not followed, so a
  `3xx` is a failure too.
- Timeouts: `webhooks.connect_timeout_secs` (5 s) to connect,
  `webhooks.timeout_secs` (10 s) for the whole request. Answer quickly and
  process asynchronously.
- Retries follow `webhooks.retry_schedule_secs` (default 10 s, 1 min,
  10 min, then every hour) until `webhooks.max_age_secs` (24 h) after the
  event was created, then the event is abandoned.
- Delivery is at least once, and not ordered: deduplicate on the event id
  and rely on `data.status` and `data.updated_at` rather than on arrival
  order.
- Events for a disabled app, or created before its webhook was removed, are
  dropped.
- The payload is serialized again for each attempt, so do not rely on key
  order or whitespace; verify the signature over the exact bytes received.

### Verifying the signature

The signing secret is the full string printed by the CLI, `whsec_` prefix
included, used as UTF-8 bytes. To verify a request:

1. Read the raw request body as bytes, before any JSON parsing.
2. Compute HMAC-SHA256 with the secret over the ASCII `X-Timestamp` value,
   a `.`, then the raw body.
3. Compare `sha256=<lowercase hex digest>` with `X-Signature` in constant
   time.
4. Reject timestamps too far from the current time (5 minutes, say) to
   limit replays, then deduplicate on `X-Webhook-Id`.

Python (standard library only):

```python
import hashlib
import hmac
import time


def verify(secret: str, headers: dict, body: bytes, tolerance: int = 300) -> bool:
    timestamp = headers["X-Timestamp"]
    if abs(time.time() - int(timestamp)) > tolerance:
        return False
    expected = hmac.new(
        secret.encode(), timestamp.encode() + b"." + body, hashlib.sha256
    ).hexdigest()
    return hmac.compare_digest("sha256=" + expected, headers["X-Signature"])
```

Shell, to check a captured request by hand:

```sh
printf '%s.%s' "$TIMESTAMP" "$(cat body.json)" \
  | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" -r | cut -d' ' -f1
# compare with the X-Signature value after "sha256="
```

(`$(cat ...)` strips trailing newlines; the gateway sends none.)

Test vector: secret `whsec_test`, timestamp `1700000000`, body `{"a":1}`
give `sha256=38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789`.

Rotating the secret (`sms-gateway app update NAME --rotate-webhook-secret`)
takes effect at the next attempt, including retries of pending events.
There is no overlap period: update the receiver right after.

## Development routes

Mounted only with `modem.kind = "mock"`, admin apps only. Nothing is really
sent with the mock modem.

| Route | Description |
|---|---|
| `POST /v1/dev/inbound` `{"from", "text"}` | Simulates a received SMS (`202`, no body). The inbound poller picks it up at its next cycle. |
| `GET /v1/dev/sent` | SMS the mock "sent": `[{"to", "text"}]`. |
| `POST /v1/dev/faults` `{"send"?, "offline"?}` | `send`: outcomes of the next send calls, in order, replacing the current list: `unavailable`, `unknown`, `rejected`, `protocol`, `submitted`. `offline: true` makes every device call fail as unavailable. Returns the current fault state. |

```sh
curl -sS -X POST http://localhost:8080/v1/dev/inbound \
  -H "Authorization: Bearer $SMS_ADMIN_KEY" -H 'Content-Type: application/json' \
  -d '{"from": "+33600000000", "text": "PAY 42"}'

curl -sS -X POST http://localhost:8080/v1/dev/faults \
  -H "Authorization: Bearer $SMS_ADMIN_KEY" -H 'Content-Type: application/json' \
  -d '{"send": ["unavailable", "unknown"]}'
```
