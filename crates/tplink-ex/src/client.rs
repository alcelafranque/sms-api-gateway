//! Session management and transport.

use std::fmt;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::header::{ACCEPT, REFERER};
use serde_json::Value;
use tracing::{debug, info, trace, warn};

use crate::act::{Act, Operation, parse_reply};
use crate::crypto::{
    RsaPublicKey, SessionCipher, SessionMode, SignaturePadding, credentials_hash, signature_text,
};
use crate::error::{Error, LoginError, Result};
use crate::sms::ResultSourceCache;

/// Browser-like user agent; some firmwares are picky about unknown clients.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Attempts for requests hit by the sporadic HTTP 500 / 406 answers of the
/// device httpd (only for requests that are safe to send again).
const QUIRK_ATTEMPTS: usize = 3;
const QUIRK_DELAY: Duration = Duration::from_millis(100);
/// Pause between two login attempts while probing cipher variants.
const PROBE_DELAY: Duration = Duration::from_millis(500);
/// Endpoint of every encrypted request.
const GDPR_PATH: &str = "/cgi_gdpr?9";

/// Encryption scheme of the `/cgi_gdpr` API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Variant {
    /// Probe the device at first login (see [`Client::login`]).
    #[default]
    Auto,
    /// AES-128-CBC session, raw RSA signature (older EX firmwares).
    Cbc,
    /// AES-128-GCM session, raw RSA signature.
    Gcm,
    /// AES-128-GCM session, RSA-OAEP signature (EX920-class firmwares).
    GcmOaep,
}

impl Variant {
    /// Concrete variants, in the default probing order.
    pub const PROBE_ORDER: [Variant; 3] = [Variant::Gcm, Variant::GcmOaep, Variant::Cbc];

    fn scheme(self) -> Option<(SessionMode, SignaturePadding)> {
        match self {
            Variant::Auto => None,
            Variant::Cbc => Some((SessionMode::Cbc, SignaturePadding::Raw)),
            Variant::Gcm => Some((SessionMode::Gcm, SignaturePadding::Raw)),
            Variant::GcmOaep => Some((SessionMode::Gcm, SignaturePadding::Oaep)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Auto => "auto",
            Variant::Cbc => "cbc",
            Variant::Gcm => "gcm",
            Variant::GcmOaep => "gcm-oaep",
        }
    }
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Variant {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "auto" => Ok(Variant::Auto),
            "cbc" => Ok(Variant::Cbc),
            "gcm" => Ok(Variant::Gcm),
            "gcm-oaep" | "gcmoaep" | "oaep" => Ok(Variant::GcmOaep),
            other => Err(Error::InvalidInput(format!(
                "unknown variant {other:?} (expected auto, cbc, gcm or gcm-oaep)"
            ))),
        }
    }
}

/// Client settings.
#[derive(Clone)]
pub struct ClientConfig {
    /// Base URL of the web UI, e.g. `http://192.168.254.1`.
    pub base_url: String,
    /// Web UI user. EX firmwares usually expect `user`, MR ones `admin`.
    pub username: String,
    pub password: String,
    pub variant: Variant,
    /// Whole-request timeout.
    pub timeout: Duration,
    /// TCP/TLS connect timeout (capped by `timeout`).
    pub connect_timeout: Duration,
    /// Accept invalid TLS certificates (self-signed device certificates).
    pub accept_invalid_certs: bool,
    /// How long [`Client::send_sms`] polls for a send result after the
    /// device accepted the message. Zero disables polling.
    pub send_result_timeout: Duration,
    /// Interval between two send-result polls.
    pub send_result_interval: Duration,
}

impl ClientConfig {
    pub fn new(
        base_url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            username: username.into(),
            password: password.into(),
            variant: Variant::Auto,
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            accept_invalid_certs: false,
            send_result_timeout: Duration::from_secs(20),
            send_result_interval: Duration::from_millis(500),
        }
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("base_url", &self.base_url)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("variant", &self.variant)
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("accept_invalid_certs", &self.accept_invalid_certs)
            .field("send_result_timeout", &self.send_result_timeout)
            .field("send_result_interval", &self.send_result_interval)
            .finish()
    }
}

