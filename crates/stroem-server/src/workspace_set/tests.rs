use super::*;
use serde_json::json;
use std::collections::HashMap;
use stroem_common::models::workflow::{ConnectionDef, ConnectionPropertyDef, ConnectionTypeDef};

fn ws(secret: &str) -> WorkspaceConfig {
    let mut c = WorkspaceConfig::default();
    c.secrets.insert("token".to_string(), json!(secret));
    c
}

#[test]
fn lookup_distinguishes_local_found_unknown_unavailable() {
    let local = ws("local-secret");
    let other = Arc::new(ws("other-secret"));
    let set = WorkspaceSet::from_parts(
        "A",
        Some(&local),
        vec![("B".to_string(), other)],
        vec!["A".to_string(), "B".to_string(), "C".to_string()],
    );
    assert_eq!(set.local_name(), "A");
    assert!(matches!(set.get("A"), Lookup::Found(c) if c.secrets["token"] == "local-secret"));
    assert!(matches!(set.get("B"), Lookup::Found(_)));
    assert!(matches!(set.get("C"), Lookup::Unavailable));
    assert!(matches!(set.get("Z"), Lookup::Unknown));
    assert!(!set.offline());
}

#[test]
fn iter_configs_local_first_then_sorted() {
    let local = ws("l");
    let set = WorkspaceSet::from_parts(
        "M",
        Some(&local),
        vec![
            ("Z".to_string(), Arc::new(ws("z"))),
            ("A".to_string(), Arc::new(ws("a"))),
        ],
        vec![],
    );
    let names: Vec<&str> = set.iter_configs().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["M", "A", "Z"]);
}

#[tokio::test]
async fn load_reads_all_healthy_configs_from_manager() {
    let mgr = WorkspaceManager::from_configs(vec![
        ("A".to_string(), ws("a"), None),
        ("B".to_string(), ws("b"), None),
    ]);
    let set = WorkspaceSet::load(&mgr, "A", None).await;
    assert!(matches!(set.get("A"), Lookup::Found(_)));
    assert!(matches!(set.get("B"), Lookup::Found(_)));
    assert!(matches!(set.get("C"), Lookup::Unknown));
}

#[tokio::test]
async fn load_classifies_source_construction_failure_as_unavailable_not_unknown() {
    // A workspace whose SOURCE failed to construct (e.g. a bad
    // `GitSource::new()`) has no `entries` row at all — only a
    // `load_errors` entry. `WorkspaceManager::names()` alone would miss
    // it entirely, making `WorkspaceSet::load` misclassify it as
    // `Unknown` (400, "author mistake") when it is really a configured
    // workspace that is transiently unavailable (500).
    let mut mgr = WorkspaceManager::from_configs(vec![("A".to_string(), ws("a"), None)]);
    mgr.insert_load_error_for_test("broken", "failed to construct git source");

    let set = WorkspaceSet::load(&mgr, "A", None).await;
    assert!(matches!(set.get("broken"), Lookup::Unavailable));
    assert!(matches!(set.get("nonexistent"), Lookup::Unknown));
}

#[test]
fn redact_secrets_in_str_replaces_every_occurrence() {
    let secrets = vec!["s3cr3t".to_string()];
    let out = redact_secrets_in_str("got `s3cr3t` and again s3cr3t", &secrets);
    assert_eq!(out, format!("got `{REDACTED}` and again {REDACTED}"));
}

#[test]
fn redact_secrets_in_str_leaves_unrelated_text_alone() {
    let secrets = vec!["s3cr3t".to_string()];
    let msg = "Filter `round` was called on an incorrect value";
    assert_eq!(redact_secrets_in_str(msg, &secrets), msg);
}

/// An empty secret value must be skipped: `str::replace` with an empty
/// pattern inserts the replacement between EVERY character, which would
/// destroy the message (and hide the real error) rather than redact it.
#[test]
fn redact_secrets_in_str_ignores_empty_secret_values() {
    let secrets = vec![String::new(), "s3cr3t".to_string()];
    let out = redact_secrets_in_str("a s3cr3t b", &secrets);
    assert_eq!(out, format!("a {REDACTED} b"));
}

