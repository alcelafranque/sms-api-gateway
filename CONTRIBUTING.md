# Contributing

Bug reports, device reports and pull requests are welcome. Please write
issues, commits and pull requests in English.

For security issues, follow [SECURITY.md](SECURITY.md) instead of opening a
public issue.

## Development setup

- A stable Rust toolchain. CI and the container image use the version
  pinned in the [`Containerfile`](Containerfile) (`RUST_VERSION`).
- PostgreSQL, for the gateway's integration tests and for running the
  gateway locally. A container is enough (see below).
- No device is needed: the driver is tested against an in-process fake
  device, and the gateway can run with `modem.kind = "mock"`.

```sh
cargo build --workspace
```

To run the gateway without hardware:

```sh
export SMS_GATEWAY__DATABASE__URL=postgres://postgres:postgres@localhost:5432/postgres
export SMS_GATEWAY__MODEM__KIND=mock
cargo run -p gateway -- migrate
cargo run -p gateway -- app create dev --admin
cargo run -p gateway -- serve
```

The [development routes](docs/api.md#development-routes) then simulate
inbound SMS and device faults.

## Tests

```sh
# Unit tests and driver tests, no database needed
cargo test --workspace --exclude gateway
cargo test -p gateway --lib

# Everything, including the database-backed gateway tests
podman run -d --rm --name sms-test-db -e POSTGRES_PASSWORD=postgres \
  -p 5432:5432 docker.io/library/postgres:18
DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test --workspace
```

The tests in `crates/gateway/tests/db` use `#[sqlx::test]`: each test gets
a fresh database, created from `DATABASE_URL` with the migrations applied,
so the role must be allowed to create databases. Without `DATABASE_URL`
these tests fail rather than being skipped.

Where tests live:

| Path | What |
|---|---|
| `crates/tplink-ex/src/**` | Unit tests: crypto known-answer vectors (from the Python reference implementation), act encoding, reply parsing. |
| `crates/tplink-ex/tests/fake_device.rs` | End-to-end driver tests against a fake device modelled on the NE200-Outdoor v1 firmware. |
| `crates/modem/src/tplink.rs` | Error mapping, inbox locators, delete resolution. |
| `crates/gateway/src/**` | Unit tests: config loading, phone numbers, segment counting, retry decisions, routing helpers. |
| `crates/gateway/tests/db/` | API, outbound worker, inbound poller and webhooks against PostgreSQL and the mock modem. |

Follow the style of the existing tests, and test behaviour that matters
(what reaches the device, what is retried, what is stored, what is sent to
apps) rather than implementation details.

## Formatting and lints

CI runs these and fails on any warning:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Pull requests

- Keep a pull request focused on one change, and describe what it provides
  and how it was tested (unit tests, fake device, real device and model).
- Update the documentation (`README.md`, `docs/`) and add an entry under
  "Unreleased" in [CHANGELOG.md](CHANGELOG.md) for user-visible changes.
- Schema changes go in a new file in `crates/gateway/migrations/`; never
  edit a migration that has been released.
- Never commit real phone numbers, message texts, device passwords, RSA
  moduli, SMSC numbers or packet captures of a real device. Use
  placeholders such as `+33600000000`.
- Contributions are licensed under GPL-3.0-or-later, the license of the
  project.

## Device reports

Reports from other TP-Link models or firmware versions are very useful. The
[`probe` tool](README.md#testing-a-device-with-probe) shows what a device
supports: `probe login` (cipher variant), `probe status -v` (raw status
objects and send-result attributes), `probe inbox -v`. Include the model,
hardware and firmware versions, and redact numbers, texts and keys from the
output.

## Adding a modem backend

The gateway only talks to the [`SmsModem`](crates/modem/src/lib.rs) trait,
so supporting other hardware (a USB LTE modem driven by AT commands or
ModemManager, another router) means implementing it:

```rust
#[async_trait]
pub trait SmsModem: Send + Sync {
    async fn send(&self, to: &str, text: &str) -> Result<SendOutcome>;
    async fn unread_count(&self) -> Result<u32>;
    async fn inbox(&self) -> Result<Vec<InboundSms>>;
    async fn delete(&self, inbox_ref: &InboxRef) -> Result<()>;
    async fn status(&self) -> Result<ModemStatus>;
    async fn release(&self) -> Result<()> { Ok(()) }
}
```

1. Add a module to `crates/modem`, behind a Cargo feature if it brings
   dependencies (as `tplink` does).
2. Respect the error contract, which drives every retry decision of the
   gateway:
   - `Unavailable` only when the device certainly did not execute the
     request. The gateway retries such sends.
   - `Unknown` when a send may have been executed (timeout or broken
     connection after the request left). The gateway never re-sends it.
     Read-only operations may report an unknown outcome as `Unavailable`.
   - `Rejected` when the device refused the request, `Protocol` when its
     answer cannot be understood.
3. `send` receives a validated E.164 number and text. Return `Sent` only
   when the device confirmed the message left, `Submitted` otherwise.
4. `inbox` returns every stored message, with the reception time as the
   device reports it (naive local time). `InboxRef::locator` is yours: it
   must let `delete` find the right message even if the inbox changed since
   the listing. Deleting a message that is already gone is not an error.
5. `release` frees any device session (used by pause and shutdown); the
   next call must reopen it transparently.
6. Implementations must be safe to share between tasks. The gateway
   serializes device calls through its `ModemHandle`, but the backend must
   not rely on being the only user of the hardware it opens.
7. Wire it into the gateway: a variant of `ModemKind` and its settings in
   `crates/gateway/src/config.rs`, the backend construction in
   `crates/gateway/src/server.rs`, then `config.example.toml` and
   [docs/configuration.md](docs/configuration.md).
8. Test it without hardware where possible, for example against a fake
   device as `crates/tplink-ex/tests/fake_device.rs` does.
