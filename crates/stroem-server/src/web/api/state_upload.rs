//! Admin/operator endpoints for uploading state snapshots out-of-band.
//!
//! See `docs/internal/2026-04-21-state-upload-design.md`.

use std::collections::BTreeMap;

/// Build the JSON map that becomes `state.json` from the upload's query
/// string. The reserved `mode` key is filtered out; any other key/value
/// pair is copied verbatim as a string. Duplicate keys: last wins (already
/// how Axum's `Query<HashMap>` delivers them, but documenting here anyway).
///
/// Returns `None` if the resulting map is empty, so callers can decide
/// whether to emit a `state.json` entry at all.
#[allow(dead_code)]
pub(crate) fn build_state_json_from_params(
    params: &BTreeMap<String, String>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    for (k, v) in params {
        if k == "mode" {
            continue;
        }
        out.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_params_returns_none() {
        assert!(build_state_json_from_params(&params(&[])).is_none());
    }

    #[test]
    fn only_mode_returns_none() {
        assert!(build_state_json_from_params(&params(&[("mode", "merge")])).is_none());
    }

    #[test]
    fn single_param_becomes_string_entry() {
        let out = build_state_json_from_params(&params(&[("domain", "example.com")])).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out["domain"], serde_json::Value::String("example.com".into()));
    }

    #[test]
    fn mode_is_filtered_out() {
        let out = build_state_json_from_params(&params(&[
            ("mode", "replace"),
            ("domain", "example.com"),
        ]))
        .unwrap();
        assert!(!out.contains_key("mode"));
        assert_eq!(out["domain"], serde_json::Value::String("example.com".into()));
    }

    #[test]
    fn empty_value_is_preserved() {
        let out = build_state_json_from_params(&params(&[("foo", "")])).unwrap();
        assert_eq!(out["foo"], serde_json::Value::String(String::new()));
    }

    #[test]
    fn values_are_always_strings() {
        let out = build_state_json_from_params(&params(&[("days", "30"), ("ok", "true")])).unwrap();
        assert_eq!(out["days"], serde_json::Value::String("30".into()));
        assert_eq!(out["ok"], serde_json::Value::String("true".into()));
    }
}