/// Client for one device.
///
/// Holds at most one logged-in session, opened lazily by the first request
/// and re-opened once, transparently, when the device rejects it. Methods
/// take `&mut self`: the device accepts a single administrator session, so
/// callers sharing a client must serialize access (e.g. behind a mutex).
pub struct Client {
    config: ClientConfig,
    base: String,
    hash: String,
    detected: Option<Variant>,
    pub(crate) session: Option<Session>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("username", &self.config.username)
            .field("variant", &self.config.variant)
            .field("detected", &self.detected)
            .field("logged_in", &self.session.is_some())
            .finish()
    }
}

/// Runs `$body` with `$s` bound to a logged-in session. If the device
/// rejected the session (nothing executed), logs in again and runs the body
/// once more from the start. Multi-step operations rely on this to stay
/// consistent (e.g. an inbox page selection is lost with the session).
///
/// A macro rather than a closure-taking method so that the body is inlined
/// in the caller's future, which keeps it `Send`.
macro_rules! run_session {
    ($client:expr, $s:ident => $body:expr) => {{
        let mut retried = false;
        loop {
            let $s = $client.ensure_session().await?;
            let result = $body;
            $client.discard_poisoned();
            match result {
                Err(e) if e.is_session_expired() && !retried => {
                    tracing::info!(reason = %e, "device rejected the session, logging in again");
                    $client.drop_session();
                    retried = true;
                }
                other => break other,
            }
        }
    }};
}
pub(crate) use run_session;

/// Outcome of a single login attempt with one variant.
enum Attempt {
    Success(Box<Session>),
    /// The device did not understand us: probably the wrong variant.
    Mismatch(String),
    /// Anything else; stop probing.
    Fatal(Error),
}

impl Client {
    /// Validates the configuration. Does not contact the device.
    pub fn new(config: ClientConfig) -> Result<Self> {
        let url = reqwest::Url::parse(config.base_url.trim())
            .map_err(|e| Error::InvalidInput(format!("invalid base URL: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(Error::InvalidInput(
                "base URL must be http(s)://host[:port]".into(),
            ));
        }
        if config.timeout.is_zero() {
            return Err(Error::InvalidInput("timeout must not be zero".into()));
        }
        let base = url.as_str().trim_end_matches('/').to_owned();
        let hash = credentials_hash(&config.username, &config.password);
        let detected = config.variant.scheme().map(|_| config.variant);
        Ok(Self {
            config,
            base,
            hash,
            detected,
            session: None,
        })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The variant in use: the configured one, or the one detected by the
    /// first successful login when configured as [`Variant::Auto`].
    pub fn variant(&self) -> Option<Variant> {
        self.detected
    }

    pub fn is_logged_in(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.poisoned)
    }

    /// Forgets the current session without contacting the device.
    pub fn drop_session(&mut self) {
        self.session = None;
    }

