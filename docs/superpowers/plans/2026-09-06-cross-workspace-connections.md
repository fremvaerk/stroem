# Cross-Workspace Connections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a task or action input reference another workspace's connection (`jobs.clickhouse-prod`) and connection type (`type: jobs.clickhouse`), gated by a per-connection `shared: true` flag, with server-side resolution, dropdown support, and redaction of foreign secrets.

**Architecture:** A `WorkspaceLookup` trait in stroem-common abstracts "give me workspace X's config"; the resolver canonicalises every type reference to `(workspace, name)` and resolves connection names with a `shared` gate. The server implements the trait with `WorkspaceSet` (a snapshot of all loaded configs) and threads it through job creation, claim-time rendering, the task-detail dropdown, and job-detail redaction. The CLI implements it with `SingleWorkspace` and refuses qualified references with a clear message. The worker is untouched.

**Tech Stack:** Rust (anyhow, serde, sqlx runtime queries, axum, tokio), testcontainers Postgres for integration tests, bash `tests/e2e.sh` for end-to-end.

**Spec:** `docs/superpowers/specs/2026-09-06-cross-workspace-connections-design.md`

**Deviation from spec, decided while planning:** spec §4.2 pre-collects only the workspaces named by prefixes in the inputs. This plan instead loads **every** healthy workspace config into `WorkspaceSet` (in-memory `Arc` clones via `WorkspaceManager::get_all_configs()`). It removes the two-hop prefix scan entirely, cannot miss a workspace, and the dropdown and redaction already needed all configs. Task 12 updates the spec text accordingly.

## Global Constraints

- Error handling: `anyhow::Result` + `.context(...)`. Never `unwrap()` outside tests.
- All not-found messages must contain the phrase `does not exist` or `not found`; not-shared messages must contain `is not shared`; unknown-workspace messages must contain `unknown workspace`. `web/api/tasks.rs::is_user_error` matches on these to return 400.
- `execute_task` returns HTTP 200 on success (plain `Json`). Tests assert 200.
- No new DB migration. No worker changes. No UI changes.
- Run `cargo fmt --all` before every commit. `cargo clippy --workspace -- -D warnings` must stay clean.
- Disk is tight on this machine (~11 GiB free). Use `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target` for every cargo command so the worktree shares one target dir. Run `cargo test -p <crate>` for the crate you touched, not `--workspace`, until Task 13.
- Commit messages: conventional prefix (`feat:`, `test:`, `docs:`, `refactor:`), no AI co-author trailer, no `Co-Authored-By` line (project preference).
- Work only in the worktree `/Users/ala/workspace/fremvaerk/stroem/.claude/worktrees/cross-workspace-connections` on branch `worktree-cross-workspace-connections`.

---

## File map

| File | Responsibility after this plan |
|------|-------------------------------|
| `crates/stroem-common/src/models/workflow.rs` | `ConnectionDef.shared`; `ConnectionDef::values_with_type_defaults` |
| `crates/stroem-common/src/template.rs` | `WorkspaceLookup`, `Lookup`, `SingleWorkspace`, `CanonicalType`, `canonical_type_ref`, `ResolveScope`, `resolve_connection_ref`, `resolve_connection_inputs(_scoped)`, `prepare_action_input(_cross)` |
| `crates/stroem-common/src/validation.rs` | `check_connection_values` (shared with resolver); dotted-type skip + warning |
| `crates/stroem-server/src/workspace_set.rs` (new) | `WorkspaceSet`: snapshot of all configs implementing `WorkspaceLookup`; `collect_redaction_values` |
| `crates/stroem-server/src/job_creator.rs` | Task-input resolution via `WorkspaceSet`; literal pre-check per flow step; `handle_task_steps` via `WorkspaceSet` |
| `crates/stroem-server/src/web/worker_api/rendering.rs` | `RenderContext.lookup` + `action_workspace_name`; two-pass for cross-workspace steps |
| `crates/stroem-server/src/web/worker_api/jobs.rs` | Build `WorkspaceSet` in `claim_job` |
| `crates/stroem-server/src/web/api/tasks.rs` | Dropdown across workspaces; `is_user_error` phrases |
| `crates/stroem-server/src/web/api/jobs.rs` | Redaction against all workspaces + secret-marked connection properties |
| `crates/stroem-cli/src/local/run.rs` | `SingleWorkspace` |
| `crates/stroem-server/tests/integration_test.rs` | New integration tests |
| `tests/e2e-workspace/connections.yaml`, `workspace/.workflows/xref-conn.yaml`, `tests/e2e.sh` | E2E scenario |
| `docs/src/content/docs/guides/{cross-workspace-references,connections}.md`, `CLAUDE.md`, `docs/internal/TODO.md`, spec | Docs |

---

### Task 1: `shared` flag on `ConnectionDef`

**Files:**
- Modify: `crates/stroem-common/src/models/workflow.rs:27-36`
- Modify (add `shared: false,` to every `ConnectionDef {` literal): `crates/stroem-common/src/models/workflow.rs` (tests), `crates/stroem-common/src/template.rs` (tests), `crates/stroem-server/src/web/worker_api/rendering.rs` (tests), `crates/stroem-server/src/workspace/library.rs`, `crates/stroem-server/tests/integration_test.rs`, `crates/stroem-server/tests/rerun_integration_test.rs`
- Test: `crates/stroem-common/src/models/workflow.rs` (`mod tests`)

**Interfaces:**
- Produces: `ConnectionDef { connection_type: Option<String>, shared: bool, values: HashMap<String, Value> }`; `ConnectionDef::values_with_type_defaults(&self, &ConnectionTypeDef) -> HashMap<String, Value>`.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `crates/stroem-common/src/models/workflow.rs`:

```rust
    #[test]
    fn test_connection_def_shared_defaults_false_and_is_not_a_value() {
        let yaml = r#"
type: pg
host: db.example.com
"#;
        let conn: ConnectionDef = serde_yaml::from_str(yaml).unwrap();
        assert!(!conn.shared);
        assert_eq!(conn.values.get("host").unwrap(), "db.example.com");
        assert!(!conn.values.contains_key("shared"));
    }

    #[test]
    fn test_connection_def_shared_true_is_consumed_as_flag() {
        let yaml = r#"
type: pg
shared: true
host: db.example.com
"#;
        let conn: ConnectionDef = serde_yaml::from_str(yaml).unwrap();
        assert!(conn.shared);
        assert!(!conn.values.contains_key("shared"));
    }

    #[test]
    fn test_connection_def_shared_non_bool_is_an_error() {
        let yaml = r#"
type: pg
shared: "yes"
host: db.example.com
"#;
        let err = serde_yaml::from_str::<ConnectionDef>(yaml).unwrap_err();
        // serde_yaml reports the type mismatch with a line/column, not the field
        // name; the loader prefixes the file path. That is enough to locate it.
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    #[test]
    fn test_connection_def_serialize_omits_shared_when_false() {
        let conn = ConnectionDef {
            connection_type: Some("pg".into()),
            shared: false,
            values: HashMap::from([("host".to_string(), serde_json::json!("h"))]),
        };
        let v = serde_json::to_value(&conn).unwrap();
        assert!(v.get("shared").is_none());
        let shared = ConnectionDef { shared: true, ..conn };
        let v = serde_json::to_value(&shared).unwrap();
        assert_eq!(v["shared"], true);
    }

    #[test]
    fn test_values_with_type_defaults_fills_missing_only() {
        let type_def = ConnectionTypeDef {
            properties: HashMap::from([
                (
                    "port".to_string(),
                    ConnectionPropertyDef {
                        property_type: "integer".into(),
                        required: false,
                        default: Some(serde_json::json!(5432)),
                        secret: false,
                    },
                ),
                (
                    "host".to_string(),
                    ConnectionPropertyDef {
                        property_type: "string".into(),
                        required: true,
                        default: Some(serde_json::json!("ignored")),
                        secret: false,
                    },
                ),
            ]),
        };
        let conn = ConnectionDef {
            connection_type: Some("pg".into()),
            shared: false,
            values: HashMap::from([("host".to_string(), serde_json::json!("real"))]),
        };
        let v = conn.values_with_type_defaults(&type_def);
        assert_eq!(v["host"], "real");
        assert_eq!(v["port"], 5432);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-common connection_def 2>&1 | tail -20`
Expected: compile error `no field shared` / `no method values_with_type_defaults`.

- [ ] **Step 3: Implement**

Replace the struct at `crates/stroem-common/src/models/workflow.rs:27-36`:

```rust
/// Connection definition — a named, typed object storing external system config.
///
/// Flat syntax: `type` is the connection type reference, `shared` opts the
/// connection into cross-workspace use, all other fields are values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionDef {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    /// When `true`, other workspaces may reference this connection as
    /// `<workspace>.<name>`. Bare-name use inside the owning workspace ignores it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
    #[serde(flatten)]
    pub values: HashMap<String, serde_json::Value>,
}

impl ConnectionDef {
    /// The connection's values with the type's property defaults filled in for
    /// any property the connection does not set. Used at load time for local
    /// types and at resolution time for connections whose type lives in another
    /// workspace (which the load-time pass never sees).
    pub fn values_with_type_defaults(
        &self,
        type_def: &ConnectionTypeDef,
    ) -> HashMap<String, serde_json::Value> {
        let mut out = self.values.clone();
        for (prop_name, prop_def) in &type_def.properties {
            if !out.contains_key(prop_name) {
                if let Some(ref default_value) = prop_def.default {
                    out.insert(prop_name.clone(), default_value.clone());
                }
            }
        }
        out
    }
}
```

In `WorkspaceConfig::render_connections` (around line 1008-1027) replace the Phase 2 body so it reuses the helper:

```rust
        // Phase 2: Apply defaults from type properties (clone types to avoid borrow issues)
        let types = self.connection_types.clone();
        for (conn_name, conn) in &mut self.connections {
            if let Some(ref type_name) = conn.connection_type {
                if let Some(type_def) = types.get(type_name) {
                    conn.values = conn.values_with_type_defaults(type_def);
                } else {
                    tracing::warn!(
                        "Connection '{}' references unknown type '{}'",
                        conn_name,
                        type_name
                    );
                }
            }
        }
```

Then add `shared: false,` to every `ConnectionDef {` struct literal in the six files listed above (grep: `grep -rn "ConnectionDef {" crates --include='*.rs' | grep -v "pub struct"`). Insert the line directly after the `connection_type: ...,` line of each literal.

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-common 2>&1 | tail -5`
Expected: all pass. Then `CARGO_TARGET_DIR=... cargo check --workspace --tests` must be clean (this proves every literal was patched).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A crates
git commit -m "feat(connections): add shared flag to ConnectionDef"
```

---

### Task 2: `WorkspaceLookup`, canonical types, and the scoped resolver

**Files:**
- Modify: `crates/stroem-common/src/template.rs` (`resolve_connection_inputs` at ~321-393, `prepare_action_input` at ~510-520, tests from ~1242)
- Modify: `crates/stroem-common/src/validation.rs` (factor `check_connection_values` out of `validate_connections` at 629-720)
- Test: `crates/stroem-common/src/template.rs` `mod tests`

