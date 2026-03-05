use serde::{Deserialize, Serialize};
use std::fmt;

/// A human-readable duration that serializes as seconds (u64).
///
/// Accepts:
/// - Plain integer (seconds): `300`, `60`
/// - String with units: `"30s"`, `"5m"`, `"1h"`, `"1h30m"`, `"2h15m30s"`, `"300"`
///
/// Always serializes as u64 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub u64);

impl HumanDuration {
    pub fn as_secs(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0;
        if secs == 0 {
            return write!(f, "0s");
        }
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            write!(f, "{}h", h)?;
        }
        if m > 0 {
            write!(f, "{}m", m)?;
        }
        if s > 0 || (h == 0 && m == 0) {
            write!(f, "{}s", s)?;
        }
        Ok(())
    }
}

/// Parse a human-readable duration string into seconds.
///
/// Supported formats:
/// - Plain number (seconds): `"300"`, `"60"`
/// - Units: `"30s"`, `"5m"`, `"1h"`, `"1h30m"`, `"2h15m30s"`
///
/// Returns error for zero, negative, or invalid formats.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("duration string is empty".to_string());
    }

    // Try parsing as plain integer first
    if let Ok(n) = s.parse::<u64>() {
        if n == 0 {
            return Err("duration must be greater than 0".to_string());
        }
        return Ok(n);
    }

    // Parse unit-based format: combinations of Nh, Nm, Ns
    let mut total_secs: u64 = 0;
    let mut current_num = String::new();
    let mut has_any_unit = false;

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current_num.push(ch);
        } else {
            if current_num.is_empty() {
                return Err(format!(
                    "unexpected character '{}' without preceding number",
                    ch
                ));
            }
            let n: u64 = current_num
                .parse()
                .map_err(|_| format!("invalid number: {}", current_num))?;
            current_num.clear();

            let component = match ch {
                'h' => n
                    .checked_mul(3600)
                    .ok_or_else(|| format!("duration value {}h overflows", n))?,
                'm' => n
                    .checked_mul(60)
                    .ok_or_else(|| format!("duration value {}m overflows", n))?,
                's' => n,
                _ => {
                    return Err(format!(
                        "unknown duration unit '{}' (expected h, m, or s)",
                        ch
                    ))
                }
            };
            total_secs = total_secs.saturating_add(component);
            has_any_unit = true;
        }
    }

    // If there are trailing digits without a unit, that's an error
    if !current_num.is_empty() {
        return Err(format!(
            "trailing number '{}' without unit (expected h, m, or s)",
            current_num
        ));
    }

    if !has_any_unit {
        return Err(format!("invalid duration format: '{}'", s));
    }

    if total_secs == 0 {
        return Err("duration must be greater than 0".to_string());
    }

    Ok(total_secs)
}

