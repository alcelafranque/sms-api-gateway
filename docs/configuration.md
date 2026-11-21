# Configuration

`sms-gateway` reads a TOML file, then applies environment variable
overrides. [`config.example.toml`](../config.example.toml) lists every key
with its default and is a good starting point.

- [Loading](#loading)
- [Environment overrides](#environment-overrides)
- [Sections](#sections): [server](#server), [metrics](#metrics),
  [log](#log), [database](#database), [modem](#modem),
  [modem.tplink](#modemtplink), [outbound](#outbound),
  [inbound](#inbound), [validation](#validation), [webhooks](#webhooks),
  [retention](#retention)
- [Validation rules](#validation-rules)
- [SMS length](#sms-length)

## Loading

| Source | Notes |
|---|---|
| `--config <path>` / `-c <path>` | Explicit file; an error if it does not exist. |
| `SMS_GATEWAY_CONFIG=<path>` | Same, from the environment (single underscore). |
| `./config.toml` | Loaded when present and no path was given. |
| `SMS_GATEWAY__*` variables | Applied on top of the file. |

A file is optional: the whole configuration can come from the environment.
Unknown keys are rejected at start-up, which catches typos. The configuration is
validated before anything starts; see [validation rules](#validation-rules).

Only `database.url` is always required. `[modem]` is required by `serve`,
not by `migrate` or `app`.

## Environment overrides

Every key can be set with a variable named
`SMS_GATEWAY__<SECTION>__<KEY>` (upper case, double underscores between
levels).

```sh
SMS_GATEWAY__DATABASE__URL=postgres://sms:secret@db:5432/sms
SMS_GATEWAY__MODEM__KIND=tplink
SMS_GATEWAY__MODEM__TPLINK__URL=http://192.168.254.1
SMS_GATEWAY__MODEM__TPLINK__PASSWORD=secret
SMS_GATEWAY__OUTBOUND__MAX_ATTEMPTS=3
SMS_GATEWAY__INBOUND__DELETE_AFTER_INGEST=false
SMS_GATEWAY__VALIDATION__ALLOWED_PREFIXES='["+33", "+32"]'
SMS_GATEWAY__WEBHOOKS__RETRY_SCHEDULE_SECS='[10, 60, 600]'
```

Values are parsed: numbers, `true`/`false` and arrays (`[a, b]`) get their
type. These keys are always read verbatim as strings, so that for example
a numeric password is not turned into a number: `database.url`,
`modem.timezone`, `modem.tplink.url`, `modem.tplink.username`,
`modem.tplink.password`, `inbound.default_app`, `log.filter`,
`validation.default_region`.

Keep secrets (database URL, device password) in the environment or a
secret store rather than in the file.

## Sections

### `[server]`

| Key | Default | Description |
|---|---|---|
| `listen` | `"0.0.0.0:8080"` | Address of the API listener. |
| `request_timeout_secs` | `30` | Maximum processing time of an API request (`408` beyond). |
| `body_limit_bytes` | `65536` | Maximum request body size (`413` beyond). |
| `swagger_ui` | `true` | Serve Swagger UI at `/docs`. `/openapi.json` is always served. |
| `shutdown_timeout_secs` | `30` | Grace period for in-flight work on `SIGTERM` / Ctrl-C. |

The gateway speaks plain HTTP; put it behind a TLS reverse proxy when the
API crosses an untrusted network.

### `[metrics]`

| Key | Default | Description |
|---|---|---|
| `listen` | unset | Dedicated listener for `/metrics`, without authentication (e.g. `"127.0.0.1:9100"`). When unset, `/metrics` is served on the main listener to admin apps only. |

### `[log]`

| Key | Default | Description |
|---|---|---|
| `format` | `"text"` | `text` or `json` (one JSON object per line, on stderr). |
| `filter` | `"info,sqlx=warn,tower_http=info"` | [`tracing` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) directives. `RUST_LOG`, when set, takes precedence. |

SMS texts are not logged and phone numbers are masked, with one exception:
at `trace` level the `tplink_ex` driver logs decrypted device replies,
which contain SMS texts and numbers. Do not enable `tplink_ex=trace` in
production.

### `[database]`

| Key | Default | Description |
|---|---|---|
| `url` | required | PostgreSQL connection URL. Prefer `SMS_GATEWAY__DATABASE__URL`. |
| `max_connections` | `10` | Pool size. |
| `connect_timeout_secs` | `10` | Timeout to acquire a connection. |
| `migrate_on_start` | `true` | Apply pending migrations when `serve` starts. `sms-gateway migrate` does it explicitly. |

### `[modem]`

| Key | Default | Description |
|---|---|---|
| `kind` | required | `tplink` (TP-Link NE200-Outdoor and other EX-family CPEs) or `mock` (in-memory device for development; also enables the [development routes](api.md#development-routes)). |
| `timezone` | `"Europe/Paris"` | IANA time zone of the device clock, used to convert the naive reception time of inbound SMS to UTC. |
| `status_interval_secs` | `60` | Radio status refresh interval (minimum 5). |
| `op_timeout_secs` | `120` | Safety timeout around each device call. A send exceeding it ends in `unknown` and is never re-sent. |

### `[modem.tplink]`

Required when `kind = "tplink"`.

| Key | Default | Description |
|---|---|---|
| `url` | required | Base URL of the device web UI, e.g. `"http://192.168.254.1"`. |
| `username` | `"user"` | Web UI user. EX firmwares, including the NE200, use `user` with a password-only login page; MR-style firmwares use `admin`. |
| `password` | required | Web UI password. Prefer `SMS_GATEWAY__MODEM__TPLINK__PASSWORD`. |
| `variant` | `"auto"` | Session cipher: `auto` (detect at login), `cbc`, `gcm` or `gcm_oaep`. The NE200-Outdoor v1 uses `cbc`; setting it skips detection. |
| `timeout_secs` | `30` | HTTP request timeout (connect timeout: the same, capped at 10 s). |
| `accept_invalid_certs` | `false` | Accept a self-signed certificate when `url` is `https`. |

### `[outbound]`

| Key | Default | Description |
|---|---|---|
| `min_interval_ms` | `3000` | Minimum delay between two send attempts. |
| `max_attempts` | `5` | Attempts for a message failing with a retryable error (nothing reached the device). Other errors are never retried. |
| `retry_base_secs` | `10` | Backoff base: the delay after attempt *n* is `retry_base_secs * 2^(n-1)`... |
| `retry_max_secs` | `600` | ...capped at this value. |
| `sending_stale_secs` | `600` | A message left in `sending` longer than this (gateway crash during a send) is moved to `unknown`. Must exceed `modem.op_timeout_secs`. |
| `idle_poll_ms` | `1000` | Queue polling interval when idle. New messages wake the worker anyway. |

### `[inbound]`

| Key | Default | Description |
|---|---|---|
| `poll_interval_secs` | `15` | Interval between two unread-count checks. |
| `full_scan_interval_secs` | `600` | Full inbox scan even when the device reports no unread message. |
| `delete_after_ingest` | `true` | Delete messages from the device once stored. The device inbox is small and new SMS are lost when it is full. |
| `reply_window_hours` | `72` | Reply correlation: an inbound SMS matching no keyword goes to the app that last sent to the same number within this window. |
| `default_app` | unset | Name of the app receiving inbound SMS that match no keyword nor reply. Unset: such messages are only stored. |

### `[validation]`

| Key | Default | Description |
|---|---|---|
| `default_region` | `"FR"` | ISO 3166 region used to parse national numbers (recipients and senders without a leading `+`). |
| `max_segments` | `5` | Maximum SMS segments per outbound message (1 to 255). See [SMS length](#sms-length). |
| `allowed_prefixes` | `[]` | If non-empty, recipients must start with one of these E.164 prefixes (e.g. `["+33"]`). |
| `denied_prefixes` | `[]` | Recipients starting with one of these E.164 prefixes are refused, even if also allowed (e.g. premium-rate ranges). |

Prefixes are compared with the normalized E.164 recipient.

### `[webhooks]`

| Key | Default | Description |
|---|---|---|
| `timeout_secs` | `10` | Timeout of a whole webhook request. |
| `connect_timeout_secs` | `5` | Connect timeout. |
| `retry_schedule_secs` | `[10, 60, 600, 3600]` | Delays between attempts; the last one repeats until `max_age_secs`. |
| `max_age_secs` | `86400` | A delivery still failing this long after the event was created is abandoned. |
| `concurrency` | `8` | Deliveries attempted in parallel. |
| `poll_interval_ms` | `2000` | Interval between two checks for due deliveries when idle. |

### `[retention]`

| Key | Default | Description |
|---|---|---|
| `interval_secs` | `3600` | Retention job period (minimum 60). |
| `message_body_days` | `30` | Erase message texts (and the texts in finished webhook payloads) older than this. `0` keeps them. |
| `message_days` | `365` | Delete messages older than this. `0` keeps them. |
| `webhook_days` | `30` | Delete finished webhook deliveries and their attempt log older than this. `0` keeps them. |

Messages still `queued` or `sending` are never purged. With
`inbound.delete_after_ingest = false`, keep `message_days` longer than
messages stay on the device, or they would be ingested again once their row
is deleted.

## Validation rules

Start-up fails when:

- `modem.kind = "tplink"` has no `[modem.tplink]` section;
- `modem.timezone` is not a known IANA time zone;
- `outbound.sending_stale_secs` is not greater than `modem.op_timeout_secs`;
- `validation.default_region` is not a known region;
- a prefix in `allowed_prefixes` / `denied_prefixes` is not `+` followed by
  digits;
- `validation.max_segments` is outside 1..=255;
- `webhooks.retry_schedule_secs` is empty;
- `outbound.max_attempts` or `webhooks.concurrency` is 0.

## SMS length

The gateway counts SMS segments the way the NE200 web UI does (3GPP TS
23.038 / 23.040) and refuses texts longer than `validation.max_segments`
with `422 validation_failed`.

- A text made only of characters of the GSM-7 default alphabet is sent as
  **GSM-7**. The alphabet covers ASCII letters and digits, common
  punctuation, line breaks, `@£$¥èéùìòÇØøÅåÄÖÑÜäöñüàßÉÆæ`, a few Greek
  capitals, and more. Characters of the extension table, `€ [ ] { } ^ ~ | \`
  and form feed, take two septets each.
- Any other character (`ç`, `ê`, `á`, Cyrillic, CJK, emoji...) switches the
  **whole text** to **UCS-2**, counted in UTF-16 code units: characters
  outside the Basic Multilingual Plane, such as most emoji, count twice.

| Encoding | Single SMS | Per segment of a long SMS | 5 segments (default) |
|---|---|---|---|
| GSM-7 | 160 septets | 153 septets | 765 septets |
| UCS-2 | 70 units | 67 units | 335 units |

A long message is sent by the device as several concatenated segments,
each billed as one SMS by most operators. The NE200 web UI allows at most 5
segments, hence the default; longer texts are untested with this device.
Outbound messages report the computed `encoding` (`gsm7` or `ucs2`) and
`segments`.
