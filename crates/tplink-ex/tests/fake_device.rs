//! End-to-end tests against an in-process fake device speaking the
//! `/cgi_gdpr` protocol (raw-RSA variants; OAEP is covered by the
//! known-answer tests of the crypto module). The fake models the
//! NE200-Outdoor v1 firmware: send result in
//! `DEV2_LTE_SMS_SENDNEWMSG.sendResult`, `smsSendResult` refused on
//! `DEV2_LTE_NET_STATUS`, and headerless replies to logins sealed with the
//! wrong variant in CBC mode.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use num_bigint::BigUint;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tplink_ex::crypto::{SessionCipher, SessionMode, credentials_hash};
use tplink_ex::{Client, ClientConfig, Error, ErrorKind, LoginError, SendStatus, Variant};

/// Firmware error code for an unknown object or attribute.
const UNSUPPORTED: &str = r#"{"success":false,"errorcode":9003}"#;

/// Test-only 512-bit key pair.
const NN: &str = concat!(
    "808a338df1ae2de086ee020dca55c7134206a0aa1dd261ab28afd8c328ab1e7c",
    "506e32a08c3917f74561265e4075aa4ef72a6b1818e6f08fab97d2400988ae65"
);
const DD: &str = concat!(
    "711bab73fb383f0c750f18e593a7596f3bc963356eda82b13f3345b5ada2ae46",
    "7c6af5c0f00aa0322a4f64efcf772137becc0c9b7716a926718b02699be441c1"
);
const SEQ: u64 = 424_242;
const USER: &str = "user";
const PASSWORD: &str = "secret";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Failure {
    /// Reject the session with HTTP 403, without executing the act.
    Forbidden,
    /// Drop the session and answer bytes nobody can decrypt, without
    /// executing the act.
    Garbage,
    /// Another client logged in just before this act: the session is gone and
    /// the connection is closed without any reply.
    Replaced,
}

#[derive(Clone)]
struct Sms {
    index: u32,
    from: String,
    /// Raw device content: line breaks are stored as 0x12.
    content: String,
    unread: bool,
}

struct Session {
    cipher: SessionCipher,
    cookie: String,
    token: String,
}

#[derive(Default)]
struct Device {
    mode: Option<SessionMode>,
    session: Option<Session>,
    inbox: Vec<Sms>,
    /// Whether `totalNumber` is filled: only after a `PageNumber=0`
    /// selection, on the current session.
    box_loaded: bool,
    page: u32,
    /// Current `DEV2_LTE_SMS_SENDNEWMSG.sendResult`: 1 at rest, 3 while
    /// sending, then `send_outcome`.
    send_result: &'static str,
    /// Final `sendResult` of a send: 1 = sent, 2 = modem busy, 0 = failed.
    send_outcome: &'static str,
    /// Reads answering "3" after a send before `send_outcome` shows up.
    sending_polls: u32,
    polls_since_send: u32,
    /// Firmware without a `sendResult` attribute.
    no_send_result: bool,
    /// Sent messages: recipient and `textContent` as received on the wire.
    sent: Vec<(String, String)>,
    /// `/js/tpEncrypt.js`, when served.
    encrypt_js: Option<&'static str>,
    /// Requests to `/cgi_gdpr` carrying a login signature.
    login_requests: u32,
    /// Login replies sent without status line nor headers.
    headerless_replies: u32,
    /// Logins whose credentials were actually checked.
    credential_checks: u32,
    logins_ok: u32,
    /// One-shot failure injected on the first act matching (op, oid).
    fail_on: Option<(&'static str, &'static str, Failure)>,
}

type Shared = Arc<Mutex<Device>>;

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: u16,
    body: String,
    set_cookie: Option<String>,
    /// Write `body` as is, without status line nor headers.
    bare: bool,
}