**Interfaces:**
- Consumes: `ConnectionDef.shared`, `ConnectionDef::values_with_type_defaults` (Task 1), `parse_qualified_ref` (exists).
- Produces (all `pub` in `stroem_common::template`):
  ```rust
  pub enum Lookup<'a> { Found(&'a WorkspaceConfig), Unknown, Unavailable }
  pub trait WorkspaceLookup {
      fn local_name(&self) -> &str;
      fn get(&self, name: &str) -> Lookup<'_>;
      fn offline(&self) -> bool { false }
  }
  pub struct SingleWorkspace<'a> { pub name: &'a str, pub config: &'a WorkspaceConfig }
  #[derive(Debug, Clone, PartialEq, Eq)] pub struct CanonicalType { pub workspace: String, pub name: String }  // Display = "ws.name"
  pub fn canonical_type_ref(type_ref: &str, defining_ws: &str, lookup: &dyn WorkspaceLookup) -> Result<CanonicalType>
  pub struct ResolveScope<'a> { pub lookup: &'a dyn WorkspaceLookup, pub schema_ws: &'a str, pub value_ws: &'a str, pub fallback_ws: Option<&'a str> }
  pub struct ResolvedConnection<'a> { pub workspace: String, pub name: String, pub def: &'a ConnectionDef }
  pub fn resolve_connection_ref<'a>(conn_ref: &str, scope: &ResolveScope<'a>) -> Result<ResolvedConnection<'a>>
  pub fn resolve_connection_inputs_scoped(input: &Value, schema: &HashMap<String, InputFieldDef>, scope: &ResolveScope) -> Result<Value>
  pub fn resolve_connection_inputs(input: &Value, schema: &HashMap<String, InputFieldDef>, lookup: &dyn WorkspaceLookup) -> Result<Value>
  pub fn prepare_action_input(rendered: &Value, schema: &HashMap<String, InputFieldDef>, lookup: &dyn WorkspaceLookup) -> Result<Value>
  pub fn prepare_action_input_cross(rendered: &Value, schema: &HashMap<String, InputFieldDef>, lookup: &dyn WorkspaceLookup, caller_ws: &str, owner_ws: &str) -> Result<Value>
  ```
  and in `stroem_common::validation`:
  ```rust
  pub fn check_connection_values(conn_name: &str, values: &HashMap<String, Value>, type_name: &str, type_def: &ConnectionTypeDef) -> Result<Vec<String>>
  ```

- [ ] **Step 1: Write the failing tests**

In `crates/stroem-common/src/template.rs` `mod tests`, add a two-workspace lookup helper and tests. Put the helper next to `make_ws_with_connection` (~line 1242):

```rust
    /// Test lookup over several named workspaces. `unavailable` names are
    /// configured-but-unloaded (Lookup::Unavailable); everything else unknown.
    struct MultiWs {
        local: String,
        configs: HashMap<String, WorkspaceConfig>,
        unavailable: Vec<String>,
    }
    impl WorkspaceLookup for MultiWs {
        fn local_name(&self) -> &str {
            &self.local
        }
        fn get(&self, name: &str) -> Lookup<'_> {
            if let Some(c) = self.configs.get(name) {
                Lookup::Found(c)
            } else if self.unavailable.iter().any(|u| u == name) {
                Lookup::Unavailable
            } else {
                Lookup::Unknown
            }
        }
    }

    fn conn(type_name: Option<&str>, shared: bool, host: &str) -> ConnectionDef {
        ConnectionDef {
            connection_type: type_name.map(|s| s.to_string()),
            shared,
            values: HashMap::from([("host".to_string(), json!(host))]),
        }
    }

    fn empty_type() -> ConnectionTypeDef {
        ConnectionTypeDef {
            properties: HashMap::new(),
        }
    }

    /// caller: no types/connections. jobs: type `clickhouse`, connections
    /// `clickhouse-prod` (shared) and `private-ch` (not shared).
    /// infra: connection `ch-eu` with `type: jobs.clickhouse`, shared.
    fn three_workspaces() -> MultiWs {
        let caller = WorkspaceConfig::default();
        let mut jobs = WorkspaceConfig::default();
        jobs.connection_types
            .insert("clickhouse".to_string(), empty_type());
        jobs.connections.insert(
            "clickhouse-prod".to_string(),
            conn(Some("clickhouse"), true, "ch.jobs.internal"),
        );
        jobs.connections.insert(
            "private-ch".to_string(),
            conn(Some("clickhouse"), false, "ch.private.internal"),
        );
        let mut infra = WorkspaceConfig::default();
        infra.connections.insert(
            "ch-eu".to_string(),
            conn(Some("jobs.clickhouse"), true, "ch.eu.internal"),
        );
        MultiWs {
            local: "caller".to_string(),
            configs: HashMap::from([
                ("caller".to_string(), caller),
                ("jobs".to_string(), jobs),
                ("infra".to_string(), infra),
            ]),
            unavailable: vec!["broken".to_string()],
        }
    }

    fn schema_of(field_type: &str) -> HashMap<String, InputFieldDef> {
        HashMap::from([("ch".to_string(), field(field_type, false, None))])
    }

    // ── canonical_type_ref ──────────────────────────────────────────────

    #[test]
    fn test_canonical_bare_type_is_local() {
        let ws = three_workspaces();
        let ct = canonical_type_ref("clickhouse", "jobs", &ws).unwrap();
        assert_eq!(ct, CanonicalType { workspace: "jobs".into(), name: "clickhouse".into() });
        assert_eq!(ct.to_string(), "jobs.clickhouse");
    }

    #[test]
    fn test_canonical_dotted_type_resolves_to_owner() {
        let ws = three_workspaces();
        let ct = canonical_type_ref("jobs.clickhouse", "caller", &ws).unwrap();
        assert_eq!(ct.workspace, "jobs");
        assert_eq!(ct.name, "clickhouse");
    }

    #[test]
    fn test_canonical_literal_local_key_wins_over_split() {
        // Library-flattened type `common.pg` is a literal local key.
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("caller")
            .unwrap()
            .connection_types
            .insert("common.pg".to_string(), empty_type());
        let ct = canonical_type_ref("common.pg", "caller", &ws).unwrap();
        assert_eq!(ct, CanonicalType { workspace: "caller".into(), name: "common.pg".into() });
    }

    #[test]
    fn test_canonical_unknown_prefix_is_opaque_local() {
        let ws = three_workspaces();
        let ct = canonical_type_ref("nope.thing", "caller", &ws).unwrap();
        assert_eq!(ct, CanonicalType { workspace: "caller".into(), name: "nope.thing".into() });
    }

    #[test]
    fn test_canonical_known_ws_missing_type_is_error() {
        let ws = three_workspaces();
        let err = canonical_type_ref("jobs.mysql", "caller", &ws).unwrap_err();
        assert!(err.to_string().contains("has no connection type 'mysql'"), "{err}");
    }

    #[test]
    fn test_canonical_unavailable_ws_is_error() {
        let ws = three_workspaces();
        let err = canonical_type_ref("broken.x", "caller", &ws).unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }

    // ── resolve_connection_inputs (qualified refs) ─────────────────────

    #[test]
    fn test_resolve_qualified_shared_connection_from_owner() {
        let ws = three_workspaces();
        let out = resolve_connection_inputs(
            &json!({"ch": "jobs.clickhouse-prod"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.jobs.internal");
    }

    #[test]
    fn test_resolve_qualified_unshared_connection_is_rejected() {
        let ws = three_workspaces();
        let err = resolve_connection_inputs(
            &json!({"ch": "jobs.private-ch"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");
    }

    #[test]
    fn test_resolve_two_hop_connection_declaring_foreign_type() {
        // infra.ch-eu declares `type: jobs.clickhouse` → canonical (jobs, clickhouse)
        let ws = three_workspaces();
        let out = resolve_connection_inputs(
            &json!({"ch": "infra.ch-eu"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.eu.internal");
    }

    #[test]
    fn test_resolve_local_bare_type_rejects_foreign_connection() {
        // caller declares its OWN `clickhouse` type → (caller, clickhouse) ≠ (jobs, clickhouse)
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("caller")
            .unwrap()
            .connection_types
            .insert("clickhouse".to_string(), empty_type());
        let err = resolve_connection_inputs(
            &json!({"ch": "jobs.clickhouse-prod"}),
            &schema_of("clickhouse"),
            &ws,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expects type 'caller.clickhouse'"), "{msg}");
        assert!(msg.contains("is type 'jobs.clickhouse'"), "{msg}");
    }

    #[test]
    fn test_resolve_unknown_workspace_is_user_error() {
        let ws = three_workspaces();
        let err = resolve_connection_inputs(
            &json!({"ch": "nope.x"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown workspace 'nope'"), "{err:#}");
    }

    #[test]
    fn test_resolve_unavailable_workspace_is_distinct_error() {
        let ws = three_workspaces();
        let err = resolve_connection_inputs(
            &json!({"ch": "broken.x"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not available"), "{msg}");
        assert!(!msg.contains("unknown workspace"), "{msg}");
    }

    #[test]
    fn test_resolve_local_connection_declaring_foreign_type_matches() {
        // caller's own `local-ch` says `type: jobs.clickhouse` → satisfies `type: jobs.clickhouse`
        let mut ws = three_workspaces();
        ws.configs.get_mut("caller").unwrap().connections.insert(
            "local-ch".to_string(),
            conn(Some("jobs.clickhouse"), false, "ch.local"),
        );
        let out = resolve_connection_inputs(
            &json!({"ch": "local-ch"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.local");
    }

    #[test]
    fn test_resolve_untyped_shared_connection_matches_any_type() {
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("jobs")
            .unwrap()
            .connections
            .insert("loose".to_string(), conn(None, true, "loose.host"));
        let out = resolve_connection_inputs(
            &json!({"ch": "jobs.loose"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "loose.host");
    }

    #[test]
    fn test_resolve_foreign_typed_connection_gets_type_defaults_and_is_checked() {
        let mut ws = three_workspaces();
        // jobs.clickhouse gains a defaulted `port` and a required `host`.
        let type_def = ConnectionTypeDef {
            properties: HashMap::from([
                (
                    "port".to_string(),
                    crate::models::workflow::ConnectionPropertyDef {
                        property_type: "integer".into(),
                        required: false,
                        default: Some(json!(8443)),
                        secret: false,
                    },
                ),
                (
                    "host".to_string(),
                    crate::models::workflow::ConnectionPropertyDef {
                        property_type: "string".into(),
                        required: true,
                        default: None,
                        secret: false,
                    },
                ),
            ]),
        };
        ws.configs
            .get_mut("jobs")
            .unwrap()
            .connection_types
            .insert("clickhouse".to_string(), type_def);
        // Two-hop infra.ch-eu: gets port default applied at resolution.
        let out = resolve_connection_inputs(
            &json!({"ch": "infra.ch-eu"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["port"], 8443);
        // A foreign-typed connection missing a required field is rejected.
        ws.configs.get_mut("infra").unwrap().connections.insert(
            "bad".to_string(),
            ConnectionDef {
                connection_type: Some("jobs.clickhouse".into()),
                shared: true,
                values: HashMap::new(),
            },
        );
        let err = resolve_connection_inputs(
            &json!({"ch": "infra.bad"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("missing required field 'host'"), "{err:#}");
    }

    #[test]
    fn test_resolve_opaque_dotted_type_string_compare_compat() {
        // Pre-existing configs: input and connection both say `type: foo.bar`,
        // no such type and no workspace `foo`. Must keep working.
        let mut ws = three_workspaces();
        ws.configs.get_mut("caller").unwrap().connections.insert(
            "legacy".to_string(),
            conn(Some("foo.bar"), false, "legacy.host"),
        );
        let out = resolve_connection_inputs(&json!({"ch": "legacy"}), &schema_of("foo.bar"), &ws)
            .unwrap();
        assert_eq!(out["ch"]["host"], "legacy.host");
    }

    #[test]
    fn test_single_workspace_rejects_qualified_refs_with_server_hint() {
        let ws = make_ws_with_connection();
        let single = SingleWorkspace { name: "local", config: &ws };
        let err = resolve_connection_inputs(
            &json!({"db": "other.prod_db"}),
            &{
                let mut s = HashMap::new();
                s.insert("db".to_string(), field("postgres", false, None));
                s
            },
            &single,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("require a server"), "{err:#}");
    }

    // ── prepare_action_input_cross (provenance-aware two-pass) ─────────

    #[test]
    fn test_cross_caller_supplied_bare_name_prefers_caller_then_shared_owner() {
        let mut ws = three_workspaces();
        // Owner action schema (written in jobs): ch: type clickhouse, default private-ch
        let mut schema = HashMap::new();
        schema.insert("ch".to_string(), field("clickhouse", false, Some(json!("private-ch"))));
        // (1) caller supplies shared owner name bare → falls back to owner, ok
        let out = prepare_action_input_cross(
            &json!({"ch": "clickhouse-prod"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.jobs.internal");
        // (2) caller supplies UNSHARED owner name bare → rejected
        let err = prepare_action_input_cross(
            &json!({"ch": "private-ch"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found in workspace 'caller'"), "{msg}");
        assert!(msg.contains("'jobs.private-ch' exists but is not shared"), "{msg}");
        // (3) caller-local bare name wins over an owner name of the same spelling
        ws.configs.get_mut("caller").unwrap().connections.insert(
            "clickhouse-prod".to_string(),
            conn(Some("jobs.clickhouse"), false, "ch.caller.local"),
        );
        let out = prepare_action_input_cross(
            &json!({"ch": "clickhouse-prod"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.caller.local");
    }

    #[test]
    fn test_cross_owner_default_resolves_ungated_in_owner() {
        let ws = three_workspaces();
        let mut schema = HashMap::new();
        schema.insert("ch".to_string(), field("clickhouse", false, Some(json!("private-ch"))));
        // Caller supplies nothing → owner's default `private-ch` (unshared) resolves.
        let out = prepare_action_input_cross(&json!({}), &schema, &ws, "caller", "jobs").unwrap();
        assert_eq!(out["ch"]["host"], "ch.private.internal");
    }

    #[test]
    fn test_prepare_action_input_local_unchanged() {
        let ws = make_ws_with_connection();
        let single = SingleWorkspace { name: "local", config: &ws };
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, Some(json!("prod_db"))));
        let out = prepare_action_input(&json!({}), &schema, &single).unwrap();
        assert_eq!(out["db"]["host"], "db.example.com");
    }
```