impl Serialize for HumanDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HumanDurationVisitor;

        impl<'de> serde::de::Visitor<'de> for HumanDurationVisitor {
            type Value = HumanDuration;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "a positive integer (seconds) or a duration string like \"5m\", \"1h30m\"",
                )
            }

            fn visit_u64<E>(self, value: u64) -> Result<HumanDuration, E>
            where
                E: serde::de::Error,
            {
                if value == 0 {
                    return Err(E::custom("duration must be greater than 0"));
                }
                Ok(HumanDuration(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<HumanDuration, E>
            where
                E: serde::de::Error,
            {
                if value <= 0 {
                    return Err(E::custom("duration must be greater than 0"));
                }
                Ok(HumanDuration(value as u64))
            }

            fn visit_str<E>(self, value: &str) -> Result<HumanDuration, E>
            where
                E: serde::de::Error,
            {
                parse_duration(value).map(HumanDuration).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(HumanDurationVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_duration tests ---

    #[test]
    fn test_parse_plain_seconds() {
        assert_eq!(parse_duration("300"), Ok(300));
        assert_eq!(parse_duration("1"), Ok(1));
        assert_eq!(parse_duration("86400"), Ok(86400));
    }

    #[test]
    fn test_parse_seconds_unit() {
        assert_eq!(parse_duration("30s"), Ok(30));
        assert_eq!(parse_duration("1s"), Ok(1));
    }

    #[test]
    fn test_parse_minutes() {
        assert_eq!(parse_duration("5m"), Ok(300));
        assert_eq!(parse_duration("1m"), Ok(60));
        assert_eq!(parse_duration("90m"), Ok(5400));
    }

    #[test]
    fn test_parse_hours() {
        assert_eq!(parse_duration("1h"), Ok(3600));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert_eq!(parse_duration("24h"), Ok(86400));
    }

    #[test]
    fn test_parse_combined() {
        assert_eq!(parse_duration("1h30m"), Ok(5400));
        assert_eq!(parse_duration("2h15m30s"), Ok(8130));
        assert_eq!(parse_duration("1m30s"), Ok(90));
    }

    #[test]
    fn test_parse_zero_rejected() {
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("0h0m0s").is_err());
    }

    #[test]
    fn test_parse_empty_rejected() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("  ").is_err());
    }

    #[test]
    fn test_parse_invalid_format() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("m5").is_err());
        assert!(parse_duration("1h2").is_err()); // trailing number without unit
    }

    #[test]
    fn test_parse_overflow_returns_error() {
        // Huge hours value: n * 3600 would overflow u64 without checked_mul
        assert!(parse_duration("6000000000000000000h").is_err());
        assert!(parse_duration("999999999999999999m").is_err());
    }

    #[test]
    fn test_parse_negative_via_json_rejected() {
        let result: Result<HumanDuration, _> = serde_json::from_str("-1");
        assert!(result.is_err());
        let result: Result<HumanDuration, _> = serde_json::from_str("-100");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_whitespace_trimmed() {
        assert_eq!(parse_duration(" 5m "), Ok(300));
        assert_eq!(parse_duration(" 300 "), Ok(300));
    }

    // --- HumanDuration serde tests ---

    #[test]
    fn test_serde_yaml_integer() {
        let d: HumanDuration = serde_yaml::from_str("300").unwrap();
        assert_eq!(d.0, 300);
    }

    #[test]
    fn test_serde_yaml_string() {
        let d: HumanDuration = serde_yaml::from_str("\"5m\"").unwrap();
        assert_eq!(d.0, 300);
    }

    #[test]
    fn test_serde_yaml_string_combined() {
        let d: HumanDuration = serde_yaml::from_str("\"1h30m\"").unwrap();
        assert_eq!(d.0, 5400);
    }

    #[test]
    fn test_serde_yaml_zero_rejected() {
        let result: Result<HumanDuration, _> = serde_yaml::from_str("0");
        assert!(result.is_err());
    }

    #[test]
    fn test_serde_json_integer() {
        let d: HumanDuration = serde_json::from_str("300").unwrap();
        assert_eq!(d.0, 300);
    }

    #[test]
    fn test_serde_json_string() {
        let d: HumanDuration = serde_json::from_str("\"10m\"").unwrap();
        assert_eq!(d.0, 600);
    }

    #[test]
    fn test_serialize_as_u64() {
        let d = HumanDuration(300);
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "300");
    }

    #[test]
    fn test_display() {
        assert_eq!(HumanDuration(0).to_string(), "0s");
        assert_eq!(HumanDuration(30).to_string(), "30s");
        assert_eq!(HumanDuration(60).to_string(), "1m");
        assert_eq!(HumanDuration(90).to_string(), "1m30s");
        assert_eq!(HumanDuration(3600).to_string(), "1h");
        assert_eq!(HumanDuration(3661).to_string(), "1h1m1s");
        assert_eq!(HumanDuration(5400).to_string(), "1h30m");
    }

    // --- Option<HumanDuration> serde tests ---

    #[test]
    fn test_option_none_yaml() {
        #[derive(Deserialize)]
        struct S {
            #[serde(default)]
            timeout: Option<HumanDuration>,
        }
        let s: S = serde_yaml::from_str("{}").unwrap();
        assert!(s.timeout.is_none());
    }

    #[test]
    fn test_option_some_yaml() {
        #[derive(Deserialize)]
        struct S {
            timeout: Option<HumanDuration>,
        }
        let s: S = serde_yaml::from_str("timeout: 5m").unwrap();
        assert_eq!(s.timeout, Some(HumanDuration(300)));
    }

    #[test]
    fn test_option_some_integer_yaml() {
        #[derive(Deserialize)]
        struct S {
            timeout: Option<HumanDuration>,
        }
        let s: S = serde_yaml::from_str("timeout: 300").unwrap();
        assert_eq!(s.timeout, Some(HumanDuration(300)));
    }
}
