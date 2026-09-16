# Cross-Workspace `type: task` Actions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A flow step whose action is `type: task` can name a task in another workspace (`task: B.deploy`, or `action: B.run-deploy` where B's action is `type: task`), and the child job runs as a real job of the task's owner workspace while its result propagates back to the caller's step.

**Architecture:** Resolution happens at dispatch from columns the step row already carries (`action_workspace`, `action_spec`): the task owner `T` is resolved relative to the action owner `O` (= `action_workspace` ?? job workspace) by one function, `job_creator::resolve_task_ref`; the child is created with `T`'s config snapshot and current revision. Connection inputs are resolved once, against the **task's** schema, by provenance (caller bucket in `A`→`T`-if-shared, action-default bucket in `O`→`T`-if-shared), with non-string values refused across a boundary. Error text is scrubbed with all three workspaces' secrets by span-union masking. No migration; the dispatch/propagation lifecycle is deliberately untouched (spec § 4).

**Tech Stack:** Rust (anyhow, tokio, sqlx runtime queries, serde_json, Tera via `stroem_common::template`), Axum, React 19 + TypeScript (bun), testcontainers Postgres for integration tests.

**Spec:** `docs/superpowers/specs/2026-09-16-cross-workspace-task-actions-design.md` (revision 5, Codex sign-off). Read it first; every task below cites its section.

## Global Constraints

