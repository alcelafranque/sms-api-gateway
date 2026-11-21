//! Phone number normalization, validation and log masking.

use std::fmt;

use phonenumber::Mode;
use phonenumber::country::Id;

use crate::config::ValidationConfig;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NumberError {
    #[error("not a valid phone number")]
    Invalid,
    #[error("destination not allowed by the gateway policy")]
    NotAllowed,
}

pub struct NumberPolicy {
    region: Id,
    allowed: Vec<String>,
    denied: Vec<String>,
}

impl NumberPolicy {
    pub fn new(config: &ValidationConfig) -> Self {
        Self {
            region: config.default_region.parse().unwrap_or(Id::FR),
            allowed: config.allowed_prefixes.clone(),
            denied: config.denied_prefixes.clone(),
        }
    }

    /// Normalizes to E.164, without applying the prefix policy.
    pub fn normalize(&self, raw: &str) -> Result<String, NumberError> {
        let number =
            phonenumber::parse(Some(self.region), raw.trim()).map_err(|_| NumberError::Invalid)?;
        if !phonenumber::is_valid(&number) {
            return Err(NumberError::Invalid);
        }
        Ok(number.format().mode(Mode::E164).to_string())
    }

    /// Normalizes an outbound destination and applies the prefix policy.
    pub fn destination(&self, raw: &str) -> Result<String, NumberError> {
        let e164 = self.normalize(raw)?;
        if self.denied.iter().any(|p| e164.starts_with(p.as_str())) {
            return Err(NumberError::NotAllowed);
        }
        if !self.allowed.is_empty() && !self.allowed.iter().any(|p| e164.starts_with(p.as_str())) {
            return Err(NumberError::NotAllowed);
        }
        Ok(e164)
    }

    /// Best-effort normalization of an inbound sender: E.164 when it parses,
    /// otherwise the raw value (alphanumeric senders, short codes).
    pub fn sender(&self, raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.chars().any(|c| c.is_ascii_alphabetic()) {
            return trimmed.to_owned();
        }
        self.normalize(trimmed)
            .unwrap_or_else(|_| trimmed.to_owned())
    }
}

/// Displays a phone number with its middle digits hidden, for logs.
pub struct Masked<'a>(pub &'a str);

impl fmt::Display for Masked<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chars: Vec<char> = self.0.chars().collect();
        let n = chars.len();
        if n <= 4 {
            return f.write_str(&"*".repeat(n));
        }
        let head = if n > 8 { 4 } else { 1 };
        let tail = 2;
        for (i, c) in chars.iter().enumerate() {
            if i < head || i >= n - tail {
                write!(f, "{c}")?;
            } else {
                f.write_str("*")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&str], denied: &[&str]) -> NumberPolicy {
        NumberPolicy::new(&ValidationConfig {
            default_region: "FR".into(),
            allowed_prefixes: allowed.iter().map(|p| p.to_string()).collect(),
            denied_prefixes: denied.iter().map(|p| p.to_string()).collect(),
            ..ValidationConfig::default()
        })
    }

    #[test]
    fn normalizes_national_and_international_forms_to_e164() {
        let p = policy(&[], &[]);
        for raw in [
            "0612345678",
            "06 12 34 56 78",
            "+33 6 12 34 56 78",
            "0033612345678",
        ] {
            assert_eq!(p.destination(raw).as_deref(), Ok("+33612345678"), "{raw}");
        }
        assert_eq!(
            p.destination("+447911123456").as_deref(),
            Ok("+447911123456")
        );
        for raw in ["12", "0612", "not a number", ""] {
            assert_eq!(p.destination(raw), Err(NumberError::Invalid), "{raw}");
        }
    }

    #[test]
    fn denied_prefixes_win_over_allowed_ones() {
        let p = policy(&["+33"], &["+3389"]);
        assert!(p.destination("0612345678").is_ok());
        assert_eq!(p.destination("0899123456"), Err(NumberError::NotAllowed));
        assert_eq!(p.destination("+447911123456"), Err(NumberError::NotAllowed));
        // Normalization happens before the policy check.
        assert_eq!(
            p.destination("+33 8 99 12 34 56"),
            Err(NumberError::NotAllowed)
        );
    }

    #[test]
    fn inbound_senders_keep_non_numeric_values() {
        let p = policy(&[], &[]);
        assert_eq!(p.sender("0611111111"), "+33611111111");
        assert_eq!(p.sender(" INFO "), "INFO");
        assert_eq!(p.sender("36179"), "36179");
    }

    #[test]
    fn masking_hides_the_middle_digits() {
        assert_eq!(Masked("+33612345678").to_string(), "+336******78");
        assert_eq!(Masked("36179").to_string(), "3**79");
        assert_eq!(Masked("1234").to_string(), "****");
    }
}
