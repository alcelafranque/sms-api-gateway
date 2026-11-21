//! SMS encoding and segment counting (3GPP TS 23.038 / 23.040).
//!
//! A text fits GSM-7 when every character is in the default alphabet or its
//! extension table (the latter costing an escape septet each). Otherwise it
//! is sent as UCS-2, counted in UTF-16 code units, so characters outside the
//! Basic Multilingual Plane (most emoji) count twice. Concatenated messages
//! lose room to the user data header: 153 septets or 67 UCS-2 units per
//! segment instead of 160 / 70 for a single SMS. This matches the counting
//! of the NE200 web UI, which allows up to 5 segments.

use serde::Serialize;
use utoipa::ToSchema;

/// GSM 03.38 default alphabet (escape 0x1B excluded).
const GSM7_BASIC: &str = concat!(
    "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?",
    "¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà"
);
/// GSM 03.38 extension table, each character sent as escape + code.
const GSM7_EXTENSION: &str = "\u{c}^{}\\[~]|€";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Gsm7,
    Ucs2,
}

impl Encoding {
    /// Units (septets or UTF-16 code units) that fit in a single SMS.
    fn single(self) -> usize {
        match self {
            Encoding::Gsm7 => 160,
            Encoding::Ucs2 => 70,
        }
    }

    /// Units per segment of a concatenated SMS.
    fn per_segment(self) -> usize {
        match self {
            Encoding::Gsm7 => 153,
            Encoding::Ucs2 => 67,
        }
    }

    /// Largest text, in units, that fits in `segments` segments.
    pub fn max_units(self, segments: u32) -> usize {
        match segments {
            0 => 0,
            1 => self.single(),
            n => n as usize * self.per_segment(),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Gsm7 => "GSM-7",
            Encoding::Ucs2 => "UCS-2",
        }
    }
}

/// Size of a text once encoded for SMS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmsSize {
    pub encoding: Encoding,
    /// GSM-7 septets or UCS-2 (UTF-16) code units.
    pub units: usize,
    /// Segments needed to send the text (1 for an empty text).
    pub segments: u32,
}

impl SmsSize {
    pub fn of(text: &str) -> Self {
        let gsm7 = text.chars().try_fold(0usize, |n, c| {
            if GSM7_BASIC.contains(c) {
                Some(n + 1)
            } else if GSM7_EXTENSION.contains(c) {
                Some(n + 2)
            } else {
                None
            }
        });
        let (encoding, units) = match gsm7 {
            Some(n) => (Encoding::Gsm7, n),
            None => (Encoding::Ucs2, text.encode_utf16().count()),
        };
        let segments = if units <= encoding.single() {
            1
        } else {
            units.div_ceil(encoding.per_segment())
        };
        Self {
            encoding,
            units,
            segments: u32::try_from(segments).unwrap_or(u32::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(text: &str) -> (Encoding, usize, u32) {
        let s = SmsSize::of(text);
        (s.encoding, s.units, s.segments)
    }

    #[test]
    fn single_sms_boundaries() {
        assert_eq!(size(&"a".repeat(160)), (Encoding::Gsm7, 160, 1));
        assert_eq!(size(&"a".repeat(161)), (Encoding::Gsm7, 161, 2));
        assert_eq!(size(&"é".repeat(70)), (Encoding::Gsm7, 70, 1));
        assert_eq!(size(&"ê".repeat(70)), (Encoding::Ucs2, 70, 1));
        assert_eq!(size(&"ê".repeat(71)), (Encoding::Ucs2, 71, 2));
        assert_eq!(size(""), (Encoding::Gsm7, 0, 1));
    }

    #[test]
    fn extension_characters_cost_two_septets() {
        assert_eq!(size("€[]{}^~|\\"), (Encoding::Gsm7, 18, 1));
        // 159 + 2 septets: one character over a single SMS.
        let text = format!("{}€", "a".repeat(159));
        assert_eq!(size(&text), (Encoding::Gsm7, 161, 2));
        assert_eq!(size(&"€".repeat(382)), (Encoding::Gsm7, 764, 5));
        assert_eq!(size(&"€".repeat(383)), (Encoding::Gsm7, 766, 6));
        // Line breaks, accents of the default alphabet and Greek capitals.
        assert_eq!(size("Ça\r\nva? ΔΩ ñ"), (Encoding::Gsm7, 12, 1));
    }

    #[test]
    fn non_gsm_text_falls_back_to_ucs2_units() {
        // Lowercase ç is not in the default alphabet (only Ç is).
        assert_eq!(size("ç"), (Encoding::Ucs2, 1, 1));
        // Emoji outside the BMP are surrogate pairs.
        assert_eq!(size("ok 📡"), (Encoding::Ucs2, 5, 1));
        assert_eq!(size(&"😀".repeat(35)), (Encoding::Ucs2, 70, 1));
        assert_eq!(size(&"😀".repeat(36)), (Encoding::Ucs2, 72, 2));
        // One non-GSM character switches the whole text, extension
        // characters included, to UCS-2.
        let text = format!("{}€ш", "a".repeat(100));
        assert_eq!(size(&text), (Encoding::Ucs2, 102, 2));
    }

    #[test]
    fn segment_limits() {
        assert_eq!(Encoding::Gsm7.max_units(1), 160);
        assert_eq!(Encoding::Gsm7.max_units(5), 765);
        assert_eq!(Encoding::Ucs2.max_units(1), 70);
        assert_eq!(Encoding::Ucs2.max_units(5), 335);
        assert_eq!(size(&"a".repeat(765)).2, 5);
        assert_eq!(size(&"a".repeat(766)).2, 6);
        assert_eq!(size(&"ш".repeat(335)).2, 5);
    }
}