- No database migration. `job_step.action_workspace` / `action_revision` keep their meaning (the *action's* owner); nothing new is stamped on rows.
- `anyhow::Result` everywhere; add context with `.context("…")` / `.with_context(|| …)`.
- sqlx **runtime** queries (`sqlx::query` / `query_as`), never compile-time macros.
- Any struct that can carry a rendered secret is wrapped in `stroem_common::secret::Secret<T>`; never put a whole request/step struct into an `#[instrument]` span.
- Every error phrase that must reach `web/api/mod.rs::classify_execute_error` stays in the **outermost** message: `"unknown workspace"` / `"is not shared"` / `"has no connection"` / `"is not available"` are matched anywhere in the chain; `"has no task"` (new), `"has no action"`, `"not found"`, `"resolve connection"`, `"invalid"` only on the outermost message.
- Commits: conventional prefix (`feat:`, `fix:`, `test:`, `docs:`), imperative, **no AI co-author trailer** (user rule).
- Before each commit that touches Rust: `cargo fmt --all` and `cargo clippy -p <crate> --all-targets -- -D warnings`; run tests per crate (`cargo test -p stroem-common`, `cargo test -p stroem-server --lib`, `cargo test -p stroem-server --test integration_test <filter>`); the full `cargo test --workspace` is unreliable on this host under disk pressure — never override `CARGO_TARGET_DIR` (global target dir is `~/.tmp/cargo`).
- Integration tests need Docker: `export DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true`; if `orb status` says Stopped, run `orb start`.
- UI: `cd ui && bun run lint && bunx tsc --noEmit` before committing TS.
- Work happens in the worktree `.claude/worktrees/cross-workspace-task-actions`, branch `worktree-cross-workspace-task-actions`.

---

### Task 1: Span-union secret masking in `redact_secrets_in_str`

Spec § 3.3 "Error scrubbing", § 6 first bullet.

**Files:**
- Modify: `crates/stroem-server/src/workspace_set.rs:110-133` (`REDACTED`, `redact_secrets_in_str`)
- Test: `crates/stroem-server/src/workspace_set.rs` (`mod tests`, after `redact_secrets_in_str_ignores_empty_secret_values`, ~:279)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub fn redact_secrets_in_str(s: &str, secret_values: &[String]) -> String` — same signature, new semantics: every occurrence (overlapping included) of every non-empty value is located in the ORIGINAL text, intersecting/adjacent spans are merged, each merged span becomes one `REDACTED`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `crates/stroem-server/src/workspace_set.rs`:

```rust
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
        // The mask in the output is never rescanned; the literal value is masked once.
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib workspace_set::tests::redact_ -- --nocapture`
Expected: `redact_crossing_occurrences_leave_no_fragment`, `redact_self_overlapping_value_covers_every_byte`, `redact_adjacent_occurrences_merge`, `redact_equal_length_overlap` FAIL (fragments left); the three pre-existing `redact_secrets_in_str_*` tests still PASS.

- [ ] **Step 3: Replace the implementation**

Replace the body of `redact_secrets_in_str` (`workspace_set.rs:122-133`) with:

```rust
pub fn redact_secrets_in_str(s: &str, secret_values: &[String]) -> String {
    // Collect every occurrence — overlapping ones included — against the
    // ORIGINAL text. Replacing sequentially against already-modified text
    // leaves fragments whenever two occurrences intersect.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for secret in secret_values {
        if secret.is_empty() {
            continue;
        }
        let mut from = 0usize;
        while from <= s.len() {
            let Some(rel) = s[from..].find(secret.as_str()) else {
                break;
            };
            let begin = from + rel;
            spans.push((begin, begin + secret.len()));
            // Advance by one character so an occurrence that starts inside
            // this one is still found.
            let step = s[begin..].chars().next().map_or(1, char::len_utf8);
            from = begin + step;
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
```

Update the doc comment above it: replace "Replace every occurrence of a known secret value in `s` with [`REDACTED`]." with "Mask every occurrence of every known secret value in `s`. Occurrences are found in the original text (overlapping ones included) and intersecting or adjacent spans are merged, so two values whose occurrences cross — or one value overlapping itself — leave no legible fragment."

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib workspace_set::tests -- --nocapture`
Expected: all `redact_*` tests PASS, existing `workspace_set` tests unchanged.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/workspace_set.rs
git commit -m "fix(redaction): mask the union of every secret occurrence, not sequential replacements"
```

---

### Task 2: `resolve_task_input_by_provenance` with the boundary rule

Spec § 3.3 step 5, § 6 first bullet.

**Files:**
- Modify: `crates/stroem-common/src/template.rs` — add after `prepare_action_input_cross` (ends ~:891)
- Test: `crates/stroem-common/src/template.rs` `mod tests` (reuse `MultiWs`, `conn`, `empty_type` helpers at ~:1657-1700)

**Interfaces:**
- Consumes: `resolve_connection_inputs_scoped`, `ResolveScope`, `WorkspaceLookup`, `PRIMITIVE_TYPES` (all in this file).
- Produces:
  ```rust
  pub fn resolve_task_input_by_provenance(
      caller_input: &serde_json::Value,     // bucket C (what the caller's flow step supplied)
      action_defaults: &serde_json::Value,  // bucket D (keys ADDED by merge_action_defaults; disjoint from C)
      task_schema: &HashMap<String, InputFieldDef>,
      lookup: &dyn WorkspaceLookup,
      caller_ws: &str,   // A
      action_ws: &str,   // O
      task_ws: &str,     // T
  ) -> Result<serde_json::Value>
  ```
  Returns C ∪ D with connection-typed fields resolved; C wins on a key collision.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `template.rs`:

```rust
    /// A: connection `shared-a` (untyped, shared) and `mine` (type pg, not shared).
    /// O: connection `o-private` (type pg, not shared), `o-shared` (type pg, shared).
    /// T: type `pg`, type `redis`; connections `t-shared` (pg, shared),
    ///    `t-private` (pg, not shared), `t-redis` (redis, shared), `t-untyped` (no type, shared).
    fn provenance_workspaces() -> MultiWs {
        let mut a = WorkspaceConfig::default();
        a.connection_types.insert("pg".to_string(), empty_type());
        a.connections.insert("mine".to_string(), conn(Some("pg"), false, "a.mine"));
        a.connections.insert("shared-a".to_string(), conn(None, true, "a.shared"));
        let mut o = WorkspaceConfig::default();
        o.connection_types.insert("pg".to_string(), empty_type());
        o.connections.insert("o-private".to_string(), conn(Some("pg"), false, "o.private"));
        o.connections.insert("o-shared".to_string(), conn(Some("pg"), true, "o.shared"));
        let mut t = WorkspaceConfig::default();
        t.connection_types.insert("pg".to_string(), empty_type());
        t.connection_types.insert("redis".to_string(), empty_type());
        t.connections.insert("t-shared".to_string(), conn(Some("pg"), true, "t.shared"));
        t.connections.insert("t-private".to_string(), conn(Some("pg"), false, "t.private"));
        t.connections.insert("t-redis".to_string(), conn(Some("redis"), true, "t.redis"));
        t.connections.insert("t-untyped".to_string(), conn(None, true, "t.untyped"));
        MultiWs {
            local: "A".to_string(),
            configs: HashMap::from([
                ("A".to_string(), a),
                ("O".to_string(), o),
                ("T".to_string(), t),
            ]),
            unavailable: vec![],
        }
    }

    fn pg_schema() -> HashMap<String, InputFieldDef> {
        let f: InputFieldDef = serde_yaml::from_str("type: pg").unwrap();
        HashMap::from([("db".to_string(), f)])
    }

    #[test]
    fn provenance_caller_value_found_in_caller_first() {
        let ws = provenance_workspaces();
        let out = resolve_task_input_by_provenance(
            &json!({"db": "mine"}), &json!({}), &pg_schema(), &ws, "A", "A", "T",
        )
        .unwrap();
        assert_eq!(out["db"]["host"], "a.mine");
    }

    #[test]
    fn provenance_caller_bare_name_falls_back_to_shared_task_owner() {
        let ws = provenance_workspaces();
        let out = resolve_task_input_by_provenance(
            &json!({"db": "t-shared"}), &json!({}), &pg_schema(), &ws, "A", "A", "T",
        )
        .unwrap();
        assert_eq!(out["db"]["host"], "t.shared");
    }

    #[test]
    fn provenance_caller_bare_name_to_unshared_task_owner_is_rejected() {
        let ws = provenance_workspaces();
        let err = resolve_task_input_by_provenance(
            &json!({"db": "t-private"}), &json!({}), &pg_schema(), &ws, "A", "A", "T",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");
    }

    #[test]
    fn provenance_action_default_resolves_ungated_in_action_owner() {
        let ws = provenance_workspaces();
        let out = resolve_task_input_by_provenance(
            &json!({}), &json!({"db": "o-private"}), &pg_schema(), &ws, "A", "O", "T",
        )
        .unwrap();
        assert_eq!(out["db"]["host"], "o.private");
    }

    #[test]
    fn provenance_action_default_naming_task_owner_is_shared_gated() {
        let ws = provenance_workspaces();
        let ok = resolve_task_input_by_provenance(
            &json!({}), &json!({"db": "t-shared"}), &pg_schema(), &ws, "A", "O", "T",
        )
        .unwrap();
        assert_eq!(ok["db"]["host"], "t.shared");
        let err = resolve_task_input_by_provenance(
            &json!({}), &json!({"db": "t-private"}), &pg_schema(), &ws, "A", "O", "T",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");
    }

    #[test]
    fn provenance_declared_type_mismatch_is_rejected() {
        let ws = provenance_workspaces();
        let err = resolve_task_input_by_provenance(
            &json!({"db": "t-redis"}), &json!({}), &pg_schema(), &ws, "A", "A", "T",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("expects type"), "{err:#}");
    }

    #[test]
    fn provenance_named_untyped_connection_is_accepted() {
        let ws = provenance_workspaces();
        let out = resolve_task_input_by_provenance(
            &json!({"db": "t-untyped"}), &json!({}), &pg_schema(), &ws, "A", "A", "T",
        )
        .unwrap();
        assert_eq!(out["db"]["host"], "t.untyped");
    }

    #[test]
    fn provenance_boundary_rule_refuses_non_strings_per_bucket() {
        let ws = provenance_workspaces();
        for bad in [json!({"host": "x"}), json!([1]), json!(1), json!(true), json!(null)] {
            // Caller bucket foreign (A != T), default bucket local (O == T).
            let err = resolve_task_input_by_provenance(
                &json!({"db": bad}), &json!({}), &pg_schema(), &ws, "A", "T", "T",
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("must be a connection name"),
                "caller bucket, value {bad}: {err:#}"
            );
            // Default bucket foreign (O != T), caller bucket local (A == T).
            let err = resolve_task_input_by_provenance(
                &json!({}), &json!({"db": bad}), &pg_schema(), &ws, "T", "O", "T",
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("must be a connection name"),
                "default bucket, value {bad}: {err:#}"
            );
        }
    }

    #[test]
    fn provenance_local_bucket_keeps_object_passthrough_and_scalar_rejection() {
        let ws = provenance_workspaces();
        // A == T: an object passes through untouched (inherited behaviour) …
        let out = resolve_task_input_by_provenance(
            &json!({"db": {"host": "pre-resolved"}}), &json!({}), &pg_schema(), &ws, "T", "T", "T",
        )
        .unwrap();
        assert_eq!(out["db"]["host"], "pre-resolved");
        // … while an array / scalar still fails with the resolver's own message.
        let err = resolve_task_input_by_provenance(
            &json!({"db": 5}), &json!({}), &pg_schema(), &ws, "T", "T", "T",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("expects a connection name"), "{err:#}");
    }

    #[test]
    fn provenance_all_local_equals_plain_resolution() {
        let ws = provenance_workspaces();
        let by_prov = resolve_task_input_by_provenance(
            &json!({"db": "t-private"}), &json!({}), &pg_schema(), &ws, "T", "T", "T",
        )
        .unwrap();
        let plain = resolve_connection_inputs_scoped(
            &json!({"db": "t-private"}),
            &pg_schema(),
            &ResolveScope { lookup: &ws, schema_ws: "T", value_ws: "T", fallback_ws: None },
        )
        .unwrap();
        assert_eq!(by_prov, plain);
    }

    #[test]
    fn provenance_caller_key_wins_over_default_key() {
        let ws = provenance_workspaces();
        let out = resolve_task_input_by_provenance(
            &json!({"db": "mine", "x": 1}), &json!({"db": "o-private", "y": 2}), &pg_schema(), &ws, "A", "O", "T",
        )
        .unwrap();
        assert_eq!(out["db"]["host"], "a.mine");
        assert_eq!(out["x"], 1);
        assert_eq!(out["y"], 2);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-common --lib template::tests::provenance_`
Expected: compile error `cannot find function resolve_task_input_by_provenance`.

- [ ] **Step 3: Implement the helper**

Add after `prepare_action_input_cross` in `template.rs`:

```rust
/// Resolve a `type: task` child's connection-typed inputs against the TASK's
/// schema, by provenance. `caller_input` was supplied by the caller's flow
/// step (workspace `caller_ws`); `action_defaults` are the keys the action's
/// own `input` defaults added (workspace `action_ws`); the task lives in
/// `task_ws`. Each bucket resolves a bare name in the workspace whose YAML
/// wrote it first, then in `task_ws` only if the connection is `shared`.
///
/// Boundary rule: a value crossing a workspace boundary into a
/// connection-typed field must be a connection NAME. Objects are refused
/// there because `resolve_connection_inputs_scoped` passes any object
/// through unchecked; within one workspace that pass-through is unchanged.
pub fn resolve_task_input_by_provenance(
    caller_input: &serde_json::Value,
    action_defaults: &serde_json::Value,
    task_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
    caller_ws: &str,
    action_ws: &str,
    task_ws: &str,
) -> Result<serde_json::Value> {
    let caller = resolve_provenance_bucket(caller_input, task_schema, lookup, caller_ws, task_ws)?;
    let defaults =
        resolve_provenance_bucket(action_defaults, task_schema, lookup, action_ws, task_ws)?;
    let mut out = caller.as_object().cloned().unwrap_or_default();
    if let Some(d) = defaults.as_object() {
        for (k, v) in d {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    Ok(serde_json::Value::Object(out))
}

fn resolve_provenance_bucket(
    input: &serde_json::Value,
    task_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
    value_ws: &str,
    task_ws: &str,
) -> Result<serde_json::Value> {
    if value_ws != task_ws {
        if let Some(map) = input.as_object() {
            for (field, def) in task_schema {
                if PRIMITIVE_TYPES.contains(&def.field_type.as_str()) {
                    continue;
                }
                if let Some(v) = map.get(field) {
                    if !v.is_string() {
                        bail!(
                            "input '{}': a connection passed across workspaces must be a connection name, got {}",
                            field,
                            json_type_name(v)
                        );
                    }
                }
            }
        }
    }
    resolve_connection_inputs_scoped(
        input,
        task_schema,
        &ResolveScope {
            lookup,
            schema_ws: task_ws,
            value_ws,
            fallback_ws: if value_ws == task_ws { None } else { Some(task_ws) },
        },
    )
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-common --lib template::tests::provenance_`
Expected: 11 PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-common --all-targets -- -D warnings
git add crates/stroem-common/src/template.rs
git commit -m "feat(template): resolve task-child connection inputs by provenance with a cross-workspace boundary rule"
```

---

### Task 3: Validation — `CrossWorkspaceResolver` trait, dotted `task:` refs, hook rejection

Spec § 5.

**Files:**
- Modify: `crates/stroem-common/src/validation.rs:17-19` (alias → trait), `:46-51` (public fn), `:53-78` (type: task check), `:85-91` and `:337-370`, `:519-551` (hook call sites), `:1824-1836` (`validate_hook_action_exists`), `:130-149` (closure call → `has_action`)
- Test: `crates/stroem-common/src/validation.rs` `mod tests` (existing `test_validate_with_resolver_*` at ~:5140-5185 must be updated to the trait)

**Interfaces:**
- Produces:
  ```rust
  pub trait CrossWorkspaceResolver {
      fn has_action(&self, workspace: &str, action: &str) -> bool;
      fn has_task(&self, workspace: &str, task: &str) -> bool;
  }
  pub fn validate_workflow_config_with_cross_workspace_resolver(
      config: &WorkspaceConfig,
      resolver: &dyn CrossWorkspaceResolver,
  ) -> Result<Vec<String>>
  ```
- Warning text (CLI, `libraries_resolved == false`): `Action '{name}' references task '{ref}' outside this workspace - cannot validate cross-workspace task reference offline` and for hooks `{label} uses action '{a}' whose task '{ref}' is not local - cannot validate offline`.
- Error text (server): `action '{name}': workspace '{ws}' has no task '{task}'`; malformed: `Action '{name}' references malformed task '{ref}' (empty workspace or task name)`; hook: `{label} uses action '{a}' whose task '{ref}' is in another workspace; hook actions cannot call tasks across workspaces`.

- [ ] **Step 1: Write the failing tests**

Replace the two existing resolver tests' closure with a struct, and add new tests, inside `mod tests` of `validation.rs`:

```rust
    struct FakeResolver {
        actions: Vec<(&'static str, &'static str)>,
        tasks: Vec<(&'static str, &'static str)>,
    }
    impl CrossWorkspaceResolver for FakeResolver {
        fn has_action(&self, ws: &str, a: &str) -> bool {
            self.actions.iter().any(|(w, x)| *w == ws && *x == a)
        }
        fn has_task(&self, ws: &str, t: &str) -> bool {
            self.tasks.iter().any(|(w, x)| *w == ws && *x == t)
        }
    }
```

In `test_validate_with_resolver_accepts_cross_workspace_action` replace `let resolver = |ws: &str, action: &str| ws == "B" && action == "remote";` with `let resolver = FakeResolver { actions: vec![("B", "remote")], tasks: vec![] };` and in `..._rejects_unknown_cross_workspace_action` use `FakeResolver { actions: vec![], tasks: vec![] }`. Then add:

```rust
    const DOTTED_TASK_YAML: &str = r#"
actions:
  run-remote:
    type: task
    task: B.deploy
tasks:
  caller:
    flow:
      go:
        action: run-remote
"#;

    #[test]
    fn dotted_task_ref_offline_is_a_warning_not_an_error() {
        let config: WorkspaceConfig = serde_yaml::from_str(DOTTED_TASK_YAML).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("cannot validate cross-workspace task reference offline")),
            "{warnings:?}"
        );
    }

    #[test]
    fn dotted_task_ref_server_accepts_when_resolver_has_task() {
        let config: WorkspaceConfig = serde_yaml::from_str(DOTTED_TASK_YAML).unwrap();
        let resolver = FakeResolver { actions: vec![], tasks: vec![("B", "deploy")] };
        assert!(validate_workflow_config_with_cross_workspace_resolver(&config, &resolver).is_ok());
    }

    #[test]
    fn dotted_task_ref_server_rejects_when_resolver_lacks_task() {
        let config: WorkspaceConfig = serde_yaml::from_str(DOTTED_TASK_YAML).unwrap();
        let resolver = FakeResolver { actions: vec![], tasks: vec![] };
        let err = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "action 'run-remote': workspace 'B' has no task 'deploy'");
    }

    #[test]
    fn dotted_task_ref_local_flattened_key_wins_over_split() {
        let yaml = r#"
actions:
  run-remote:
    type: task
    task: common.deploy
tasks:
  common.deploy:
    flow:
      a:
        action: run-remote
  caller:
    flow:
      go:
        action: run-remote
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        // No resolver at all: the flattened key is found locally, never split.
        assert!(validate_workflow_config_with_libraries(&config).is_ok());
    }

    #[test]
    fn dotted_task_ref_with_empty_side_is_an_error_in_both_modes() {
        for bad in [".deploy", "B."] {
            let yaml = format!(
                "actions:\n  run-remote:\n    type: task\n    task: \"{bad}\"\ntasks:\n  caller:\n    flow:\n      go:\n        action: run-remote\n"
            );
            let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
            let err = validate_workflow_config(&config).unwrap_err().to_string();
            assert!(err.contains("malformed task"), "{bad}: {err}");
            let resolver = FakeResolver { actions: vec![], tasks: vec![] };
            let err = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver)
                .unwrap_err()
                .to_string();
            assert!(err.contains("malformed task"), "{bad}: {err}");
        }
    }

    const HOOK_WITH_DOTTED_TASK_YAML: &str = r#"
actions:
  notify:
    type: task
    task: B.notify
  greet:
    type: script
    script: "echo hi"
tasks:
  deploy:
    flow:
      a:
        action: greet
    on_error:
      - action: notify
"#;

    #[test]
    fn hook_with_qualified_task_is_rejected_on_server() {
        let config: WorkspaceConfig = serde_yaml::from_str(HOOK_WITH_DOTTED_TASK_YAML).unwrap();
        let resolver = FakeResolver { actions: vec![], tasks: vec![("B", "notify")] };
        let err = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("hook actions cannot call tasks across workspaces"),
            "{err}"
        );
    }

    #[test]
    fn hook_with_qualified_task_is_a_warning_offline() {
        let config: WorkspaceConfig = serde_yaml::from_str(HOOK_WITH_DOTTED_TASK_YAML).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("cannot validate offline")),
            "{warnings:?}"
        );
    }

    #[test]
    fn hook_with_unresolved_library_task_offline_stays_a_warning() {
        // `common.notify` might be a library task the CLI cannot see. Must not fail.
        let yaml = HOOK_WITH_DOTTED_TASK_YAML.replace("B.notify", "common.notify");
        let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(validate_workflow_config(&config).is_ok());
    }

    #[test]
    fn missing_bare_task_ref_is_still_an_error_offline() {
        let yaml = DOTTED_TASK_YAML.replace("B.deploy", "deploy");
        let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = validate_workflow_config(&config).unwrap_err().to_string();
        assert!(err.contains("references non-existent task 'deploy'"), "{err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-common --lib validation::tests::dotted_task_ref validation::tests::hook_with validation::tests::missing_bare`
Expected: compile error (`CrossWorkspaceResolver` not found).

- [ ] **Step 3: Implement**

(a) `validation.rs:17-19` — replace the alias:

```rust
/// Resolves cross-workspace references for server-side validation:
/// `(workspace, name) -> exists`. The server implements it over its loaded
/// workspace configs; the CLI has no resolver and warns instead.
pub trait CrossWorkspaceResolver {
    fn has_action(&self, workspace: &str, action: &str) -> bool;
    fn has_task(&self, workspace: &str, task: &str) -> bool;
}
```

(b) `:46-57` — change the parameter types: `resolve_cross_workspace_action: CrossWorkspaceActionResolver` → `resolver: &dyn CrossWorkspaceResolver`, and in `validate_workflow_config_inner` `resolve_cross_workspace_action: Option<CrossWorkspaceActionResolver>` → `resolver: Option<&dyn CrossWorkspaceResolver>`. At `:134-137` replace `(Some(ws), Some(resolver)) => { if resolver(ws, bare_action) {` with `(Some(ws), Some(resolver)) => { if resolver.has_action(ws, bare_action) {`.

(c) `:64-78` — replace the `type: task` block:

```rust
        // For type: task, verify the referenced task exists — locally (incl.
        // library-flattened keys), or in another workspace via the resolver.
        if action.action_type == "task" {
            let task_ref = action.task.as_ref().expect(
                "task field is required for action_type == task, enforced by validate_action",
            );
            if config.tasks.contains_key(task_ref) {
                // local or library-flattened key
            } else if let (Some(ws), name) = crate::template::parse_qualified_ref(task_ref) {
                if ws.is_empty() || name.is_empty() {
                    bail!(
                        "Action '{}' references malformed task '{}' (empty workspace or task name)",
                        action_name,
                        task_ref
                    );
                }
                match (libraries_resolved, resolver) {
                    (false, _) => warnings.push(format!(
                        "Action '{}' references task '{}' outside this workspace - cannot validate cross-workspace task reference offline",
                        action_name, task_ref
                    )),
                    (true, Some(r)) if r.has_task(ws, name) => {}
                    (true, Some(_)) => bail!(
                        "action '{}': workspace '{}' has no task '{}'",
                        action_name,
                        ws,
                        name
                    ),
                    (true, None) => bail!(
                        "Action '{}' references non-existent task '{}'",
                        action_name,
                        task_ref
                    ),
                }
            } else {
                bail!(
                    "Action '{}' references non-existent task '{}'",
                    action_name,
                    task_ref
                );
            }
        }
```

Check `parse_qualified_ref` (`template.rs:91`): if it returns `(None, s)` for a leading-dot string, handle `.deploy` explicitly by testing `task_ref.starts_with('.') || task_ref.ends_with('.')` before the `if let` and bailing with the malformed message.

(d) `validate_hook_action_exists` (`:1824-1836`) — new signature and body:

```rust
/// Validates that a hook references an existing action (or a library action),
/// and that a `type: task` hook action does not point outside this workspace —
/// hook actions are never resolved cross-workspace.
fn validate_hook_action_exists(
    label: &str,
    action: &str,
    config: &WorkspaceConfig,
    libraries_resolved: bool,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if !libraries_resolved && action.contains('.') {
        return Ok(()); // library action — skip when libraries not resolved
    }
    let Some(def) = config.actions.get(action) else {
        bail!("{} references non-existent action '{}'", label, action);
    };
    if def.action_type == "task" {
        if let Some(task_ref) = def.task.as_deref() {
            if !config.tasks.contains_key(task_ref) && task_ref.contains('.') {
                if libraries_resolved {
                    bail!(
                        "{} uses action '{}' whose task '{}' is in another workspace; hook actions cannot call tasks across workspaces",
                        label, action, task_ref
                    );
                }
                warnings.push(format!(
                    "{} uses action '{}' whose task '{}' is not local - cannot validate offline",
                    label, action, task_ref
                ));
            }
        }
    }
    Ok(())
}
```

Update all eight call sites (`:339,347,355,363` task-level; `:521,529,537,545` workspace-level) to pass `&mut warnings` as the last argument.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p stroem-common --lib validation::tests`
Expected: all PASS, including the two updated resolver tests and the pre-existing `test_validate_hook_*` / `test_validate_task_action_*` tests.

- [ ] **Step 5: Fix any other compile sites and commit**

Run: `cargo build --workspace 2>&1 | grep -E '^error' | head` — expected: no errors (no production code uses the resolver today; if a server call site appears, adapt it to a struct implementing the trait).

```bash
cargo fmt --all && cargo clippy -p stroem-common --all-targets -- -D warnings
git add crates/stroem-common/src/validation.rs
git commit -m "feat(validation): resolve dotted type: task references cross-workspace; reject them in hook actions"
```

---

### Task 4: `classify_execute_error` learns `"has no task"`

Spec § 3.1 table.

**Files:**
- Modify: `crates/stroem-server/src/web/api/mod.rs:404-411`
- Test: `crates/stroem-server/src/web/api/mod.rs` `mod classify_execute_error_tests` (~:420)

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn owner_workspace_has_no_task_is_bad_request() {
        let e = anyhow::anyhow!("task 'B.nope': workspace 'B' has no task 'nope'");
        match classify_execute_error(e) {
            AppError::BadRequest(msg) => assert!(msg.contains("has no task"), "{msg}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn has_no_task_buried_under_infra_context_is_internal() {
        let e = anyhow::anyhow!("task 'B.nope': workspace 'B' has no task 'nope'")
            .context("Failed to create job");
        assert!(matches!(classify_execute_error(e), AppError::Internal(_)));
    }

    #[test]
    fn task_step_literal_boundary_error_wrapped_in_resolve_context_is_bad_request() {
        let e = anyhow::anyhow!(
            "input 'db': a connection passed across workspaces must be a connection name, got object"
        )
        .context("step 'run': failed to resolve connection inputs");
        match classify_execute_error(e) {
            AppError::BadRequest(msg) => assert!(msg.contains("must be a connection name"), "{msg}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn task_step_unavailable_owner_wrapped_in_resolve_context_is_internal() {
        let e = anyhow::anyhow!("connection 'x': workspace 'B' is not available")
            .context("step 'run': failed to resolve connection inputs");
        assert!(matches!(classify_execute_error(e), AppError::Internal(_)));
    }
```

- [ ] **Step 2: Run to verify the first fails**

Run: `cargo test -p stroem-server --lib classify_execute_error_tests`
Expected: `owner_workspace_has_no_task_is_bad_request` FAILS (Internal); the other three PASS already (they pin existing behaviour).

- [ ] **Step 3: Implement**

At `mod.rs:407` add a line after the `"has no action"` one:

```rust
        || outer.contains("has no task") // cross-workspace: owner workspace exists, task doesn't
```

- [ ] **Step 4: Run tests** — `cargo test -p stroem-server --lib classify_execute_error_tests` → all PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/web/api/mod.rs
git commit -m "feat(api): classify 'has no task' as a 400 at job creation"
```

---

### Task 5: `resolve_task_ref` and the creation-time pre-check

Spec § 3.1, § 3.2.

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs` — imports (`:1-18`), step loop (`:324-409`), new fns near `precheck_literal_connection_inputs` (`:617-666`)
- Test: `crates/stroem-server/src/job_creator.rs` `mod tests` (`:668+`) — pure tests over `WorkspaceManager::from_configs`

**Interfaces:**
- Consumes: `WorkspaceManager::{has_workspace, get_config, from_configs}`, `stroem_common::template::{parse_qualified_ref, resolve_task_input_by_provenance, PRIMITIVE_TYPES}`, `WorkspaceSet::load`.
- Produces:
  ```rust
  pub(crate) enum OwnerConfig { Base, Foreign(Arc<WorkspaceConfig>) }
  pub(crate) struct ResolvedTask {
      pub workspace: String,      // T
      pub task_name: String,      // bare
      pub task: TaskDef,          // cloned from T's config
      pub config: OwnerConfig,    // Base ⇒ the base_cfg the caller passed in
  }
  impl ResolvedTask {
      pub(crate) fn config<'a>(&'a self, base: &'a WorkspaceConfig) -> &'a WorkspaceConfig;
  }
  pub(crate) async fn resolve_task_ref(
      workspaces: &WorkspaceManager, base_ws: &str, base_cfg: &WorkspaceConfig, task_ref: &str,
  ) -> Result<ResolvedTask>;
  pub(crate) async fn precheck_task_step_literals(
      step_name: &str, flow_step: &FlowStep, resolved: &ResolvedTask,
      workspaces: &WorkspaceManager, caller_ws: &str, base_cfg: &WorkspaceConfig,
  ) -> Result<()>;
  ```

- [ ] **Step 1: Write the failing tests**

Inside `mod tests` in `job_creator.rs` (uses `tokio::test`; `WorkspaceManager::from_configs(vec![(name, cfg, revision)])` builds a manager without a DB; `WorkspaceManager::with_load_error`/placeholder helper — check `workspace/mod.rs:442` for the test-only helper that registers an unavailable workspace and use it for the `Unavailable` case):

```rust
    fn cfg_with_task(task: &str) -> WorkspaceConfig {
        let mut c = WorkspaceConfig::default();
        c.tasks.insert(task.to_string(), TaskDef {
            name: None, description: None, mode: "distributed".to_string(), folder: None,
            input: HashMap::new(), flow: HashMap::new(), timeout: None, retry: None,
            on_success: vec![], on_error: vec![], on_suspended: vec![], on_cancel: vec![],
        });
        c
    }

    #[tokio::test]
    async fn resolve_task_ref_local_hit() {
        let a = cfg_with_task("deploy");
        let mgr = WorkspaceManager::from_configs(vec![("A".into(), a.clone(), None)]);
        let r = resolve_task_ref(&mgr, "A", &a, "deploy").await.unwrap();
        assert_eq!(r.workspace, "A");
        assert_eq!(r.task_name, "deploy");
        assert!(matches!(r.config, OwnerConfig::Base));
    }

    #[tokio::test]
    async fn resolve_task_ref_library_flattened_key_wins() {
        let a = cfg_with_task("common.deploy");
        let mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a.clone(), None),
            ("common".into(), cfg_with_task("deploy"), None),
        ]);
        let r = resolve_task_ref(&mgr, "A", &a, "common.deploy").await.unwrap();
        assert_eq!(r.workspace, "A");
        assert_eq!(r.task_name, "common.deploy");
    }

    #[tokio::test]
    async fn resolve_task_ref_qualified_hit() {
        let a = WorkspaceConfig::default();
        let mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a.clone(), None),
            ("B".into(), cfg_with_task("deploy"), Some("rev-b".into())),
        ]);
        let r = resolve_task_ref(&mgr, "A", &a, "B.deploy").await.unwrap();
        assert_eq!(r.workspace, "B");
        assert_eq!(r.task_name, "deploy");
        assert!(matches!(r.config, OwnerConfig::Foreign(_)));
        assert!(r.config(&a).tasks.contains_key("deploy"));
    }

    #[tokio::test]
    async fn resolve_task_ref_error_phrases() {
        let a = WorkspaceConfig::default();
        let mut mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a.clone(), None),
            ("B".into(), cfg_with_task("deploy"), None),
        ]);
        mgr = mgr.with_load_error_for_test("C", "git clone failed"); // see workspace/mod.rs:442 helper name
        let e = resolve_task_ref(&mgr, "A", &a, "Z.deploy").await.unwrap_err().to_string();
        assert_eq!(e, "task 'Z.deploy': unknown workspace 'Z'");
        let e = resolve_task_ref(&mgr, "A", &a, "B.nope").await.unwrap_err().to_string();
        assert_eq!(e, "task 'B.nope': workspace 'B' has no task 'nope'");
        let e = resolve_task_ref(&mgr, "A", &a, "deploy").await.unwrap_err().to_string();
        assert_eq!(e, "Task 'deploy' not found in workspace 'A'");
        let e = resolve_task_ref(&mgr, "A", &a, "C.deploy").await.unwrap_err().to_string();
        // `has_workspace` ignores load_errors: a source-construction failure reads as unknown.
        assert_eq!(e, "task 'C.deploy': unknown workspace 'C'");
    }
```

If no test-only helper for an unavailable placeholder exists at `workspace/mod.rs:442`, drop the `C` assertions and add a comment; the `Unavailable` → 500 path is covered by the integration test in Task 10.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p stroem-server --lib job_creator::tests::resolve_task_ref` → compile error.

- [ ] **Step 3: Implement `resolve_task_ref`**

Add to imports: `use std::sync::Arc;` and `use stroem_common::models::workflow::TaskDef;` (extend the existing `models::workflow::{…}` import). Add after `compute_depth`:

```rust
/// Which config a resolved task lives in: the base config the caller already
/// holds, or another workspace's snapshot.
pub(crate) enum OwnerConfig {
    Base,
    Foreign(Arc<WorkspaceConfig>),
}

/// A `type: task` reference resolved to its owner (spec § 3.1).
pub(crate) struct ResolvedTask {
    pub workspace: String,
    pub task_name: String,
    pub task: TaskDef,
    pub config: OwnerConfig,
}

impl ResolvedTask {
    pub(crate) fn config<'a>(&'a self, base: &'a WorkspaceConfig) -> &'a WorkspaceConfig {
        match &self.config {
            OwnerConfig::Base => base,
            OwnerConfig::Foreign(c) => c.as_ref(),
        }
    }
}

/// Resolve `task_ref` relative to `base_ws` (the ACTION's owner): a key of
/// `base_cfg.tasks` first (local and library-flattened names), else a dotted
/// `ws.task` against another loaded workspace. Errors are the outermost
/// message on purpose — `classify_execute_error` keys off them.
pub(crate) async fn resolve_task_ref(
    workspaces: &WorkspaceManager,
    base_ws: &str,
    base_cfg: &WorkspaceConfig,
    task_ref: &str,
) -> Result<ResolvedTask> {
    if let Some(task) = base_cfg.tasks.get(task_ref) {
        return Ok(ResolvedTask {
            workspace: base_ws.to_string(),
            task_name: task_ref.to_string(),
            task: task.clone(),
            config: OwnerConfig::Base,
        });
    }
    if let (Some(ws), name) = stroem_common::template::parse_qualified_ref(task_ref) {
        if !workspaces.has_workspace(ws) {
            bail!("task '{}': unknown workspace '{}'", task_ref, ws);
        }
        let cfg = workspaces.get_config(ws).await.ok_or_else(|| {
            anyhow::anyhow!("task '{}': workspace '{}' is not available", task_ref, ws)
        })?;
        let task = cfg.tasks.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!("task '{}': workspace '{}' has no task '{}'", task_ref, ws, name)
        })?;
        return Ok(ResolvedTask {
            workspace: ws.to_string(),
            task_name: name.to_string(),
            task,
            config: OwnerConfig::Foreign(cfg),
        });
    }
    bail!("Task '{}' not found in workspace '{}'", task_ref, base_ws)
}
```

- [ ] **Step 4: Run `resolve_task_ref` tests** — `cargo test -p stroem-server --lib job_creator::tests::resolve_task_ref` → PASS.

- [ ] **Step 5: Add the pre-check and wire it into the step loop**

Add after `precheck_literal_connection_inputs`:

```rust
/// Creation-time pre-check for a `type: task` step (spec § 3.2 item 3): the
/// caller's LITERAL values for the TASK's connection-typed inputs are checked
/// with the same scope dispatch will use (caller first, task owner if shared),
/// including the cross-workspace shape rule. `when`-guarded steps are not
/// pre-checked (the step may never run). The error is wrapped so the
/// classifier's "resolve connection" phrase answers 400; an unavailable owner
/// inside the chain still answers 500.
pub(crate) async fn precheck_task_step_literals(
    step_name: &str,
    flow_step: &FlowStep,
    resolved: &ResolvedTask,
    workspaces: &WorkspaceManager,
    caller_ws: &str,
    base_cfg: &WorkspaceConfig,
) -> Result<()> {
    if flow_step.when.is_some() {
        return Ok(());
    }
    let mut literals = serde_json::Map::new();
    for (field, def) in &resolved.task.input {
        if stroem_common::template::PRIMITIVE_TYPES.contains(&def.field_type.as_str()) {
            continue;
        }
        match flow_step.input.get(field) {
            Some(serde_json::Value::String(s)) if s.contains("{{") => {}
            Some(v) => {
                literals.insert(field.clone(), v.clone());
            }
            None => {}
        }
    }
    if literals.is_empty() {
        return Ok(());
    }
    let t_cfg = resolved.config(base_cfg);
    let set = WorkspaceSet::load(workspaces, &resolved.workspace, Some(t_cfg)).await;
    stroem_common::template::resolve_task_input_by_provenance(
        &serde_json::Value::Object(literals),
        &serde_json::json!({}),
        &resolved.task.input,
        &set,
        caller_ws,
        caller_ws,
        &resolved.workspace,
    )
    .with_context(|| format!("step '{}': failed to resolve connection inputs", step_name))
    .map(|_| ())
}
```

In the step loop (`:336-372`) change the `if is_cross { … } else { … }` to also return the owner's config so it outlives the block — replace the 4-tuple with a 5-tuple:

```rust
            let (owned_action, action_workspace, action_revision, action_name, cross_cfg) =
                if is_cross {
                    let ws = owner_ws.unwrap();
                    let owner_cfg = workspaces.get_config(ws).await.ok_or_else(|| { /* unchanged */ })?;
                    let a = owner_cfg.actions.get(bare_action).cloned().ok_or_else(|| { /* unchanged */ })?;
                    (a, Some(ws.to_string()), workspaces.get_revision(ws), bare_action.to_string(), Some(owner_cfg))
                } else {
                    let a = /* unchanged */;
                    (a, None, None, flow_step.action.clone(), None)
                };
            let action = &owned_action;
```

Then replace the `precheck_literal_connection_inputs(...)?;` call (`:377-384`) with:

```rust
            if action.action_type == "task" {
                let task_ref = action
                    .task
                    .as_deref()
                    .context("type: task action missing task field")?;
                let base_ws: &str = action_workspace.as_deref().unwrap_or(workspace_name);
                let base_cfg: &WorkspaceConfig = match cross_cfg.as_deref() {
                    Some(c) => c,
                    None => workspace_config,
                };
                let resolved = resolve_task_ref(workspaces, base_ws, base_cfg, task_ref).await?;
                if resolved.workspace == workspace_name && resolved.task_name == task_name {
                    bail!(
                        "task '{}' is a self-reference to '{}/{}' (invalid)",
                        task_ref,
                        workspace_name,
                        task_name
                    );
                }
                precheck_task_step_literals(
                    step_name, flow_step, &resolved, workspaces, workspace_name, base_cfg,
                )
                .await?;
            } else {
                precheck_literal_connection_inputs(
                    step_name,
                    flow_step,
                    action,
                    &ws_set,
                    workspace_name,
                    action_workspace.as_deref(),
                )?;
            }
```

- [ ] **Step 6: Build and run the crate's unit tests**

Run: `cargo build -p stroem-server && cargo test -p stroem-server --lib job_creator`
Expected: builds; tests PASS. Then run the existing integration tests that touch task actions to check nothing regressed: `cargo test -p stroem-server --test integration_test task_action` → PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/job_creator.rs
git commit -m "feat(job-creator): resolve type: task references relative to the action owner and pre-check them at creation"
```

---

### Task 6: Dispatch creates the child in the task owner's workspace

Spec § 3.3, "Error scrubbing".

**Files:**
- Modify: `crates/stroem-server/src/settlement/dispatch.rs:1-19` (imports), `:63-92` (`fail_task_step`), `:97-310` (`handle_task_steps_pass`), approval callers of `fail_task_step` (`:386`, `:436`)

**Interfaces:**
- Consumes: `job_creator::{resolve_task_ref, ResolvedTask, create_job_for_task_inner, CreationMode, compute_depth, MAX_TASK_DEPTH}`, `stroem_common::template::{merge_action_defaults, resolve_task_input_by_provenance, render_input_map}`, `workspace_set::collect_config_secret_values`, `WorkspaceSet::load`.
- Produces: `fail_task_step(pool, job_id, step_name, err, task, workspace_config, snapshots, extra_secret_values: &[String])` — one new trailing parameter; behaviour otherwise unchanged.

- [ ] **Step 1: Change `fail_task_step`**

```rust
async fn fail_task_step(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    err: &str,
    task: &stroem_common::models::workflow::TaskDef,
    workspace_config: &WorkspaceConfig,
    snapshots: &Snapshots,
    extra_secret_values: &[String],
) -> Result<()> {
    // The caller's own secrets plus — for a `type: task` step — the action
    // owner's and task owner's (spec § 3.3), because an owner-side default
    // rendering error quotes the owner's value.
    let mut secret_values = crate::workspace_set::collect_config_secret_values(workspace_config);
    secret_values.extend_from_slice(extra_secret_values);
    let err = &crate::workspace_set::redact_secrets_in_str(err, &secret_values)[..];
    /* rest unchanged */
```

Update the two approval callers (`:386`, `:436`) to pass `&[]`.

- [ ] **Step 2: Rewrite the per-step body of `handle_task_steps_pass`**

Replace imports line 11 with `use stroem_common::template::{merge_action_defaults, render_input_map, resolve_task_input_by_provenance};` and line 16 with `use crate::job_creator::{compute_depth, create_job_for_task_inner, resolve_task_ref, CreationMode, MAX_TASK_DEPTH};`. Add `use stroem_common::models::workflow::InputFieldDef;`.

Inside the `for step in &steps` loop, after the depth check (`:135-154`) and BEFORE the step-input render, insert the owner/resolution block; every `fail_task_step(...)` call in this loop gains the scrub slice as last argument (`&scrub` once it exists, `&[]` for the depth check which precedes it):

```rust
        // 1. Action owner O and its config snapshot.
        let base_ws: &str = step.action_workspace.as_deref().unwrap_or(workspace_name);
        let base_arc = if base_ws == workspace_name {
            None
        } else {
            workspaces.get_config(base_ws).await
        };
        let base_cfg: &WorkspaceConfig = if base_ws == workspace_name {
            workspace_config
        } else {
            match base_arc.as_deref() {
                Some(c) => c,
                None => {
                    let err = format!(
                        "workspace '{}' is not available (owner of action '{}')",
                        base_ws, step.action_name
                    );
                    fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config, snapshots, &[]).await?;
                    failed_any = true;
                    continue;
                }
            }
        };
        let mut scrub = crate::workspace_set::collect_config_secret_values(base_cfg);

        // 2. Task owner T.
        let resolved = match resolve_task_ref(workspaces, base_ws, base_cfg, task_ref).await {
            Ok(r) => r,
            Err(e) => {
                let err = format!("Failed to resolve task for step '{}': {:#}", step.step_name, e);
                fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config, snapshots, &scrub).await?;
                failed_any = true;
                continue;
            }
        };
        let t_cfg: &WorkspaceConfig = resolved.config(base_cfg);
        scrub.extend(crate::workspace_set::collect_config_secret_values(t_cfg));
```

Keep the existing step-input render (`:156-203`) as is — it produces `rendered_input`, the caller bucket `C`. Then REPLACE the action-defaults block (`:205-236`) with:

```rust
        // 4. Action-level defaults from the PERSISTED action_spec (never a live
        //    lookup), rendered with O's live secrets. Keys it adds are bucket D.
        let action_schema: HashMap<String, InputFieldDef> = match action_spec.get("input") {
            Some(v) if !v.is_null() => match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let err = format!("step '{}': action_spec.input is not an input schema: {}", step.step_name, e);
                    fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config, snapshots, &scrub).await?;
                    failed_any = true;
                    continue;
                }
            },
            _ => HashMap::new(),
        };
        let caller_bucket = rendered_input;
        let default_bucket = if action_schema.is_empty() {
            serde_json::json!({})
        } else {
            let secrets_ctx = serde_json::json!({ "secret": &base_cfg.secrets });
            match merge_action_defaults(&caller_bucket, &action_schema, &secrets_ctx) {
                Ok(merged) => {
                    let caller_keys = caller_bucket.as_object().cloned().unwrap_or_default();
                    let mut d = serde_json::Map::new();
                    if let Some(m) = merged.as_object() {
                        for (k, v) in m {
                            if !caller_keys.contains_key(k) {
                                d.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    serde_json::Value::Object(d)
                }
                Err(e) => {
                    let err = format!("Failed to prepare action input for task step '{}': {:#}", step.step_name, e);
                    fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config, snapshots, &scrub).await?;
                    failed_any = true;
                    continue;
                }
            }
        };

        // 5. Connection resolution against the TASK's schema, by provenance.
        let ws_set = WorkspaceSet::load(workspaces, &resolved.workspace, Some(t_cfg)).await;
        let rendered_input = match resolve_task_input_by_provenance(
            &caller_bucket,
            &default_bucket,
            &resolved.task.input,
            &ws_set,
            workspace_name,
            base_ws,
            &resolved.workspace,
        ) {
            Ok(v) => v,
            Err(e) => {
                let err = format!("Failed to resolve connection inputs for task step '{}': {:#}", step.step_name, e);
                fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config, snapshots, &scrub).await?;
                failed_any = true;
                continue;
            }
        };
```

Keep `update_input`, `mark_running_server`, `mark_running_if_pending_server` and `source_id` exactly as they are (`:238-252`). Replace the creation call (`:254-285`) with:

```rust
        // 7. Revision: inherit the parent's for a same-workspace child; the
        //    owner's current one for a foreign child (spec § 3.3 step 7).
        let revision: Option<String> = if resolved.workspace == job.workspace {
            job.revision.clone()
        } else {
            workspaces.get_revision(&resolved.workspace)
        };

        match create_job_for_task_inner(
            workspaces,
            pool,
            t_cfg,
            &resolved.workspace,
            &resolved.task_name,
            rendered_input,
            "task",
            Some(&source_id),
            Some(job_id),
            Some(&step.step_name),
            revision.as_deref(),
            CreationMode::Normal,
            None,
            defaults,
        )
        .await
        {
            Ok(created) => {
                if resolved.workspace == job.workspace {
                    tracing::info!(
                        "Created child job {} for task step '{}' -> task '{}'",
                        created.job_id, step.step_name, resolved.task_name
                    );
                } else {
                    tracing::info!(
                        "Created child job {} for task step '{}' -> task '{}' in workspace '{}'",
                        created.job_id, step.step_name, resolved.task_name, resolved.workspace
                    );
                }
            }
            Err(e) => {
                let err = format!("Failed to create child job for task '{}': {:#}", task_ref, e);
                fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config, snapshots, &scrub).await?;
                failed_any = true;
            }
        }
```

Remove the now-unused `prepare_action_input` import. `WorkspaceSet` stays imported.

- [ ] **Step 3: Build, run existing task-action tests**

Run: `cargo build -p stroem-server && cargo test -p stroem-server --lib && cargo test -p stroem-server --test integration_test task_action`
Expected: all PASS (same-workspace behaviour is unchanged for declared fields).

- [ ] **Step 4: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/settlement/dispatch.rs
git commit -m "feat(dispatch): create type: task children in the task owner's workspace with provenance-resolved input"
```

---

### Task 7: Hook runtime refuses a qualified task in a hook action

Spec § 5 "Hooks".

**Files:**
- Modify: `crates/stroem-server/src/settlement/hooks.rs:566-579` (`fire_single_hook`, `type: task` branch)
- Test: covered by the integration test in Task 10 (`hook with qualified task fails the hook job with the § 5 wording`); add a unit test only if `fire_single_hook` has an existing pure test seam (grep `mod tests` in `hooks.rs`; if none, skip).

- [ ] **Step 1: Implement**

Immediately after `let task_ref = action.task.as_ref().context("type: task action missing task field")?;` insert:

```rust
        if !workspace_config.tasks.contains_key(task_ref.as_str()) && task_ref.contains('.') {
            anyhow::bail!(
                "hook uses action '{}' whose task '{}' is in another workspace; hook actions cannot call tasks across workspaces",
                hook.action,
                task_ref
            );
        }
```

(`hook` is the `HookDef` parameter of `fire_single_hook`; confirm the binding name at `hooks.rs:28-40` and adapt.)

- [ ] **Step 2: Build** — `cargo build -p stroem-server` → ok.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/settlement/hooks.rs
git commit -m "fix(hooks): fail a hook job clearly when its type: task action names another workspace's task"
```

---

### Task 8: CLI `stroem validate` surfaces the offline warning

Spec § 5 (CLI path).

**Files:**
- Test: `crates/stroem-cli/src/local/validate.rs` `mod tests`

`cmd_validate` already prints every warning `validate_workflow_config` returns (`validate.rs:21-26`), so no production change is expected; this task pins it.

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn validate_warns_on_cross_workspace_task_ref_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let wf = dir.path().join(".workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(
            wf.join("x.yaml"),
            "actions:\n  run-remote:\n    type: task\n    task: B.deploy\ntasks:\n  caller:\n    flow:\n      go:\n        action: run-remote\n",
        )
        .unwrap();
        assert!(cmd_validate(dir.path().to_str().unwrap()).is_ok());
    }
```

(If the loader expects a different layout than `.workflows/*.yaml`, copy the layout used by the neighbouring tests in this file.)

- [ ] **Step 2: Run** — `cargo test -p stroem-cli validate_warns_on_cross_workspace_task_ref` → PASS (if it fails, the loader is rejecting the dotted task earlier: fix in `stroem-common` validation, not here).

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-cli/src/local/validate.rs
git commit -m "test(cli): validate accepts a cross-workspace task reference offline with a warning"
```

---

### Task 9: Parent → child link on the step DTO and in the UI

Spec § 3.7.

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs` (after `get_child_jobs`, ~:781)
- Modify: `crates/stroem-server/src/web/api/jobs.rs:309-360` (`get_job` step JSON)
- Modify: `ui/src/lib/types.ts:89-122` (`JobStep`), `ui/src/components/step-timeline.tsx` (`StepRow`, near the worker link at `:186-194`)
- Test: `crates/stroem-server/tests/integration_test.rs` (Task 10 asserts `child_jobs`), `ui` typecheck + lint

**Interfaces:**
- Produces: `JobRepo::list_children(pool, parent_job_id) -> Result<Vec<JobRow>>` (ALL statuses, `ORDER BY created_at DESC, job_id DESC`); step JSON field `child_jobs: [{ id, workspace, task_name, status, created_at }]` on `type: task` steps only; TS `ChildJobRef` + `JobStep.child_jobs?: ChildJobRef[]`.

- [ ] **Step 1: Repo query**

```rust
    /// Every child job of `parent_job_id`, any status, newest first — the
    /// execution history of the parent's `type: task` steps. Ties on
    /// `created_at` are broken by id so the order is deterministic.
    pub async fn list_children(pool: &PgPool, parent_job_id: Uuid) -> Result<Vec<JobRow>> {
        let jobs = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {} FROM job WHERE parent_job_id = $1 ORDER BY created_at DESC, job_id DESC",
            JOB_COLUMNS
        ))
        .bind(parent_job_id)
        .fetch_all(pool)
        .await
        .context("Failed to list child jobs")?;
        Ok(jobs)
    }
```

- [ ] **Step 2: Step JSON**

In `get_job`, before `let mut steps_json` add:

```rust
    let children = JobRepo::list_children(&state.pool, job_id)
        .await
        .context("list child jobs")?;
```

and inside the `.map(|step| { … })` closure, after the approval block, add:

```rust
            if step.action_type == "task" {
                let refs: Vec<serde_json::Value> = children
                    .iter()
                    .filter(|c| c.parent_step_name.as_deref() == Some(step.step_name.as_str()))
                    .map(|c| json!({
                        "id": c.job_id,
                        "workspace": c.workspace,
                        "task_name": c.task_name,
                        "status": c.status,
                        "created_at": c.created_at,
                    }))
                    .collect();
                step_json["child_jobs"] = serde_json::Value::Array(refs);
            }
```

- [ ] **Step 3: UI types and row**

`types.ts` — add before `JobStep`:

```ts
export interface ChildJobRef {
  id: string;
  workspace: string;
  task_name: string;
  status: string;
  created_at: string;
}
```

and inside `JobStep` after `skip_reason`: `child_jobs?: ChildJobRef[];`.

`step-timeline.tsx` `StepRow` — after the worker `<Link>` block (`:186-194`) add:

```tsx
            {step.child_jobs && step.child_jobs.length > 0 && (
              <span className="flex flex-wrap items-center gap-1 text-xs text-muted-foreground/60">
                child jobs:
                {step.child_jobs.map((c) => (
                  <Link
                    key={c.id}
                    to={`/jobs/${c.id}`}
                    className="hover:underline"
                    onClick={(e) => e.stopPropagation()}
                    title={`${c.workspace}/${c.task_name} (${c.status})`}
                  >
                    {c.workspace}/{c.task_name}
                  </Link>
                ))}
              </span>
            )}
```

- [ ] **Step 4: Verify**

Run: `cargo build -p stroem-server && cargo test -p stroem-db --lib` and `cd ui && bun run lint && bunx tsc --noEmit`
Expected: all clean.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-db/src/repos/job.rs crates/stroem-server/src/web/api/jobs.rs ui/src/lib/types.ts ui/src/components/step-timeline.tsx
git commit -m "feat(ui): link a type: task step to its child jobs"
```

---

### Task 10: Integration tests

Spec § 6. All in `crates/stroem-server/tests/integration_test.rs`. Reuse the existing helpers: `setup_two_workspaces` (`:1332`), `trivial_script_action` (`:1289`), `api_request`, `body_json`, `register_test_worker`, and the `after_step` / local `handle_task_steps` pattern used by `test_task_action_child_completion_updates_parent` (`:13227-13300`) to drive a child to completion and propagate.

**Files:**
- Modify: `crates/stroem-server/tests/integration_test.rs` — new fixture + tests appended after `test_execute_task_cross_workspace_unknown_action_returns_400` (`:2912`)
- Modify (test-only helper): `crates/stroem-server/src/workspace/mod.rs` — add `pub async fn replace_config_for_test(&self, name: &str, cfg: WorkspaceConfig)` that swaps the entry's `config` `Arc<RwLock<Arc<_>>>` contents (for the "task removed between creation and dispatch" case)

**Interfaces:**
- Produces fixture:
  ```rust
  struct CrossTaskOpts { acl: Option<AclConfig>, auth: bool }
  async fn setup_cross_task_workspaces(opts: CrossTaskOpts)
      -> Result<(Router, PgPool, Arc<WorkspaceManager>, TempDir, ContainerAsync<Postgres>)>
  ```
  Workspaces: `A` (revision `rev-a-1`), `B` (`rev-b-1`), `C` (`rev-c-1`). `A` tasks: `pipeline` (step `run`: local action `call-deploy` = `type: task, task: B.deploy`), `pipeline-via-owner` (step `run`: `action: B.run-deploy`), `pipeline-self` (`task: A.pipeline-self`), `pipeline-nope` (`task: B.nope`), `pipeline-unknown` (`task: Z.deploy`), `pipeline-conn-literal` (step input `db: "b-shared"`), `pipeline-conn-object` (step input `db: {host: x}`), `pipeline-conn-object-when` (same + `when: "{{ true }}"`), `pipeline-collision` (`action: B.run-deploy` AND `A` has its own task `deploy`), `pipeline-secret-error` (`action: B.run-bad-default`). `B` tasks: `deploy` (single script step `echo deployed`; input `db: {type: pg}` optional; `on_error` hook to a `B` script action), `nope` absent. `B` actions: `run-deploy` (`type: task, task: deploy`, `input: { db: { type: pg, default: "b-private" } }`), `run-deploy-via-c` (`type: task, task: C.build`), `run-bad-default` (`type: task, task: deploy, input: { note: { type: string, default: "{{ secret.TOKEN | round }}" } }`). `B` secrets: `TOKEN: "ABCD-token"`; `A` secrets: `PREFIX: "incorrect value: got \"ABCD"`. `B` connections: `b-shared` (pg, shared), `b-private` (pg, not shared). `C` task `build`.

- [ ] **Step 1: Add the test-only manager helper**

`workspace/mod.rs`, next to `from_configs`:

```rust
    /// Test-only: replace a loaded workspace's config in place (simulates a
    /// reload that changed the YAML) without touching the source.
    #[doc(hidden)]
    pub async fn replace_config_for_test(&self, name: &str, cfg: WorkspaceConfig) {
        if let Some(entry) = self.entries.get(name) {
            *entry.config.write().await = Arc::new(cfg);
        }
    }
```

- [ ] **Step 2: Write the fixture**

Model it on `setup_two_workspaces`; build the three configs with `trivial_script_action`, `FlowStep { … }` and `TaskDef { … }` literals exactly as that fixture does; call `WorkspaceManager::from_configs(vec![("A", ws_a, Some("rev-a-1")), ("B", ws_b, Some("rev-b-1")), ("C", ws_c, Some("rev-c-1"))])`, wrap: `let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None); let mgr_handle = Arc::clone(&state.workspaces); let router = build_router(state, CancellationToken::new());` and return `mgr_handle`. For `opts.acl`/`opts.auth`, copy the `AclConfig { default: Deny, rules: … }` + user seeding from `setup_with_auth_and_acl` (`:9258-9330`) with the rule `A/pipeline` → Run for group `callers`.

- [ ] **Step 3: Write the tests** (one `#[tokio::test]` each; names are the spec's seams)

```rust
#[tokio::test]
async fn test_xws_task_form_a_creates_child_in_owner_workspace() -> Result<()> {
    let (router, pool, _mgr, _tmp, _c) = setup_cross_task_workspaces(CrossTaskOpts::default()).await?;
    let resp = router.clone().oneshot(api_request("POST", "/api/workspaces/A/tasks/pipeline/execute", json!({}))).await?;
    assert_eq!(resp.status(), 200);
    let parent: Uuid = body_json(resp).await["job_id"].as_str().unwrap().parse()?;
    let child = JobRepo::get_child_jobs(&pool, parent).await?.pop().expect("child");
    assert_eq!(child.workspace, "B");
    assert_eq!(child.task_name, "deploy");
    assert_eq!(child.revision.as_deref(), Some("rev-b-1"));
    assert_eq!(child.parent_step_name.as_deref(), Some("run"));
    // Complete the child's step and propagate: follow the pattern at :13268-13300
    // (register_test_worker, mark_running, mark_completed, then settle + propagate).
    // Then:
    let steps = JobStepRepo::get_steps_for_job(&pool, parent).await?;
    assert_eq!(steps[0].status, "completed");
    let parent_row = JobRepo::get(&pool, parent).await?.unwrap();
    assert_eq!(parent_row.status, "completed");
    Ok(())
}
```

Then, following the same shape, add:

- `test_xws_task_form_b_uses_persisted_action_defaults`: execute `pipeline-via-owner`; assert child in `B`, child `input["db"]["host"]` equals `b-private`'s host (owner default, ungated); then `mgr.replace_config_for_test("B", <B with run-deploy default changed to "b-shared">)` and dispatch a SECOND ready step of the same parent is not possible — instead execute again and assert the new child uses the new default, while asserting the FIRST child's persisted input is unchanged (defaults were read from `action_spec`).
- `test_xws_task_owner_task_qualified_to_third_workspace`: `pipeline-via-c` → child in `C`, task `build`, revision `rev-c-1`.
- `test_xws_task_name_collision_runs_owner_task`: `pipeline-collision` → child `workspace == "B"` (regression for spec § 7 item 1).
- `test_xws_task_same_workspace_child_inherits_parent_revision`: a local `task:` in `A` → child revision equals parent's `rev-a-1`.
- `test_xws_task_execute_errors`: `pipeline-unknown` → 400 body contains `unknown workspace`; `pipeline-nope` → 400 contains `has no task`; `pipeline-self` → 400 contains `self-reference`; make `B` unavailable via `mgr.replace_config_for_test` is not enough (it stays loaded) — use the manager's placeholder helper (`workspace/mod.rs:442`) in a fixture variant to get a 500 for `pipeline`.
- `test_xws_task_chain_missing_grandchild_is_200_then_failed_step`: `A` → `B.run-deploy-via-c` where `C.build` is removed with `replace_config_for_test("C", empty)` after fixture build → execute returns 200; parent step `run` ends `failed` with `has no task 'build'` in `error_message`; parent job `failed`.
- `test_xws_task_owner_task_removed_before_dispatch_fails_step`: create `pipeline` with its `run` step behind a dependency (`when`-less but `depends_on` a local script step); remove `deploy` from `B` via `replace_config_for_test`; complete the first step so `run` dispatches → `run` failed, job failed, no child.
- `test_xws_task_secret_scrub_covers_owner_default_error`: `pipeline-secret-error` → parent step `failed`; `error_message` contains `••••••` and neither `ABCD-token` nor `-token`; job log lines (`LogStorage` read as other tests do) same; MCP `get_job_status` (see `mcp_test.rs` for the call pattern) same.
- `test_xws_task_provenance_through_http`: `pipeline-conn-literal` → 200, child `input["db"]["host"]` == `b-shared` host; `pipeline-conn-object` → 400 with `must be a connection name`; `pipeline-conn-object-when` → 200, and after dispatch (the `when` is `{{ true }}`) the step is `failed` with `must be a connection name`; a variant with a `{{ "b-shared" }}` template → 200 and child created.
- `test_xws_task_persisted_library_action_stays_local`: a task in `A` named `common.deploy` and an action `common.run` with `task: common.deploy` → child in `A`, `task_name == "common.deploy"`.
- `test_xws_task_two_pass_default_is_pinned_behaviour`: `B` secrets `X: "{{ secret.Y }}"`, `Y: "yval"`; action default `note: "{{ secret.X }}"` → child input `note == "yval"`.
- `test_xws_task_child_detail_is_owner_acl_404`: `setup_cross_task_workspaces(CrossTaskOpts { acl: Some(deny_default_with_run_on_A_pipeline), auth: true })`; as the `callers` user: 200 on execute, 200 on `GET /api/jobs/{parent}` with `steps[0].child_jobs[0].id` present, **404** on `GET /api/jobs/{child}` and `/logs`; as admin: 200 on both.
- `test_xws_task_child_jobs_list_after_retry`: parent step with `retry: { max_attempts: 2 }`; fail the first child; after retry dispatch, `steps[0].child_jobs.len() == 2`, newest first, no `current` key.
- `test_xws_task_hooks`: child failure fires `B/deploy`'s `on_error` hook job in `B` (a `hook`-sourced job in workspace `B` exists); `A`'s workspace-level `on_error` produces no job for the child; `B/deploy` with `retry` produces no retry job (child has `parent_job_id`); a hook action in `A` of `type: task, task: B.notify` → the hook job is `failed` with `hook actions cannot call tasks across workspaces`.

- [ ] **Step 4: Run**

Run: `export DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true; cargo test -p stroem-server --test integration_test test_xws_task -- --test-threads=4`
Expected: all PASS. Then the neighbours: `cargo test -p stroem-server --test integration_test 'cross_workspace' task_action` → PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/tests/integration_test.rs crates/stroem-server/src/workspace/mod.rs
git commit -m "test(server): cross-workspace type: task resolution, provenance, errors, access and hooks"
```

---

### Task 11: E2E — cross-workspace task step

Spec § 6 last bullet. The e2e stack (`docker-compose.yml`) mounts `./workspace` as `default`, `./workspace-ops` as `ops`, `./tests/e2e-workspace` as `test`; `tests/e2e.sh` § 17 already exercises `default` → `test.remote-cat`.

**Files:**
- Create: `tests/e2e-workspace/xtask-target.yaml`
- Create: `workspace/.workflows/xtask.yaml`
- Modify: `tests/e2e.sh` — new section after § 18

- [ ] **Step 1: Owner task in `test`**

`tests/e2e-workspace/xtask-target.yaml`:

```yaml
secrets:
  XTASK_SECRET: "only-in-test-ws"

actions:
  cat-and-secret:
    type: script
    runner: local
    script: |
      echo "XTASK_FILE=$(cat data/marker.txt)"
      echo "XTASK_SECRET={{ secret.XTASK_SECRET }}"
      echo 'OUTPUT: {"seen": "yes"}'

tasks:
  xtask-target:
    mode: distributed
    flow:
      run:
        action: cat-and-secret
```

(Check `tests/e2e-workspace/remote-cat.yaml` and the other files there for the exact `OUTPUT:` protocol / secrets syntax used by this e2e workspace and match it.)

- [ ] **Step 2: Caller in `default`**

`workspace/.workflows/xtask.yaml`:

```yaml
actions:
  call-xtask:
    type: task
    task: test.xtask-target

tasks:
  xtask:
    mode: distributed
    flow:
      call:
        action: call-xtask
      echo:
        action: shout
        depends_on: [call]
        input:
          text: "child said {{ call.output.seen }}"
```

(`shout` is the existing `default` action used by `hello-world`; confirm its input field name in `workspace/.workflows/` and adapt.)

- [ ] **Step 3: e2e assertions**

Append to `tests/e2e.sh` after section 18, copying section 17's poll loop shape:

```bash
# --- 19. Cross-workspace type: task action ---
# xtask (in "default") calls test.xtask-target; the child must run as a
# "test" job (its files and its secret), and its output must reach the
# parent's next step.
info "Triggering xtask task (cross-workspace type: task)..."
EXEC_RESP_XT=$(acurl -X POST "$BASE_URL/api/workspaces/default/tasks/xtask/execute" \
    -H "Content-Type: application/json" -d '{"input": {}}')
XT_JOB_ID=$(echo "$EXEC_RESP_XT" | jq -r '.job_id')
[ -n "$XT_JOB_ID" ] && [ "$XT_JOB_ID" != "null" ] || fail "xtask execute failed: $EXEC_RESP_XT"
XT_POLLED=0; XT_STATUS="pending"
while [ "$XT_STATUS" != "completed" ] && [ "$XT_STATUS" != "failed" ]; do
    sleep 2; XT_POLLED=$((XT_POLLED + 2))
    [ "$XT_POLLED" -lt "$MAX_POLL" ] || { acurl "$BASE_URL/api/jobs/$XT_JOB_ID" | jq .; fail "xtask did not finish"; }
    XT_DETAIL=$(acurl "$BASE_URL/api/jobs/$XT_JOB_ID"); XT_STATUS=$(echo "$XT_DETAIL" | jq -r '.status'); printf "."
done; echo ""
[ "$XT_STATUS" = "completed" ] || { echo "$XT_DETAIL" | jq .; fail "xtask failed"; }
pass "xtask completed"

XT_CHILD_ID=$(echo "$XT_DETAIL" | jq -r '.steps[] | select(.step_name=="call") | .child_jobs[0].id')
XT_CHILD_WS=$(echo "$XT_DETAIL" | jq -r '.steps[] | select(.step_name=="call") | .child_jobs[0].workspace')
[ "$XT_CHILD_WS" = "test" ] || fail "child workspace is '$XT_CHILD_WS', expected 'test'"
pass "child job $XT_CHILD_ID runs in workspace test"

XT_CHILD_LOGS=$(acurl "$BASE_URL/api/jobs/$XT_CHILD_ID/logs" | jq -r '.logs')
echo "$XT_CHILD_LOGS" | grep -q "XTASK_FILE=CROSS_WS_OK" || fail "child did not read the test workspace's file"
echo "$XT_CHILD_LOGS" | grep -q "XTASK_SECRET=only-in-test-ws" || fail "child did not render the test workspace's secret"
XT_LOGS=$(acurl "$BASE_URL/api/jobs/$XT_JOB_ID/logs" | jq -r '.logs')
echo "$XT_LOGS" | grep -qi "child said yes" || fail "child output did not reach the parent's next step"
pass "cross-workspace task: owner files, owner secret and output propagation verified"
```

- [ ] **Step 4: Run locally if disk allows** (`df -h .` must show > 15 GiB free; otherwise leave it to CI and say so in the PR):

Run: `./tests/e2e.sh`
Expected: `pass "cross-workspace task: …"` printed; exit 0.

- [ ] **Step 5: Commit**

```bash
git add tests/e2e-workspace/xtask-target.yaml workspace/.workflows/xtask.yaml tests/e2e.sh
git commit -m "test(e2e): cross-workspace type: task step runs as the owner and propagates output"
```

---

### Task 12: Documentation

Spec § 7, § 9.

**Files:**
- Modify: `docs/src/content/docs/guides/cross-workspace-references.md` (new section before "Errors"; delete the first bullet under "Not yet supported", `:245`)
- Modify: `docs/src/content/docs/guides/action-types.md` (`type: task` section, ~`:326-340`)
- Modify: `CLAUDE.md` § Cross-Workspace References (the "Deferred" bullet), § Task Actions
- Modify: `CONTEXT.md` (glossary)
- Modify: `docs/internal/TODO.md`
- Regenerate: `docs/public/llms.txt` (`cd docs && bun run generate-llms`)

- [ ] **Step 1: Guide section** — add to `cross-workspace-references.md`:

```markdown
## Calling tasks in other workspaces

A `type: task` action can name a task in another workspace, two ways:

```yaml
# Form A — direct
actions:
  call-deploy:
    type: task
    task: platform.deploy        # workspace "platform", task "deploy"

# Form B — through the owner's own wrapper action
tasks:
  release:
    flow:
      deploy:
        action: platform.run-deploy   # platform defines run-deploy as type: task
```

Three workspaces can be involved: the **caller** (where the flow step lives), the **action owner** (the workspace that owns the `type: task` action — the caller for form A, `platform` for form B), and the **task owner**. The `task:` name is looked up in the action owner first — library-flattened names like `common.deploy` live there — and only on a miss is it split into `workspace.task`.

The child is a real job of the task owner: it runs with that workspace's files, secrets, connections and revision, appears in its job list, fires that task's own `on_*` hooks, and is governed by that workspace's ACL. The caller's step receives the child's status and output exactly as for a local child; the step row links to the child job (newest first if the step was retried).

### Inputs

- The flow step's `input:` renders in the **caller's** context.
- The `type: task` action's own `input` defaults are read from the definition persisted on the step at job creation (so editing the action does not change an in-flight job), rendered with the action owner's live secrets.
- Connection-typed fields of the **task's** input schema resolve by where the value came from: a caller value resolves in the caller first, then in the task owner if the connection is `shared: true`; an action default resolves in the action owner first, then the task owner if shared; the task's own defaults resolve in the task owner without a gate.
- A value that crosses a workspace boundary into a connection-typed field **must be a connection name**; an object is rejected (400 at submit for a literal on an unguarded step, otherwise a failed step).

### Revision

A same-workspace child inherits its parent's revision. A cross-workspace child gets the task owner's **current** revision at the moment it is created — the value the owner's own triggers would use — unlike cross-workspace *actions*, whose owner revision is pinned when the parent job is created.

### Trust model

Calling a task is delegation to its author. Any workspace can call any task; the `Run` permission on the caller's task is the authorization, and there is no per-child check. Write tasks other workspaces may call as you would a webhook-triggered task: any input the schema accepts may arrive. The connection rules above cover declared connection-typed inputs only — a primitive input that your task templates into a connection name binds in your own workspace. The child's output is returned to the caller verbatim.

A child is not a scheduler run of the owner's task: it takes no task-level retry, fires no workspace-level hooks, and is cancelled when the caller's job is cancelled.

### Caveats a cross-team caller must know

These apply to every `type: task` child today and are tracked for a separate fix:

- Two concurrent settlements of the same job can dispatch the same step twice — the owner's task runs twice.
- A step failed by a stale dispatcher after another dispatcher succeeded; a child committed just after a cancellation enumerated children is not cancelled.
- A parent step that times out does not stop its child; the child's later result is written over the step.
- Removing a task that other workspaces call leaves their running jobs `running` until a job timeout or an operator cancels them.
- A child whose first step is an approval fires no `on_suspended` hook.
- A crash between a child's terminal claim and the write to the parent step loses that delivery.

### Errors

`task: nope.deploy` (unknown workspace), `task: platform.nope` (no such task) and a task that references itself by qualified name are `400` at submit; the task owner being temporarily unavailable is `500`. Only the submitted task's own steps are checked — a nested reference that fails resolves when its step is dispatched and fails that step.
```

Delete the bullet "**Cross-workspace `type: task` actions.** …" under "Not yet supported" and add: "- **Hook actions** are not resolved cross-workspace; a `type: task` hook action naming another workspace's task is rejected by server-side validation and fails the hook job."

- [ ] **Step 2: `action-types.md`** — in the `type: task` section add a "Behaviour changes in 0.17" list with spec § 7 items 1–6, verbatim.

- [ ] **Step 3: `CLAUDE.md`** — replace the `- **Deferred** (not yet supported): cross-workspace \`type: task\` actions; …` sentence so it reads: "- **Cross-workspace `type: task`**: `job_creator::resolve_task_ref` resolves `action_spec.task` relative to the ACTION owner (`step.action_workspace ?? job.workspace`): base `tasks` key first (library-flattened names), then `ws.task`; the child is created in the TASK owner's workspace (`settlement/dispatch.rs::handle_task_steps_pass`) from one config snapshot, with the owner's current revision when foreign. Connection inputs resolve once, against the task's schema, by provenance (`template::resolve_task_input_by_provenance`); non-strings are refused across a boundary. `fail_task_step` scrubs with caller + action-owner + task-owner secrets; `redact_secrets_in_str` masks the union of all match spans. Still deferred: cross-workspace hook actions; cross-workspace `agent` steps render against the caller." Under § Task Actions add: "Action-level defaults for a `type: task` step come from the persisted `action_spec.input`, never a live actions lookup; the creation pre-check for `type: task` actions checks caller literals against the TASK schema (`precheck_task_step_literals`), skipping `when`-guarded steps."

- [ ] **Step 4: `CONTEXT.md`** — add glossary entries "Action owner (O)", "Task owner (T)", "Provenance bucket".

- [ ] **Step 5: `TODO.md`** — add under Architecture: the seven § 4 carried risks (one line each, pointing at `docs/superpowers/specs/2026-09-16-task-step-lifecycle-hardening-design.md`), "ancestry-based cycle detection for `type: task` (rejected for now: bounded indirect recursion with `when` is legitimate)", "atomic `(config, revision)` read on `WorkspaceManager`", "same-workspace object trust in connection-typed inputs". Tick any existing cross-workspace `type: task` item.

- [ ] **Step 6: Regenerate llms.txt and build docs**

Run: `cd docs && bun install && bun run generate-llms && bun run build`
Expected: build succeeds; `docs/public/llms.txt` diff shows the new section.

- [ ] **Step 7: Commit**

```bash
git add docs/src/content/docs/guides/cross-workspace-references.md docs/src/content/docs/guides/action-types.md CLAUDE.md CONTEXT.md docs/internal/TODO.md docs/public/llms.txt
git commit -m "docs: cross-workspace type: task actions — guide, behaviour corrections, carried risks"
```

---

## Final verification (before hand-off to review)

- [ ] `cargo fmt --check --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test -p stroem-common && cargo test -p stroem-db && cargo test -p stroem-cli && cargo test -p stroem-server --lib`
- [ ] `cargo test -p stroem-server --test integration_test test_xws_task task_action cross_workspace`
- [ ] `cd ui && bun run lint && bunx tsc --noEmit && bun run build`
- [ ] Spec § 7 items 1–6 each have a test or doc line; spec § 6 bullets each map to a test name above.