impl Response {
    fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            set_cookie: None,
            bare: false,
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            body: format!("<html><title>{status}</title></html>"),
            ..Self::ok("")
        }
    }

    /// What the NE200 does with any request of a session it no longer knows
    /// (replaced by another login): it closes the connection without a reply.
    fn closed() -> Self {
        Self {
            bare: true,
            ..Self::ok("")
        }
    }

    /// What the NE200 sends back to a login it cannot decrypt: a lone
    /// chunked body, then it closes the connection.
    fn headerless() -> Self {
        Self {
            bare: true,
            ..Self::ok(format!("40\r\n{}\r\n0\r\n\r\n", B64.encode([0x5au8; 48])))
        }
    }
}

async fn spawn(device: Device) -> (String, Shared) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let shared: Shared = Arc::new(Mutex::new(device));
    let state = shared.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move { serve(stream, state).await });
        }
    });
    (url, shared)
}

async fn serve(mut stream: TcpStream, state: Shared) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split(' ');
    let (method, path) = (
        first.next().unwrap().to_owned(),
        first.next().unwrap().to_owned(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await.unwrap();
        body.extend_from_slice(&chunk[..n]);
    }
    let req = Request {
        method,
        path,
        headers,
        body: String::from_utf8(body).unwrap(),
    };
    let resp = handle(&mut state.lock().unwrap(), &req);
    if resp.bare {
        stream.write_all(resp.body.as_bytes()).await.unwrap();
        let _ = stream.shutdown().await;
        return;
    }
    let mut out = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.body.len()
    );
    if let Some(c) = resp.set_cookie {
        out.push_str(&format!("Set-Cookie: JSESSIONID={c}; Path=/\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&resp.body);
    stream.write_all(out.as_bytes()).await.unwrap();
    let _ = stream.shutdown().await;
}

fn handle(dev: &mut Device, req: &Request) -> Response {
    let cookie_ok = || {
        let sent = req.header("Cookie").unwrap_or_default();
        dev.session
            .as_ref()
            .is_some_and(|s| sent.contains(&format!("JSESSIONID={}", s.cookie)))
    };
    match (req.method.as_str(), req.path.split('?').next().unwrap()) {
        ("POST", "/cgi/getGDPRParm") => Response::ok(format!(
            "var ee=\"010001\";\nvar nn=\"{NN}\";\nvar seq=\"{SEQ}\";\n$.ret=0;\n"
        )),
        ("GET", "/") => {
            let token = if cookie_ok() {
                dev.session.as_ref().unwrap().token.clone()
            } else {
                String::new()
            };
            Response::ok(format!("<script>var token=\"{token}\";</script>"))
        }
        ("GET", "/js/tpEncrypt.js") => match dev.encrypt_js {
            // Like the NE200 httpd.
            Some(_) if req.header("Referer").is_none() => Response::status(406),
            Some(js) => Response::ok(js),
            None => Response::status(404),
        },
        ("POST", "/cgi_gdpr") if req.path.ends_with("?9") => gdpr(dev, req, cookie_ok()),
        _ => Response::status(404),
    }
}

fn fields(body: &str) -> Vec<(&str, &str)> {
    body.split("\r\n")
        .filter_map(|l| l.split_once('='))
        .collect()
}

fn field<'a>(f: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    f.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// Raw RSA decryption of the hex `sign` blocks.
fn decrypt_sign(sign: &str) -> Option<String> {
    let n = BigUint::parse_bytes(NN.as_bytes(), 16).unwrap();
    let d = BigUint::parse_bytes(DD.as_bytes(), 16).unwrap();
    let mut out = Vec::new();
    for block in sign.as_bytes().chunks(128) {
        let c = BigUint::parse_bytes(block, 16)?;
        let mut m = c.modpow(&d, &n).to_bytes_be();
        while m.last() == Some(&0) {
            m.pop();
        }
        out.extend(m);
    }
    String::from_utf8(out).ok()
}

fn garbage() -> Response {
    Response::ok(B64.encode([0x5au8; 48]))
}

fn seal(cipher: &SessionCipher, text: &str) -> String {
    let sealed = cipher.seal(text);
    sealed.data + sealed.tag.as_deref().unwrap_or("")
}

/// Reply to a login sealed with another variant.
fn login_mismatch(dev: &mut Device) -> Response {
    match dev.mode.unwrap() {
        SessionMode::Cbc => {
            dev.headerless_replies += 1;
            Response::headerless()
        }
        SessionMode::Gcm => garbage(),
    }
}

fn gdpr(dev: &mut Device, req: &Request, cookie_ok: bool) -> Response {
    let f = fields(&req.body);
    let mode = dev.mode.unwrap();
    let Some(sign) = field(&f, "sign").and_then(decrypt_sign) else {
        // Not understood (e.g. an OAEP signature): treated as a login attempt.
        dev.login_requests += 1;
        return login_mismatch(dev);
    };
    let params: Vec<(&str, &str)> = sign.split('&').filter_map(|p| p.split_once('=')).collect();
    let data = field(&f, "data").unwrap();
    let ciphertext = format!("{data}{}", field(&f, "tag").unwrap_or_default());
    let s: u64 = field(&params, "s").unwrap().parse().unwrap();
    assert_eq!(s, SEQ + data.len() as u64, "signature sequence number");
    let hash_ok = field(&params, "h") == Some(credentials_hash(USER, PASSWORD).as_str());

    if let (Some(key), Some(iv)) = (field(&params, "key"), field(&params, "iv")) {
        dev.login_requests += 1;
        let material = match mode {
            SessionMode::Cbc => Some((key.to_owned(), iv.to_owned())),
            SessionMode::Gcm => B64
                .decode(key)
                .ok()
                .zip(B64.decode(iv).ok())
                .and_then(|(k, i)| String::from_utf8(k).ok().zip(String::from_utf8(i).ok())),
        };
        let Some(cipher) = material.and_then(|(k, i)| SessionCipher::from_parts(mode, &k, &i).ok())
        else {
            return login_mismatch(dev);
        };
        if (mode == SessionMode::Gcm) != field(&f, "tag").is_some() {
            return login_mismatch(dev);
        }
        let Ok(login) = cipher.open(&ciphertext) else {
            return login_mismatch(dev);
        };
        let login: Value = serde_json::from_str(&login).unwrap();
        dev.credential_checks += 1;
        let decode = |k: &str| {
            String::from_utf8(B64.decode(login["data"][k].as_str().unwrap()).unwrap()).unwrap()
        };
        if !hash_ok || decode("UserName") != USER || decode("Passwd") != PASSWORD {
            return Response::ok(seal(&cipher, "$.ret=71233;\n"));
        }
        let n = dev.logins_ok;
        dev.logins_ok += 1;
        let body = seal(&cipher, "$.ret=0;\n");
        dev.box_loaded = false;
        dev.session = Some(Session {
            cipher,
            cookie: format!("cookie{n}"),
            token: format!("token{n}"),
        });
        let mut resp = Response::ok(body);
        resp.set_cookie = Some(format!("cookie{n}"));
        return resp;
    }

    assert!(hash_ok, "act signed with a wrong hash");
    let token_ok = dev.session.as_ref().map(|s| s.token.as_str()) == req.header("TokenID");
    if !cookie_ok || !token_ok {
        return Response::closed();
    }
    let cipher = dev.session.as_ref().unwrap().cipher.clone();
    let act: Value = serde_json::from_str(&cipher.open(&ciphertext).unwrap()).unwrap();
    let (op, oid) = (
        act["operation"].as_str().unwrap(),
        act["oid"].as_str().unwrap(),
    );
    if let Some((_, _, failure)) = dev
        .fail_on
        .filter(|(fop, foid, _)| *fop == op && *foid == oid)
    {
        dev.fail_on = None;
        dev.session = None;
        return match failure {
            Failure::Forbidden => Response::status(403),
            Failure::Garbage => garbage(),
            Failure::Replaced => Response::closed(),
        };
    }
    let reply = execute(dev, op, oid, &act["data"]);
    Response::ok(if reply.is_empty() {
        reply
    } else {
        seal(&cipher, &reply)
    })
}

fn execute(dev: &mut Device, op: &str, oid: &str, data: &Value) -> String {
    let ok = r#"{"success":true,"errorcode":0}"#.to_owned();
    let wrap =
        |d: Value| json!({"data": d, "operation": op, "oid": oid, "success": true}).to_string();
    match (op, oid) {
        ("cgi", "/cgi/logout") => {
            dev.session = None;
            String::new()
        }
        // The NE200 refuses the whole act when it names the MR-style send
        // result attributes.
        ("go", "DEV2_LTE_NET_STATUS")
            if ["smsSendResult", "smsSendCause"]
                .iter()
                .any(|a| data.get(a).is_some()) =>
        {
            UNSUPPORTED.into()
        }
        ("go", "DEV2_LTE_NET_STATUS") => wrap(json!({
            "smsUnreadCount": dev.inbox.len().to_string(),
            "sigLevel": "3",
            "rfInfoRsrp": "-97",
            "stack": "1,0,0,0,0,0"
        })),
        ("go", "DEV2_LTE_SMS_SENDNEWMSG") => {
            if dev.send_result == "3" {
                if dev.polls_since_send >= dev.sending_polls {
                    dev.send_result = dev.send_outcome;
                } else {
                    dev.polls_since_send += 1;
                }
            }
            let (to, text) = dev.sent.last().cloned().unwrap_or_default();
            let mut obj = json!({
                "index": "1",
                "to": to,
                "textContent": text,
                "sendResult": dev.send_result,
                "sendTime": "2026-11-21 10:00:00",
                "stack": "0,0,0,0,0,0"
            });
            if dev.no_send_result {
                obj.as_object_mut().unwrap().remove("sendResult");
            }
            wrap(obj)
        }
        ("go", "DEV2_LTE_SMS_RECVMSGBOX") => {
            let total = if dev.box_loaded { dev.inbox.len() } else { 0 };
            wrap(json!({
                "amountPerPage": "0",
                "pageNumber": dev.page.to_string(),
                "stack": "0,0,0,0,0,0",
                "totalNumber": total.to_string(),
                "unreadNumber": dev.inbox.iter().filter(|m| m.unread).count().to_string()
            }))
        }
        ("so", "DEV2_LTE_SMS_RECVMSGBOX") => {
            // Any other spelling (e.g. `pageNumber`) is silently ignored.
            if let Some(page) = data.get("PageNumber") {
                dev.page = page.as_str().unwrap().parse().unwrap();
                dev.box_loaded |= dev.page == 0;
            }
            ok
        }
        ("gl", "DEV2_LTE_SMS_RECVMSGENTRY") => {
            let entries: Vec<String> = page_entries(dev)
                .into_iter()
                .enumerate()
                .map(|(pos, m)| {
                    // Firmware-style raw control characters inside strings.
                    let content = serde_json::to_string(&m.content)
                        .unwrap()
                        .replace("\\u0012", "\u{12}");
                    format!(
                        r#"{{"index":"{}","from":"{}","content":{content},"receivedTime":"2026-11-0{} 16:04:2{}","unread":"{}","stack":"{},0,0,0,0,0"}}"#,
                        m.index,
                        m.from,
                        m.index % 9 + 1,
                        m.index % 10,
                        u8::from(m.unread),
                        pos + 1
                    )
                })
                .collect();
            format!(
                r#"{{"data":[{}],"operation":"gl","oid":"{oid}","success":true}}"#,
                entries.join(",")
            )
        }
        ("so", "DEV2_LTE_SMS_RECVMSGENTRY") => match entry_at(dev, data) {
            Some(index) => {
                let m = dev.inbox.iter_mut().find(|m| m.index == index).unwrap();
                m.unread = data["unread"].as_str().unwrap() == "1";
                ok
            }
            None => r#"{"success":false,"errorcode":9203}"#.into(),
        },
        ("do", "DEV2_LTE_SMS_RECVMSGENTRY") => match entry_at(dev, data) {
            Some(index) => {
                dev.inbox.retain(|m| m.index != index);
                ok
            }
            None => r#"{"success":false,"errorcode":9203}"#.into(),
        },
        ("so", "DEV2_LTE_SMS_SENDNEWMSG") => {
            let to = data["to"].as_str().unwrap().to_owned();
            dev.sent
                .push((to, data["textContent"].as_str().unwrap().to_owned()));
            dev.send_result = "3";
            dev.polls_since_send = 0;
            ok
        }
        _ => UNSUPPORTED.into(),
    }
}

/// Entries of the selected page, newest (highest index) first.
fn page_entries(dev: &Device) -> Vec<Sms> {
    let mut sorted = dev.inbox.clone();
    sorted.sort_by_key(|m| std::cmp::Reverse(m.index));
    let start = ((dev.page.max(1) - 1) * 8) as usize;
    sorted.into_iter().skip(start).take(8).collect()
}

/// Index of the message an entry act targets: its stack is the 1-based
/// position on the selected page (an NE200 page lists index 8 at stack 1).
fn entry_at(dev: &Device, data: &Value) -> Option<u32> {
    let stack = data["stack"].as_str().unwrap();
    let pos: usize = stack.split(',').next().unwrap().parse().unwrap();
    page_entries(dev).get(pos.checked_sub(1)?).map(|m| m.index)
}

fn inbox(messages: &[(u32, &str)]) -> Vec<Sms> {
    messages
        .iter()
        .map(|&(index, content)| Sms {
            index,
            from: format!("+3361234{index:04}"),
            content: content.to_owned(),
            unread: true,
        })
        .collect()
}

fn device(mode: SessionMode, messages: u32) -> Device {
    Device {
        mode: Some(mode),
        send_result: "1",
        send_outcome: "1",
        sending_polls: 2,
        inbox: (1..=messages)
            .map(|i| Sms {
                index: i,
                from: format!("+3361234{i:04}"),
                content: if i == 2 {
                    "two\u{12}lines é😀".to_owned()
                } else {
                    format!("message {i}")
                },
                unread: true,
            })
            .collect(),
        ..Default::default()
    }
}

fn client(url: &str, password: &str) -> Client {
    let mut cfg = ClientConfig::new(url, USER, password);
    cfg.timeout = Duration::from_secs(5);
    cfg.send_result_timeout = Duration::from_secs(2);
    cfg.send_result_interval = Duration::from_millis(100);
    Client::new(cfg).unwrap()
}

#[tokio::test]
async fn auto_probe_reaches_cbc_without_extra_credential_checks() {
    let (url, dev) = spawn(device(SessionMode::Cbc, 0)).await;
    let mut c = client(&url, PASSWORD);
    assert_eq!(c.login().await.unwrap(), Variant::Cbc);
    assert_eq!(c.variant(), Some(Variant::Cbc));
    let d = dev.lock().unwrap();
    // GCM and GCM-OAEP got the headerless reply, then CBC was understood.
    assert_eq!(
        (
            d.login_requests,
            d.headerless_replies,
            d.credential_checks,
            d.logins_ok
        ),
        (3, 2, 1, 1)
    );
}

#[tokio::test]
async fn ui_script_hint_selects_cbc_first() {
    let mut d = device(SessionMode::Cbc, 0);
    d.encrypt_js = Some("var aes = CryptoJS.AES.encrypt(s, k, {mode: CryptoJS.mode.CBC});");
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    assert_eq!(c.login().await.unwrap(), Variant::Cbc);
    let d = dev.lock().unwrap();
    assert_eq!((d.login_requests, d.headerless_replies), (1, 0));
}

#[tokio::test]
async fn wrong_password_costs_a_single_credential_check() {
    let (url, dev) = spawn(device(SessionMode::Gcm, 0)).await;
    let mut c = client(&url, "wrong");
    let err = c.unread_count().await.unwrap_err();
    assert!(
        matches!(err, Error::Login(LoginError::BadCredentials)),
        "{err:?}"
    );
    assert_eq!(err.kind(), ErrorKind::NotExecuted);
    let d = dev.lock().unwrap();
    assert_eq!((d.login_requests, d.credential_checks), (1, 1));
}

#[tokio::test]
async fn inbox_spans_pages_and_survives_session_expiry() {
    let mut d = device(SessionMode::Gcm, 11);
    // Expire the session in the middle of the page reads.
    d.fail_on = Some(("gl", "DEV2_LTE_SMS_RECVMSGENTRY", Failure::Forbidden));
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    let inbox = c.inbox().await.unwrap();
    assert_eq!(inbox.len(), 11);
    assert_eq!(inbox.iter().filter(|e| e.location.page == 2).count(), 3);
    let two = inbox.iter().find(|e| e.index == 2).unwrap();
    assert_eq!(two.content, "two\nlines é😀");
    assert!(two.received_at.is_some());
    assert_eq!(dev.lock().unwrap().logins_ok, 2);
}

#[tokio::test]
async fn inbox_is_loaded_before_listing_and_marked_read_in_place() {
    let mut d = device(SessionMode::Cbc, 0);
    d.inbox = inbox(&[
        (4, "first"),
        (7, "second"),
        (
            8,
            "accents éèàçù €, emoji 📡\u{12}ligne 2\u{11}\u{12}ligne 3",
        ),
    ]);
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    assert_eq!(c.unread_count().await.unwrap(), 3);
    assert_eq!(c.inbox_total().await.unwrap(), 3);
    let entries = c.inbox().await.unwrap();
    let listed: Vec<(u32, &str)> = entries
        .iter()
        .map(|e| (e.index, e.location.stack.as_str()))
        .collect();
    assert_eq!(
        listed,
        [(8, "1,0,0,0,0,0"), (7, "2,0,0,0,0,0"), (4, "3,0,0,0,0,0")]
    );
    assert_eq!(
        entries[0].content,
        "accents éèàçù €, emoji 📡\nligne 2\r\nligne 3"
    );
    c.mark_read(&entries[1].location).await.unwrap();
    let unread: Vec<u32> = dev
        .lock()
        .unwrap()
        .inbox
        .iter()
        .filter(|m| m.unread)
        .map(|m| m.index)
        .collect();
    assert_eq!(unread, [4, 8]);
}

#[tokio::test]
async fn send_rejected_before_execution_is_submitted_once_after_relogin() {
    let mut d = device(SessionMode::Gcm, 0);
    d.fail_on = Some(("so", "DEV2_LTE_SMS_SENDNEWMSG", Failure::Forbidden));
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    let report = c
        .send_sms("+33612345678", "hello\r\nworld\n€")
        .await
        .unwrap();
    assert_eq!(report.status, SendStatus::Sent);
    assert_eq!(report.source, Some("DEV2_LTE_SMS_SENDNEWMSG.sendResult"));
    assert_eq!(report.baseline.as_deref(), Some("1"));
    assert_eq!(report.observed, ["3", "3", "1"]);
    let d = dev.lock().unwrap();
    // Line breaks are escaped like the web UI does.
    assert_eq!(
        d.sent,
        vec![(
            "+33612345678".to_owned(),
            "hello\u{11}\u{12}world\u{12}€".to_owned()
        )]
    );
    assert_eq!(d.logins_ok, 2);
}

#[tokio::test]
async fn send_status_follows_the_first_final_send_result() {
    let cases = [
        ("1", 1, SendStatus::Sent, &["3", "1"][..]),
        ("2", 2, SendStatus::Busy, &["3", "3", "2"][..]),
        // Already final at the first read, which comes right after the send.
        ("0", 0, SendStatus::Failed, &["0"][..]),
    ];
    for (outcome, sending_polls, status, observed) in cases {
        let mut d = device(SessionMode::Gcm, 0);
        d.send_outcome = outcome;
        d.sending_polls = sending_polls;
        let (url, dev) = spawn(d).await;
        let mut c = client(&url, PASSWORD);
        let report = c.send_sms("+33612345678", "hi").await.unwrap();
        assert_eq!(report.status, status, "{outcome}");
        assert_eq!(report.observed, observed);
        let d = dev.lock().unwrap();
        assert_eq!((d.sent.len(), d.send_result), (1, outcome));
    }
}

#[tokio::test]
async fn send_without_result_attribute_is_reported_submitted() {
    let mut d = device(SessionMode::Gcm, 0);
    d.no_send_result = true;
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    // The DEV2_LTE_NET_STATUS fallback is refused (9003): still sent once.
    for _ in 0..2 {
        let report = c.send_sms("+33612345678", "hi").await.unwrap();
        assert_eq!(
            (report.status, report.source),
            (SendStatus::Submitted, None)
        );
    }
    let d = dev.lock().unwrap();
    assert_eq!((d.sent.len(), d.logins_ok), (2, 1));
}

#[tokio::test]
async fn ambiguous_send_reply_is_never_resent() {
    let mut d = device(SessionMode::Cbc, 0);
    d.fail_on = Some(("so", "DEV2_LTE_SMS_SENDNEWMSG", Failure::Garbage));
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    let err = c.send_sms("+33612345678", "hi").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::OutcomeUnknown, "{err:?}");
    assert!(!c.is_logged_in());
    {
        let d = dev.lock().unwrap();
        assert!(d.sent.is_empty());
        assert_eq!(d.logins_ok, 1);
    }
    // The next call logs in again transparently.
    assert_eq!(c.unread_count().await.unwrap(), 0);
    let d = dev.lock().unwrap();
    assert_eq!(d.logins_ok, 2);
    assert!(d.sent.is_empty());
}

#[tokio::test]
async fn replaced_session_is_renewed_transparently_on_reads() {
    let mut d = device(SessionMode::Cbc, 0);
    d.fail_on = Some(("go", "DEV2_LTE_NET_STATUS", Failure::Replaced));
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    assert_eq!(c.unread_count().await.unwrap(), 0);
    assert_eq!(dev.lock().unwrap().logins_ok, 2);
}

#[tokio::test]
async fn send_on_a_replaced_session_is_unknown_and_never_resent() {
    let mut d = device(SessionMode::Cbc, 0);
    d.fail_on = Some(("so", "DEV2_LTE_SMS_SENDNEWMSG", Failure::Replaced));
    let (url, dev) = spawn(d).await;
    let mut c = client(&url, PASSWORD);
    let err = c.send_sms("+33612345678", "hi").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::OutcomeUnknown, "{err:?}");
    assert!(!c.is_logged_in());
    assert!(dev.lock().unwrap().sent.is_empty());
    // The dead session is not reused: the next call logs in again.
    assert_eq!(c.unread_count().await.unwrap(), 0);
    let d = dev.lock().unwrap();
    assert_eq!(d.logins_ok, 2);
    assert!(d.sent.is_empty());
}

#[tokio::test]
async fn resolved_delete_targets_the_selected_message_and_logout_ends_the_session() {
    let (url, dev) = spawn(device(SessionMode::Gcm, 10)).await;
    let mut c = client(&url, PASSWORD);
    let deleted = c
        .delete_sms_resolved(|entries| entries.iter().position(|e| e.index == 2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((deleted.index, deleted.location.page), (2, 2));
    assert_eq!(c.delete_sms_resolved(|_| None).await.unwrap(), None);
    let left: Vec<u32> = dev.lock().unwrap().inbox.iter().map(|m| m.index).collect();
    assert_eq!(left, vec![1, 3, 4, 5, 6, 7, 8, 9, 10]);

    c.logout().await.unwrap();
    assert!(!c.is_logged_in());
    assert!(dev.lock().unwrap().session.is_none());
}
