//! Exact-match step filter for JSONL log lines.

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// `true` when `line` is a JSON object whose `step` field — the last one if
/// the key repeats — is a string equal to `step_name`. Same contract as the
/// `serde_json::Value` matcher it replaces, without building a `Value`:
/// ignored fields are skipped by `IgnoredAny`, the `step` value is compared
/// inside the visitor, so matching an `n`-byte line allocates at most
/// serde_json's escape scratch for that line.
pub fn line_matches_step(line: &str, step_name: &str) -> bool {
    // Fast path: most lines of a multi-step job belong to other steps.
    if !line.contains(step_name) {
        return false;
    }
    let mut de = serde_json::Deserializer::from_str(line);
    let matched = match de::Deserializer::deserialize_map(&mut de, LineVisitor { step_name }) {
        Ok(matched) => matched,
        Err(_) => return false,
    };
    // Reject trailing content, as `serde_json::from_str` does.
    de.end().is_ok() && matched
}

/// [`line_matches_step`] over raw bytes; invalid UTF-8 never matches.
pub fn matches_bytes(line: &[u8], step_name: &str) -> bool {
    std::str::from_utf8(line).is_ok_and(|s| line_matches_step(s, step_name))
}

/// The matcher this module replaced, kept for tests that pin equivalence
/// and the documented divergences.
#[cfg(test)]
pub(crate) fn legacy_value_matcher(line: &str, step_name: &str) -> bool {
    if !line.contains(step_name) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .map(|v| v.get("step").and_then(|s| s.as_str()) == Some(step_name))
        .unwrap_or(false)
}

struct LineVisitor<'s> {
    step_name: &'s str,
}

impl<'de> Visitor<'de> for LineVisitor<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut matched = false;
        while let Some(is_step) = map.next_key_seed(KeyIsStep)? {
            if is_step {
                matched = map.next_value_seed(StepValueEquals {
                    step_name: self.step_name,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(matched)
    }
}

/// Deserialises a map key and reports whether it is `step`. Escaped keys
/// arrive through `visit_str`, unescaped ones through `visit_borrowed_str`,
/// which forwards to `visit_str` by default.
struct KeyIsStep;

impl<'de> DeserializeSeed<'de> for KeyIsStep {
    type Value = bool;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for KeyIsStep {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string key")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
        Ok(v == "step")
    }
}

/// Deserialises the `step` value and compares it in place. Compound values
/// are drained so a later duplicate `step` key is still reached.
struct StepValueEquals<'s> {
    step_name: &'s str,
}

impl<'de> DeserializeSeed<'de> for StepValueEquals<'_> {
    type Value = bool;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StepValueEquals<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
        Ok(v == self.step_name)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(false)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::line_matches_step as m;

    #[test]
    fn matches_the_exact_step() {
        assert!(m(
            r#"{"ts":"t","stream":"stdout","step":"build","line":"x"}"#,
            "build"
        ));
    }

    #[test]
    fn rejects_another_step_that_contains_the_name() {
        assert!(!m(r#"{"step":"build-docs","line":"build"}"#, "build"));
    }

    #[test]
    fn rejects_plain_text_and_non_objects() {
        assert!(!m("build started", "build"));
        assert!(!m(r#"["build"]"#, "build"));
        assert!(!m(r#""build""#, "build"));
    }

    #[test]
    fn the_last_duplicate_step_key_wins() {
        assert!(m(r#"{"step":"test","step":"build"}"#, "build"));
        assert!(!m(r#"{"step":"build","step":"test"}"#, "build"));
        assert!(!m(r#"{"step":"build","step":1}"#, "build"));
    }

    #[test]
    fn a_compound_first_step_value_is_drained() {
        assert!(m(
            r#"{"step":{"x":[1,{"y":"build"}]},"step":"build"}"#,
            "build"
        ));
    }

    #[test]
    fn non_string_step_values_do_not_match() {
        assert!(!m(r#"{"step":1,"line":"build"}"#, "build"));
        assert!(!m(r#"{"step":null,"line":"build"}"#, "build"));
        assert!(!m(r#"{"step":true,"line":"build"}"#, "build"));
        assert!(!m(r#"{"step":["build"]}"#, "build"));
    }

    #[test]
    fn an_escaped_step_key_matches() {
        assert!(m(r#"{"step":"build"}"#, "build"));
    }

    #[test]
    fn an_escaped_step_value_is_rejected_by_the_fast_guard_as_before() {
        // The raw line does not contain "build", so the `contains` guard
        // rejects it before parsing — the old matcher behaved the same way.
        let line = r#"{"step":"b\u0075ild"}"#;
        assert!(!m(line, "build"));
        assert!(!super::legacy_value_matcher(line, "build"));
    }

    #[test]
    fn an_escaped_step_value_matches_when_the_name_also_appears_literally() {
        assert!(m(r#"{"step":"build","line":"build"}"#, "build"));
    }

    #[test]
    fn an_unrelated_escaped_key_does_not_break_the_match() {
        assert!(m(r#"{"step":"build","a":0}"#, "build"));
    }

    #[test]
    fn trailing_garbage_rejects_the_line() {
        assert!(!m(r#"{"step":"build"} garbage"#, "build"));
    }

    #[test]
    fn missing_or_nested_step_does_not_match() {
        assert!(!m(r#"{"line":"build"}"#, "build"));
        assert!(!m(r#"{"meta":{"step":"build"}}"#, "build"));
    }

    #[test]
    fn agrees_with_the_old_value_matcher_on_ordinary_lines() {
        for (line, step) in [
            (
                r#"{"ts":"t","stream":"stdout","step":"build","line":"compiling"}"#,
                "build",
            ),
            (
                r#"{"ts":"t","stream":"stderr","step":"build","line":"warn"}"#,
                "build",
            ),
            (
                r#"{"ts":"t","stream":"stdout","step":"build-notify","line":"build"}"#,
                "build",
            ),
            (
                r#"{"ts":"t","stream":"stderr","step":"_server","line":"hook failed"}"#,
                "_server",
            ),
            ("plain text line", "build"),
        ] {
            assert_eq!(
                m(line, step),
                super::legacy_value_matcher(line, step),
                "{line}"
            );
        }
    }

    // ── The documented divergence class: malformed content in a field the
    // matcher ignores. Each test asserts the NEW behaviour and checks the
    // premise that the old `Value` matcher rejected the line.

    #[test]
    fn divergence_invalid_surrogate_in_an_unrelated_field_now_matches() {
        let line = r#"{"step":"build","line":"\uDC00"}"#;
        assert!(m(line, "build"));
        assert!(
            !super::legacy_value_matcher(line, "build"),
            "premise: old matcher rejected it"
        );
    }

    #[test]
    fn divergence_nesting_deeper_than_128_in_an_unrelated_field_now_matches() {
        let line = format!(
            r#"{{"step":"build","line":{}{}}}"#,
            "[".repeat(129),
            "]".repeat(129)
        );
        assert!(m(&line, "build"));
        assert!(
            !super::legacy_value_matcher(&line, "build"),
            "premise: old matcher rejected it"
        );
    }

    #[test]
    fn divergence_out_of_range_number_in_an_unrelated_field_now_matches() {
        let line = r#"{"step":"build","n":1e400}"#;
        assert!(m(line, "build"));
        assert!(
            !super::legacy_value_matcher(line, "build"),
            "premise: old matcher rejected it"
        );
    }
}