    /// Opens a new session (dropping any current one) and returns the
    /// variant used.
    ///
    /// With [`Variant::Auto`], variants are tried one by one, starting with
    /// the previously detected one, then a hint taken from the web UI
    /// scripts, then [`Variant::PROBE_ORDER`]. Probing only moves on to the
    /// next variant when the device's answer cannot be decrypted, which means
    /// it did not understand the request and therefore cannot have checked
    /// the credentials. Any decryptable answer (including "wrong password")
    /// ends the probe, so a bad password costs a single failed attempt.
    pub async fn login(&mut self) -> Result<Variant> {
        self.session = None;
        let candidates = self.candidates().await;
        let mut tried = Vec::new();
        for (i, variant) in candidates.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(PROBE_DELAY).await;
            }
            tried.push(variant);
            match self.try_login(variant).await {
                Attempt::Success(session) => {
                    info!(base = %self.base, %variant, "logged in to device");
                    self.detected = Some(variant);
                    self.session = Some(*session);
                    return Ok(variant);
                }
                Attempt::Mismatch(why) => {
                    debug!(%variant, reason = %why, "login reply not understood, variant mismatch");
                }
                Attempt::Fatal(e) => return Err(e),
            }
        }
        warn!(base = %self.base, ?tried, "no cipher variant matched the device");
        Err(LoginError::NoMatchingVariant { tried }.into())
    }

    /// Logs out (so that a human can use the web UI). No-op without a
    /// session. The next request logs in again.
    pub async fn logout(&mut self) -> Result<()> {
        let Some(mut session) = self.session.take() else {
            return Ok(());
        };
        if session.poisoned {
            return Ok(());
        }
        let result = session
            .act(&Act::new(Operation::Cgi, "/cgi/logout"))
            .await
            .map(|_| ());
        match &result {
            Ok(()) => info!(base = %self.base, "logged out from device"),
            Err(e) => debug!(error = %e, "logout request failed"),
        }
        result
    }

    /// Sends one act and returns its `data` (if any).
    ///
    /// Logs in when needed. When the device rejects the session before
    /// executing the act, the client logs in again and re-sends it once.
    /// Acts that are not [replay-safe](Act::replay_safe) are never re-sent
    /// after an ambiguous failure.
    pub async fn act(&mut self, act: &Act) -> Result<Option<Value>> {
        run_session!(self, s => s.act(act).await)
    }

    /// Drops the session if the device stopped recognizing it.
    pub(crate) fn discard_poisoned(&mut self) {
        if self.session.as_ref().is_some_and(|s| s.poisoned) {
            self.session = None;
        }
    }

    pub(crate) async fn ensure_session(&mut self) -> Result<&mut Session> {
        self.discard_poisoned();
        if self.session.is_none() {
            self.login().await?;
        }
        Ok(self.session.as_mut().expect("session just opened"))
    }

    async fn candidates(&self) -> Vec<Variant> {
        let mut order = Vec::with_capacity(3);
        if self.config.variant != Variant::Auto {
            order.push(self.config.variant);
            return order;
        }
        if let Some(v) = self.detected {
            order.push(v);
        } else if let Some(hint) = self.variant_hint().await {
            debug!(%hint, "web UI scripts suggest a cipher variant");
            order.push(hint);
        }
        for v in Variant::PROBE_ORDER {
            if !order.contains(&v) {
                order.push(v);
            }
        }
        order
    }

    /// Looks for the signature padding in the web UI encryption script.
    /// Purely passive: a static file, no credentials involved.
    async fn variant_hint(&self) -> Option<Variant> {
        let http = self.http_client().ok()?;
        let url = format!("{}/js/tpEncrypt.js", self.base);
        let resp = http
            .get(&url)
            .header(ACCEPT, "*/*")
            // Requests without a Referer are answered 406 Not Acceptable.
            .header(REFERER, format!("{}/", self.base))
            .timeout(self.config.timeout.min(Duration::from_secs(10)))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let js = resp.text().await.ok()?;
        if js.contains("OAEP") || js.contains("Oaep") {
            Some(Variant::GcmOaep)
        } else if js.contains("GCM") || js.contains("gcm") {
            Some(Variant::Gcm)
        } else if js.contains("CBC") {
            // NE200-Outdoor v1 firmware: `mode.CBC` in tpEncrypt.js.
            Some(Variant::Cbc)
        } else {
            None
        }
    }

    fn http_client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .cookie_store(true)
            .timeout(self.config.timeout)
            .connect_timeout(self.config.connect_timeout.min(self.config.timeout))
            .redirect(reqwest::redirect::Policy::none())
            // A fresh connection per request: a stale keep-alive connection
            // closed by the device would turn a request into "outcome
            // unknown".
            .pool_max_idle_per_host(0)
            .danger_accept_invalid_certs(self.config.accept_invalid_certs)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| Error::InvalidInput(format!("cannot build HTTP client: {e}")))
    }

    async fn try_login(&self, variant: Variant) -> Attempt {
        match self.login_steps(variant).await {
            Ok(attempt) => attempt,
            Err(e) => Attempt::Fatal(e),
        }
    }

    async fn login_steps(&self, variant: Variant) -> Result<Attempt> {
        let (mode, padding) = variant
            .scheme()
            .ok_or_else(|| Error::InvalidInput("cannot log in with variant auto".into()))?;
        let http = self.http_client()?;
        let referer = match variant {
            Variant::Cbc => format!("{}/", self.base),
            _ => self.base.clone(),
        };

        // 1. Ephemeral RSA key and sequence number.
        let url = format!("{}/cgi/getGDPRParm?_={}", self.base, now_ms());
        let (status, text) = send_with_quirks(
            || {
                http.post(&url)
                    .header(ACCEPT, "*/*")
                    .header(REFERER, &referer)
            },
            QUIRK_ATTEMPTS,
        )
        .await
        .map_err(login_transport)?;
        if status != 200 {
            return Err(LoginError::KeyExchange(format!("HTTP {status}")).into());
        }
        let params = KeyParams::parse(&text).map_err(LoginError::KeyExchange)?;
        let rsa = RsaPublicKey::from_hex(&params.nn, &params.ee)
            .map_err(|e| LoginError::KeyExchange(e.to_string()))?;
        if rsa.size() != 64 {
            debug!(bits = rsa.size() * 8, "unusual RSA key size");
        }

        let mut session = Session {
            http,
            base: self.base.clone(),
            referer,
            cipher: SessionCipher::generate(mode),
            rsa,
            padding,
            seq: params.seq,
            hash: self.hash.clone(),
            token: String::new(),
            poisoned: false,
            result_source: ResultSourceCache::default(),
        };

        // 2. Login act, which also hands the session key to the device.
        let login = format!(
            concat!(
                "{{\"data\":{{\"UserName\":\"{}\",\"Passwd\":\"{}\",\"Action\":\"1\",",
                "\"stack\":\"0,0,0,0,0,0\",\"pstack\":\"0,0,0,0,0,0\"}},",
                "\"operation\":\"cgi\",\"oid\":\"/cgi/login\"}}"
            ),
            B64.encode(self.config.username.as_bytes()),
            B64.encode(self.config.password.as_bytes()),
        );
        let body = session.seal_body(&login, true)?;
        debug!(%variant, username = %self.config.username, "sending login");
        let url = format!("{}{GDPR_PATH}", self.base);
        let (status, text) =
            match send_with_quirks(|| session.post(&url, body.clone()), QUIRK_ATTEMPTS).await {
                Ok(reply) => reply,
                // A CBC firmware (NE200-Outdoor) answers a login sealed with
                // another variant with a bare chunked body and no status line,
                // or by closing the connection. The device could not decrypt
                // the credentials, so trying the next variant is safe.
                Err(e) if !e.is_connect() && !e.is_timeout() => {
                    return Ok(Attempt::Mismatch(format!("malformed login reply: {e}")));
                }
                Err(e) => return Err(login_transport(e)),
            };
        if status != 200 {
            return Ok(Attempt::Mismatch(format!("HTTP {status}")));
        }
        let plain = match session.cipher.open(&text) {
            Ok(p) => p,
            Err(e) => return Ok(Attempt::Mismatch(format!("reply not decryptable: {e}"))),
        };
        trace!(reply = %plain, "login reply");
        match parse_ret(&plain) {
            Some(0) => {}
            Some(71233) => return Err(LoginError::BadCredentials.into()),
            Some(code) => return Err(LoginError::Refused(code).into()),
            None if mode == SessionMode::Gcm => {
                // An authenticated decryption proves the device has our key.
                return Err(Error::Protocol(format!(
                    "login reply has no return code ({} bytes)",
                    plain.len()
                )));
            }
            None => return Ok(Attempt::Mismatch("no return code in reply".into())),
        }

        // 3. TokenID, inlined in the index page once logged in.
        let url = format!("{}/?_={}", self.base, now_ms());
        let (status, text) = send_with_quirks(
            || {
                session
                    .http
                    .get(&url)
                    .header(ACCEPT, "*/*")
                    .header(REFERER, &session.referer)
            },
            QUIRK_ATTEMPTS,
        )
        .await
        .map_err(login_transport)?;
        if status != 200 {
            return Err(LoginError::Status {
                step: "index page",
                status,
            }
            .into());
        }
        session.token = js_var(&text, "token")
            .filter(|t| !t.is_empty())
            .ok_or(LoginError::MissingToken)?;
        Ok(Attempt::Success(Box::new(session)))
    }
}

