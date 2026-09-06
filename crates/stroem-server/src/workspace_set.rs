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
