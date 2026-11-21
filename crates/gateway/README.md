# gateway

The `sms-gateway` binary: HTTP API, PostgreSQL storage, outbound worker,
inbound poller, webhook dispatcher and CLI.

| Module | Role |
|---|---|
| `api` | axum routes, authentication, JSON errors, OpenAPI. |
| `apps` | Client applications and API keys. |
| `config` | TOML + environment configuration. |
| `messages` | Message storage and API representation. |
| `modem_ctl` | Single serialized entry point to the device, pause, safety timeout. |
| `workers` | Outbound worker, inbound poller, status refresher, retention. |
| `webhooks` | Event queue, HMAC signing, dispatcher. |
| `phone`, `sms_text` | E.164 normalization, SMS segment counting. |
| `migrations/` | Database schema. |

Documentation:

- [README](../../README.md): overview and quick start.
- [docs/api.md](../../docs/api.md): HTTP API, CLI and webhooks.
- [docs/configuration.md](../../docs/configuration.md): every setting.
- [docs/architecture.md](../../docs/architecture.md): design and data model.
- [docs/deployment.md](../../docs/deployment.md): container image, Compose,
  Kubernetes.

Tests: `cargo test -p gateway --lib` for unit tests; the integration tests
in `tests/db` need `DATABASE_URL` (see
[CONTRIBUTING.md](../../CONTRIBUTING.md#tests)).

License: GPL-3.0-or-later.