fn login_transport(e: reqwest::Error) -> Error {
    if e.is_connect() {
        Error::Unreachable(e)
    } else {
        LoginError::Transport(e).into()
    }
}

/// Logged-in session state.
pub(crate) struct Session {
    http: reqwest::Client,
    base: String,
    referer: String,
    cipher: SessionCipher,
    rsa: RsaPublicKey,
    padding: SignaturePadding,
    seq: u64,
    hash: String,
    token: String,
    /// Set when the device stopped recognizing the session.
    pub(crate) poisoned: bool,
    pub(crate) result_source: ResultSourceCache,
}

impl Session {
    fn seal_body(&self, plaintext: &str, login: bool) -> Result<String> {
        let sealed = self.cipher.seal(plaintext);
        let seq = self.seq + sealed.data.len() as u64;
        let text = signature_text(login.then_some(&self.cipher), &self.hash, seq);
        let sign = self.rsa.sign(self.padding, &text)?;
        let mut body = format!("sign={sign}\r\ndata={}\r\n", sealed.data);
        if let Some(tag) = sealed.tag {
            body.push_str("tag=");
            body.push_str(&tag);
            body.push_str("\r\n");
        }
        Ok(body)
    }

    fn post(&self, url: &str, body: String) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(url)
            .header(ACCEPT, "*/*")
            .header(REFERER, &self.referer)
            .body(body);
        if !self.token.is_empty() {
            req = req.header("TokenID", &self.token);
        }
        req
    }

    /// Sends one act on this session, without any re-login.
    pub(crate) async fn act(&mut self, act: &Act) -> Result<Option<Value>> {
        debug!(
            op = %act.operation(),
            oid = act.oid(),
            stack = act.stack_str(),
            attrs = ?act.attr_names(),
            "act"
        );
        let body = self.seal_body(&act.to_json(), false)?;
        let url = format!("{}{GDPR_PATH}", self.base);
        let attempts = if act.is_replay_safe() {
            QUIRK_ATTEMPTS
        } else {
            1
        };
        let (status, text) =
            match send_with_quirks(|| self.post(&url, body.clone()), attempts).await {
                Ok(r) => r,
                Err(e) if e.is_connect() => return Err(Error::Unreachable(e)),
                Err(e) if e.is_builder() => return Err(Error::InvalidInput(e.to_string())),
                Err(e) => {
                    return Err(Error::OutcomeUnknown {
                        reason: format!("transport error on {} {}", act.operation(), act.oid()),
                        source: Some(e),
                    });
                }
            };
        self.interpret(act, status, &text)
    }

    fn interpret(&mut self, act: &Act, status: u16, text: &str) -> Result<Option<Value>> {
        match status {
            200 => {}
            401 | 403 => {
                self.poisoned = true;
                return Err(Error::SessionExpired(format!("HTTP {status}")));
            }
            300..=399 => {
                self.poisoned = true;
                return Err(Error::SessionExpired(format!("redirected (HTTP {status})")));
            }
            s if is_quirk(s, text) || s >= 500 => {
                return Err(Error::unknown(format!(
                    "HTTP {s} on {} {}",
                    act.operation(),
                    act.oid()
                )));
            }
            s => {
                return Err(Error::Protocol(format!(
                    "HTTP {s} on {} {}",
                    act.operation(),
                    act.oid()
                )));
            }
        }
        if is_quirk(status, text) {
            return Err(Error::unknown(format!(
                "server error page on {} {}",
                act.operation(),
                act.oid()
            )));
        }
        if text.trim().is_empty() {
            debug!(oid = act.oid(), "empty reply");
            return Ok(None);
        }
        let plain = match self.cipher.open(text) {
            Ok(p) => p,
            Err(e) => {
                // The device did not use our session key: it no longer knows
                // this session (another login, idle timeout, reboot).
                self.poisoned = true;
                if looks_plaintext(text) {
                    debug!(bytes = text.len(), "reply is not encrypted");
                    return Err(Error::SessionExpired(
                        "reply not encrypted with the session key".into(),
                    ));
                }
                return if act.is_replay_safe() {
                    Err(Error::SessionExpired(format!("undecryptable reply ({e})")))
                } else {
                    Err(Error::unknown(format!(
                        "undecryptable reply to {} {} ({e})",
                        act.operation(),
                        act.oid()
                    )))
                };
            }
        };
        trace!(oid = act.oid(), reply = %plain, "act reply");
        parse_reply(act, &plain)
    }
}

