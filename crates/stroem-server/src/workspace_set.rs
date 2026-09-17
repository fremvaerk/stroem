//! Snapshot of every loaded workspace config, implementing
//! [`stroem_common::template::WorkspaceLookup`] for cross-workspace connection
//! resolution, the task-detail dropdown, and job-detail redaction.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::template::{canonical_type_ref, Lookup, WorkspaceLookup};

use crate::workspace::WorkspaceManager;

pub struct WorkspaceSet<'a> {
    local_name: String,
    /// The exact config the caller is already holding for the local workspace
    /// (may be a hair newer than the manager's if a reload raced). `None` ⇒ use
    /// the manager's copy.
    local_override: Option<&'a WorkspaceConfig>,
    configs: HashMap<String, Arc<WorkspaceConfig>>,
    known: HashSet<String>,
}

impl<'a> WorkspaceSet<'a> {
    /// Snapshot every healthy workspace config from the manager. Cheap: `Arc`
    /// clones plus one `RwLock` read per workspace, no I/O.
    pub async fn load(
        workspaces: &WorkspaceManager,
        local_name: &str,
        local_override: Option<&'a WorkspaceConfig>,
    ) -> WorkspaceSet<'a> {
        let configs = workspaces.get_all_configs().await;
        let known = workspaces.configured_names();
        Self::from_parts(local_name, local_override, configs, known)
    }

    pub fn from_parts(
        local_name: &str,
        local_override: Option<&'a WorkspaceConfig>,
        configs: Vec<(String, Arc<WorkspaceConfig>)>,
        known: Vec<String>,
    ) -> WorkspaceSet<'a> {
        let mut known: HashSet<String> = known.into_iter().collect();
        let configs: HashMap<String, Arc<WorkspaceConfig>> = configs.into_iter().collect();
        known.extend(configs.keys().cloned());
        known.insert(local_name.to_string());
        WorkspaceSet {
            local_name: local_name.to_string(),
            local_override,
            configs,
            known,
        }
    }

    /// Every config in the set: the local workspace first, then the rest
    /// sorted by name. Skips a local workspace that is neither overridden nor
    /// loaded.
    pub fn iter_configs(&self) -> impl Iterator<Item = (&str, &WorkspaceConfig)> {
        let local: Option<(&str, &WorkspaceConfig)> = match self.get(&self.local_name) {
            Lookup::Found(c) => Some((self.local_name.as_str(), c)),
            _ => None,
        };
        let mut others: Vec<(&str, &WorkspaceConfig)> = self
            .configs
            .iter()
            .filter(|(n, _)| **n != self.local_name)
            .map(|(n, c)| (n.as_str(), c.as_ref()))
            .collect();
        others.sort_by(|a, b| a.0.cmp(b.0));
        local.into_iter().chain(others)
    }
}

impl WorkspaceLookup for WorkspaceSet<'_> {
    fn local_name(&self) -> &str {
        &self.local_name
    }

    fn get(&self, name: &str) -> Lookup<'_> {
        if name == self.local_name {
            if let Some(cfg) = self.local_override {
                return Lookup::Found(cfg);
            }
        }
        if let Some(cfg) = self.configs.get(name) {
            Lookup::Found(cfg)
        } else if self.known.contains(name) {
            Lookup::Unavailable
        } else {
            Lookup::Unknown
        }
    }
}

/// Every string that must be masked in job-detail responses: all workspaces'
/// `secrets` values plus the values of connection properties whose type marks
/// them `secret: true`. Strings of 3 chars or fewer are dropped (existing rule).
/// Secret value strings declared by a single workspace config.
///
/// The cascade and settlement render against one `WorkspaceConfig` and put its
/// `secrets` into the template context, but they are pure / transaction-scoped
/// and cannot build a [`WorkspaceSet`], so they scrub against this narrower set
/// rather than [`collect_redaction_values`].
pub fn collect_config_secret_values(cfg: &WorkspaceConfig) -> Vec<String> {
    let mut out = Vec::new();
    for value in cfg.secrets.values() {
        collect_strings(value, &mut out);
    }
    out
}

