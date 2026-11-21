//! "Acts": the JSON operations carried by `/cgi_gdpr`.

use std::fmt;

use serde_json::Value;

use crate::error::{Error, Result};

/// Default object stack (`stack` / `pstack` fields).
pub const ROOT_STACK: &str = "0,0,0,0,0,0";

/// Act operation codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// `go`: read one object.
    Get,
    /// `gl`: read every instance of an object (list).
    GetList,
    /// `gs`: read a subtree.
    GetSubtree,
    /// `so`: write attributes of an object (also used to trigger actions,
    /// e.g. sending an SMS).
    Set,
    /// `add`: create an instance.
    Add,
    /// `del`: delete an instance.
    Del,
    /// `do`: execute an object-specific action (e.g. delete an SMS entry).
    Do,
    /// `op`: generic operation.
    Op,
    /// `cgi`: call a CGI endpoint (`/cgi/login`, `/cgi/logout`...).
    Cgi,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Get => "go",
            Operation::GetList => "gl",
            Operation::GetSubtree => "gs",
            Operation::Set => "so",
            Operation::Add => "add",
            Operation::Del => "del",
            Operation::Do => "do",
            Operation::Op => "op",
            Operation::Cgi => "cgi",
        }
    }

    /// Whether the operation only reads device state.
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Operation::Get | Operation::GetList | Operation::GetSubtree
        )
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Operation {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "go" | "get" => Operation::Get,
            "gl" => Operation::GetList,
            "gs" => Operation::GetSubtree,
            "so" | "set" => Operation::Set,
            "add" => Operation::Add,
            "del" => Operation::Del,
            "do" => Operation::Do,
            "op" => Operation::Op,
            "cgi" => Operation::Cgi,
            other => return Err(Error::InvalidInput(format!("unknown operation {other:?}"))),
        })
    }
}

/// One operation on a `DEV2_*` object.
///
/// Read operations list the wanted attribute names (sent with empty values);
/// write operations carry key/value pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct Act {
    operation: Operation,
    oid: String,
    stack: String,
    pstack: String,
    attrs: Vec<(String, Value)>,
    replay_safe: bool,
}

impl Act {
    pub fn new(operation: Operation, oid: impl Into<String>) -> Self {
        Self {
            operation,
            oid: oid.into(),
            stack: ROOT_STACK.into(),
            pstack: ROOT_STACK.into(),
            attrs: Vec::new(),
            replay_safe: operation.is_read(),
        }
    }

    /// `go` reading the given attributes (all of them when empty).
    pub fn get(oid: impl Into<String>, attrs: &[&str]) -> Self {
        Self::new(Operation::Get, oid).fields(attrs)
    }

    /// `gl` reading the given attributes of every instance.
    pub fn get_list(oid: impl Into<String>, attrs: &[&str]) -> Self {
        Self::new(Operation::GetList, oid).fields(attrs)
    }

    /// `so` on an object; add values with [`Act::attr`].
    pub fn set(oid: impl Into<String>) -> Self {
        Self::new(Operation::Set, oid)
    }