Also update every existing test call `resolve_connection_inputs(&input, &schema, &ws)` and `prepare_action_input(&input, &schema, &ws)` in this module to pass `&SingleWorkspace { name: "local", config: &ws }` instead of `&ws` (mechanical; ~25 sites).

Add to `crates/stroem-common/src/validation.rs` `mod tests`:

```rust
    #[test]
    fn test_check_connection_values_required_unknown_empty() {
        use crate::models::workflow::{ConnectionPropertyDef, ConnectionTypeDef};
        let type_def = ConnectionTypeDef {
            properties: HashMap::from([(
                "host".to_string(),
                ConnectionPropertyDef {
                    property_type: "string".into(),
                    required: true,
                    default: None,
                    secret: false,
                },
            )]),
        };
        // missing required
        let err = check_connection_values("c", &HashMap::new(), "t", &type_def).unwrap_err();
        assert!(err.to_string().contains("missing required field 'host'"));
        // unknown field → warning, not error
        let vals = HashMap::from([
            ("host".to_string(), serde_json::json!("h")),
            ("extra".to_string(), serde_json::json!(1)),
        ]);
        let warnings = check_connection_values("c", &vals, "t", &type_def).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("'extra'"));
        // empty string → error
        let vals = HashMap::from([("host".to_string(), serde_json::json!(""))]);
        let err = check_connection_values("c", &vals, "t", &type_def).unwrap_err();
        assert!(err.to_string().contains("empty value"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-common 2>&1 | grep -E "^error|cannot find" | head`
Expected: compile errors for `WorkspaceLookup`, `canonical_type_ref`, `SingleWorkspace`, `prepare_action_input_cross`, `check_connection_values`.

- [ ] **Step 3: Factor `check_connection_values` in validation.rs**

Add above `validate_connections` (line ~629):

```rust
/// Check one connection's values against its type: required properties present
/// (unless the type supplies a default), unknown fields (warning), empty
/// strings (error). Shared by load-time validation and by the resolver for
/// connections whose type lives in another workspace.
pub fn check_connection_values(
    conn_name: &str,
    values: &HashMap<String, serde_json::Value>,
    type_name: &str,
    type_def: &ConnectionTypeDef,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (prop_name, prop_def) in &type_def.properties {
        if prop_def.required && prop_def.default.is_none() && !values.contains_key(prop_name) {
            bail!(
                "Connection '{}' is missing required field '{}' (type '{}')",
                conn_name,
                prop_name,
                type_name
            );
        }
    }
    for key in values.keys() {
        if !type_def.properties.contains_key(key) {
            warnings.push(format!(
                "Connection '{}' has field '{}' not defined in type '{}'",
                conn_name, key, type_name
            ));
        }
    }
    for (key, value) in values {
        if let Some(s) = value.as_str() {
            if s.is_empty() {
                bail!(
                    "Connection '{}' field '{}' has an empty value",
                    conn_name,
                    key
                );
            }
        }
    }
    Ok(warnings)
}
```

(`ConnectionTypeDef` import: `use crate::models::workflow::ConnectionTypeDef;` at the top if not already imported; `HashMap` is already imported.)

Then rewrite the per-connection loop in `validate_connections` to:

```rust
    for (conn_name, conn) in &config.connections {
        if let Some(ref type_name) = conn.connection_type {
            if let Some(type_def) = config.connection_types.get(type_name) {
                warnings.extend(check_connection_values(
                    conn_name,
                    &conn.values,
                    type_name,
                    type_def,
                )?);
                continue;
            }
            bail!(
                "Connection '{}' references non-existent connection type '{}'",
                conn_name,
                type_name
            );
        }
        // Untyped: still reject empty strings.
        for (key, value) in &conn.values {
            if let Some(s) = value.as_str() {
                if s.is_empty() {
                    bail!("Connection '{}' field '{}' has an empty value", conn_name, key);
                }
            }
        }
    }
```

(The dotted-type skip in this loop is added in Task 3; keep the bail here for now.)

- [ ] **Step 4: Implement the lookup + resolver in template.rs**

Add after `parse_qualified_ref` (line ~96):

```rust
/// Outcome of asking a [`WorkspaceLookup`] for a workspace by name.
pub enum Lookup<'a> {
    Found(&'a WorkspaceConfig),
    /// No workspace of that name is configured — an author error.
    Unknown,
    /// Configured, but not loaded / unhealthy right now — a server condition.
    Unavailable,
}

/// Access to workspace configs by name. The server implements this over its
/// in-memory `WorkspaceManager` snapshot; the CLI over the single local config.
pub trait WorkspaceLookup {
    /// The workspace the caller's YAML lives in.
    fn local_name(&self) -> &str;
    fn get(&self, name: &str) -> Lookup<'_>;
    /// `true` when no server is present, so a qualified reference can never be
    /// satisfied. Changes the error wording only.
    fn offline(&self) -> bool {
        false
    }
}

/// A [`WorkspaceLookup`] over exactly one config (CLI, unit tests).
pub struct SingleWorkspace<'a> {
    pub name: &'a str,
    pub config: &'a WorkspaceConfig,
}

impl WorkspaceLookup for SingleWorkspace<'_> {
    fn local_name(&self) -> &str {
        self.name
    }
    fn get(&self, name: &str) -> Lookup<'_> {
        if name == self.name {
            Lookup::Found(self.config)
        } else {
            Lookup::Unknown
        }
    }
    fn offline(&self) -> bool {
        true
    }
}

/// A connection-type reference normalised to the workspace that defines it.
/// Two references match only if both fields are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalType {
    pub workspace: String,
    pub name: String,
}

impl std::fmt::Display for CanonicalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.workspace, self.name)
    }
}

fn found_config<'a>(lookup: &'a dyn WorkspaceLookup, ws: &str, what: &str) -> Result<&'a WorkspaceConfig> {
    match lookup.get(ws) {
        Lookup::Found(c) => Ok(c),
        Lookup::Unknown => bail!("{}: unknown workspace '{}'", what, ws),
        Lookup::Unavailable => bail!("{}: workspace '{}' is not available", what, ws),
    }
}

/// Canonicalise a connection-type reference written in `defining_ws`.
///
/// Precedence: a literal key in `defining_ws` (covers library-flattened
/// `lib.type`), then `ws.type` split on the first dot against a known
/// workspace, then — if the prefix is not a configured workspace — an opaque
/// local name (keeps pre-existing `type: foo.bar` string-compare configs
/// working). A bare name is always `(defining_ws, name)`.
pub fn canonical_type_ref(
    type_ref: &str,
    defining_ws: &str,
    lookup: &dyn WorkspaceLookup,
) -> Result<CanonicalType> {
    let local = |name: &str| CanonicalType {
        workspace: defining_ws.to_string(),
        name: name.to_string(),
    };
    let defining_cfg = found_config(lookup, defining_ws, &format!("connection type '{}'", type_ref))?;
    if defining_cfg.connection_types.contains_key(type_ref) {
        return Ok(local(type_ref));
    }
    match parse_qualified_ref(type_ref) {
        (Some(ws), item) => match lookup.get(ws) {
            Lookup::Found(cfg) => {
                if cfg.connection_types.contains_key(item) {
                    Ok(CanonicalType {
                        workspace: ws.to_string(),
                        name: item.to_string(),
                    })
                } else {
                    bail!(
                        "connection type '{}': workspace '{}' has no connection type '{}'",
                        type_ref,
                        ws,
                        item
                    )
                }
            }
            Lookup::Unknown => Ok(local(type_ref)),
            Lookup::Unavailable => bail!(
                "connection type '{}': workspace '{}' is not available",
                type_ref,
                ws
            ),
        },
        (None, _) => Ok(local(type_ref)),
    }
}

/// Where a connection-typed input is resolved.
pub struct ResolveScope<'a> {
    pub lookup: &'a dyn WorkspaceLookup,
    /// Workspace whose YAML declares the input schema (types canonicalise here).
    pub schema_ws: &'a str,
    /// Workspace in which a bare connection name is looked up first.
    pub value_ws: &'a str,
    /// Second workspace tried for a bare name on a miss — only `shared`
    /// connections qualify. Used for caller-supplied values on a
    /// cross-workspace action step.
    pub fallback_ws: Option<&'a str>,
}

/// A connection located by [`resolve_connection_ref`].
pub struct ResolvedConnection<'a> {
    pub workspace: String,
    pub name: String,
    pub def: &'a ConnectionDef,
}

/// Locate a connection by bare or qualified name, enforcing the `shared` gate
/// for any reference that crosses a workspace boundary.
pub fn resolve_connection_ref<'a>(
    conn_ref: &str,
    scope: &ResolveScope<'a>,
) -> Result<ResolvedConnection<'a>> {
    let lookup = scope.lookup;
    let value_cfg = found_config(lookup, scope.value_ws, &format!("connection '{}'", conn_ref))?;

    // 1. Literal local key (bare name, or a connection literally named "a.b").
    if let Some(def) = value_cfg.connections.get(conn_ref) {
        return Ok(ResolvedConnection {
            workspace: scope.value_ws.to_string(),
            name: conn_ref.to_string(),
            def,
        });
    }

    // 2. Qualified `ws.name`.
    if let (Some(ws), item) = parse_qualified_ref(conn_ref) {
        return match lookup.get(ws) {
            Lookup::Found(cfg) => match cfg.connections.get(item) {
                Some(def) if ws == scope.value_ws || def.shared => Ok(ResolvedConnection {
                    workspace: ws.to_string(),
                    name: item.to_string(),
                    def,
                }),
                Some(_) => bail!(
                    "connection '{}' exists in workspace '{}' but is not shared (set `shared: true` on it in workspace '{}')",
                    conn_ref,
                    ws,
                    ws
                ),
                None => bail!(
                    "connection '{}' does not exist: workspace '{}' has no connection '{}'",
                    conn_ref,
                    ws,
                    item
                ),
            },
            Lookup::Unknown if lookup.offline() => bail!(
                "connection '{}': cross-workspace connection references require a server (run this task through `stroem-api trigger`)",
                conn_ref
            ),
            Lookup::Unknown => bail!("connection '{}': unknown workspace '{}'", conn_ref, ws),
            Lookup::Unavailable => bail!(
                "connection '{}': workspace '{}' is not available",
                conn_ref,
                ws
            ),
        };
    }

    // 3. Bare name missed locally: try the fallback workspace, gated by `shared`.
    if let Some(fb) = scope.fallback_ws.filter(|fb| *fb != scope.value_ws) {
        if let Lookup::Found(cfg) = lookup.get(fb) {
            match cfg.connections.get(conn_ref) {
                Some(def) if def.shared => {
                    return Ok(ResolvedConnection {
                        workspace: fb.to_string(),
                        name: conn_ref.to_string(),
                        def,
                    })
                }
                Some(_) => bail!(
                    "connection '{}' not found in workspace '{}'; '{}.{}' exists but is not shared",
                    conn_ref,
                    scope.value_ws,
                    fb,
                    conn_ref
                ),
                None => {}
            }
        }
    }

    bail!(
        "connection '{}' does not exist in workspace '{}'",
        conn_ref,
        scope.value_ws
    )
}
```

