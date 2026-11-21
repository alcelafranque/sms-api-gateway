# TP-Link NE200-Outdoor web protocol

> Interoperability notes, obtained by observing the device's local web
> management interface. No TP-Link code is reproduced here. This project is
> not affiliated with TP-Link.

Notes on the protocol spoken by the web UI of the TP-Link NE200-Outdoor 5G
CPE, as used by the [`tplink-ex`](../crates/tplink-ex) driver. They are
meant for anyone who wants to drive this device, or a related TP-Link
"EX"/"MR"-family device, from another language.

The protocol is undocumented. Most of it was reverse-engineered by the
authors of [`tplinkrouterc6u`](https://github.com/AlexandrErohin/TP-Link-Archer-C6U)
(GPL-3.0); the NE200-specific behaviour below was observed on a live
**NE200-Outdoor v1** and is covered by the driver's tests. Behaviour may
differ on other models and change with firmware updates.

- [Overview](#overview)
- [Cryptography](#cryptography)
- [Login](#login)
- [Acts](#acts)
- [Sending an SMS](#sending-an-sms)
- [Reading the inbox](#reading-the-inbox)
- [Status objects](#status-objects)
- [Other objects](#other-objects)
- [Open questions](#open-questions)
- [Exploring with `probe`](#exploring-with-probe)

## Overview

The NE200-Outdoor has no official API, no SSH or telnet, and no AT command
access. Its only interface is the web UI at `http://192.168.254.1` (plain
HTTP). The UI asks for a password only; the login username is `user`.

| Request | Purpose |
|---|---|
| `POST /cgi/getGDPRParm` | Fetch the device RSA public key and a sequence number. |
| `POST /cgi_gdpr?9` | Every encrypted operation: login, logout, reads, writes. |
| `GET /` | After login, the index page carries the `TokenID`. |
| `GET /js/tpEncrypt.js` | The UI encryption script; tells which cipher the firmware uses. |

Headers:

- `Referer: http://192.168.254.1/` is required. Without it the device
  answers `406 Not Acceptable`.
- `TokenID: <token>` on every `/cgi_gdpr` request after login, plus the
  cookies set during login.
- The httpd sporadically answers `500` or `406` to valid requests. For
  requests that are safe to send again, the driver makes up to 3 attempts.
  It also opens a fresh TCP connection per request.

Only **one administrator session** can exist at a time. A client must keep a
single session, serialize its requests, and expect to lose the session when
someone logs in to the web UI.

## Cryptography

Each `/cgi_gdpr` request body is encrypted with a session AES key chosen by
the client, and carries a signature encrypted with the device RSA key. The
login request is how the device learns the AES key.

### RSA key exchange

`POST /cgi/getGDPRParm` (empty body, `Referer` required) returns
JavaScript:

```js
var ee="<public exponent, hex>";
var nn="<modulus, 128 hex digits>";
var seq="<decimal sequence number>";
$.ret=0;
```

`nn` is a **512-bit** RSA modulus. `seq` is a number used in every
signature of the session.

### Session key

The client generates an AES-128 key and an IV as ASCII digit strings, whose
bytes are used directly as key material. Like the web UI, the driver takes
the current time in milliseconds followed by a random 9-digit number,
truncated to 16 characters, drawn independently for the key and the IV.

### Session cipher variants

| Variant | Payload | RSA signature | Seen on |
|---|---|---|---|
| CBC | AES-128-CBC, PKCS#7, 16-byte IV | raw (no padding) | **NE200-Outdoor v1** (verified) |
| GCM | AES-128-GCM, 12-byte nonce, detached 16-byte tag | raw | other EX firmwares (per `tplinkrouterc6u`) |
| GCM-OAEP | AES-128-GCM | RSA-OAEP, SHA-1, MGF1-SHA-1 | EX920-class firmwares (per `tplinkrouterc6u`) |

The NE200's `/js/tpEncrypt.js` contains `mode.CBC`, which the driver uses
as a hint before probing.

CBC ciphertexts are base64 encoded. With GCM, the request carries the tag
in a separate `tag=` field, and a response is the base64 ciphertext
immediately followed by the base64 tag (its last 24 characters).

### Signature

The signature text is:

```text
login:      key=<key>&iv=<iv>&h=<hash>&s=<seq + len(data)>
other acts: h=<hash>&s=<seq + len(data)>
```

- `hash` is the lowercase hex MD5 of `username + password`.
- `len(data)` is the length of the base64 `data` field of this request.
- On GCM firmwares, `key` and `iv` are base64 encoded in the login
  signature; on CBC firmwares they are sent as is.

With raw RSA, the text is cut into 64-byte blocks (the modulus length).
Each block is right-padded with zero bytes to 64 bytes, read as a
big-endian integer `m`, and encrypted as `m^e mod n`. Each result is
written as 128 lowercase hex digits, and the blocks are concatenated.

### Request body

```text
sign=<hex signature>\r\n
data=<base64 AES ciphertext of the JSON payload>\r\n
```

GCM firmwares add a `tag=<base64 tag>\r\n` line. The response body is the
AES-encrypted reply, base64 encoded.

## Login

1. `POST /cgi/getGDPRParm` for `nn`, `ee` and `seq`.
2. `POST /cgi_gdpr?9` with the login payload, signed with the `key=...&iv=...`
   prefix:

   ```json
   {"data":{"UserName":"<base64 username>","Passwd":"<base64 password>","Action":"1","stack":"0,0,0,0,0,0","pstack":"0,0,0,0,0,0"},"operation":"cgi","oid":"/cgi/login"}
   ```

   The decrypted reply contains `$.ret=<code>`:

   | `$.ret` | Meaning |
   |---|---|
   | `0` | Logged in. |
   | `71233` | Wrong username or password. |
   | `71234` | Generic "bad request" (reported by `tplinkrouterc6u`). |

3. `GET /` and read `var token="..."` from the page. Send it as the
   `TokenID` header from then on.

Logout is the act `cgi /cgi/logout` (`{"data":{"stack":...,"pstack":...},
"operation":"cgi","oid":"/cgi/logout"}`).

### Wrong variant

On the NE200 (a CBC firmware), a login sealed with GCM gets a reply **with
no HTTP status line and no headers**, just a bare chunked body, and the
connection is closed. Strict HTTP clients fail with an error such as
"invalid HTTP version". The driver treats any malformed or undecryptable
login reply as "wrong variant" and tries the next one.

A wrong-variant attempt never reaches the password check: the device could
not decrypt the credentials. Probing variants therefore does not count as
failed login attempts, and a wrong password costs a single attempt, since
the driver stops probing at the first reply it can decrypt.

### Session loss

When another client logs in (the web UI, for instance), the previous session
is dead: the device closes the connection without any reply to every request
made with it, even after the other client has logged out. The session never
comes back; only a new login does.

The driver considers the session lost, logs in again once, and repeats the
request when the device answers `401` or `403`, redirects (`3xx`), replies
with a body that is not encrypted with the session key (plain text, or a
ciphertext that does not decrypt), or closes the connection without a reply
to a read. Requests that change state (a send, a delete) are not repeated
when the outcome is ambiguous; the session is dropped and the next request
logs in again. A send is preceded by a read, which renews a dead session
before anything is submitted.

## Acts

Every operation after login is an "act" posted to `/cgi_gdpr?9`. The
plaintext payload is:

```json
{"data":{"stack":"0,0,0,0,0,0","pstack":"0,0,0,0,0,0","<attribute>":"<value>"},"operation":"go","oid":"DEV2_LTE_NET_STATUS"}
```

- `oid` names a data-model object (`DEV2_*`).
- `stack` selects an instance: `0,0,0,0,0,0` is the root, the LTE status
  objects below live at `1,0,0,0,0,0`. `pstack` is the parent stack, left
  at the root here.
- Reads list the wanted attribute names with empty values. Writes carry
  `name: value` pairs. Values are strings.
- The web UI sends the fields in this order (`stack`, `pstack`, attributes)
  and so does the driver.

| `operation` | Meaning |
|---|---|
| `go` | Read one object. |
| `gl` | Read every instance of an object (a list). |
| `gs` | Read a subtree. |
| `so` | Write attributes of an object; also triggers actions (sending an SMS). |
| `add` / `del` | Create / delete an instance. |
| `do` | Object-specific action (deleting an SMS entry). |
| `op` | Generic operation. |
| `cgi` | Call a CGI endpoint (`/cgi/login`, `/cgi/logout`). |

Replies, once decrypted:

```json
{"data":{"smsUnreadCount":"0","stack":"1,0,0,0,0,0"},"operation":"go","oid":"DEV2_LTE_NET_STATUS","success":true}
```

- `data` is an object for `go` and usually an array for `gl`. A write may
  answer `{"success":true,"errorcode":0}` or an empty body.
- A refusal is `{"success":false,"errorcode":<code>}`. Requesting an
  attribute the object does not have makes the device refuse **the whole
  act** with errorcode `9003`, so probe unknown attributes one at a time.
- Replies may contain raw control characters inside JSON strings (such as
  the line-break codes of SMS texts); escape them before handing the reply
  to a strict JSON parser.

## Sending an SMS

1. Submit:

   ```json
   {"data":{"stack":"0,0,0,0,0,0","pstack":"0,0,0,0,0,0","index":"1","to":"+33600000000","textContent":"Hello"},"operation":"so","oid":"DEV2_LTE_SMS_SENDNEWMSG"}
   ```

   `index: "1"` sends the message; `index: "2"` saves it as a draft
   instead.

2. Read the result with `go DEV2_LTE_SMS_SENDNEWMSG` and the attribute
   `sendResult`:

   | `sendResult` | Meaning |
   |---|---|
   | `3` | Sending. The web UI polls every 500 ms while it reads 3. |
   | `1` | Sent. |
   | `2` | Modem busy: the message was not sent and can be submitted again. |
   | `0` | Failed. |

   At rest the value stays at the last result. Observed sequences: `["3",
   "1"]` in about 0.7 s for a short message, `["3", "3", "1"]` in about
   1.2 s for a 355-character message. The first read after the `so`
   already returned `3`; the web UI trusts the first value it reads, and so
   does the driver.

`DEV2_LTE_NET_STATUS.smsSendResult`, where MR-family firmwares report the
send result, does not exist on this firmware (errorcode 9003).

The driver reads `sendResult` once before submitting. That read also proves
the session is alive, so an expired session is detected and renewed
*before* the message is submitted rather than after. After the `so`, it
polls for up to 20 s; without a terminal value it reports the message as
"submitted" rather than sent.

### Text encoding

- Before sending, the web UI replaces `\n` with the character 0x12 and `\r`
  with 0x11. It decodes them the same way when displaying the inbox.
- UTF-8 text (accents, `€`, emoji) goes through unchanged. The device picks
  the SMS encoding (GSM-7 or UCS-2) itself.
- The web UI limits a message to 765 GSM-7 or 335 UCS-2 characters, that is
  5 concatenated parts. The device splits and concatenates long messages
  itself when sending.
- Long inbound SMS arrive already reassembled, as a single inbox entry.

## Reading the inbox

The inbox must be loaded first:

```json
{"data":{"stack":"0,0,0,0,0,0","pstack":"0,0,0,0,0,0","PageNumber":"0"},"operation":"so","oid":"DEV2_LTE_SMS_RECVMSGBOX"}
```

The attribute is spelled **`PageNumber`**, with a capital P; other
spellings are ignored. Until this is done, `go DEV2_LTE_SMS_RECVMSGBOX`
reads `totalNumber` as 0, even though `DEV2_LTE_NET_STATUS.smsUnreadCount`
already counts the messages.

Then, for each page `n` from 1 to `ceil(totalNumber / 8)`:

1. `so DEV2_LTE_SMS_RECVMSGBOX` with `PageNumber: "<n>"`.
2. `gl DEV2_LTE_SMS_RECVMSGENTRY` with the attributes `index`, `from`,
   `content`, `receivedTime` and `unread`.

```json
{"data":[{"index":"5","from":"+33600000000","content":"Hello","receivedTime":"2026-11-21 15:05:40","unread":"1","stack":"5,0,0,0,0,0"}],"operation":"gl","oid":"DEV2_LTE_SMS_RECVMSGENTRY","success":true}
```

- Pages hold 8 entries and start at 1. Entries are listed newest first.
- Indexes are sparse. Do not treat them as stable identifiers: the driver
  identifies an entry by sender, text and reception time, and re-reads the
  inbox right before deleting one.
- Each entry carries its own `stack`, used to address it.
- `receivedTime` is `YYYY-MM-DD HH:MM:SS` in the device's local time, with
  no offset.
- `unread` is `"1"` or `"0"`.

Once the box has been loaded, page selection also works on a fresh session.

To act on an entry, select its page first, then:

| Action | Act |
|---|---|
| Mark as read | `so DEV2_LTE_SMS_RECVMSGENTRY` with `stack: "<entry stack>"` and `unread: "0"` |
| Delete | `do DEV2_LTE_SMS_RECVMSGENTRY` with `stack: "<entry stack>"` |

Both work on the NE200-Outdoor v1.

## Status objects

| Object | `stack` | Operation | Attributes |
|---|---|---|---|
| `DEV2_LTE_NET_STATUS` | `1,0,0,0,0,0` | `go` | `smsUnreadCount`, `sigLevel` (signal bars), `rfInfoRsrp`, `rfInfoRsrq`, `rfInfoSnr` |
| `DEV2_LTE_PROF_STAT` | `1,0,0,0,0,0` | `go` | `ispName` (operator) |
| `DEV2_LTE_SERVING_CELL_INFO` | root | `gl` | per radio access technology: `networkType`, `cellConnectionStatus`, `signalStrength`, `SSRSRP` / `SSRSRQ` / `SSSINR` (5G), `RSRP` / `RSRQ` / `SNR` |

`smsUnreadCount` is the cheapest way to poll for new messages. On 5G links
the serving-cell values are the meaningful ones. The driver also reads
`DEV2_LTE_LINK_CFG` (`networkType`, `connectStatus`, `simStatus`) to pick
the active cell. Values are strings, sometimes with decimals (`"12.0000"`);
`268435455` stands for "not available". RSRP is in dBm and RSRQ in dB; the
SNR unit is not confirmed (the values look like tenths of dB).

## Other objects

Seen in the web UI but not used by the driver:

- `DEV2_LTE_SMS_*` boxes besides the inbox: `SENDMSGBOX` / `SENDMSGENTRY`
  (outbox), `DRAFTMSGBOX`, `UNREADMSGBOX`.
- `DEV2_LTE_SMS_CFG`: SMS settings, including `messageCenter` (SMSC number)
  and `saveSentMsg`.
- An `INCLUDE_SMS_RECEIPT` feature flag, which suggests delivery-report
  support.

## Open questions

- Idle timeout of a session.
- Capacity of the inbox (new SMS are lost once it is full).
- Delivery reports (`INCLUDE_SMS_RECEIPT`).
- The SNR unit.
- Messages longer than 5 parts, which the web UI does not allow.

Contributions with observations from other models or firmware versions are
welcome. Never publish captures containing real phone numbers, message
texts, passwords, RSA moduli or SMSC numbers.

## Exploring with `probe`

The `probe` example of `tplink-ex` logs in, runs one command and logs out:

```sh
export TPLINK_URL=http://192.168.254.1 TPLINK_USER=user TPLINK_PASSWORD=...
alias probe='cargo run -q -p tplink-ex --example probe --'

probe login                          # detects and prints the cipher variant
probe status -v                      # radio status, raw objects, send-result candidates
probe inbox -v                       # inbox with page and stack of each entry
probe send +33600000000 'Test'       # send and print the sendResult sequence
probe raw go DEV2_LTE_SMS_SENDNEWMSG sendResult
probe raw go DEV2_LTE_NET_STATUS --stack 1,0,0,0,0,0 smsUnreadCount sigLevel
```

`raw <op> <oid> [--stack S] [--pstack S] [name | key=value]...` sends any
act: bare names are read attributes, `key=value` pairs are written. `-vv`
logs the decrypted device replies, including message texts and phone
numbers; keep such logs private.