    pub fn stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = stack.into();
        self
    }

    pub fn pstack(mut self, pstack: impl Into<String>) -> Self {
        self.pstack = pstack.into();
        self
    }

    /// Adds a `key: value` attribute.
    pub fn attr(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attrs.push((key.into(), Value::String(value.into())));
        self
    }

    /// Requests attributes by name (empty values, for read operations).
    pub fn fields(mut self, names: &[&str]) -> Self {
        self.attrs.extend(
            names
                .iter()
                .map(|n| ((*n).to_owned(), Value::String(String::new()))),
        );
        self
    }

    /// Declares whether the act may be sent again when its first attempt
    /// ended in an ambiguous way (e.g. an undecryptable reply after which
    /// the session is re-established). Defaults to `true` for reads only;
    /// set it for idempotent writes such as selecting an inbox page.
    pub fn replay_safe(mut self, safe: bool) -> Self {
        self.replay_safe = safe;
        self
    }

    pub fn operation(&self) -> Operation {
        self.operation
    }

    pub fn oid(&self) -> &str {
        &self.oid
    }

    pub fn stack_str(&self) -> &str {
        &self.stack
    }

    pub fn is_replay_safe(&self) -> bool {
        self.replay_safe
    }

    /// Attribute names, for logging without values.
    pub(crate) fn attr_names(&self) -> Vec<&str> {
        self.attrs.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// Serializes the act to the JSON the device expects, keeping the field
    /// order used by the web UI (`stack`, `pstack`, attributes).
    pub fn to_json(&self) -> String {
        let q = |s: &str| Value::String(s.to_owned()).to_string();
        let mut out = String::with_capacity(96 + self.attrs.len() * 24);
        out.push_str("{\"data\":{\"stack\":");
        out.push_str(&q(&self.stack));
        out.push_str(",\"pstack\":");
        out.push_str(&q(&self.pstack));
        for (k, v) in &self.attrs {
            out.push(',');
            out.push_str(&q(k));
            out.push(':');
            out.push_str(&v.to_string());
        }
        out.push_str("},\"operation\":");
        out.push_str(&q(self.operation.as_str()));
        out.push_str(",\"oid\":");
        out.push_str(&q(&self.oid));
        out.push('}');
        out
    }
}

/// Escapes raw control characters that some firmwares leave inside JSON
/// strings (e.g. line breaks in SMS bodies), so that the reply parses without
/// losing them.
pub(crate) fn sanitize_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        } else {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
        }
    }
    out
}

/// Interprets the decrypted reply of an act: `{"data":...,"success":true}`,
/// `{"success":true,"errorcode":0}`, or an empty body.
pub(crate) fn parse_reply(act: &Act, plain: &str) -> Result<Option<Value>> {
    let trimmed = plain.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&sanitize_json(trimmed)).map_err(|e| {
        Error::Protocol(format!(
            "reply to {} {} is not JSON ({e}, {} bytes)",
            act.operation,
            act.oid,
            trimmed.len()
        ))
    })?;
    let Value::Object(mut obj) = value else {
        return Err(Error::Protocol(format!(
            "reply to {} {} is not a JSON object",
            act.operation, act.oid
        )));
    };
    let code = obj.get("errorcode").and_then(error_code);
    let success = obj.get("success").and_then(Value::as_bool);
    if success == Some(false) || code.is_some() {
        return Err(Error::Refused {
            operation: act.operation.to_string(),
            oid: act.oid.clone(),
            code: code.unwrap_or_else(|| "unknown".into()),
        });
    }
    Ok(obj.remove("data").filter(|d| !d.is_null()))
}

/// Returns the error code when it denotes a failure.
fn error_code(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) if n.as_i64() == Some(0) => None,
        Value::String(s) if s.is_empty() || s == "0" => None,
        Value::Null => None,
        Value::Bool(false) => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Normalizes a `data` value to a list of objects (`gl` returns a single
/// object instead of a one-element array on some firmwares).
pub(crate) fn as_list(data: Option<Value>) -> Vec<serde_json::Map<String, Value>> {
    match data {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(o) => Some(o),
                _ => None,
            })
            .collect(),
        Some(Value::Object(o)) => vec![o],
        _ => Vec::new(),
    }
}

/// Reads an attribute as a string (values are strings on the wire, numbers
/// are tolerated).
pub(crate) fn str_attr<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