/// Mask used wherever a secret value is scrubbed out of user-visible text.
pub const REDACTED: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// Append every occurrence (overlapping ones included) of `needle` in `s` to
/// `spans`, as `(start, end)` byte offsets into `s`. Advances by one
/// character (not by `needle`'s length) after each match so an occurrence
/// that starts inside a previous one is still found — see
/// [`redact_secrets_in_str`]'s doc comment for why overlap matters.
fn collect_occurrences(s: &str, needle: &str, spans: &mut Vec<(usize, usize)>) {
    let mut from = 0usize;
    while from <= s.len() {
        let Some(rel) = s[from..].find(needle) else {
            break;
        };
        let begin = from + rel;
        spans.push((begin, begin + needle.len()));
        let step = s[begin..].chars().next().map_or(1, char::len_utf8);
        from = begin + step;
    }
}

/// Mask every occurrence of every known secret value in `s`. Occurrences are
/// found in the original text (overlapping ones included) and intersecting or
/// adjacent spans are merged, so two values whose occurrences cross — or one
/// value overlapping itself — leave no legible fragment.
///
/// Used for free text that can embed a secret without any JSON structure to
/// key off — notably Tera error messages, which quote the offending value
/// (`Filter `round` was called on an incorrect value: got `"<secret>"``). Those
/// messages are persisted to `job_step.error_message` and `retry_history`,
/// appended to the job log, and returned to the worker, so they must be
/// scrubbed at the point of failure rather than only on read.
///
/// Each secret is searched for in THREE forms: the raw value, its
/// JSON-escaped representation (quotes/backslashes/control characters
/// escaped the way `serde_json` would render it inside a string), and its
/// Rust `Debug`-escaped representation (`str::escape_debug`'s rules —
/// notably a control character renders as `\u{XX}`, distinct from JSON's
/// `\u00XX`), each only when it differs from the forms already searched. A
/// value error printed via a JSON `Value`'s `Display` impl (e.g. the
/// now-fixed `resolve_connection_inputs_scoped` "expects a connection name"
/// bail) renders a secret in JSON-escaped form; Tera itself formats some
/// filter arguments with `{:?}` (e.g. `round`'s `method` argument on a
/// non-numeric/invalid value, `tera::builtins::filters::number::round`),
/// which renders a secret in Rust's Debug-escaped form instead — a secret
/// containing a quote, backslash, or control character then survives an
/// exact match on the raw value alone, and a JSON-escaped search alone
/// misses the Debug form wherever the two escaping rules diverge (e.g. a
/// control character (JSON's `\u00XX` vs Rust's `\u{XX}` numeral base).
/// Searching for all three forms keeps any future error path that
/// serialises or Debug-prints a value covered, not just the ones known
/// today.
pub fn redact_secrets_in_str(s: &str, secret_values: &[String]) -> String {
    // Collect every occurrence — overlapping ones included — against the
    // ORIGINAL text. Replacing sequentially against already-modified text
    // leaves fragments whenever two occurrences intersect.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for secret in secret_values {
        if secret.is_empty() {
            continue;
        }
        collect_occurrences(s, secret, &mut spans);

        // The JSON-escaped form (without the surrounding quotes `to_string`
        // would add): only search it when it differs from the raw value, so
        // a secret with no special characters doesn't get searched twice.
        let json_escaped = serde_json::to_string(secret).ok().map(|escaped| {
            escaped
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(str::to_string)
                .unwrap_or(escaped)
        });
        if let Some(ref escaped) = json_escaped {
            if escaped != secret && !escaped.is_empty() {
                collect_occurrences(s, escaped, &mut spans);
            }
        }

        // The Rust Debug-escaped form (without the surrounding quotes
        // `{:?}` would add): only search it when it differs from BOTH the
        // raw value and the JSON-escaped form already searched above.
        let debug_escaped = format!("{secret:?}");
        let debug_escaped = debug_escaped
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&debug_escaped);
        if debug_escaped != secret
            && Some(debug_escaped) != json_escaped.as_deref()
            && !debug_escaped.is_empty()
        {
            collect_occurrences(s, debug_escaped, &mut spans);
        }
    }
    if spans.is_empty() {
        return s.to_string();
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (b, e) in spans {
        match merged.last_mut() {
            Some(last) if b <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((b, e)),
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    for (b, e) in merged {
        out.push_str(&s[cursor..b]);
        out.push_str(REDACTED);
        cursor = e;
    }
    out.push_str(&s[cursor..]);
    out
}

pub fn collect_redaction_values(set: &WorkspaceSet) -> Vec<String> {
    let mut out = Vec::new();
    for (ws_name, cfg) in set.iter_configs() {
        for value in cfg.secrets.values() {
            collect_strings(value, &mut out);
        }
        for conn in cfg.connections.values() {
            let Some(ref declared) = conn.connection_type else {
                continue;
            };
            let Ok(ct) = canonical_type_ref(declared, ws_name, set) else {
                continue;
            };
            let Lookup::Found(type_cfg) = set.get(&ct.workspace) else {
                continue;
            };
            let Some(type_def) = type_cfg.connection_types.get(&ct.name) else {
                continue;
            };
            // Use the values as resolved (own values + the type's property
            // defaults filled in), not `conn.values` alone: a `secret: true`
            // property whose value comes only from the foreign type's
            // `default:` is still materialised into the persisted job input
            // by `values_with_type_defaults` and must be masked too.
            let effective = conn.values_with_type_defaults(type_def);
            for (prop, def) in &type_def.properties {
                if def.secret {
                    if let Some(v) = effective.get(prop) {
                        collect_strings(v, &mut out);
                    }
                }
            }
        }
    }
    out.retain(|v| v.len() > 3);
    out.sort();
    out.dedup();
    out
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        serde_json::Value::Array(arr) => arr.iter().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use stroem_common::models::workflow::{
        ConnectionDef, ConnectionPropertyDef, ConnectionTypeDef,
    };

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
        // The exact shape Tera produces: the caller-controlled value covers the
        // error prefix and the START of the owner secret; the owner secret
        // continues past it. Sequential replacement leaves `-token"`.
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

    /// Template-layer regression: Tera's `round` filter Debug-formats its
    /// `method` argument on an invalid value
    /// (`tera::builtins::filters::number::round`, `got \`{:?}\``). Rendering
    /// `{{ 1 | round(method=secret.TOKEN) }}` with a secret containing
    /// U+001B (ESC) produces a real Tera error whose text embeds the secret
    /// in Rust's Debug-escaped form — confirming the shape `redact_secrets_in_str`
    /// must handle is not just a synthetic string but an actual render error.
    /// Runs that error text through `redact_secrets_in_str` and asserts
    /// neither the raw secret pieces (`prefix`/`suffix`) nor its
    /// Debug-escaped `\u{1b}` form survive.
    #[test]
    fn redact_secrets_in_str_masks_tera_round_filter_debug_error() {
        let secret = "prefix\u{1b}suffix";
        let context = serde_json::json!({"secret": {"TOKEN": secret}});
        let err = stroem_common::template::render_template(
            "{{ 1 | round(method=secret.TOKEN) }}",
            &context,
        )
        .expect_err("an invalid `method` value must fail rendering");
        let err_text = format!("{err:#}");
        assert!(
            err_text.contains("prefix") && err_text.contains("suffix"),
            "test setup: the raw Tera error must actually embed the secret: {err_text}"
        );

        let out = redact_secrets_in_str(&err_text, &secrets(&[secret]));
        assert!(!out.contains("prefix"), "{out}");
        assert!(!out.contains("suffix"), "{out}");
        assert!(!out.contains("\\u{1b}"), "{out}");
        assert!(out.contains(REDACTED), "{out}");
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
        let set =
            WorkspaceSet::from_parts("A", Some(&a), vec![("B".to_string(), Arc::new(b))], vec![]);
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
        let set =
            WorkspaceSet::from_parts("A", Some(&a), vec![("B".to_string(), Arc::new(b))], vec![]);
        let vals = collect_redaction_values(&set);
        assert!(
            vals.contains(&"default-token-secret-value".to_string()),
            "missing default-token-secret-value: {vals:?}"
        );
    }
}