Replace the body of `resolve_connection_inputs` (line ~321) with the scoped version plus a thin wrapper:

```rust
/// Resolve connection inputs: replace connection name strings with the full
/// connection object. See [`resolve_connection_inputs_scoped`]; this wrapper
/// resolves everything in `lookup.local_name()`.
pub fn resolve_connection_inputs(
    input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
) -> Result<serde_json::Value> {
    let local = lookup.local_name();
    resolve_connection_inputs_scoped(
        input,
        input_schema,
        &ResolveScope {
            lookup,
            schema_ws: local,
            value_ws: local,
            fallback_ws: None,
        },
    )
}

/// Resolve connection inputs within an explicit [`ResolveScope`].
///
/// For each field in `input_schema` whose `field_type` is not a primitive, the
/// value must be a connection name (string) or an already-resolved object
/// (passed through). The name is located by [`resolve_connection_ref`], its
/// declared type is canonicalised in the connection's own workspace and must
/// equal the field's canonical type (untyped connections match anything —
/// existing rule). A connection whose type lives in a different workspace than
/// the connection itself gets that type's property defaults applied and its
/// values checked here, because the owner's load-time pass never saw the type.
pub fn resolve_connection_inputs_scoped(
    input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    scope: &ResolveScope,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let input_map = input.as_object().unwrap_or(&empty);
    let mut result = input_map.clone();

    for (field_name, field_def) in input_schema {
        if PRIMITIVE_TYPES.contains(&field_def.field_type.as_str()) {
            continue;
        }
        let value = match result.get(field_name) {
            Some(v) => v.clone(),
            None => continue,
        };
        let conn_name = match value.as_str() {
            Some(s) => s,
            None => {
                if value.is_object() {
                    continue;
                }
                bail!(
                    "Input field '{}' expects a connection name (string), got {}",
                    field_name,
                    value
                );
            }
        };

        let field_ct = canonical_type_ref(&field_def.field_type, scope.schema_ws, scope.lookup)
            .with_context(|| format!("Input field '{}'", field_name))?;
        let resolved = resolve_connection_ref(conn_name, scope)
            .with_context(|| format!("Input field '{}' references connection '{}'", field_name, conn_name))?;

        let values = match resolved.def.connection_type {
            None => resolved.def.values.clone(),
            Some(ref declared) => {
                let conn_ct = canonical_type_ref(declared, &resolved.workspace, scope.lookup)
                    .with_context(|| format!("connection '{}'", conn_name))?;
                if conn_ct != field_ct {
                    bail!(
                        "Input field '{}' expects type '{}' but connection '{}' is type '{}'",
                        field_name,
                        field_ct,
                        conn_name,
                        conn_ct
                    );
                }
                if conn_ct.workspace != resolved.workspace {
                    // Foreign-typed connection: defaults + checks not done at load.
                    let type_cfg = found_config(scope.lookup, &conn_ct.workspace, "connection type")?;
                    let type_def = type_cfg
                        .connection_types
                        .get(&conn_ct.name)
                        .with_context(|| format!("connection type '{}' vanished", conn_ct))?;
                    let with_defaults = resolved.def.values_with_type_defaults(type_def);
                    crate::validation::check_connection_values(
                        &format!("{}.{}", resolved.workspace, resolved.name),
                        &with_defaults,
                        &conn_ct.to_string(),
                        type_def,
                    )?;
                    with_defaults
                } else {
                    resolved.def.values.clone()
                }
            }
        };

        let values_json =
            serde_json::to_value(&values).context("Failed to serialize connection values")?;
        result.insert(field_name.clone(), values_json);
    }

    Ok(serde_json::Value::Object(result))
}
```

Replace `prepare_action_input` (line ~510) and add the cross variant:

```rust
/// Prepare action input: merge defaults, resolve connection references, all
/// within `lookup.local_name()` (a local step: caller == owner).
pub fn prepare_action_input(
    rendered_input: &serde_json::Value,
    action_input_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
) -> Result<serde_json::Value> {
    let local = lookup.local_name();
    prepare_action_input_cross(rendered_input, action_input_schema, lookup, local, local)
}

/// Prepare action input for a step whose action is owned by `owner_ws` while
/// the flow step (and `rendered_input`) belong to `caller_ws`.
///
/// Provenance-aware two-pass:
/// 1. Fields present in `rendered_input` were supplied by the caller: resolve
///    them with bare names in the caller first, falling back to the owner only
///    for `shared` connections.
/// 2. Merge the owner's action defaults (rendered with the owner's secrets).
/// 3. Fields filled by defaults are the owner reading its own config: resolve
///    ungated in the owner. Fields resolved in pass 1 are objects by now and
///    pass through.
/// When `caller_ws == owner_ws` the passes collapse to the local behaviour.
pub fn prepare_action_input_cross(
    rendered_input: &serde_json::Value,
    action_input_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
    caller_ws: &str,
    owner_ws: &str,
) -> Result<serde_json::Value> {
    let owner_cfg = found_config(lookup, owner_ws, "action owner")?;

    let caller_resolved = resolve_connection_inputs_scoped(
        rendered_input,
        action_input_schema,
        &ResolveScope {
            lookup,
            schema_ws: owner_ws,
            value_ws: caller_ws,
            fallback_ws: if caller_ws == owner_ws { None } else { Some(owner_ws) },
        },
    )
    .context("Failed to resolve action connection inputs")?;

    let secrets_ctx = serde_json::json!({ "secret": &owner_cfg.secrets });
    let merged = merge_action_defaults(&caller_resolved, action_input_schema, &secrets_ctx)
        .context("Failed to merge action input defaults")?;

    resolve_connection_inputs_scoped(
        &merged,
        action_input_schema,
        &ResolveScope {
            lookup,
            schema_ws: owner_ws,
            value_ws: owner_ws,
            fallback_ws: None,
        },
    )
    .context("Failed to resolve action connection inputs")
}
```

Add `use crate::models::workflow::ConnectionDef;` to the imports at the top of template.rs if it is not already there (it is used by `ResolvedConnection`).

- [ ] **Step 5: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-common 2>&1 | tail -5`
Expected: all pass. The other crates will not compile yet (signature change) — that is expected until Tasks 4-8.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/stroem-common
git commit -m "feat(connections): WorkspaceLookup, canonical types, shared-gated cross-workspace resolver"
```

---

### Task 3: Offline validation tolerates qualified type references