fn secrets(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn redact_containment_masks_the_whole_longer_value_in_any_order() {
    let text = "got prefix-sensitive-token here";
    for order in [
        secrets(&["prefix", "prefix-sensitive-token"]),
        secrets(&["prefix-sensitive-token", "prefix"]),
    ] {
        let out = redact_secrets_in_str(text, &order);
        assert_eq!(out, format!("got {REDACTED} here"), "order {order:?}");
        assert!(!out.contains("sensitive"));
    }
}

#[test]
fn redact_crossing_occurrences_leave_no_fragment() {
    // The shape Tera 1's error text had (synthetic input since Tera 2,
    // whose template errors carry no value): the caller-controlled value
    // covers the error prefix and the START of the owner secret; the owner
    // secret continues past it. Sequential replacement leaves `-token"`.
    let text = r#"incorrect value: got "ABCD-token""#;
    for order in [
        secrets(&[r#"incorrect value: got "ABCD"#, "ABCD-token"]),
        secrets(&["ABCD-token", r#"incorrect value: got "ABCD"#]),
    ] {
        let out = redact_secrets_in_str(text, &order);
        assert_eq!(out, format!("{REDACTED}\""), "order {order:?}");
    }
}

#[test]
fn redact_equal_length_overlap() {
    let out = redact_secrets_in_str("xabcdx", &secrets(&["abc", "bcd"]));
    assert_eq!(out, format!("x{REDACTED}x"));
}

#[test]
fn redact_self_overlapping_value_covers_every_byte() {
    // `aba` occurs at 0 and at 2 in `ababa`; a non-overlapping matcher
    // finds only the first and leaves `ba`.
    let out = redact_secrets_in_str("ababa", &secrets(&["aba"]));
    assert_eq!(out, REDACTED);
}

#[test]
fn redact_value_containing_mask_glyph_and_mask_substring() {
    let bullet = "\u{2022}";
    let text = format!("v={bullet}x{bullet} and mask={REDACTED}");
    let out = redact_secrets_in_str(&text, &secrets(&[&format!("{bullet}x{bullet}"), bullet]));
    // Values are matched against the INPUT only, so the mask this function emits is
    // never rescanned. A `••••••` already in the input is ordinary text: its bullets
    // match the `•` secret, merge into one span, and come out as a single mask.
    assert_eq!(out, format!("v={REDACTED} and mask={REDACTED}"));
}

#[test]
fn redact_multibyte_adjacent_values_keep_utf8_boundaries() {
    let out = redact_secrets_in_str("héllo wörld", &secrets(&["héllo", " wörld"]));
    assert_eq!(out, REDACTED); // adjacent spans merge into one mask
    let out = redact_secrets_in_str("aé", &secrets(&["é"]));
    assert_eq!(out, format!("a{REDACTED}"));
}

#[test]
fn redact_adjacent_occurrences_merge() {
    let out = redact_secrets_in_str("abab", &secrets(&["ab"]));
    assert_eq!(out, REDACTED);
}

// ─── H1 regression: JSON-escaped occurrences of a secret ─────────────────

/// A secret containing a quote, a backslash, and a newline appears in the
/// text ONLY in its JSON-escaped form (as `serde_json::Value`'s `Display`
/// impl would render it inside a string, e.g. an array literal like
/// `["contains\"quote\\and\nnewline"]`). The raw value never occurs
/// verbatim, so a matcher that only searches the raw string finds
/// nothing; `redact_secrets_in_str` must still fully mask it.
#[test]
fn redact_secrets_in_str_masks_json_escaped_only_occurrence() {
    let secret = "contains\"quote\\and\nnewline";
    let escaped = serde_json::to_string(secret).unwrap();
    let text = format!("Input field 'db' expects a connection name, got [{escaped}]");
    assert!(
        !text.contains(secret),
        "test setup: raw value must not appear verbatim in the escaped text"
    );
    let out = redact_secrets_in_str(&text, &secrets(&[secret]));
    assert!(!out.contains(secret));
    assert!(!out.contains("quote"));
    assert!(!out.contains("backslash") && !out.contains('\\'));
    assert_eq!(
        out,
        format!("Input field 'db' expects a connection name, got [\"{REDACTED}\"]")
    );
}

/// Both the raw and the JSON-escaped form of the same secret appear in
/// the text (e.g. the raw value logged once, and a JSON-serialised copy
/// of it logged elsewhere) — both occurrences must be masked.
#[test]
fn redact_secrets_in_str_masks_both_raw_and_escaped_occurrences() {
    let secret = "has\"quote";
    let escaped = serde_json::to_string(secret).unwrap(); // `"has\"quote"`
    let text = format!("raw={secret} escaped={escaped}");
    let out = redact_secrets_in_str(&text, &secrets(&[secret]));
    assert!(!out.contains("has"));
    assert!(!out.contains("quote"));
    assert_eq!(out, format!("raw={REDACTED} escaped=\"{REDACTED}\""));
}

/// A secret with no JSON-special characters has an escaped form equal to
/// its raw form — searching for it a second time must not double-count
/// or otherwise change the result versus searching the raw value alone.
#[test]
fn redact_secrets_in_str_escaped_form_equal_to_raw_adds_nothing() {
    let secret = "plain-secret-value";
    assert_eq!(
        serde_json::to_string(secret).unwrap(),
        format!("\"{secret}\"")
    );
    let text = format!("got {secret} here");
    let out = redact_secrets_in_str(&text, &secrets(&[secret]));
    assert_eq!(out, format!("got {REDACTED} here"));
}

// ─── H1 follow-up: Rust Debug-escaped occurrences of a secret ────────────

/// A secret containing U+001B (ESC), a NUL, a quote, and a backslash
/// appears in the text ONLY in its Rust `Debug`-escaped form — the shape
/// Tera itself produces for some filter arguments via `{:?}` (e.g.
/// `round`'s `method` argument on an invalid value). Neither the raw
/// value nor its JSON-escaped form occurs verbatim (JSON and Rust's
/// Debug escaping render a control character like ESC differently),
/// so a matcher limited to those two forms finds nothing;
/// `redact_secrets_in_str` must still fully mask it.
#[test]
fn redact_secrets_in_str_masks_debug_escaped_only_occurrence() {
    let secret = "prefix\u{1b}suffix\0end\"q\\b";
    let debug_escaped = format!("{secret:?}");
    let debug_escaped = debug_escaped
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap()
        .to_string();
    let json_escaped = serde_json::to_string(secret).unwrap();
    assert_ne!(
        debug_escaped,
        json_escaped[1..json_escaped.len() - 1],
        "test setup: Rust Debug and JSON escaping must diverge on this secret"
    );
    let text = format!("round(method=\"{debug_escaped}\")");
    assert!(
        !text.contains(secret),
        "test setup: raw value must not appear verbatim in the Debug-escaped text"
    );
    assert!(
        !text.contains(&json_escaped[1..json_escaped.len() - 1]),
        "test setup: JSON-escaped form must not appear verbatim either"
    );
    let out = redact_secrets_in_str(&text, &secrets(&[secret]));
    assert!(!out.contains(secret));
    assert!(!out.contains("prefix") && !out.contains("suffix"));
    assert!(!out.contains("\\u{1b}"));
    assert_eq!(out, format!("round(method=\"{REDACTED}\")"));
}

/// The raw value, its JSON-escaped form, and its Rust Debug-escaped form
/// ALL appear in the same text — every occurrence must be masked.
#[test]
fn redact_secrets_in_str_masks_raw_json_and_debug_occurrences() {
    let secret = "ctl\u{1b}val\"q";
    let json_escaped = serde_json::to_string(secret).unwrap();
    let json_escaped = &json_escaped[1..json_escaped.len() - 1];
    let debug_escaped = format!("{secret:?}");
    let debug_escaped = &debug_escaped[1..debug_escaped.len() - 1];
    assert_ne!(json_escaped, debug_escaped, "test setup: forms must differ");
    let text = format!("raw={secret} json={json_escaped} debug={debug_escaped}");
    let out = redact_secrets_in_str(&text, &secrets(&[secret]));
    assert!(!out.contains("ctl"));
    assert!(!out.contains("val"));
    assert_eq!(
        out,
        format!("raw={REDACTED} json={REDACTED} debug={REDACTED}")
    );
}

/// Formerly rendered a real Tera error. Since Tera 2 a template error is
/// value-free (spec 2026-10-06 § 3.2) and never brings a value to this
/// scrub, so the shapes Tera's raw text uses for `round`'s rejected
/// `method` — Tera 2 quotes it verbatim, Tera 1 Debug-escaped it — are
/// synthetic input here: a secret containing U+001B (ESC) must be masked
/// in both, leaving neither `prefix`/`suffix` nor the Debug-escaped
/// `\u{1b}`.
#[test]
fn redact_secrets_in_str_masks_round_method_error_shapes() {
    let secret = "prefix\u{1b}suffix";
    let debug_quoted = format!("{secret:?}");
    let text = format!(
        "Invalid argument for `method`: {secret}. Only `ceil` and `floor` are allowed. \
         Filter `round` received an incorrect value for argument `method`: got `{debug_quoted}`"
    );
    assert!(
        text.contains(secret) && text.contains("\\u{1b}"),
        "test setup: both the raw and the Debug-escaped form are present: {text:?}"
    );

    let out = redact_secrets_in_str(&text, &secrets(&[secret]));
    assert!(!out.contains("prefix"), "{out}");
    assert!(!out.contains("suffix"), "{out}");
    assert!(!out.contains("\\u{1b}"), "{out}");
    assert_eq!(out.matches(REDACTED).count(), 2, "{out}");
}

#[test]
fn redaction_values_union_secrets_and_secret_marked_connection_props() {
    let mut a = ws("secret-a-value");
    let mut b = ws("secret-b-value");
    // B defines type `db` with a secret `password`; connection `prod` sets a
    // literal password that is NOT in any `secrets` map.
    b.connection_types.insert(
        "db".to_string(),
        ConnectionTypeDef {
            properties: HashMap::from([
                (
                    "password".to_string(),
                    ConnectionPropertyDef {
                        property_type: "string".into(),
                        required: true,
                        default: None,
                        secret: true,
                    },
                ),
                (
                    "host".to_string(),
                    ConnectionPropertyDef {
                        property_type: "string".into(),
                        required: true,
                        default: None,
                        secret: false,
                    },
                ),
            ]),
        },
    );
    b.connections.insert(
        "prod".to_string(),
        ConnectionDef {
            connection_type: Some("db".into()),
            shared: true,
            values: HashMap::from([
                ("password".to_string(), json!("literal-pw-value")),
                ("host".to_string(), json!("public-host")),
            ]),
        },
    );
    // A has a connection declaring B's type — its secret prop must be found too.
    a.connections.insert(
        "mirror".to_string(),
        ConnectionDef {
            connection_type: Some("B.db".into()),
            shared: false,
            values: HashMap::from([
                ("password".to_string(), json!("mirror-pw-value")),
                ("host".to_string(), json!("h")),
            ]),
        },
    );
    let set = WorkspaceSet::from_parts("A", Some(&a), vec![("B".to_string(), Arc::new(b))], vec![]);
    let vals = collect_redaction_values(&set);
    for expected in [
        "secret-a-value",
        "secret-b-value",
        "literal-pw-value",
        "mirror-pw-value",
    ] {
        assert!(
            vals.contains(&expected.to_string()),
            "missing {expected}: {vals:?}"
        );
    }
    assert!(!vals.contains(&"public-host".to_string()));
}

#[test]
fn redaction_values_include_secret_from_foreign_type_default() {
    let mut a = ws("secret-a-value");
    let mut b = ws("secret-b-value");
    // B defines type `db` with a secret `token` that has a type-level
    // default. Connection `mirror` in A does NOT set `token` at all —
    // the resolver materialises the default into the persisted job
    // input via `values_with_type_defaults`, so it must be redacted too.
    b.connection_types.insert(
        "db".to_string(),
        ConnectionTypeDef {
            properties: HashMap::from([(
                "token".to_string(),
                ConnectionPropertyDef {
                    property_type: "string".into(),
                    required: false,
                    default: Some(json!("default-token-secret-value")),
                    secret: true,
                },
            )]),
        },
    );
    a.connections.insert(
        "mirror".to_string(),
        ConnectionDef {
            connection_type: Some("B.db".into()),
            shared: false,
            values: HashMap::new(),
        },
    );
    let set = WorkspaceSet::from_parts("A", Some(&a), vec![("B".to_string(), Arc::new(b))], vec![]);
    let vals = collect_redaction_values(&set);
    assert!(
        vals.contains(&"default-token-secret-value".to_string()),
        "missing default-token-secret-value: {vals:?}"
    );
}
