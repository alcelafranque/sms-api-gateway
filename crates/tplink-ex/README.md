# tplink-ex

Async Rust client for the `/cgi_gdpr` web API of TP-Link "EX"-family CPEs
and 5G/LTE routers, such as the NE200-Outdoor. This is the encrypted
endpoint behind their web UI, operating on `DEV2_*` data-model objects.

It provides:

- the session crypto: AES-128-CBC or AES-128-GCM session keys, signatures
  with raw or OAEP RSA under the device's 512-bit key, and automatic
  detection of the variant;
- login, `TokenID`/cookie handling, and a transparent re-login when the
  device drops the session;
- a generic `act` API (`go`/`gl`/`so`/`do`/...) plus typed helpers:
  sending SMS with send-result polling, inbox listing, mark-as-read,
  delete, LTE status, and logout;
- errors that say whether a request may have been executed
  (`Error::kind()`), so that callers never re-send an SMS blindly.

```rust
use tplink_ex::{Client, ClientConfig};

async fn demo() -> Result<(), tplink_ex::Error> {
    let mut client = Client::new(ClientConfig::new("http://192.168.254.1", "user", "secret"))?;
    let report = client.send_sms("+33600000000", "hello").await?;
    println!("{:?}", report.status);
    for sms in client.inbox().await? {
        println!("{} {}: {}", sms.received_time, sms.from, sms.content);
    }
    client.logout().await?;
    Ok(())
}
```

A `Client` holds at most one session and its methods take `&mut self`: the
device accepts a single administrator session, so callers sharing a client
must serialize access (for example behind a mutex).

## Trying it on a device

The `probe` example is meant for a first hands-on session:

```sh
TPLINK_URL=http://192.168.254.1 TPLINK_USER=user TPLINK_PASSWORD=... \
    cargo run -p tplink-ex --example probe -- status -v
```

Commands: `login`, `status`, `inbox`, `send <number> <text>`,
`read <index>`, `delete <index>`, and `raw <op> <oid> [attrs]`.
Use `--variant` to force a cipher variant, and `-v`/`-vv` for protocol logs
(`-vv` includes decrypted replies, with message texts and numbers).

EX firmwares, including the NE200, expect the username `user`, even though
their web UI only asks for a password.

## Status

Verified on a TP-Link **NE200-Outdoor v1** (AES-128-CBC variant): login,
sending with its result (`DEV2_LTE_SMS_SENDNEWMSG.sendResult`), inbox
listing, mark-as-read, delete and radio status.

Not verified on real hardware: the GCM and GCM-OAEP variants (covered by
a fake device and known-answer tests), the `DEV2_LTE_NET_STATUS`
send-result fallback aimed at MR-style firmwares (the NE200 does not have
it), and the SNR unit. Other models may work but are untested.

This is an unofficial protocol: a firmware update may break it. What is
known about it is written up in
[docs/ne200-protocol.md](https://github.com/alcelafranque/sms-api-gateway/blob/main/docs/ne200-protocol.md).

## Credits

The protocol knowledge (endpoints, object names, message formats,
encryption scheme, return codes) comes from
[`tplinkrouterc6u`](https://github.com/AlexandrErohin/TP-Link-Archer-C6U)
by Alex Erohin and contributors (GPL-3.0), complemented by observations on
an NE200-Outdoor. This crate is a separate Rust implementation, licensed
under GPL-3.0-or-later.
