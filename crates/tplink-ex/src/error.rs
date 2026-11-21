use crate::Variant;
use crate::crypto::CryptoError;

/// How an [`Error`] relates to the execution of the request on the device.
///
/// This is what callers need to decide whether a retry is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request was not executed by the device: it never left the client,
    /// the device was unreachable, the login failed, or the device rejected
    /// the session before executing anything. Retrying is safe.
    NotExecuted,
    /// The request was sent but no usable answer came back (timeout, broken
    /// connection, server error, undecryptable reply). It may or may not have
    /// been executed.
    OutcomeUnknown,
    /// The device answered and refused the request.
    Refused,
    /// The device answered something this client does not understand.
    Protocol,
}

/// Why logging in failed. Nothing but the login itself was attempted.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The device rejected the credentials (`$.ret=71233`).
    #[error(
        "wrong username or password (EX firmwares usually expect the username \"user\", not \"admin\")"
    )]
    BadCredentials,
    /// The device refused the login with another return code (71234 is the
    /// generic "bad request"; other codes may mean the account is
    /// temporarily locked or another administrator is logged in).
    #[error("login refused by the device ($.ret={0})")]
    Refused(i64),
    /// None of the attempted cipher variants produced a reply we could
    /// decrypt: the firmware probably uses an unsupported scheme.
    #[error("no cipher variant matched the device (tried {tried:?})")]
    NoMatchingVariant { tried: Vec<Variant> },
    /// `cgi/getGDPRParm` did not return a usable RSA key / sequence number.
    #[error("cannot fetch the device public key: {0}")]
    KeyExchange(String),
    /// The `TokenID` could not be found in the index page after login.
    #[error("logged in but no TokenID found in the web UI page")]
    MissingToken,
    /// Transport failure (timeout, I/O) during the login steps.
    #[error("transport error during login")]
    Transport(#[source] reqwest::Error),
    /// Unexpected HTTP status during the login steps.
    #[error("HTTP {status} from {step}")]
    Status { step: &'static str, status: u16 },
}

/// Errors returned by [`Client`](crate::Client).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid configuration or argument; nothing was sent.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The device could not be reached (connection refused, no route,
    /// connect timeout, TLS failure); nothing was sent.
    #[error("device unreachable")]
    Unreachable(#[source] reqwest::Error),
    /// Logging in failed; the requested operation was not attempted.
    #[error("login failed: {0}")]
    Login(#[source] LoginError),
    /// The device rejected the session (HTTP 401/403, redirect, or a reply
    /// that was not encrypted with our session key) before executing the
    /// request. The client re-logs in once transparently before surfacing
    /// this error.
    #[error("session rejected by the device: {0}")]
    SessionExpired(String),
    /// The request was sent but its outcome is unknown.
    #[error("request outcome unknown: {reason}")]
    OutcomeUnknown {
        reason: String,
        #[source]
        source: Option<reqwest::Error>,
    },
    /// A multi-step operation was abandoned before its effectful step (for
    /// example the inbox could not be listed before a delete, or the
    /// pre-send session check failed): nothing was changed on the device.
    #[error("{reason}")]
    Aborted {
        reason: String,
        #[source]
        source: Box<Error>,
    },
    /// The device answered `success:false` / a non-zero `errorcode`.
    #[error("device refused {operation} {oid} (errorcode {code})")]
    Refused {
        operation: String,
        oid: String,
        code: String,
    },
    /// The device answer could not be understood.
    #[error("unexpected device response: {0}")]
    Protocol(String),
    /// Local cryptographic failure (bad key material from the device).
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

impl From<LoginError> for Error {
    fn from(e: LoginError) -> Self {
        Error::Login(e)
    }
}

impl Error {
    /// The message followed by the messages of its sources, joined by `: `
    /// (e.g. `device unreachable: error sending request: Connection refused`).
    pub fn detailed(&self) -> String {
        let mut out = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(s) = source {
            let msg = s.to_string();
            if !out.ends_with(&msg) {
                out.push_str(": ");
                out.push_str(&msg);
            }
            source = s.source();
        }
        out
    }

    /// Classifies the error with respect to execution on the device.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::InvalidInput(_)
            | Error::Unreachable(_)
            | Error::Login(_)
            | Error::SessionExpired(_)
            | Error::Aborted { .. }
            | Error::Crypto(_) => ErrorKind::NotExecuted,
            Error::OutcomeUnknown { .. } => ErrorKind::OutcomeUnknown,
            Error::Refused { .. } => ErrorKind::Refused,
            Error::Protocol(_) => ErrorKind::Protocol,
        }
    }

    /// Whether the device rejected the session itself (as opposed to the
    /// request).
    pub fn is_session_expired(&self) -> bool {
        matches!(self, Error::SessionExpired(_))
    }

    pub(crate) fn unknown(reason: impl Into<String>) -> Self {
        Error::OutcomeUnknown {
            reason: reason.into(),
            source: None,
        }
    }

    /// Wraps an error raised by a preparatory (read-only) step of an
    /// operation whose effectful step was then skipped. Session rejections
    /// are kept as is so that the caller can re-login and retry.
    pub(crate) fn before_effect(self, reason: &str) -> Self {
        match self.kind() {
            ErrorKind::NotExecuted => self,
            _ => Error::Aborted {
                reason: reason.to_owned(),
                source: Box::new(self),
            },
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