/// Sends a request, retrying on the sporadic HTTP 500 / 406 answers.
async fn send_with_quirks(
    build: impl Fn() -> reqwest::RequestBuilder,
    attempts: usize,
) -> std::result::Result<(u16, String), reqwest::Error> {
    let mut attempt = 1;
    loop {
        let resp = build().send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if attempt >= attempts || !is_quirk(status, &text) {
            return Ok((status, text));
        }
        debug!(status, attempt, "device answered a server error, retrying");
        attempt += 1;
        tokio::time::sleep(QUIRK_DELAY).await;
    }
}

fn is_quirk(status: u16, text: &str) -> bool {
    status == 500
        || status == 406
        || text.contains("<title>500 Internal Server Error</title>")
        || text.contains("<title>406 Not Acceptable</title>")
}

/// Whether a body contains characters that cannot appear in a base64
/// ciphertext.
fn looks_plaintext(text: &str) -> bool {
    text.chars().any(|c| {
        !(c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c.is_ascii_whitespace())
    })
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// Extracts `value` from `var <name>="value"` in a script/HTML page.
fn js_var(text: &str, name: &str) -> Option<String> {
    let needle = format!("var {name}=");
    let mut rest = text;
    while let Some(pos) = rest.find(&needle) {
        rest = &rest[pos + needle.len()..];
        let trimmed = rest.trim_start();
        if let Some(quoted) = trimmed
            .strip_prefix('"')
            .or_else(|| trimmed.strip_prefix('\''))
        {
            let quote = trimmed.as_bytes()[0] as char;
            if let Some(end) = quoted.find(quote) {
                return Some(quoted[..end].to_owned());
            }
        }
    }
    None
}

/// Parses the `$.ret=<code>;` marker of CGI replies, or an `errorcode`
/// field when the reply is JSON.
fn parse_ret(text: &str) -> Option<i64> {
    if let Some(pos) = text.find("$.ret=") {
        let digits: String = text[pos + 6..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        return digits.parse().ok();
    }
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    match v.get("errorcode")? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

struct KeyParams {
    nn: String,
    ee: String,
    seq: u64,
}

impl KeyParams {
    fn parse(text: &str) -> std::result::Result<Self, String> {
        if let Some(code) = parse_ret(text).filter(|c| *c != 0) {
            return Err(format!("$.ret={code}"));
        }
        let nn = js_var(text, "nn").ok_or("no modulus (nn) in reply")?;
        let ee = js_var(text, "ee").ok_or("no exponent (ee) in reply")?;
        let seq = js_var(text, "seq")
            .ok_or("no sequence number (seq) in reply")?
            .trim()
            .parse()
            .map_err(|_| "sequence number is not numeric".to_owned())?;
        if nn.is_empty() || nn.len() % 2 != 0 {
            return Err(format!(
                "unexpected modulus length ({} hex chars)",
                nn.len()
            ));
        }
        Ok(Self { nn, ee, seq })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_exchange_reply_parses() {
        let text = "var ee=\"010001\";\nvar nn=\"C7A1\";\nvar seq=\"851402736\";\n$.ret=0;\n";
        let p = KeyParams::parse(text).unwrap();
        assert_eq!(
            (p.nn.as_str(), p.ee.as_str(), p.seq),
            ("C7A1", "010001", 851_402_736)
        );
        assert!(KeyParams::parse("var ee=\"010001\";\n$.ret=0;").is_err());
        assert!(KeyParams::parse("$.ret=71017;").is_err());
        assert!(KeyParams::parse("var ee=\"3\";var nn=\"ABC\";var seq=\"1\";").is_err());
    }

    #[test]
    fn token_is_read_from_the_index_page() {
        let page = r#"<script type="text/javascript">var token="086724f57013f16e042e012becf825";</script>"#;
        assert_eq!(
            js_var(page, "token").as_deref(),
            Some("086724f57013f16e042e012becf825")
        );
        assert_eq!(js_var("<html>login</html>", "token"), None);
    }

    #[test]
    fn login_return_codes_parse() {
        assert_eq!(parse_ret("$.ret=0;\n"), Some(0));
        assert_eq!(parse_ret("var x=1;\n$.ret=71233;"), Some(71233));
        assert_eq!(
            parse_ret(r#"{"errorcode":71234,"success":false}"#),
            Some(71234)
        );
        assert_eq!(parse_ret("garbage"), None);
    }

    #[test]
    fn plaintext_replies_are_told_apart_from_ciphertext() {
        assert!(!looks_plaintext("q8Zx+/09AbC=\r\n"));
        assert!(looks_plaintext(r#"{"success":false}"#));
        assert!(looks_plaintext("<html><title>403 Forbidden</title></html>"));
    }

    #[test]
    fn variant_names_round_trip() {
        for v in [Variant::Auto, Variant::Cbc, Variant::Gcm, Variant::GcmOaep] {
            assert_eq!(v.as_str().parse::<Variant>().unwrap(), v);
        }
        assert_eq!("GCM_OAEP".parse::<Variant>().unwrap(), Variant::GcmOaep);
        assert!("aes".parse::<Variant>().is_err());
    }
}
