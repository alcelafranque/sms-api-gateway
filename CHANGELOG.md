# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-11-21

### Fixed

- The TP-Link driver now recovers when another client (for instance the
  device web UI) takes over the single administrator session. The device
  closes the connection without a reply to every request of the replaced
  session; the driver used to keep reusing that dead session, so every poll
  failed until the gateway restarted. Any transport error now drops the
  session, and a read closed without a reply triggers a transparent re-login.

## [0.1.0] - 2026-11-21

Initial release.

### Added

- `sms-gateway` binary with the `serve`, `migrate` and `app` commands
  (create, list, update, enable, disable, rotate-key).
- HTTP API: `POST /v1/messages` with `Idempotency-Key` support,
  `GET /v1/messages/{id}`, `GET /v1/messages` with filters and cursor
  pagination, `GET /v1/modem`, `POST /v1/modem/pause` and
  `POST /v1/modem/resume`, `/healthz`, `/readyz`, `/metrics`, an OpenAPI
  document at `/openapi.json` and Swagger UI at `/docs`.
- Per-app API keys stored as SHA-256 hashes, admin apps.
- PostgreSQL storage with embedded migrations.
- Outbound worker: pacing, retries with exponential backoff only when
  nothing reached the device, `sent` / `submitted` / `failed` / `unknown`
  outcomes, recovery of sends interrupted by a crash as `unknown`.
- Inbound poller: unread-count polling with periodic full scans,
  deduplication, deletion from the device after storage, routing by
  keyword, reply correlation and default app.
- Webhooks `message.received`, `message.sent`, `message.failed` and
  `message.unknown`, signed with HMAC-SHA256, with retries and an attempt
  log.
- Recipient validation: E.164 normalization, allowed and denied prefixes,
  GSM-7 / UCS-2 segment limits.
- Persistent modem pause, optionally timed, releasing the device session.
- Retention job for message texts, messages and webhook deliveries.
- Prometheus metrics, text or JSON logs with masked phone numbers.
- Mock modem with development routes for inbound injection and fault
  injection.
- `tplink-ex` driver for TP-Link EX-family CPEs: AES-128-CBC and
  AES-128-GCM sessions with raw or OAEP RSA signatures and automatic
  variant detection, transparent re-login, SMS sending with send-result
  polling, inbox listing, mark as read, delete, radio status, and the
  `probe` tool. Verified on a TP-Link NE200-Outdoor v1.
- `modem` crate with the `SmsModem` trait, the TP-Link backend and an
  in-memory mock.
- Container image and Compose file.

[Unreleased]: https://github.com/alcelafranque/sms-api-gateway/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/alcelafranque/sms-api-gateway/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/alcelafranque/sms-api-gateway/releases/tag/v0.1.0