**Files:**
- Modify: `crates/stroem-common/src/validation.rs` (`validate_connections` ~629, `validate_connection_inputs` ~837)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: nothing new.
- Produces: warnings instead of errors for dotted type refs that are not literal local keys.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn test_dotted_input_type_is_warning_not_error() {
        let yaml = r#"
tasks:
  t:
    input:
      ch: { type: jobs.clickhouse }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("jobs.clickhouse") && w.contains("cross-workspace")),
            "{warnings:?}"
        );
    }

    #[test]
    fn test_dotted_connection_type_is_warning_not_error() {
        let yaml = r#"
connections:
  ch-eu:
    type: jobs.clickhouse
    shared: true
    host: ch.eu
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("ch-eu") && w.contains("cross-workspace")),
            "{warnings:?}"
        );
    }

    #[test]
    fn test_literal_library_type_key_is_not_warned() {
        let yaml = r#"
connection_types:
  common.pg:
    host: { type: string }
tasks:
  t:
    input:
      db: { type: common.pg }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(!warnings.iter().any(|w| w.contains("cross-workspace")), "{warnings:?}");
    }

    #[test]
    fn test_bare_unknown_type_is_still_an_error() {
        let yaml = r#"
tasks:
  t:
    input:
      ch: { type: clickhouse }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(validate_workflow_config(&config).is_err());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-common dotted 2>&1 | tail -20`
Expected: the two "warning" tests fail with `Err`.

- [ ] **Step 3: Implement**

In `validate_connection_inputs`, change the `check` closure to take `&mut Vec<String>` warnings. Replace the unknown-type bail with:

```rust
        if !config.connection_types.contains_key(&field_def.field_type) {
            if field_def.field_type.contains('.') {
                warnings.push(format!(
                    "{} '{}' input '{}': type '{}' is a cross-workspace reference; validated at job creation",
                    scope, owner, field_name, field_def.field_type
                ));
                return Ok(());
            }
            bail!(
                "{} '{}' input '{}' references unknown type '{}' (not a primitive or connection type)",
                scope, owner, field_name, field_def.field_type
            );
        }
```

Since a closure cannot borrow `warnings` mutably while also being called in two loops cleanly, convert `check` into a nested `fn check(config: &WorkspaceConfig, warnings: &mut Vec<String>, scope: &str, owner: &str, field_name: &str, field_def: &InputFieldDef) -> Result<()>` and make `let mut warnings = Vec::new();`.

In `validate_connections`' per-connection loop, before the `bail!("... non-existent connection type ...")`:

```rust
            if type_name.contains('.') {
                warnings.push(format!(
                    "Connection '{}': type '{}' is a cross-workspace reference; validated at job creation",
                    conn_name, type_name
                ));
                // Still reject empty strings below.
            } else {
                bail!(
                    "Connection '{}' references non-existent connection type '{}'",
                    conn_name,
                    type_name
                );
            }
```

(and let the loop fall through to the empty-string check instead of `continue`ing past it — restructure so the empty-string check runs for typed-but-unresolved and untyped connections alike).

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-common 2>&1 | tail -5`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-common/src/validation.rs
git commit -m "feat(validation): warn instead of fail on cross-workspace connection type refs"
```

---

### Task 4: `WorkspaceSet` in the server

**Files:**
- Create: `crates/stroem-server/src/workspace_set.rs`
- Modify: `crates/stroem-server/src/lib.rs:28` (add `pub mod workspace_set;` after `pub mod workspace;`)
- Test: in-module

**Interfaces:**
- Consumes: `WorkspaceManager::{names, get_all_configs, from_configs}`; `WorkspaceLookup`, `Lookup`, `canonical_type_ref` (Task 2).
- Produces:
  ```rust
  pub struct WorkspaceSet<'a> { .. }
  impl<'a> WorkspaceSet<'a> {
      pub async fn load(workspaces: &WorkspaceManager, local_name: &str, local_override: Option<&'a WorkspaceConfig>) -> WorkspaceSet<'a>;
      pub fn from_parts(local_name: &str, local_override: Option<&'a WorkspaceConfig>, configs: Vec<(String, Arc<WorkspaceConfig>)>, known: Vec<String>) -> WorkspaceSet<'a>;  // tests
      pub fn iter_configs(&self) -> impl Iterator<Item = (&str, &WorkspaceConfig)>;  // local first, then others sorted by name
  }
  impl WorkspaceLookup for WorkspaceSet<'_>;
  pub fn collect_redaction_values(set: &WorkspaceSet) -> Vec<String>;
  ```

- [ ] **Step 1: Write the failing tests**

`crates/stroem-server/src/workspace_set.rs`:

```rust
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
        c.secrets
            .insert("token".to_string(), json!(secret));
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
        let set = WorkspaceSet::from_parts(
            "A",
            Some(&a),
            vec![("B".to_string(), Arc::new(b))],
            vec![],
        );
        let vals = collect_redaction_values(&set);
        for expected in ["secret-a-value", "secret-b-value", "literal-pw-value", "mirror-pw-value"] {
            assert!(vals.contains(&expected.to_string()), "missing {expected}: {vals:?}");
        }
        assert!(!vals.contains(&"public-host".to_string()));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --lib workspace_set 2>&1 | grep -E "^error" | head`
Expected: missing `from_parts`, `load`, `iter_configs`, `collect_redaction_values` (server crate may also fail elsewhere from Task 2's signature change — fix those in Tasks 5-8; for this step it is acceptable to temporarily see those errors too).

- [ ] **Step 3: Implement**

Fill the file between the struct and the tests:

```rust
impl<'a> WorkspaceSet<'a> {
    /// Snapshot every healthy workspace config from the manager. Cheap: `Arc`
    /// clones plus one `RwLock` read per workspace, no I/O.
    pub async fn load(
        workspaces: &WorkspaceManager,
        local_name: &str,
        local_override: Option<&'a WorkspaceConfig>,
    ) -> WorkspaceSet<'a> {
        let configs = workspaces.get_all_configs().await;
        let known = workspaces.names().into_iter().map(str::to_owned).collect();
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
            let Some(ref declared) = conn.connection_type else { continue };
            let Ok(ct) = canonical_type_ref(declared, ws_name, set) else { continue };
            let Lookup::Found(type_cfg) = set.get(&ct.workspace) else { continue };
            let Some(type_def) = type_cfg.connection_types.get(&ct.name) else { continue };
            for (prop, def) in &type_def.properties {
                if def.secret {
                    if let Some(v) = conn.values.get(prop) {
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
```

Register the module in `crates/stroem-server/src/lib.rs`: add `pub mod workspace_set;` directly after the existing `pub mod workspace;` line (line 28).

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --lib workspace_set 2>&1 | tail -5`
Expected: 4 tests pass (once Tasks 5-8 make the crate compile — if the crate still fails to compile elsewhere, proceed to Task 5 and re-run this at the end of Task 6).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/workspace_set.rs crates/stroem-server/src/lib.rs
git commit -m "feat(server): WorkspaceSet lookup over all loaded workspaces + redaction value collector"
```

---

### Task 5: Job creation — task inputs, literal pre-check, task steps

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs:165-175` (task input), `:180-310` (step loop — add pre-check), `:489-498` (`handle_task_steps`)
- Modify: `crates/stroem-server/src/web/api/tasks.rs:503-512` (`is_user_error`)
- Test: `crates/stroem-server/src/job_creator.rs` `mod tests` (unit test for the pre-check helper)

**Interfaces:**
- Consumes: `WorkspaceSet::load`, `resolve_connection_inputs`, `resolve_connection_inputs_scoped`, `ResolveScope`, `prepare_action_input`.
- Produces: `fn precheck_literal_connection_inputs(step_name: &str, flow_step: &FlowStep, action: &ActionDef, set: &WorkspaceSet, caller_ws: &str, owner_ws: Option<&str>) -> Result<()>` (private).

- [ ] **Step 1: Write the failing unit test**

In `job_creator.rs` `mod tests` (create the module if absent; check first with `grep -n "mod tests" crates/stroem-server/src/job_creator.rs`):

```rust
    #[test]
    fn precheck_rejects_literal_unshared_ref_and_ignores_templates() {
        use crate::workspace_set::WorkspaceSet;
        use std::sync::Arc;
        use stroem_common::models::workflow::{
            ActionDef, ConnectionDef, ConnectionTypeDef, FlowStep, InputFieldDef, WorkspaceConfig,
        };

        let mut owner = WorkspaceConfig::default();
        owner.connection_types.insert(
            "ch".to_string(),
            ConnectionTypeDef { properties: Default::default() },
        );
        owner.connections.insert(
            "private".to_string(),
            ConnectionDef {
                connection_type: Some("ch".into()),
                shared: false,
                values: Default::default(),
            },
        );
        owner.connections.insert(
            "open".to_string(),
            ConnectionDef {
                connection_type: Some("ch".into()),
                shared: true,
                values: Default::default(),
            },
        );
        let caller = WorkspaceConfig::default();
        let set = WorkspaceSet::from_parts(
            "caller",
            Some(&caller),
            vec![("owner".to_string(), Arc::new(owner))],
            vec![],
        );

        let mut action: ActionDef = serde_yaml::from_str("type: script\nscript: echo").unwrap();
        action.input.insert(
            "conn".to_string(),
            InputFieldDef {
                field_type: "owner.ch".to_string(),
                ..serde_yaml::from_str("type: string").unwrap()
            },
        );

        let step = |v: &str| -> FlowStep {
            serde_yaml::from_str(&format!("action: a\ninput:\n  conn: \"{v}\"")).unwrap()
        };

        // Literal, unshared → error mentioning "is not shared"
        let err = precheck_literal_connection_inputs("s", &step("owner.private"), &action, &set, "caller", None)
            .unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");
        // Literal, shared → ok
        precheck_literal_connection_inputs("s", &step("owner.open"), &action, &set, "caller", None).unwrap();
        // Templated → skipped (no error even though it would not resolve)
        precheck_literal_connection_inputs("s", &step("{{ input.pick }}"), &action, &set, "caller", None)
            .unwrap();
    }
```

(If `InputFieldDef` cannot be built with struct-update syntax because of private fields, build it with `serde_yaml::from_str::<InputFieldDef>("type: owner.ch")` — check how `deserialize_field_type` treats dotted names; it must pass them through unchanged. If it rejects dots, fix `canonicalize_field_type` to leave unknown strings untouched, which is its current behaviour for non-aliases.)

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --lib precheck 2>&1 | grep -E "^error" | head`
Expected: `precheck_literal_connection_inputs` not found.

- [ ] **Step 3: Implement**

At the top of `job_creator.rs` add imports:

```rust
use crate::workspace_set::WorkspaceSet;
use stroem_common::template::{resolve_connection_inputs_scoped, ResolveScope};
```

In `create_job_for_task_inner`, replace lines 165-175 (merge + resolve) with:

```rust
        // Merge input defaults from the task schema
        let secrets_ctx = serde_json::json!({ "secret": workspace_config.secrets });
        let merged_input = merge_defaults(&effective_input, &task.input, &secrets_ctx)
            .context("Failed to merge input defaults")?;

        // Resolve connection inputs (replace connection names with full objects).
        // Qualified names (`ws.conn`) resolve against other workspaces, gated by `shared`.
        let ws_set = WorkspaceSet::load(workspaces, workspace_name, Some(workspace_config)).await;
        let resolved_input = resolve_connection_inputs(&merged_input, &task.input, &ws_set)
            .context("Failed to resolve connection inputs")?;
```

Inside the step loop, right after `let action = &owned_action;`, add:

```rust
            // Fail fast (400) on literal connection references the worker would
            // otherwise reject at claim time. Templated values cannot be checked here.
            precheck_literal_connection_inputs(
                step_name,
                flow_step,
                action,
                &ws_set,
                workspace_name,
                action_workspace.as_deref(),
            )?;
```

Add the helper as a free function near the bottom of the file (above `mod tests`):

```rust
/// Resolve the flow step's connection-typed inputs that are plain string
/// literals (no `{{`), using the same scope the claim path will use, so an
/// author mistake surfaces as a job-creation error instead of a failed step.
fn precheck_literal_connection_inputs(
    step_name: &str,
    flow_step: &FlowStep,
    action: &ActionDef,
    set: &WorkspaceSet,
    caller_ws: &str,
    owner_ws: Option<&str>,
) -> Result<()> {
    let owner_ws = owner_ws.unwrap_or(caller_ws);
    let mut literal_schema = HashMap::new();
    let mut literal_values = serde_json::Map::new();
    for (field, def) in &action.input {
        if stroem_common::template::PRIMITIVE_TYPES.contains(&def.field_type.as_str()) {
            continue;
        }
        if let Some(serde_json::Value::String(s)) = flow_step.input.get(field) {
            if !s.contains("{{") {
                literal_schema.insert(field.clone(), def.clone());
                literal_values.insert(field.clone(), serde_json::Value::String(s.clone()));
            }
        }
    }
    if literal_schema.is_empty() {
        return Ok(());
    }
    resolve_connection_inputs_scoped(
        &serde_json::Value::Object(literal_values),
        &literal_schema,
        &ResolveScope {
            lookup: set,
            schema_ws: owner_ws,
            value_ws: caller_ws,
            fallback_ws: if owner_ws == caller_ws { None } else { Some(owner_ws) },
        },
    )
    .with_context(|| format!("step '{}': failed to resolve connection inputs", step_name))
    .map(|_| ())
}
```

(Add `use stroem_common::models::workflow::{ActionDef, FlowStep};` if not imported. `PRIMITIVE_TYPES` is already `pub` in template.rs.)

In `handle_task_steps` (line ~489) replace the `prepare_action_input(&rendered_input, &action.input, workspace_config)` call:

```rust
        // Merge action-level input defaults and resolve connection inputs
        let rendered_input = if let Some(action) = workspace_config.actions.get(&step.action_name) {
            if !action.input.is_empty() {
                let ws_set =
                    WorkspaceSet::load(workspaces, workspace_name, Some(workspace_config)).await;
                match prepare_action_input(&rendered_input, &action.input, &ws_set) {
```

(the rest of the match unchanged).

In `web/api/tasks.rs` `is_user_error` (line ~503) add two lines:

```rust
            || msg.contains("is not shared") // cross-workspace connection gate
            || msg.contains("unknown workspace") // qualified ref to a workspace that is not configured
```

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --lib 2>&1 | tail -5`
Expected: the new unit test passes. (Rendering tests may still fail to compile until Task 6.)

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/src/web/api/tasks.rs
git commit -m "feat(server): resolve cross-workspace connections at job creation with literal pre-check"
```

---

### Task 6: Claim-time rendering with provenance-aware resolution

**Files:**
- Modify: `crates/stroem-server/src/web/worker_api/rendering.rs:20-35` (`RenderContext`), `:136-190` (`prepare_step_action_input`), tests (~20 `RenderContext {` literals)
- Modify: `crates/stroem-server/src/web/worker_api/jobs.rs:596-625` (claim_job builds the context)
- Test: `rendering.rs` `mod tests`

**Interfaces:**
- Consumes: `WorkspaceSet`, `prepare_action_input`, `prepare_action_input_cross`, `SingleWorkspace`.
- Produces: `RenderContext` gains `pub lookup: &'a dyn WorkspaceLookup` and `pub action_workspace_name: Option<&'a str>`.

- [ ] **Step 1: Write the failing tests**

In `rendering.rs` `mod tests`, next to `test_prepare_step_action_input_resolves_local_library_dotted_action`:

```rust
    #[test]
    fn test_prepare_step_action_input_cross_caller_bare_name_requires_shared() {
        use crate::workspace_set::WorkspaceSet;
        use std::sync::Arc;
        use stroem_common::models::workflow::{ConnectionDef, ConnectionTypeDef};

        // Owner B: action `remote` with connection-typed input `conn` (type pg),
        // connections `prod` (unshared) and `open` (shared).
        let mut remote = make_action("script");
        remote.input.insert("conn".to_string(), make_input_field("pg"));
        let mut owner = WorkspaceConfig::default();
        owner.actions.insert("remote".to_string(), remote);
        owner.connection_types.insert(
            "pg".to_string(),
            ConnectionTypeDef { properties: HashMap::new() },
        );
        owner.connections.insert(
            "prod".to_string(),
            ConnectionDef {
                connection_type: Some("pg".to_string()),
                shared: false,
                values: HashMap::from([("host".to_string(), json!("db.owner.internal"))]),
            },
        );
        owner.connections.insert(
            "open".to_string(),
            ConnectionDef {
                connection_type: Some("pg".to_string()),
                shared: true,
                values: HashMap::from([("host".to_string(), json!("db.open.internal"))]),
            },
        );

        let mut task = TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        };
        task.flow.insert(
            "s".to_string(),
            make_flow_step("B.remote", HashMap::from([("conn".to_string(), json!("prod"))])),
        );
        let mut caller = WorkspaceConfig::default();
        caller.tasks.insert("t".to_string(), task);

        let set = WorkspaceSet::from_parts(
            "A",
            Some(&caller),
            vec![("B".to_string(), Arc::new(owner.clone()))],
            vec![],
        );
        let step = make_step_row("s", None);
        let ctx = RenderContext {
            workspace: &caller,
            task_name: "t",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: Some(&owner),
            action_workspace_name: Some("B"),
            lookup: &set,
            job_revision: None,
        };

        // Caller-supplied bare `prod` (unshared in B) → rejected.
        let err = prepare_step_action_input(Some(json!({"conn": "prod"})), &ctx).unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");

        // Caller-supplied bare `open` (shared in B) → resolves in B.
        let out = prepare_step_action_input(Some(json!({"conn": "open"})), &ctx)
            .unwrap()
            .unwrap();
        assert_eq!(out["conn"]["host"], "db.open.internal");
    }
```

Update the existing cross-workspace test `test_prepare_step_action_input_resolves_cross_workspace_in_owner_context` (the one asserting `db.owner.internal`, ~line 1360-1410): the owner's `prod` there is referenced bare by the caller. Under the new rule it must be **shared** — set `shared: true` on that `ConnectionDef` and add a `lookup: &set` built with `WorkspaceSet::from_parts("A", Some(&caller), vec![("B", Arc::new(owner.clone()))], vec![])` plus `action_workspace_name: Some("B")`. Also add a second assertion in that test: with `shared: false` the same call errors with `is not shared` (this pins the behaviour change from the spec).

Every other `RenderContext {` literal in the tests gets:

```rust
            action_workspace_name: None,
            lookup: &stroem_common::template::SingleWorkspace { name: "default", config: &workspace },
```

(where `workspace` is whatever local config variable that test already passes as `workspace:`).

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --lib rendering 2>&1 | grep -E "^error" | head`
Expected: missing fields `lookup`, `action_workspace_name`.

- [ ] **Step 3: Implement**

In `RenderContext` add, after `action_workspace`:

```rust
    /// Name of the owner workspace when `action_workspace` is set.
    pub action_workspace_name: Option<&'a str>,
    /// Snapshot of all workspace configs for cross-workspace connection resolution.
    pub lookup: &'a dyn stroem_common::template::WorkspaceLookup,
```

In `prepare_step_action_input`, replace the final `prepare_action_input(...)` call:

```rust
    let prepared = match ctx.action_workspace_name {
        Some(owner_name) => stroem_common::template::prepare_action_input_cross(
            &input_val,
            &action.input,
            ctx.lookup,
            ctx.lookup.local_name(),
            owner_name,
        ),
        None => prepare_action_input(&input_val, &action.input, ctx.lookup),
    }
    .context("Failed to prepare action input")?;
    Ok(Some(prepared))
```

In `worker_api/jobs.rs` claim_job, just before `let rendered_input = if let Some(ref workspace) = ws_config {`, add:

```rust
    // Snapshot of every workspace config: cross-workspace connection references
    // in this step's input resolve against it (gated by `shared`).
    let ws_set = crate::workspace_set::WorkspaceSet::load(
        &state.workspaces,
        &job.workspace,
        ws_config.as_deref(),
    )
    .await;
```

and in the `RenderContext { .. }` literal add:

```rust
            action_workspace_name: if step.action_workspace.is_some() {
                step.action_workspace.as_deref()
            } else {
                None
            },
            lookup: &ws_set,
```

Grep for any other `RenderContext {` construction outside tests (`grep -rn "RenderContext {" crates/stroem-server/src | grep -v test`) — `hooks.rs` and `job_creator.rs` build **different** contexts (`build_step_render_context` returns a JSON value, not this struct), so only `claim_job` should need the change. If another site appears, give it a `SingleWorkspace` over its workspace and `action_workspace_name: None`.

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --lib 2>&1 | tail -5`
Expected: all lib tests pass, including Task 4's `workspace_set` tests and Task 5's `precheck` test.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/web/worker_api
git commit -m "feat(server): claim-time cross-workspace connection resolution with shared gate"
```

---

### Task 7: Task-detail dropdown across workspaces

**Files:**
- Modify: `crates/stroem-server/src/web/api/tasks.rs:278-300`
- Test: `crates/stroem-server/tests/integration_test.rs` (new test, uses a new `setup_shared_connections()` fixture also used by Task 9)

**Interfaces:**
- Consumes: `WorkspaceSet::{load, iter_configs}`, `canonical_type_ref`.
- Produces: `TaskDetail.connections` keyed by the input's `type:` **as written**, values are bare local names then `ws.name` foreign shared names.

- [ ] **Step 1: Write the fixture and the failing test**

In `crates/stroem-server/tests/integration_test.rs`, after `setup_two_workspaces` (~line 1425), add:

```rust
/// Three workspaces for cross-workspace CONNECTION tests.
///
/// owner:  type `clickhouse` {host, token(secret)}, connections `shared-conn`
///         (shared, token "owner-token-secret-value"), `private-conn` (unshared),
///         action `remote` with input conn: type clickhouse, default private-conn.
/// infra:  connection `ch-eu` with `type: owner.clickhouse`, shared.
/// caller: task `use-shared`   input conn: {type: owner.clickhouse, default: owner.shared-conn}
///                              step run: local `echo-conn` action, input conn: "{{ input.conn }}"
///         task `use-private`  same but default owner.private-conn
///         task `use-infra`    same but default infra.ch-eu
///         task `bad-type`     input conn: {type: clickhouse} (caller-local type) default owner.shared-conn
///         task `via-action`   step run: action owner.remote, input conn: "private-conn" (bare, caller-supplied)
///         task `literal-bad`  step run: local echo-conn, input conn: "owner.private-conn" (literal)
///         task `templated`    input pick: string; step run: local echo-conn, input conn: "{{ input.pick }}"
///         caller has a local type `clickhouse` (for bad-type) and NO connections.
async fn setup_shared_connections() -> Result<(
    Router,
    PgPool,
    TempDir,
    testcontainers::ContainerAsync<Postgres>,
)> {
    use stroem_common::models::workflow::{ConnectionDef, ConnectionPropertyDef, ConnectionTypeDef, InputFieldDef};

    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let ch_type = || ConnectionTypeDef {
        properties: HashMap::from([
            (
                "host".to_string(),
                ConnectionPropertyDef { property_type: "string".into(), required: true, default: None, secret: false },
            ),
            (
                "token".to_string(),
                ConnectionPropertyDef { property_type: "string".into(), required: false, default: None, secret: true },
            ),
        ]),
    };
    let conn = |shared: bool, host: &str, token: &str, ty: &str| ConnectionDef {
        connection_type: Some(ty.to_string()),
        shared,
        values: HashMap::from([
            ("host".to_string(), json!(host)),
            ("token".to_string(), json!(token)),
        ]),
    };
    let field = |ty: &str, default: Option<&str>| -> InputFieldDef {
        let mut f: InputFieldDef = serde_yaml::from_str(&format!("type: {ty}")).unwrap();
        f.default = default.map(|d| json!(d));
        f
    };
    let flow_step = |action: &str, conn_value: &str| FlowStep {
        action: action.to_string(),
        name: None,
        description: None,
        depends_on: vec![],
        input: HashMap::from([("conn".to_string(), json!(conn_value))]),
        continue_on_failure: false,
        timeout: None,
        when: None,
        for_each: None,
        sequential: false,
        retry: None,
        inline_action: None,
    };
    let task = |input: HashMap<String, InputFieldDef>, step: FlowStep| TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input,
        flow: HashMap::from([("run".to_string(), step)]),
        timeout: None,
        retry: None,
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
        on_cancel: vec![],
    };

    // owner
    let mut owner = WorkspaceConfig::default();
    owner.connection_types.insert("clickhouse".into(), ch_type());
    owner.connections.insert("shared-conn".into(), conn(true, "shared.host", "owner-token-secret-value", "clickhouse"));
    owner.connections.insert("private-conn".into(), conn(false, "private.host", "private-token-secret", "clickhouse"));
    let mut remote = trivial_script_action("echo $CONN");
    remote.input.insert("conn".into(), field("clickhouse", Some("private-conn")));
    owner.actions.insert("remote".into(), remote);

    // infra
    let mut infra = WorkspaceConfig::default();
    infra.connections.insert("ch-eu".into(), conn(true, "eu.host", "eu-token-secret-value", "owner.clickhouse"));

    // caller
    let mut caller = WorkspaceConfig::default();
    caller.connection_types.insert("clickhouse".into(), ch_type());
    let mut echo_conn = trivial_script_action("echo $CONN");
    echo_conn.input.insert("conn".into(), field("owner.clickhouse", None));
    caller.actions.insert("echo-conn".into(), echo_conn);
    let conn_input = |ty: &str, default: &str| HashMap::from([("conn".to_string(), field(ty, Some(default)))]);
    caller.tasks.insert("use-shared".into(), task(conn_input("owner.clickhouse", "owner.shared-conn"), flow_step("echo-conn", "{{ input.conn }}")));
    caller.tasks.insert("use-private".into(), task(conn_input("owner.clickhouse", "owner.private-conn"), flow_step("echo-conn", "{{ input.conn }}")));
    caller.tasks.insert("use-infra".into(), task(conn_input("owner.clickhouse", "infra.ch-eu"), flow_step("echo-conn", "{{ input.conn }}")));
    caller.tasks.insert("bad-type".into(), task(conn_input("clickhouse", "owner.shared-conn"), flow_step("echo-conn", "{{ input.conn }}")));
    caller.tasks.insert("via-action".into(), task(HashMap::new(), flow_step("owner.remote", "private-conn")));
    caller.tasks.insert("literal-bad".into(), task(HashMap::new(), flow_step("echo-conn", "owner.private-conn")));
    caller.tasks.insert(
        "templated".into(),
        task(HashMap::from([("pick".to_string(), field("string", Some("owner.private-conn")))]), flow_step("echo-conn", "{{ input.pick }}")),
    );

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
        },
        workspaces: HashMap::from([
            (
                "caller".to_string(),
                WorkspaceSourceDef::Folder {
                    path: temp_dir.path().to_string_lossy().to_string(),
                },
            ),
            (
                "owner".to_string(),
                WorkspaceSourceDef::Folder {
                    path: temp_dir.path().to_string_lossy().to_string(),
                },
            ),
            (
                "infra".to_string(),
                WorkspaceSourceDef::Folder {
                    path: temp_dir.path().to_string_lossy().to_string(),
                },
            ),
        ]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
    };
    let mgr = WorkspaceManager::from_configs(vec![
        ("caller".to_string(), caller, None),
        ("owner".to_string(), owner, Some("rev-owner-1".to_string())),
        ("infra".to_string(), infra, None),
    ]);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let router = build_router(state, CancellationToken::new());
    Ok((router, pool, temp_dir, container))
}
```

(`ConnectionPropertyDef` must be added to the `stroem_common::models::workflow` import list at the top of the file. `trivial_script_action` already exists in this file.)

Then the dropdown test:

```rust
#[tokio::test]
async fn test_task_detail_lists_shared_foreign_connections_only() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request("GET", "/api/workspaces/caller/tasks/use-shared", json!({})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let list = body["connections"]["owner.clickhouse"]
        .as_array()
        .expect("dropdown keyed by the type as written");
    let names: Vec<&str> = list.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(names, vec!["infra.ch-eu", "owner.shared-conn"], "{names:?}");
    Ok(())
}
```

(`body_json(response).await -> Value` already exists in this file at ~line 1641.)

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --test integration_test test_task_detail_lists_shared 2>&1 | tail -15`
Expected: FAIL — `connections` is empty (no local connection of type `owner.clickhouse`).

- [ ] **Step 3: Implement**

Replace the block at `tasks.rs:278-300` with:

```rust
    // Build connections map keyed by each input's `type:` AS WRITTEN (the UI
    // looks the list up by the field's own type string). Candidates are every
    // connection in any loaded workspace whose canonical type equals the
    // input's canonical type; foreign ones only when `shared`.
    let ws_set = crate::workspace_set::WorkspaceSet::load(&state.workspaces, &ws, Some(&workspace)).await;
    let mut connections: HashMap<String, Vec<String>> = HashMap::new();
    let connection_types_needed: BTreeSet<&str> = task
        .input
        .values()
        .map(|f| f.field_type.as_str())
        .filter(|t| !PRIMITIVE_TYPES.contains(t))
        .collect();

    for type_as_written in connection_types_needed {
        let Ok(field_ct) = stroem_common::template::canonical_type_ref(type_as_written, &ws, &ws_set) else {
            continue; // unresolvable type: no dropdown, job creation reports the error
        };
        let mut local: Vec<String> = Vec::new();
        let mut foreign: Vec<String> = Vec::new();
        for (ws_name, cfg) in ws_set.iter_configs() {
            let is_local = ws_name == ws;
            for (conn_name, conn) in &cfg.connections {
                let Some(ref declared) = conn.connection_type else { continue };
                let Ok(conn_ct) = stroem_common::template::canonical_type_ref(declared, ws_name, &ws_set) else { continue };
                if conn_ct != field_ct {
                    continue;
                }
                if is_local {
                    local.push(conn_name.clone());
                } else if conn.shared {
                    foreign.push(format!("{}.{}", ws_name, conn_name));
                }
            }
        }
        local.sort();
        foreign.sort();
        local.extend(foreign);
        if !local.is_empty() {
            connections.insert(type_as_written.to_string(), local);
        }
    }
```

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --test integration_test test_task_detail 2>&1 | tail -5`
Expected: the new test and the pre-existing task-detail tests pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/web/api/tasks.rs crates/stroem-server/tests/integration_test.rs
git commit -m "feat(api): list shared foreign connections in task-detail dropdown"
```

---

### Task 8: Job-detail redaction across workspaces

**Files:**
- Modify: `crates/stroem-server/src/web/api/jobs.rs:461-466` (and remove the now-unused `collect_secret_values` / `collect_strings` if nothing else uses them — check with grep)
- Test: `crates/stroem-server/tests/integration_test.rs`

**Interfaces:**
- Consumes: `WorkspaceSet::load`, `collect_redaction_values`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn test_job_detail_redacts_foreign_connection_secrets() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_shared_connections().await?;

    // use-shared resolves owner.shared-conn at creation → job.input holds
    // {"conn": {"host": "shared.host", "token": "owner-token-secret-value"}}
    let response = router
        .clone()
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/use-shared/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id = body_json(response).await["job_id"].as_str().unwrap().to_string();

    let response = router
        .oneshot(api_request("GET", &format!("/api/jobs/{job_id}"), json!({})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_json(response).await.to_string();
    assert!(!text.contains("owner-token-secret-value"), "owner's secret-marked token leaked: {text}");
    assert!(text.contains("shared.host"), "non-secret host must stay visible: {text}");
    assert!(text.contains("••••••"));
    Ok(())
}
```

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --test integration_test test_job_detail_redacts_foreign 2>&1 | tail -10`
Expected: FAIL — token present in the response.

- [ ] **Step 3: Implement**

Replace `jobs.rs:461-466`:

```rust
    // Redact secrets from EVERY loaded workspace plus values of connection
    // properties marked `secret: true` — a cross-workspace connection's values
    // are persisted in this job's input and provenance is not recoverable.
    let ws_set = crate::workspace_set::WorkspaceSet::load(
        &state.workspaces,
        &response.workspace,
        workspace.as_deref(),
    )
    .await;
    let secret_values = crate::workspace_set::collect_redaction_values(&ws_set);
    redact_response(&mut response, &secret_values);
```

(`response.workspace` is moved into the struct above; read `job.workspace` before the move or clone it — adapt to what the borrow checker needs.) If `collect_secret_values` / `collect_strings` in this file become unused, delete them and their tests, or keep them if other handlers use them (grep first).

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --test integration_test redact 2>&1 | tail -5`
Expected: new test plus all pre-existing redaction tests pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/web/api/jobs.rs crates/stroem-server/tests/integration_test.rs
git commit -m "fix(api): redact secrets of every workspace and secret-marked connection properties in job detail"
```

---

### Task 9: Integration tests for creation, gating, and claim

**Files:**
- Test: `crates/stroem-server/tests/integration_test.rs`

**Interfaces:**
- Consumes: `setup_shared_connections` (Task 7), existing helpers `api_request`, `worker_request`/claim helpers used by `test_cross_workspace_claim_returns_owner_workspace_and_revision` (~line 1764; copy its register+claim idiom).

- [ ] **Step 1: Write the tests**

```rust
#[tokio::test]
async fn test_execute_with_shared_foreign_connection_resolves_values() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/use-shared/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: Uuid = body_json(response).await["job_id"].as_str().unwrap().parse()?;
    let job = stroem_db::JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.input.unwrap()["conn"]["host"], "shared.host");
    Ok(())
}

#[tokio::test]
async fn test_execute_with_unshared_foreign_connection_is_400() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/use-private/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let text = body_json(response).await.to_string();
    assert!(text.contains("is not shared"), "{text}");
    Ok(())
}

#[tokio::test]
async fn test_execute_with_two_hop_connection_declaring_foreign_type() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/use-infra/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: Uuid = body_json(response).await["job_id"].as_str().unwrap().parse()?;
    let job = stroem_db::JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.input.unwrap()["conn"]["host"], "eu.host");
    Ok(())
}

#[tokio::test]
async fn test_execute_local_type_with_foreign_connection_is_400_mismatch() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/bad-type/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let text = body_json(response).await.to_string();
    assert!(text.contains("expects type 'caller.clickhouse'"), "{text}");
    Ok(())
}

#[tokio::test]
async fn test_execute_unknown_workspace_prefix_is_400() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request(
            "POST",
            "/api/workspaces/caller/tasks/use-shared/execute",
            json!({"input": {"conn": "nope.thing"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let text = body_json(response).await.to_string();
    assert!(text.contains("unknown workspace"), "{text}");
    Ok(())
}

#[tokio::test]
async fn test_literal_flow_step_ref_to_unshared_connection_is_400_at_creation() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/literal-bad/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_templated_flow_step_ref_fails_step_at_claim_not_creation() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_shared_connections().await?;
    // Creation succeeds: the value is a template.
    let response = router
        .clone()
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/templated/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: Uuid = body_json(response).await["job_id"].as_str().unwrap().parse()?;

    // Register a worker and claim — the claim endpoint fails the step in place
    // (`fail_claimed_step`) and answers 204 No Content because nothing is left
    // to hand out.
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "worker-xconn", "capabilities": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let worker_id = body_json(response).await["worker_id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["script"]}),
        ))
        .await?;
    assert!(
        response.status() == 200 || response.status() == 204,
        "claim status {}",
        response.status()
    );
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let run = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(run.status, "failed");
    assert!(run.error_message.as_deref().unwrap_or("").contains("is not shared"), "{:?}", run.error_message);
    Ok(())
}

#[tokio::test]
async fn test_cross_workspace_action_caller_bare_unshared_name_fails_at_claim() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_shared_connections().await?;
    let response = router
        .clone()
        .oneshot(api_request("POST", "/api/workspaces/caller/tasks/via-action/execute", json!({"input": {}})))
        .await?;
    // `private-conn` is a literal → pre-check rejects at creation.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let text = body_json(response).await.to_string();
    assert!(text.contains("'owner.private-conn' exists but is not shared"), "{text}");
    let _ = pool;
    Ok(())
}

#[tokio::test]
async fn test_cross_workspace_action_owner_default_resolves_ungated() -> Result<()> {
    // B.remote's own default `prod` (UNSHARED in B) must still resolve at claim
    // time — the owner reading its own config is not gated.
    let (router, _pool, _tmp, _container) = setup_two_workspaces().await?;

    let response = router
        .clone()
        .oneshot(api_request("POST", "/api/workspaces/A/tasks/caller/execute", json!({})))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "worker-xws-default", "capabilities": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let worker_id = body_json(response).await["worker_id"]
        .as_str()
        .unwrap()
        .to_string();

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let claim = body_json(response).await;
    assert_eq!(claim["workspace"].as_str().unwrap(), "B");
    assert_eq!(claim["input"]["conn"]["host"], "db.owner.internal");
    Ok(())
}
```

For the last test, extend `setup_two_workspaces` (the fixture at ~line 1280): after `ws_b.actions.insert("remote", ...)`, add

```rust
    ws_b.connection_types.insert(
        "pg".to_string(),
        ConnectionTypeDef { properties: HashMap::new() },
    );
    ws_b.connections.insert(
        "prod".to_string(),
        ConnectionDef {
            connection_type: Some("pg".to_string()),
            shared: false,
            values: HashMap::from([("host".to_string(), json!("db.owner.internal"))]),
        },
    );
    let mut remote_def = trivial_script_action("echo hi");
    remote_def.input.insert("conn".to_string(), {
        let mut f: InputFieldDef = serde_yaml::from_str("type: pg").unwrap();
        f.default = Some(json!("prod"));
        f
    });
    ws_b.actions.insert("remote".to_string(), remote_def);
```

(replacing the original `remote` insert). The two existing tests on that fixture do not send `conn` and must keep passing.

- [ ] **Step 2: Run the tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-server --test integration_test cross_workspace 2>&1 | tail -20` and then the full file: `... --test integration_test 2>&1 | tail -5`.
Expected: all pass. If `test_templated_flow_step_ref_fails_step_at_claim_not_creation` fails because the caller's `echo-conn` action input type `owner.clickhouse` cannot be claimed without a worker capability match, register the worker with `capabilities: ["script"]` (see the existing claim test).

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/tests/integration_test.rs
git commit -m "test(server): cross-workspace connection creation, gating, claim, and pre-check"
```

---

### Task 10: CLI `SingleWorkspace`

**Files:**
- Modify: `crates/stroem-cli/src/local/run.rs:52`, `:359`
- Test: `crates/stroem-cli/src/local/run.rs` `mod tests` (if one exists; otherwise `crates/stroem-cli/tests/`)

- [ ] **Step 1: Write the failing test**

Find how `run.rs` tests build a config (grep `mod tests` in that file). Add:

```rust
    #[test]
    fn local_run_rejects_qualified_connection_with_server_hint() {
        let yaml = r#"
connection_types:
  pg:
    host: { type: string }
tasks:
  t:
    input:
      db: { type: pg, default: other.prod }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
        let config: stroem_common::models::workflow::WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let task = &config.tasks["t"];
        let merged = stroem_common::template::merge_defaults(&serde_json::json!({}), &task.input, &serde_json::json!({})).unwrap();
        let err = stroem_common::template::resolve_connection_inputs(
            &merged,
            &task.input,
            &stroem_common::template::SingleWorkspace { name: "local", config: &config },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("require a server"), "{err:#}");
    }
```

- [ ] **Step 2: Implement**

At `run.rs:52`:

```rust
    let lookup = stroem_common::template::SingleWorkspace { name: "local", config: &config };
    let resolved_input = resolve_connection_inputs(&merged_input, &task.input, &lookup)
        .context("Failed to resolve connection inputs")?;
```

At `run.rs:359` (inside the step executor, which receives `config: &WorkspaceConfig`):

```rust
    let lookup = stroem_common::template::SingleWorkspace { name: "local", config };
    let action_input = prepare_action_input(&rendered_input, &action.input, &lookup)
        .with_context(|| format!("Step '{}': failed to prepare action input", step_name))?;
```

- [ ] **Step 3: Run tests**

Run: `CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target cargo test -p stroem-cli 2>&1 | tail -5`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/stroem-cli
git commit -m "feat(cli): resolve connections through SingleWorkspace; clear error for qualified refs"
```

---

### Task 11: E2E scenario

**Files:**
- Create: `tests/e2e-workspace/connections.yaml`
- Create: `workspace/.workflows/xref-conn.yaml`
- Modify: `tests/e2e.sh` (append a section 18 after the xref section ending ~line 610, before `# --- Summary ---`)

- [ ] **Step 1: Fixtures**

`tests/e2e-workspace/connections.yaml` (the `test` workspace in docker-compose):

```yaml
connection_types:
  demo:
    host:
      type: string
      required: true
    token:
      type: string
      secret: true

connections:
  demo-shared:
    type: demo
    shared: true
    host: SHARED_CONN_OK
    token: e2e-demo-secret-token-value
  demo-private:
    type: demo
    host: PRIVATE_HOST
    token: e2e-private-token
```

`workspace/.workflows/xref-conn.yaml` (the `default` workspace):

```yaml
# Cross-workspace CONNECTION reference: the `test` workspace owns the `demo`
# type and the `demo-shared` connection. This workspace defines neither.
actions:
  echo-conn:
    type: script
    runner: local
    input:
      conn:
        type: test.demo
    script: |
      echo "HOST={{ input.conn.host }}"

tasks:
  xref-conn:
    mode: distributed
    input:
      conn:
        type: test.demo
        default: test.demo-shared
    flow:
      run:
        action: echo-conn
        input:
          conn: "{{ input.conn }}"
```

- [ ] **Step 2: e2e.sh section**

Insert before `# --- Summary ---`:

```bash
# --- 18. Cross-workspace connection reference (shared flag) ---
# xref-conn (in "default") declares `type: test.demo` and defaults to
# `test.demo-shared`; neither the type nor the connection exists in "default".
info "Triggering xref-conn task (cross-workspace shared connection)..."
EXEC_RESP_XCONN=$(acurl -X POST "$BASE_URL/api/workspaces/default/tasks/xref-conn/execute" \
    -H "Content-Type: application/json" \
    -d '{"input": {}}')
XCONN_JOB_ID=$(echo "$EXEC_RESP_XCONN" | jq -r '.job_id')
if [ -z "$XCONN_JOB_ID" ] || [ "$XCONN_JOB_ID" = "null" ]; then
    fail "xref-conn execute failed: $EXEC_RESP_XCONN"
fi
pass "xref-conn job created: $XCONN_JOB_ID"

XCONN_POLLED=0
XCONN_STATUS="pending"
while [ "$XCONN_STATUS" != "completed" ] && [ "$XCONN_STATUS" != "failed" ]; do
    sleep 2
    XCONN_POLLED=$((XCONN_POLLED + 2))
    if [ "$XCONN_POLLED" -ge "$MAX_POLL" ]; then
        acurl "$BASE_URL/api/jobs/$XCONN_JOB_ID" | jq .
        fail "xref-conn job did not reach terminal state within ${MAX_POLL}s"
    fi
    XCONN_DETAIL=$(acurl "$BASE_URL/api/jobs/$XCONN_JOB_ID")
    XCONN_STATUS=$(echo "$XCONN_DETAIL" | jq -r '.status')
    printf "."
done
echo ""
if [ "$XCONN_STATUS" != "completed" ]; then
    echo "$XCONN_DETAIL" | jq .
    fail "xref-conn job failed"
fi
pass "xref-conn job completed (${XCONN_POLLED}s)"

XCONN_LOGS=$(acurl "$BASE_URL/api/jobs/$XCONN_JOB_ID/logs" | jq -r '.logs')
if echo "$XCONN_LOGS" | grep -q "HOST=SHARED_CONN_OK"; then
    pass "xref-conn resolved test.demo-shared in the owner workspace"
else
    echo "$XCONN_LOGS"
    fail "xref-conn logs missing HOST=SHARED_CONN_OK"
fi

if echo "$XCONN_DETAIL" | grep -q "e2e-demo-secret-token-value"; then
    echo "$XCONN_DETAIL" | jq .
    fail "job detail leaked the owner's secret-marked connection token"
else
    pass "job detail redacts the owner's secret-marked token"
fi

info "Triggering xref-conn with an UNSHARED foreign connection (expect 400)..."
# plain curl: `acurl` passes -f, which suppresses output on a 4xx.
XCONN_PRIV_CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -X POST \
    "$BASE_URL/api/workspaces/default/tasks/xref-conn/execute" \
    -H "Content-Type: application/json" \
    -d '{"input": {"conn": "test.demo-private"}}')
if [ "$XCONN_PRIV_CODE" = "400" ]; then
    pass "unshared foreign connection rejected with 400"
else
    fail "expected 400 for unshared foreign connection, got $XCONN_PRIV_CODE"
fi
```

(`acurl` is defined at `tests/e2e.sh:84` as `curl -sf -H "Authorization: Bearer $TOKEN" "$@"`.)

- [ ] **Step 3: Run**

Run: `./tests/e2e.sh 2>&1 | tail -30` (needs Docker; if the host has no space for the image build, run the section logic against a locally started server + worker instead and note that in the commit message).
Expected: sections 17 and 18 pass.

- [ ] **Step 4: Commit**

```bash
git add tests/e2e-workspace/connections.yaml workspace/.workflows/xref-conn.yaml tests/e2e.sh
git commit -m "test(e2e): cross-workspace shared connection scenario"
```

---

### Task 12: Documentation

**Files:**
- Modify: `docs/src/content/docs/guides/cross-workspace-references.md` (new "Connections" section; shrink "Not yet supported")
- Modify: `docs/src/content/docs/guides/connections.md` (`shared`, qualified `type:`, dropdown, redaction note)
- Modify: `CLAUDE.md` §Cross-Workspace References and §Connections
- Modify: `docs/internal/TODO.md` (tick / add entries)
- Modify: `docs/superpowers/specs/2026-09-06-cross-workspace-connections-design.md` (§4.2 → "all loaded configs"; status → Implemented)

- [ ] **Step 1: cross-workspace-references.md**

Add after "## Owner-context execution":

````markdown
## Connections

A connection-typed input may name another workspace's connection directly,
and a type may be another workspace's type. Both use the same `workspace.name`
addressing and are independent of each other:

```yaml
# ai_traffic_model — no ClickHouse type, connection, or secret defined here
tasks:
  daily:
    input:
      clickhouse:
        type: jobs.clickhouse          # the TYPE lives in workspace `jobs`
        default: jobs.clickhouse-prod  # the CONNECTION lives in `jobs` too
    flow:
      run:
        action: score-all              # a LOCAL action
        input:
          clickhouse: "{{ input.clickhouse }}"
```

### `shared: true`

A connection can be referenced from another workspace **only** if its owner
marks it shared:

```yaml
# jobs workspace
connections:
  clickhouse-prod:
    type: clickhouse
    shared: true
    host: ch.internal
    password: "{{ secret.ch_password }}"
```

Unshared connections are private to their workspace. A reference to one from
elsewhere fails at job creation with `400 Bad Request` and a message ending in
`is not shared`. Inside its own workspace the flag is ignored.

### How types match

Every type reference is normalised to `(workspace, type)`. A bare name means
the workspace the YAML is written in, so `type: clickhouse` in `ai_traffic_model`
is a *different* type from `type: clickhouse` in `jobs`, even if both exist.
A connection satisfies an input only when both resolve to the same pair.
Consequences:

| Input declares          | Connection value       | Connection's own `type:` | Result |
|-------------------------|------------------------|--------------------------|--------|
| `jobs.clickhouse`       | `jobs.clickhouse-prod` | `clickhouse` (in `jobs`) | OK if shared |
| `jobs.clickhouse`       | `infra.ch-eu`          | `jobs.clickhouse`        | OK if shared — type and connection in different workspaces |
| `clickhouse` (local)    | `jobs.clickhouse-prod` | `clickhouse` (in `jobs`) | Rejected: `caller.clickhouse ≠ jobs.clickhouse` |
| `jobs.clickhouse`       | `local-ch` (local)     | `jobs.clickhouse`        | OK — a local connection may adopt a foreign type |

Untyped connections (no `type:`) match any input type, as they do locally.

A connection whose `type:` is in another workspace gets that type's property
defaults applied and its required fields checked when the job is created (the
owner's load-time validation never saw the type).

### Cross-workspace actions and the `shared` gate

On a step whose action is `owner.action`, connection names the **caller**
supplies resolve in the caller first and then, if not found, in the owner —
but only shared ones. Names that come from the owner action's own `default:`
resolve in the owner without the gate. Before this release the caller could
name any owner connection bare; now the owner must mark it `shared: true` or
the step fails with `... exists but is not shared`.

### Dropdown

The task form lists every eligible connection: local ones by bare name, then
shared foreign ones as `workspace.name`.

### Redaction and visibility

Resolved connection values are stored with the job. Job detail redacts every
workspace's `secrets` values and every connection property whose type marks it
`secret: true`. Everything else in a shared connection is visible to anyone
with View permission on a task that uses it, in any workspace — mark
credentials `secret: true` in the connection type.

### Offline CLI

`stroem validate` warns on qualified type references (they are validated at job
creation). `stroem run` cannot resolve a qualified connection and fails with
`cross-workspace connection references require a server`.
````

Update "## Not yet supported": remove the two connection bullets; keep task actions, agent steps, hook actions.

Update the "## Open access" paragraph: replace the sentence about connection references being an ungated capability with a pointer to the `shared` flag.

- [ ] **Step 2: connections.md**

Under "## Connections" add a `shared: true` example and one sentence; under "## Using Connections as Task Inputs" add a "Cross-workspace" subsection linking to the guide above; under "## Validation" note that dotted types are warnings offline.

- [ ] **Step 3: CLAUDE.md**

In "### Cross-Workspace References":
- Replace the "**Open access**" bullet with: "**Actions**: open, no ACL. **Connections**: gated per connection by `shared: true` (`ConnectionDef.shared`); unshared foreign references are 400 at creation / failed step at claim."
- Add a bullet: "**Connections & types**: `ws.conn` and `type: ws.type` are independently addressable; matched by canonical `(workspace, type)` pair (`template::canonical_type_ref`). Resolver: `resolve_connection_inputs_scoped` + `ResolveScope`; server lookup `workspace_set::WorkspaceSet` (all loaded configs); CLI `SingleWorkspace`. Provenance-aware two-pass for cross-workspace actions: `prepare_action_input_cross` (caller-supplied names → caller, then owner-if-shared; owner defaults → ungated). Literal flow-step values pre-checked at creation (`job_creator::precheck_literal_connection_inputs`); templated ones fail the step at claim."
- Add: "**Redaction**: job detail masks all workspaces' secrets + `secret: true` connection properties (`workspace_set::collect_redaction_values`)."
- Remove from "**Deferred**": the qualified connection and connection-type items.

In "### Connections": add "`shared: bool` (default false) — opt-in for cross-workspace use."

- [ ] **Step 4: TODO.md + spec**

`grep -n "cross-workspace\|Cross-workspace\|qualified connection" docs/internal/TODO.md`; tick entries covering qualified connection / connection-type references; add an unticked "Cross-workspace hook actions / agent steps / `type: task`" if absent.

In the spec: set `**Status:** Implemented (2026-09-XX)`; rewrite §4.2's first paragraph to say `WorkspaceSet::load` snapshots every healthy workspace via `get_all_configs()` and there is no prefix scan.

- [ ] **Step 5: Regenerate llms.txt and commit**

```bash
cd docs && bun run generate-llms && cd ..
git add docs CLAUDE.md
git commit -m "docs: cross-workspace connections, shared flag, canonical type matching"
```

---

### Task 13: Full verification

- [ ] **Step 1: Format, lint, tests**

```bash
export CARGO_TARGET_DIR=/Users/ala/workspace/fremvaerk/stroem/target
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cargo test --workspace 2>&1 | tail -30
```

Expected: clean, all green. `log_storage::tests` is known-flaky under parallel runs (re-run that module alone if it fails).

- [ ] **Step 2: Frontend unchanged — sanity**

```bash
cd ui && bunx tsc --noEmit && bun run lint && cd ..
```

Expected: clean (no UI changes, this just proves the API shape is still consumed correctly by the existing types).

- [ ] **Step 3: Final commit if anything moved**

```bash
git status
git log --oneline main..HEAD
```

Expected: ~12 commits on `worktree-cross-workspace-connections`, clean tree.
