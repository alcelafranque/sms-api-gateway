# modem

An abstraction over a device that can send and receive SMS, used by the
[`sms-gateway`](../gateway) binary.

- `SmsModem`: the async trait the gateway talks to (`send`, `unread_count`,
  `inbox`, `delete`, `status`, `release`).
- `ModemError`: four error kinds (`Unavailable`, `Unknown`, `Rejected`,
  `Protocol`) that tell whether a request may have been executed, which is
  what drives retries: only `Unavailable` is safe to retry.
- `tplink::TplinkModem` (feature `tplink`, on by default): backend for
  TP-Link EX-family CPEs such as the NE200-Outdoor, built on
  [`tplink-ex`](../tplink-ex).
- `MockModem`: in-memory device for development and tests.

See the [architecture](../../docs/architecture.md#device-access) for how the
gateway uses it, and
[CONTRIBUTING.md](../../CONTRIBUTING.md#adding-a-modem-backend) to add a
backend.

License: GPL-3.0-or-later.
