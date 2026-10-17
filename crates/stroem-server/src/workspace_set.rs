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
/// key off — notably render-failure messages. Tera's own error text quotes
/// the offending value; since Tera 2 our template errors never carry that
/// text (spec 2026-10-06 § 3.2), and this scrub is the second line. Those
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
/// bail) renders a secret in JSON-escaped form; Tera 1 formatted some
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
mod tests;