/// Reads an integer attribute, tolerating decimal strings like `"12.0000"`.
pub(crate) fn int_attr(obj: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    match obj.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| {
                s.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(|f| f as i64)
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gl_entries() -> Act {
        Act::get_list("DEV2_LTE_SMS_RECVMSGENTRY", &["index", "from"])
    }

    #[test]
    fn act_json_keeps_web_ui_field_order_and_escapes_values() {
        let act = Act::set("DEV2_LTE_SMS_SENDNEWMSG")
            .attr("index", "1")
            .attr("to", "+33612345678")
            .attr("textContent", "Line one\nsaid \"hi\" é😀");
        assert_eq!(
            act.to_json(),
            concat!(
                r#"{"data":{"stack":"0,0,0,0,0,0","pstack":"0,0,0,0,0,0","index":"1","#,
                r#""to":"+33612345678","textContent":"Line one\nsaid \"hi\" é😀"},"#,
                r#""operation":"so","oid":"DEV2_LTE_SMS_SENDNEWMSG"}"#
            )
        );
        assert!(!act.is_replay_safe());

        let read = Act::get("DEV2_LTE_NET_STATUS", &["smsUnreadCount"]).stack("1,0,0,0,0,0");
        assert_eq!(
            read.to_json(),
            concat!(
                r#"{"data":{"stack":"1,0,0,0,0,0","pstack":"0,0,0,0,0,0","smsUnreadCount":""},"#,
                r#""operation":"go","oid":"DEV2_LTE_NET_STATUS"}"#
            )
        );
        assert!(read.is_replay_safe());
    }

    #[test]
    fn inbox_listing_sample_parses() {
        let reply = concat!(
            r#"{"data":[{"index":"2","from":"sender2","content":"text first","#,
            r#""receivedTime":"2024-11-15 22:23:59","unread":"0","stack":"2,0,0,0,0,0"},"#,
            r#"{"index":"1","from":"sender1","content":"text second","#,
            r#""receivedTime":"2024-11-15 22:28:09","unread":"1","stack":"1,0,0,0,0,0"}],"#,
            r#""operation":"gl","oid":"DEV2_LTE_SMS_RECVMSGENTRY","success":true}"#
        );
        let entries = as_list(parse_reply(&gl_entries(), reply).unwrap());
        assert_eq!(entries.len(), 2);
        assert_eq!(int_attr(&entries[0], "index"), Some(2));
        assert_eq!(str_attr(&entries[1], "stack"), Some("1,0,0,0,0,0"));
        assert_eq!(str_attr(&entries[1], "unread"), Some("1"));
    }

    #[test]
    fn raw_control_characters_in_strings_are_preserved() {
        let reply = "{\"data\":{\"index\":\"3\",\"content\":\"a\nb\tc\",\"stack\":\"3,0,0,0,0,0\"},\n\"success\":true}";
        let entries = as_list(parse_reply(&gl_entries(), reply).unwrap());
        // A single object instead of a one-element list is accepted too.
        assert_eq!(entries.len(), 1);
        assert_eq!(str_attr(&entries[0], "content"), Some("a\nb\tc"));
    }

    #[test]
    fn success_without_data_and_empty_replies_carry_no_data() {
        let act = Act::get("DEV2_MEM_STATUS", &[]);
        assert_eq!(
            parse_reply(&act, r#"{"success":true,"errorcode":0}"#).unwrap(),
            None
        );
        assert_eq!(
            parse_reply(&act, r#"{"success":true, "errorcode":"0"}"#).unwrap(),
            None
        );
        assert_eq!(parse_reply(&act, "  \r\n").unwrap(), None);
        assert_eq!(parse_reply(&act, "{}").unwrap(), None);
    }

    #[test]
    fn device_errors_are_refusals() {
        let act = Act::new(Operation::Do, "DEV2_LTE_SMS_RECVMSGENTRY");
        match parse_reply(&act, r#"{"success":false,"errorcode":9203}"#) {
            Err(Error::Refused {
                operation,
                oid,
                code,
            }) => {
                assert_eq!(
                    (operation.as_str(), oid.as_str(), code.as_str()),
                    ("do", act.oid(), "9203")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse_reply(&act, r#"{"errorcode":"exceeded max attempts"}"#),
            Err(Error::Refused { .. })
        ));
        assert!(matches!(
            parse_reply(&act, "[error]0"),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn numeric_attributes_tolerate_device_formats() {
        let obj: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"totalStatistics":"38819729967.0000","rfInfoRsrp":"-85","n":7,"bad":"x"}"#,
        )
        .unwrap();
        assert_eq!(int_attr(&obj, "totalStatistics"), Some(38_819_729_967));
        assert_eq!(int_attr(&obj, "rfInfoRsrp"), Some(-85));
        assert_eq!(int_attr(&obj, "n"), Some(7));
        assert_eq!(int_attr(&obj, "bad"), None);
        assert_eq!(int_attr(&obj, "missing"), None);
    }
}
