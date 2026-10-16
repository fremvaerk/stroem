# Templated Refs (`ref: "{{ … }}"`) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the person who runs a task, a webhook caller, or an earlier step choose which branch, tag or commit an action runs from, by writing a Tera template in `ref:` on a flow step, a `type: task` action, or a webhook trigger.

**Architecture:** A flow step whose `ref:` is a template is created as a *deferred* row (`action_type = required_ability = 'deferred'`, unclaimable). A new server-side pass, `settlement::deferred::resolve_deferred_steps`, runs in `Settlement::advance` and `dispatch::init` right after the cascade. For every deferred `ready` step it takes a fenced, renewable lease, renders the template in a context without `secret`, resolves the commit through a per-job memo table (`job_ref_pin`, first writer wins), checks the value against the job's ref secret set before and after the commit's config loads, and writes the step's real columns with the same code job creation uses. A `type: task` action's templated ref resolves at dispatch. A webhook's templated ref resolves when the webhook fires. Every resolution error on the new paths leaves as one value-free typed error, `RenderedRefError`.

**Tech Stack:** Rust (tokio, axum, sqlx runtime queries, Tera 2, anyhow, tracing), PostgreSQL, React 19 + TypeScript + Vitest (`ui/`), Starlight docs (`docs/`).

**Spec:** `docs/superpowers/specs/2026-10-15-templated-refs-design.md` (revision 8, Codex MERGE-READY round 6, approved by the user 2026-10-16). Builds on the git-refs spec `docs/superpowers/specs/2026-10-02-git-refs-design.md` (cited "GR"). Executors read the spec section a task cites before starting the task.

## Contract deviations (authoritative where the spec differs)

These were found while mapping the code for this plan. Where a task and the spec disagree, the task and this list win.

1. **The claim index (§ 8 EXPLAIN check).** `idx_job_step_ready_claim` is a GIN index on `required_tags`. GIN `jsonb_ops` supports `@>` but not `<@`, so the claim's `required_tags::jsonb <@ $2` can never use it. Measured on Postgres 16 (200 000 rows, also with `enable_seqscan = off`): the claim and the unmatched-step sweep both use `idx_job_step_ready_ability` (btree, partial `WHERE status = 'ready'`). D9 is unchanged and still correct (the claim SQL and the indexes do not change). The EXPLAIN test (Task 5) asserts an index scan on a partial `status = 'ready'` index and no sequential scan, not the GIN index by name. Task 17 corrects the CLAUDE.md § Capabilities sentence and files a TODO.md entry that `idx_job_step_ready_claim` looks unused. Nothing in this plan changes an index.
2. **No `vals` in a ref render (§ 4.2 rule 2).** `check_template_syntax` cannot reject `vals`: `tera_engine::check_engine()` registers it as the identity, and a test pins that (`template.rs:1750`). The only engine without `vals` is `tera_engine::base()`. Task 1 adds `tera_engine::ref_engine()` (a clone of `BASE`) and two public functions on it, `template::check_ref_template_syntax` and `template::render_ref_template`. A `| vals` in a ref template is then a compile error, "template uses an unknown filter", both at validation and at runtime.
3. **The private pin-load path (§ 5.5 change 2)** is a method, `WorkspaceManager::pinned_config_private(ws, commit)`, not a flag on `PinRef`. `PinRef` derives `Eq`/`Hash` and is compared for the self-reference check, so a display flag would change equality. The method labels a withheld load `ws (sha7)` with the existing `PinLoadWithheld::for_commit`.
4. **A non-git, unknown or library owner of a templated flow-step ref is a 400 at creation** (§ 4.2 rule 4). § 12 lists "non-git owner" among step failures. Rule 4 wins: the owner comes from the action name, which is known at creation.
5. **Lease timing is injectable.** `settlement::deferred::LeaseTiming { lease, renew }`, default `DEFERRED_LEASE_SECS = 60` / `DEFERRED_LEASE_RENEW_SECS = 20`. It is `#[doc(hidden)] pub` (like `PinStore::hold_loads_for_test`), so the lease tests take seconds, not minutes.
6. **`ready_at = NOW()` when a deferred step is resolved.** The unmatched-step sweep then measures from the moment the step became claimable, so a slow resolve does not count against `unmatched_step_timeout_secs`.
7. **The lease columns are not in `JobStepRow`**. Only SQL reads them. `ref_template` is in `JobStepRow`, `NewJobStep` and the API.
8. **Nested static refs met at resolution go through `job_ref_pin` too.** For example, a resolved action of `type: task` whose own `ref:` is static. The pass seeds its `RefMemo` from `job_ref_pin`, writes every new entry first-writer-wins, and rebuilds the row (at most 3 times) if another resolver wrote a different commit first. Success criterion 4 then holds for them as well.
9. **Restart.** `JobStepRepo::seed_steps_tx` writes only status, output, error and skip reason. A carried templated row therefore keeps `action_type = 'deferred'` and its `ref_template` with a terminal status. The pass never touches it (it is not `ready`). The UI label keys on `action_type === 'deferred'`, whatever the status.
10. **The ref secret set uses `collect_redaction_values`**, which drops values of 3 characters or fewer, the same floor as job-detail redaction. Tests use longer secrets. The user guide states the floor.
11. **`RenderedRefError` is a leaf.** It has no `source()`, it is built fresh, and it is never `.context()`-wrapped over the error it replaces. `AppError::Internal` and the webhook handler print `{:#}` chains, and the replaced error can carry the rendered value (`PinUnavailable` embeds the ref, `pins.rs:800-805`). The new paths never log the replaced chain. `RenderedRefError` carries `workspace` (a configured name) and, for `Render`, the value-free `TemplateError` text.
12. **A failed literal connection pre-check at resolution is `Unresolvable`** (§ 5.5 lists the failure; the kind list has no entry for it, and § 5.5 maps an untyped error to `Unresolvable`). A typed marker was considered and dropped: `.context(marker)` would replace the outer message that `classify_execute_error`'s "resolve connection" phrase reads at creation.
13. **§ 5.6 stamping.** After dispatch resolves a templated task ref, `JobStepRepo::stamp_task_pin` writes `task_*` on the parent step (guarded `status = 'ready' AND task_ref IS NULL`) before the existing path reads them. A re-dispatch then sees the stamp and does not render again.
14. **`MissingAtRef`.** `job_creator::action_at` / `task_named_at` return a typed `MissingAtRef` error (same `Display` text as today, so `classify_execute_error`'s "has no action" / "has no task" phrases still match). The templated paths classify it as `NoAction` by type.

## Global Constraints

- Migrations: `crates/stroem-db/migrations/051_templated_refs.sql` (columns, table, constraint widened `NOT VALID`) and `052_templated_refs_validate.sql` (`VALIDATE CONSTRAINT`). Both re-runnable.
- New columns: `job_step.ref_template TEXT`, `job_step.resolve_lease_owner UUID`, `job_step.resolve_lease_until TIMESTAMPTZ`. New table: `job_ref_pin (job_id UUID REFERENCES job ON DELETE CASCADE, workspace TEXT, git_ref TEXT, commit TEXT, PRIMARY KEY (job_id, workspace, git_ref))`.
- A deferred row: `action_type = 'deferred'`, `required_ability = 'deferred'`, `runner = 'none'`, `required_tags = []`, `action_workspace` = the owner, `action_name` = the owner's local name, `ref_template` = the template as written, `action_spec`/`action_image`/`action_revision`/`action_ref`/`task_*` = `NULL`. Constant `stroem_common::models::job::DEFERRED_ACTION_TYPE = "deferred"`.
- `KNOWN_ABILITIES` stays `["script", "docker", "kubernetes", "agent"]` and never contains `"deferred"`. The claim SQL, the claim index and `idx_job_step_ready_ability` do not change.
- Constants: `DEFERRED_LEASE_SECS = 60`, `DEFERRED_LEASE_RENEW_SECS = 20`.
- A ref template is a template iff it contains `{{` or `{%` (`stroem_common::git_ref::classify_ref`). Post-render check, in this order: trim; empty → `Empty`; contains a ref-secret-set value → `SecretValue`; `parse_git_ref` refuses → `Invalid`.
- Step failure message format: `[ref] step '{step}': {RenderedRefError}`. Example, verbatim from the spec: `[ref] step 'publish': the rendered ref was not found in workspace 'billing'`.
- `RenderedRefKind`: `Empty`, `Invalid`, `SecretValue`, `NotFound { short_sha }`, `NotGit`, `NoAction`, `AgentAction`, `LoadWithheld`, `Unavailable`, `Render`, `Unresolvable`. `classify_execute_error`: `Unavailable` → 500, every other kind → 400.
- Validation warning texts: `Task '{t}' step '{s}' references action '{a}' at a templated ref - validation skipped (resolved when the step starts)`; `Action '{a}' references task '{t}' at a templated ref - validation skipped (resolved when the step starts)`; `Trigger '{n}' targets task '{t}' at a templated ref - validation skipped (resolved when the webhook fires)`. Scheduler error: `Trigger '{n}': a templated \`ref\` is only supported on webhook triggers`.
- Conventions (CLAUDE.md): `anyhow::Result` + `.context()`; `#[tracing::instrument]` on public functions with `skip_all`/`skip(...)` and explicit id fields, never a whole struct and never a rendered ref value; sqlx runtime queries only; unit tests in-module; integration tests are `mod` lines in the crate's `tests/main.rs`, never new targets; DB tests get pools from `stroem-test-support` (`crate::common::setup_db()` in stroem-db, `pinned_workspace_fixture` in stroem-server).
- Commits: conventional messages. **No `Co-Authored-By` trailer** (the user's global CLAUDE.md overrides any other attribution instruction). Before each commit run `cargo fmt --all` and `cargo clippy -p <touched crate> --all-targets -- -D warnings`.
- Disk: the host disk runs near full. Build with `CARGO_PROFILE_DEV_DEBUG=0`. Never give a subagent its own `CARGO_TARGET_DIR`. Run tests per crate and per file (`cargo test -p stroem-server --test integration git_refs_deferred_test::`), not `--workspace`, until Task 18.
- Rollout rule (spec § 11): merge no YAML with a templated `ref:` before every server replica runs the release. Workers do not change.
- Work in a git worktree on a feature branch (`feat/templated-refs`), created by superpowers:using-git-worktrees at execution time. Before merge, the branch goes to Codex for the implementation review (user rule).

## Review Focus

Inputs and conditions the spec implies but its § 12 does not test. Each has a pinning test in the owning task.

1. **A rendered value with surrounding whitespace or a newline** (a Run-form value pasted with `\n`, a webhook body). The value is trimmed before the parse and resolves like the clean value. Unit test in Task 1, integration test in Task 10.
2. **`for_each` over objects with `ref: "{{ each.item }}"`.** The instance renders JSON text such as `{"b": "release/2.3"}`, which is not a ref name. The instance fails with the fixed `Invalid` sentence, and its message carries no part of the rendered text. Test in Task 10.
3. **A resolved action with its own `retry:` while the flow step has none.** The resolved row carries the action's retry columns (`max_retries = max_attempts - 1`). Test in Task 10.
4. **A resolved step that fails and retries.** `fail_or_retry` resets it to `ready` with its real `action_type`, the same `action_revision`, and no `ref_template` work: it is never re-resolved and is claimed again on the same commit. Test in Task 11.
5. **Restart from a templated step.** The restart job's templated step gets a fresh deferred row and resolves again. A carried templated row stays terminal (`action_type = 'deferred'`) and the pass never touches it. Test in Task 11.

---

## Task Map

| # | Task | Part | Depends on |
|---|---|---|---|
| 1 | `classify_ref`, post-render check, `vals`-free ref engine (stroem-common) | A | — |
| 2 | Validation of templated refs (stroem-common) | A | 1 |
| 3 | Migrations 051/052, `ref_template` through rows, R4 copy | B | — |
| 4 | `job_ref_pin` repo and the PinStore keep-set | B | 3 |
| 5 | Deferred-step DB primitives, sweep clause, EXPLAIN check | B | 3, 4 |
| 6 | `claim_job` rejects unknown capabilities | C | — |
| 7 | Render-context scopes `StepRef`, `TaskActionRef`, `webhook_ref` | C | 3 |
| 8 | Privacy: resolve span, private pin load, `RenderedRefError`, classifier, capture helper | C | 1 |
| 9 | Job creation: `resolve_step_row`, deferred rows, memo persisted | D | 1, 3, 4, 8 |
| 10 | The resolution pass, wired into `advance` and `init` | D | 5, 7, 8, 9 |
| 11 | Lease, concurrency, cancel, privacy, retry and restart tests | D | 10 |
| 12 | Recovery Phase 4.6 | D | 5, 10 |
| 13 | `type: task` action templated ref at dispatch (§ 5.6) | E | 10 |
| 14 | Webhook templated ref (§ 6) | E | 8, 10 |
| 15 | `ref_template` in the API, the UI label | E | 3 |
| 16 | Redaction read-path audit and E2E | E | 10, 13, 14 |
| 17 | Documentation | E | all |
| 18 | Final verification | E | all |

Tasks run in numeric order.

---
## Part A — stroem-common

### Task 1: `classify_ref`, the post-render check, and the `vals`-free ref engine

Spec § 4.2 rules 1–3, contract deviation 2. Pure functions only; nothing calls them yet.

**Files:**
- Modify: `crates/stroem-common/src/git_ref.rs` (append after `short_sha_hint`, `:79`; tests in `mod tests`, `:110`)
- Modify: `crates/stroem-common/src/tera_engine.rs` (after `check_engine`, `:47`)
- Modify: `crates/stroem-common/src/template.rs` (after `check_template_syntax`, `:110`; tests in `mod tests`, `:1305`)
- Modify: `crates/stroem-common/src/models/job.rs` (a constant next to `StepStatus`, `:76`)

**Interfaces:**
- Consumes: `parse_git_ref` (`git_ref.rs:51`), `TemplateError::{from_tera, context_conversion}` (`template_error.rs`, crate-private).
- Produces:
  ```rust
  // stroem_common::git_ref
  pub enum RefKind { Static, Template }
  pub fn classify_ref(s: &str) -> RefKind;
  pub enum RenderedRefRejection { Empty, SecretValue, Invalid }
  pub fn check_rendered_ref(rendered: &str, secrets: &[String]) -> Result<String, RenderedRefRejection>;
  // stroem_common::template
  pub fn check_ref_template_syntax(src: &str) -> std::result::Result<(), TemplateError>;
  pub fn render_ref_template(template: &str, context: &serde_json::Value) -> anyhow::Result<String>;
  // stroem_common::models::job
  pub const DEFERRED_ACTION_TYPE: &str = "deferred";
  ```

- [ ] **Step 1: Write the failing tests for `classify_ref` and `check_rendered_ref`**

Append to `mod tests` in `crates/stroem-common/src/git_ref.rs`:

```rust
    #[test]
    fn classify_ref_sees_expression_and_statement_markers_only() {
        assert_eq!(classify_ref("release/2.3"), RefKind::Static);
        assert_eq!(classify_ref("{{ input.branch }}"), RefKind::Template);
        assert_eq!(classify_ref("{% if a %}x{% endif %}"), RefKind::Template);
        assert_eq!(classify_ref("rel-{{ input.n }}"), RefKind::Template);
        // A comment marker alone is not a template (spec § 4.2 rule 1).
        assert_eq!(classify_ref("{#c#}main"), RefKind::Static);
    }

    #[test]
    fn rendered_ref_is_trimmed_before_the_parse() {
        assert_eq!(
            check_rendered_ref("  release/2.3\n", &[]),
            Ok("release/2.3".to_string())
        );
        assert_eq!(check_rendered_ref(SHA, &[]), Ok(SHA.to_string()));
        // An abbreviated SHA is a bare NAME here; resolution answers not-found
        // with the hint (spec § 12 "short-SHA hint").
        assert_eq!(check_rendered_ref("3f2a9c0", &[]), Ok("3f2a9c0".to_string()));
    }

    #[test]
    fn rendered_ref_rejections_in_spec_order() {
        assert_eq!(check_rendered_ref(" \n\t", &[]), Err(RenderedRefRejection::Empty));
        let secrets = vec!["hunter2-long".to_string(), String::new()];
        // The secret check runs before the parse, so an invalid name that
        // carries a secret is reported as the secret.
        assert_eq!(
            check_rendered_ref("feature/hunter2-long", &secrets),
            Err(RenderedRefRejection::SecretValue)
        );
        assert_eq!(
            check_rendered_ref("a..hunter2-long", &secrets),
            Err(RenderedRefRejection::SecretValue)
        );
        // An empty secret never matches everything.
        assert_eq!(check_rendered_ref("main", &secrets), Ok("main".to_string()));
        assert_eq!(check_rendered_ref("a..b", &[]), Err(RenderedRefRejection::Invalid));
        // A value cannot render into another template.
        assert_eq!(check_rendered_ref("{{ x }}", &[]), Err(RenderedRefRejection::Invalid));
        assert_eq!(check_rendered_ref("refs/pull/1", &[]), Err(RenderedRefRejection::Invalid));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib git_ref::tests`
Expected: compile error — `classify_ref`, `RefKind`, `check_rendered_ref`, `RenderedRefRejection` not found.

- [ ] **Step 3: Implement them**

Append to `crates/stroem-common/src/git_ref.rs` after `short_sha_hint`:

```rust
/// Whether a `ref:` value is a template (spec 2026-10-15 § 4.2 rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Static,
    Template,
}

/// `{{` or `{%` makes a `ref:` a template; anything else is a static ref,
/// parsed by [`parse_git_ref`].
pub fn classify_ref(s: &str) -> RefKind {
    if s.contains("{{") || s.contains("{%") {
        RefKind::Template
    } else {
        RefKind::Static
    }
}

/// Why a RENDERED ref value is refused (spec 2026-10-15 § 4.2 rule 3). It
/// carries no part of the value: callers turn it into a value-free message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedRefRejection {
    /// Empty after trimming (D6).
    Empty,
    /// Contains a value of the ref secret set (§ 10.3).
    SecretValue,
    /// Not a branch, a tag or a full commit SHA — a template marker included.
    Invalid,
}

/// The post-render check: trim; refuse an empty value; refuse a value that
/// contains any of `secrets` (empty strings ignored); refuse anything
/// [`parse_git_ref`] refuses. Returns the trimmed value, which is the string
/// the ref memo is keyed by.
pub fn check_rendered_ref(
    rendered: &str,
    secrets: &[String],
) -> Result<String, RenderedRefRejection> {
    let value = rendered.trim();
    if value.is_empty() {
        return Err(RenderedRefRejection::Empty);
    }
    if secrets
        .iter()
        .any(|s| !s.is_empty() && value.contains(s.as_str()))
    {
        return Err(RenderedRefRejection::SecretValue);
    }
    parse_git_ref(value).map_err(|_| RenderedRefRejection::Invalid)?;
    Ok(value.to_string())
}
```

Add to `crates/stroem-common/src/models/job.rs` directly above `pub enum StepStatus` (`:76`):

```rust
/// `job_step.action_type` (and `required_ability`) of a flow step whose `ref:`
/// is a template and has not been resolved yet (spec 2026-10-15 § 5.1). No
/// worker can claim it: no claim may carry the ability `deferred`.
pub const DEFERRED_ACTION_TYPE: &str = "deferred";
```

- [ ] **Step 4: Run the git_ref tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib git_ref::tests`
Expected: PASS (all tests in the module, old and new).

- [ ] **Step 5: Write the failing tests for the ref engine**

Append to `mod tests` in `crates/stroem-common/src/template.rs` (next to `check_template_syntax_accepts_vals_and_json_encode`, `:1750`):

```rust
    #[test]
    fn ref_template_syntax_refuses_vals_but_accepts_plain_filters() {
        // `check_template_syntax` accepts `vals` (identity on the check
        // engine); a ref template must not (spec 2026-10-15 § 4.2 rule 2).
        let e = check_ref_template_syntax("{{ input.b | vals }}").unwrap_err();
        assert_eq!(e.message(), "template uses an unknown filter");
        assert!(check_ref_template_syntax("{{ input.b | default(value='main') | trim }}").is_ok());
        assert!(check_ref_template_syntax("rel-{{ input.n }}").is_ok());
        // A runtime error on the empty context means the template compiled.
        assert!(check_ref_template_syntax("{{ undefined.field }}").is_ok());
        assert!(check_ref_template_syntax("{{ input.b ").is_err());
        assert!(check_ref_template_syntax("{{ input.b | nope }}").is_err());
    }

    #[test]
    fn render_ref_template_renders_and_refuses_vals_at_runtime() {
        let ctx = json!({"input": {"b": "release/2.3"}});
        assert_eq!(render_ref_template("{{ input.b }}", &ctx).unwrap(), "release/2.3");
        let err = render_ref_template("{{ 'ref+vault://x' | vals }}", &ctx).unwrap_err();
        let te = err.downcast_ref::<TemplateError>().expect("a TemplateError");
        assert_eq!(te.message(), "template uses an unknown filter");
        let err = render_ref_template("{{ input.nope }}", &ctx).unwrap_err();
        let te = err.downcast_ref::<TemplateError>().expect("a TemplateError");
        assert!(te.message().contains("undefined"), "{}", te.message());
    }
```

- [ ] **Step 6: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib template::tests::ref_template`
Expected: compile error — `check_ref_template_syntax` / `render_ref_template` not found.

- [ ] **Step 7: Implement the ref engine**

Append to `crates/stroem-common/src/tera_engine.rs`:

```rust
/// Engine for `ref:` templates (spec 2026-10-15 § 4.2 rule 2): the builtins
/// and the promised filters, but no `vals` — a ref is an identifier, never a
/// secret lookup. `| vals` is therefore an unknown filter at compile time.
pub(crate) fn ref_engine() -> tera::Tera {
    BASE.clone()
}
```

Append to `crates/stroem-common/src/template.rs` directly after `check_template_syntax` (`:110`):

```rust
/// [`check_template_syntax`] for a `ref:` template: compiled on the ref engine,
/// which has no `vals` (spec 2026-10-15 § 4.2 rule 2), so `| vals` is a compile
/// error here. A runtime error on the empty context means it compiled.
pub fn check_ref_template_syntax(src: &str) -> std::result::Result<(), TemplateError> {
    let tera = crate::tera_engine::ref_engine();
    match tera.render_str(src, &tera::Context::new(), false) {
        Ok(_) => Ok(()),
        Err(e) if matches!(e.kind(), tera::ErrorKind::RenderingError(_)) => Ok(()),
        Err(e) => Err(TemplateError::from_tera(&e, Some(src), None)),
    }
}

/// Render a `ref:` template against `context` on the ref engine (no `vals`).
/// Errors are value-free [`TemplateError`]s, like every render.
pub fn render_ref_template(template: &str, context: &serde_json::Value) -> Result<String> {
    let tera = crate::tera_engine::ref_engine();
    let ctx = tera::Context::from_serialize(context)
        .map_err(|e| anyhow::Error::new(TemplateError::context_conversion(&e)))
        .context("Failed to convert JSON to Tera context")?;
    tera.render_str(template, &ctx, false).map_err(|e| {
        let outer = if matches!(e.kind(), tera::ErrorKind::SyntaxError(_)) {
            "Failed to parse template"
        } else {
            "Failed to render template"
        };
        anyhow::Error::new(TemplateError::from_tera(&e, Some(template), None)).context(outer)
    })
}
```

The `#[allow(dead_code)]` on `tera_engine::base()` stays; `ref_engine` reads `BASE` directly.

- [ ] **Step 8: Run the stroem-common tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib`
Expected: PASS. If `te.message()` for the undefined variable reads differently, keep the `contains("undefined")` assertion and do not weaken the `vals` assertions.

- [ ] **Step 9: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-common --all-targets -- -D warnings
git add crates/stroem-common/src/git_ref.rs crates/stroem-common/src/tera_engine.rs crates/stroem-common/src/template.rs crates/stroem-common/src/models/job.rs
git commit -m "feat(common): classify templated refs, post-render ref check, vals-free ref engine"
```

---

### Task 2: Validation of templated refs

Spec § 7, § 4.1. `stroem validate` accepts a template in the three permitted places, skips the lookup with a warning, and rejects it everywhere else.

**Files:**
- Modify: `crates/stroem-common/src/validation.rs` — imports (`:1-4`); action loop (`:110-131`); flow-step ref branch (`:216-221`); trigger loop call (`:447-458`); `validate_trigger_task` (`:1301-1319`); new helper next to `check_git_ref` (`:1294`); tests (`mod tests`, `:2317`; the ref tests start at `:8242`)

**Interfaces:**
- Consumes: Task 1's `classify_ref`, `RefKind`, `template::check_ref_template_syntax`.
- Produces: no new public items. `validate_trigger_task` gains a trailing `allow_template: bool`.

- [ ] **Step 1: Replace the old "templated is rejected" test with the new behaviour (failing)**

In `crates/stroem-common/src/validation.rs` `mod tests`, replace the whole `git_ref_templated_or_invalid_is_rejected` test (`:8258-8286`) with:

```rust
    #[test]
    fn templated_step_ref_is_accepted_with_a_warning() {
        let warnings = warnings_of(
            r#"
actions:
  a:
    type: script
    script: echo hi
tasks:
  t:
    flow:
      s:
        action: a
        ref: "{{ input.release }}"
"#,
        );
        assert!(
            warnings.contains(
                &"Task 't' step 's' references action 'a' at a templated ref - validation skipped (resolved when the step starts)"
                    .to_string()
            ),
            "{warnings:?}"
        );
    }

    #[test]
    fn invalid_static_step_ref_is_still_rejected() {
        let err = error_of(
            r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: "a..b"
"#,
        );
        assert!(err.contains("invalid `ref`"), "{err}");
    }

    #[test]
    fn templated_ref_syntax_errors_and_vals_are_rejected() {
        for (template, needle) in [
            ("{{ input.release ", "template syntax error"),
            ("{{ input.release | nope }}", "unknown filter"),
            ("{{ input.release | vals }}", "unknown filter"),
        ] {
            let err = error_of(&format!(
                r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: "{template}"
"#
            ));
            assert!(err.contains("has an invalid `ref` template"), "{template}: {err}");
            assert!(err.contains(needle), "{template}: {err}");
        }
    }

    #[test]
    fn templated_task_action_ref_is_accepted_with_a_warning() {
        let warnings = warnings_of(
            r#"
actions:
  call:
    type: task
    task: other
    ref: "{{ input.branch }}"
tasks:
  t:
    flow:
      s:
        action: call
"#,
        );
        assert!(
            warnings.contains(
                &"Action 'call' references task 'other' at a templated ref - validation skipped (resolved when the step starts)"
                    .to_string()
            ),
            "{warnings:?}"
        );
    }

    #[test]
    fn templated_ref_on_a_non_task_action_is_rejected() {
        let err = error_of(
            r#"
actions:
  a:
    type: script
    script: echo hi
    ref: "{{ input.branch }}"
tasks:
  t:
    flow:
      s:
        action: a
"#,
        );
        assert!(err.contains("`ref` is only valid on type: task actions"), "{err}");
    }

    #[test]
    fn templated_webhook_ref_is_accepted_and_scheduler_ref_is_rejected() {
        let warnings = warnings_of(
            r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: main
triggers:
  on-push:
    type: webhook
    name: push
    task: t
    ref: "{{ input.body.ref }}"
"#,
        );
        assert!(
            warnings.contains(
                &"Trigger 'on-push' targets task 't' at a templated ref - validation skipped (resolved when the webhook fires)"
                    .to_string()
            ),
            "{warnings:?}"
        );
        let err = error_of(
            r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: main
triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * *"
    task: t
    ref: "{{ input.branch }}"
"#,
        );
        assert!(
            err.contains("Trigger 'nightly': a templated `ref` is only supported on webhook triggers"),
            "{err}"
        );
    }

    #[test]
    fn templated_ref_stays_rejected_where_ref_itself_is() {
        let hook = error_of(
            r#"
actions:
  a:
    type: script
    script: echo hi
tasks:
  t:
    flow:
      s:
        action: a
    on_success:
      - action: a
        ref: "{{ input.branch }}"
"#,
        );
        assert!(hook.contains("`ref` is not supported on hooks yet"), "{hook}");
        let tool = error_of(
            r#"
actions:
  ask:
    type: agent
    provider: anthropic
    model: m
    prompt: hi
    tools:
      - task: t
        ref: "{{ input.branch }}"
tasks:
  t:
    flow:
      s:
        action: ask
"#,
        );
        assert!(tool.contains("`ref` is not supported on agent task tools yet"), "{tool}");
    }
```

If the agent-action YAML above fails validation for an unrelated reason (for example a provider that must be configured), copy the minimal agent action from the existing `git_ref_on_agent_task_tool_is_rejected` test (`:8452`) instead; the assertion stays the same. The event-source rejection is already pinned by `git_ref_on_event_source_is_rejected` (`:8421`) and covers any `ref`, templated or not.

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib validation::tests::templated`
Expected: FAIL — the step and task-action tests fail with "is a template; refs are static strings"; the webhook test fails the same way.

- [ ] **Step 3: Implement**

1. Imports (`:1-4`): add `use crate::git_ref::{classify_ref, RefKind};`.

2. Next to `check_git_ref` (`:1294`), add:

```rust
/// A templated `ref:` (spec 2026-10-15 § 7): syntax only, on the ref engine —
/// it has no `vals`, so `| vals` is an unknown filter here.
fn check_ref_template(label: &str, template: &str) -> Result<()> {
    crate::template::check_ref_template_syntax(template).map_err(|e| {
        anyhow::Error::new(e).context(format!("{label} has an invalid `ref` template"))
    })
}
```

3. Action loop — replace `check_git_ref(&format!("Action '{action_name}'"), r)?;` (`:118`) with:

```rust
            let label = format!("Action '{action_name}'");
            match classify_ref(r) {
                RefKind::Static => check_git_ref(&label, r)?,
                RefKind::Template => check_ref_template(&label, r)?,
            }
```

and the skip warning (`:128-131`) with:

```rust
            if let Some(r) = action.git_ref.as_deref() {
                let (at, when) = match classify_ref(r) {
                    RefKind::Static => (format!("at ref '{r}'"), "resolved at job creation"),
                    RefKind::Template => (
                        "at a templated ref".to_string(),
                        "resolved when the step starts",
                    ),
                };
                warnings.push(format!(
                    "Action '{}' references task '{}' {} - validation skipped ({})",
                    action_name, task_ref, at, when
                ));
            } else if config.tasks.contains_key(task_ref) {
```

4. Flow-step branch (`:216-221`) becomes:

```rust
            if let Some(r) = step.git_ref.as_deref() {
                let label = format!("Task '{task_name}' step '{step_name}'");
                let at = match classify_ref(r) {
                    RefKind::Static => {
                        check_git_ref(&label, r)?;
                        format!("at ref '{r}' - validation skipped (resolved at job creation)")
                    }
                    RefKind::Template => {
                        check_ref_template(&label, r)?;
                        "at a templated ref - validation skipped (resolved when the step starts)"
                            .to_string()
                    }
                };
                warnings.push(format!(
                    "Task '{}' step '{}' references action '{}' {}",
                    task_name, step_name, action_ref, at
                ));
            } else if !libraries_resolved && action_ref.contains('.') {
```

5. Trigger loop call (`:450-458`): pass `matches!(trigger, crate::models::workflow::TriggerDef::Webhook { .. })` as the new last argument of `validate_trigger_task`.

6. `validate_trigger_task` (`:1304-1319`): add the parameter `allow_template: bool` after `warnings`, and replace its `if let Some(r) = git_ref { … }` block with:

```rust
    if let Some(r) = git_ref {
        let label = format!("Trigger '{trigger_name}'");
        match classify_ref(r) {
            RefKind::Static => {
                check_git_ref(&label, r)?;
                warnings.push(format!(
                    "Trigger '{trigger_name}' targets task '{task}' at ref '{r}' - validation skipped (resolved when the trigger fires)"
                ));
            }
            RefKind::Template if allow_template => {
                check_ref_template(&label, r)?;
                warnings.push(format!(
                    "Trigger '{trigger_name}' targets task '{task}' at a templated ref - validation skipped (resolved when the webhook fires)"
                ));
            }
            RefKind::Template => bail!(
                "Trigger '{trigger_name}': a templated `ref` is only supported on webhook triggers"
            ),
        }
        return Ok(());
    }
```

Hooks, event sources and agent task tools already refuse any `ref` before they look at it; leave them unchanged.

- [ ] **Step 4: Run the whole validation module**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib validation::tests`
Expected: PASS, including the unchanged ref tests (`git_ref_step_skips_the_existence_check_with_a_warning` still sees `at ref 'release/2.3' - validation skipped (resolved at job creation)`).

- [ ] **Step 5: Run the CLI tests (they print validation output)**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-cli`
Expected: PASS. If a CLI snapshot quotes the old "is a template" error, update it to the new behaviour.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-common --all-targets -- -D warnings
git add crates/stroem-common/src/validation.rs
git commit -m "feat(validate): accept templated refs on flow steps, task actions and webhooks"
```

---
## Part B — stroem-db

### Task 3: Migrations 051/052, `ref_template` through the step rows, R4 copy

Spec § 8, § 5.1, § 5.4. Schema plus plumbing; no behaviour yet.

**Files:**
- Create: `crates/stroem-db/migrations/051_templated_refs.sql`, `crates/stroem-db/migrations/052_templated_refs_validate.sql`
- Modify: `crates/stroem-db/src/repos/job_step.rs` — `step_columns!` (`:10-14`); `JobStepRow` (`:16-82`) + `test_default` (`:84-133`); `NewJobStep` (`:135-174`); `StepInsertRow` (`:176-210`); `create_steps_tx` (`:224-333`)
- Modify: `crates/stroem-server/src/cascade.rs` — R4 instance literal (`:1475-1506` in `phase_placeholders`); unit test next to `expand_instances_copy_the_placeholder_pins` (`:1684`)
- Mechanical: every `NewJobStep { … }` literal that lists all fields (≈ 93 sites, the files are listed in Step 5)
- Create: `crates/stroem-db/tests/templated_refs_test.rs`; Modify: `crates/stroem-db/tests/main.rs`, `crates/stroem-db/tests/migration_test.rs`

**Interfaces:**
- Produces: `JobStepRow.ref_template: Option<String>`, `NewJobStep.ref_template: Option<String>` (last field of each). Tables/columns of the Global Constraints.

- [ ] **Step 1: Write the failing migration test**

Append to `crates/stroem-db/tests/migration_test.rs`:

```rust
/// Migrations 051 + 052 (spec 2026-10-15 § 8): the new columns and the memo
/// table exist, the action-type check accepts `deferred` and is VALID after
/// 052, and both files are re-runnable.
#[tokio::test]
async fn test_051_052_templated_refs_schema() -> Result<()> {
    let pool = setup_db().await?;

    for (table, column) in [
        ("job_step", "ref_template"),
        ("job_step", "resolve_lease_owner"),
        ("job_step", "resolve_lease_until"),
        ("job_ref_pin", "job_id"),
        ("job_ref_pin", "workspace"),
        ("job_ref_pin", "git_ref"),
        ("job_ref_pin", "commit"),
    ] {
        let found: Option<(String,)> = sqlx::query_as(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_optional(&pool)
        .await?;
        assert!(found.is_some(), "{table}.{column} missing");
    }

    let job_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type) \
         VALUES ($1, 'etl', 't', 'distributed', 'running', 'api')",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, \
                               required_ability, runner, ref_template) \
         VALUES ($1, 'run', 'hello', 'deferred', 'ready', 'deferred', 'none', '{{ input.b }}')",
    )
    .bind(job_id)
    .execute(&pool)
    .await
    .expect("a deferred row inserts after 051");
    let bogus = sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, runner) \
         VALUES ($1, 'x', 'a', 'bogus', 'ready', 'none')",
    )
    .bind(job_id)
    .execute(&pool)
    .await;
    assert!(bogus.is_err(), "an unknown action type is still rejected");

    let valid = |pool: PgPool| async move {
        let (v,): (bool,) = sqlx::query_as(
            "SELECT convalidated FROM pg_constraint WHERE conname = 'job_step_action_type_check'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        v
    };
    assert!(valid(pool.clone()).await, "052 validated the widened check");

    sqlx::raw_sql(include_str!("../migrations/051_templated_refs.sql"))
        .execute(&pool)
        .await?;
    assert!(!valid(pool.clone()).await, "051 re-adds the check NOT VALID");
    sqlx::raw_sql(include_str!("../migrations/052_templated_refs_validate.sql"))
        .execute(&pool)
        .await?;
    assert!(valid(pool.clone()).await);
    Ok(())
}
```

Also add `"job_ref_pin",` to the `expected` table list of `test_schema_completeness` (`:54-62`).

- [ ] **Step 2: Run it to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration migration_test::`
Expected: FAIL — `job_step.ref_template missing` (and `job_ref_pin` not found).

- [ ] **Step 3: Write the migrations**

`crates/stroem-db/migrations/051_templated_refs.sql`:

```sql
-- Templated refs (spec docs/superpowers/specs/2026-10-15-templated-refs-design.md § 8).
-- Additive. A flow step whose `ref:` is a template is created DEFERRED
-- (`action_type = 'deferred'`): it waits at `ready` until the server renders
-- `ref_template`, resolves the commit and writes the step's real columns.

ALTER TABLE job_step ADD COLUMN IF NOT EXISTS ref_template TEXT;
-- One resolver per deferred step: a short renewable lease with an owner
-- token (§ 5.3 step 0, D12). Never read by a worker.
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS resolve_lease_owner UUID;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS resolve_lease_until TIMESTAMPTZ;

-- One commit per (job, owner workspace, ref as written / rendered) for the
-- job's whole life (§ 5.2). First writer wins: INSERT … ON CONFLICT DO NOTHING.
CREATE TABLE IF NOT EXISTS job_ref_pin (
    job_id    UUID NOT NULL REFERENCES job(job_id) ON DELETE CASCADE,
    workspace TEXT NOT NULL,
    git_ref   TEXT NOT NULL,
    commit    TEXT NOT NULL,
    PRIMARY KEY (job_id, workspace, git_ref)
);

-- Widen the action-type check (last set by 026_event_source.sql) with
-- 'deferred'. NOT VALID: no table scan under this migration's ACCESS
-- EXCLUSIVE lock. 052 validates it under SHARE UPDATE EXCLUSIVE.
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
    CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task', 'agent', 'approval', 'event_source', 'deferred')) NOT VALID;
```

`crates/stroem-db/migrations/052_templated_refs_validate.sql`:

```sql
-- Validate the check that 051 widened NOT VALID (spec 2026-10-15 § 8). A
-- migration of its own: sqlx runs each migration in one transaction, and a
-- VALIDATE inside 051 would scan job_step while holding 051's ACCESS
-- EXCLUSIVE lock. Here the scan takes SHARE UPDATE EXCLUSIVE; writes continue.
ALTER TABLE job_step VALIDATE CONSTRAINT job_step_action_type_check;
```

- [ ] **Step 4: Add `ref_template` to the step rows**

In `crates/stroem-db/src/repos/job_step.rs`:

1. `step_columns!` (`:12`): append `, ref_template` after `pin_releases`.
2. `JobStepRow`: after `pub pin_releases: i32,` add
   ```rust
       /// The `ref:` template of a flow step created deferred (spec
       /// 2026-10-15 § 5.1). Kept after resolution, for display.
       pub ref_template: Option<String>,
   ```
   and in `test_default`, after `pin_releases: 0,`, add `ref_template: None,`.
3. `NewJobStep`: after `pub task_revision: Option<String>,` add
   ```rust
       /// See [`JobStepRow::ref_template`].
       pub ref_template: Option<String>,
   ```
4. `StepInsertRow`: after `task_revision: Option<String>,` add `ref_template: Option<String>,`.
5. `create_steps_tx`: append `, ref_template` to the INSERT column list (after `task_revision`); `let cols_per_row = 30;`; in `rows.push(StepInsertRow { … })` add `ref_template: step.ref_template.clone(),`; after `.bind(row.task_revision)` add `.bind(row.ref_template)`.

- [ ] **Step 5: Fill every full `NewJobStep` literal**

Every literal that lists all fields (no `..Default::default()`) ends with a `task_revision: …,` line. Insert `ref_template: None,` after it, keeping the indentation:

```bash
for f in \
  crates/stroem-db/tests/integration_test.rs \
  crates/stroem-db/tests/job_step_status_tests.rs \
  crates/stroem-db/tests/tarball_keep_revisions_test.rs \
  crates/stroem-server/tests/cascade_apply_test.rs \
  crates/stroem-server/tests/integration_test.rs \
  crates/stroem-server/tests/orchestrator_test.rs \
  crates/stroem-server/tests/propagate_to_parent_test.rs \
  crates/stroem-server/src/job_creator.rs ; do
  perl -0pi -e 's/^(\s*)(task_revision: [^\n]*,)\n/$1$2\n$1ref_template: None,\n/mg' "$f"
done
```

Then, in the R4 instance literal in `crates/stroem-server/src/cascade.rs` (`:1505`), after `task_revision: r.task_revision.clone(),` add by hand:

```rust
                    // A deferred placeholder's instances are deferred too; each
                    // is resolved with its own `each` (spec 2026-10-15 § 5.4).
                    ref_template: r.ref_template.clone(),
```

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo check --workspace --tests 2>&1 | grep -E "^error|missing field" | head -40`
Expected: no `missing field \`ref_template\`` errors. Fix any remaining literal the compiler names by adding `ref_template: None,`. A `JobStepRow` literal that lists every field gets `ref_template: None,` after `pin_releases`.

- [ ] **Step 6: Write the round-trip and R4 tests**

Create `crates/stroem-db/tests/templated_refs_test.rs`:

```rust
//! Templated refs (spec 2026-10-15) — DB layer: the deferred row, the ref
//! memo, the resolution lease and guarded writes, the recovery listing.

use crate::common::{create_job, setup_db};
use anyhow::Result;
use stroem_db::{JobStepRepo, NewJobStep};
use uuid::Uuid;

/// A deferred `ready` step as job creation writes it (spec § 5.1).
pub(crate) fn deferred_step(job_id: Uuid, name: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "hello".to_string(),
        action_type: "deferred".to_string(),
        status: "ready".to_string(),
        required_ability: "deferred".to_string(),
        runner: "none".to_string(),
        action_workspace: Some("etl".to_string()),
        ref_template: Some("{{ input.branch }}".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn deferred_row_round_trips_with_its_template() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.action_type, "deferred");
    assert_eq!(row.required_ability, "deferred");
    assert_eq!(row.runner, "none");
    assert_eq!(row.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert!(row.action_spec.is_none() && row.action_ref.is_none());
    Ok(())
}
```

Add `mod templated_refs_test;` to `crates/stroem-db/tests/main.rs` (alphabetical, after `mod tarball_keep_revisions_test;`).

Append to the `mod tests` of `crates/stroem-server/src/cascade.rs`, after `expand_instances_copy_the_placeholder_pins`:

```rust
    /// A deferred placeholder's instances keep the template and the deferred
    /// kind, so the resolution pass can render each with its own `each`
    /// (spec 2026-10-15 § 5.4).
    #[test]
    fn expand_instances_copy_the_ref_template() {
        let t = task(vec![("p", fs(&[]))]);
        let mut p = placeholder("p", "pending", "[\"a\", \"b\"]");
        p.action_type = "deferred".into();
        p.required_ability = "deferred".into();
        p.ref_template = Some("{{ each.item }}".into());
        let plan = run(
            &t,
            &job(None),
            &[p],
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let instances = plan
            .changes
            .iter()
            .find_map(|c| match c {
                Change::Expand { instances, .. } => Some(instances),
                _ => None,
            })
            .expect("p expands");
        assert_eq!(instances.len(), 2);
        for i in instances {
            assert_eq!(i.action_type, "deferred", "{}", i.step_name);
            assert_eq!(i.required_ability, "deferred");
            assert_eq!(i.ref_template.as_deref(), Some("{{ each.item }}"));
        }
        assert_eq!(instances[0].status, "ready");
    }
```

- [ ] **Step 7: Run the tests**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration migration_test:: templated_refs_test::
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib cascade::tests
```
Expected: PASS.

- [ ] **Step 8: Run the full stroem-db suite (the literal fill touched its tests)**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-db -p stroem-server --all-targets -- -D warnings
git add crates/stroem-db crates/stroem-server/src/cascade.rs crates/stroem-server/src/job_creator.rs crates/stroem-server/tests
git commit -m "feat(db): migrations 051/052 — ref_template, resolution lease, job_ref_pin, deferred action type"
```

---

### Task 4: `job_ref_pin` repository and the PinStore keep-set

Spec § 5.2.

**Files:**
- Create: `crates/stroem-db/src/repos/job_ref_pin.rs`
- Modify: `crates/stroem-db/src/repos/mod.rs` (add `pub mod job_ref_pin;`), `crates/stroem-db/src/lib.rs` (re-export), `crates/stroem-db/src/repos/job.rs` (`pin_keep_set`, `:897-923`)
- Test: `crates/stroem-db/tests/templated_refs_test.rs`

**Interfaces:**
- Produces:
  ```rust
  // stroem_db
  #[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
  pub struct JobRefPinRow { pub workspace: String, pub git_ref: String, pub commit: String }
  pub struct JobRefPinRepo;
  impl JobRefPinRepo {
      pub async fn insert_many_tx<'e, E>(executor: E, job_id: Uuid, entries: &[JobRefPinRow]) -> Result<()>
          where E: sqlx::Executor<'e, Database = sqlx::Postgres>;
      pub async fn get(pool: &PgPool, job_id: Uuid, workspace: &str, git_ref: &str) -> Result<Option<String>>;
      pub async fn insert_first_wins(pool: &PgPool, job_id: Uuid, workspace: &str, git_ref: &str, commit: &str) -> Result<String>;
      pub async fn list_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobRefPinRow>>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-db/tests/templated_refs_test.rs` (extend the `use stroem_db::{…}` line with `JobRefPinRepo, JobRefPinRow, JobRepo`):

```rust
const C1: &str = "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901";
const C2: &str = "0123456789abcdef0123456789abcdef01234567";

fn entry(ws: &str, r: &str, c: &str) -> JobRefPinRow {
    JobRefPinRow {
        workspace: ws.to_string(),
        git_ref: r.to_string(),
        commit: c.to_string(),
    }
}

#[tokio::test]
async fn job_ref_pin_first_writer_wins() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    let mut tx = pool.begin().await?;
    JobRefPinRepo::insert_many_tx(
        &mut *tx,
        job_id,
        &[entry("etl", "release/2.3", C1), entry("billing", "v4.1.0", C2)],
    )
    .await?;
    tx.commit().await?;
    // A second writer of an existing key gets the FIRST commit back.
    let stored = JobRefPinRepo::insert_first_wins(&pool, job_id, "etl", "release/2.3", C2).await?;
    assert_eq!(stored, C1);
    // A new key is written and returned.
    let stored = JobRefPinRepo::insert_first_wins(&pool, job_id, "etl", "main", C2).await?;
    assert_eq!(stored, C2);
    // Keyed by the string as written: refs/heads/… is another key.
    assert_eq!(
        JobRefPinRepo::get(&pool, job_id, "etl", "refs/heads/release/2.3").await?,
        None
    );
    assert_eq!(
        JobRefPinRepo::get(&pool, job_id, "etl", "release/2.3").await?.as_deref(),
        Some(C1)
    );
    let all = JobRefPinRepo::list_for_job(&pool, job_id).await?;
    assert_eq!(
        all,
        vec![
            entry("billing", "v4.1.0", C2),
            entry("etl", "main", C2),
            entry("etl", "release/2.3", C1),
        ]
    );
    // insert_many_tx keeps an existing key too.
    let mut tx = pool.begin().await?;
    JobRefPinRepo::insert_many_tx(&mut *tx, job_id, &[entry("etl", "release/2.3", C2)]).await?;
    tx.commit().await?;
    assert_eq!(
        JobRefPinRepo::get(&pool, job_id, "etl", "release/2.3").await?.as_deref(),
        Some(C1)
    );
    // Deleting the job deletes its memo.
    JobRepo::delete(&pool, job_id).await?;
    assert!(JobRefPinRepo::list_for_job(&pool, job_id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn pin_keep_set_keeps_memo_commits_of_live_jobs_only() -> Result<()> {
    let pool = setup_db().await;
    let live = create_job(&pool, "etl", "t").await;
    let done = create_job(&pool, "etl", "t").await;
    sqlx::query("UPDATE job SET status = 'completed', completed_at = NOW() WHERE job_id = $1")
        .bind(done)
        .execute(&pool)
        .await?;
    JobRefPinRepo::insert_first_wins(&pool, live, "etl", "feature/x", C1).await?;
    JobRefPinRepo::insert_first_wins(&pool, done, "etl", "feature/y", C2).await?;
    let keep = JobRepo::pin_keep_set(&pool).await?;
    assert!(keep.contains(&("etl".to_string(), C1.to_string())), "{keep:?}");
    assert!(!keep.contains(&("etl".to_string(), C2.to_string())), "{keep:?}");
    Ok(())
}
```

`create_job` makes a `pending` job (`JobRepo::create`), which counts as live.

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration templated_refs_test::`
Expected: compile error — `JobRefPinRepo` not found.

- [ ] **Step 3: Implement the repository**

Create `crates/stroem-db/src/repos/job_ref_pin.rs`:

```rust
//! `job_ref_pin`: one commit per `(job, owner workspace, ref as written or
//! rendered)` for the job's whole life (spec 2026-10-15 § 5.2). Seeded with a
//! creation's `RefMemo` inside its transaction; extended by the deferred-step
//! pass. First writer wins.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

/// One memo entry.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct JobRefPinRow {
    pub workspace: String,
    pub git_ref: String,
    pub commit: String,
}

pub struct JobRefPinRepo;

impl JobRefPinRepo {
    /// Write `entries` for `job_id`; an existing key keeps its commit.
    pub async fn insert_many_tx<'e, E>(
        executor: E,
        job_id: Uuid,
        entries: &[JobRefPinRow],
    ) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        if entries.is_empty() {
            return Ok(());
        }
        let workspaces: Vec<String> = entries.iter().map(|e| e.workspace.clone()).collect();
        let refs: Vec<String> = entries.iter().map(|e| e.git_ref.clone()).collect();
        let commits: Vec<String> = entries.iter().map(|e| e.commit.clone()).collect();
        sqlx::query(
            "INSERT INTO job_ref_pin (job_id, workspace, git_ref, commit) \
             SELECT $1, w, r, c FROM UNNEST($2::text[], $3::text[], $4::text[]) AS t(w, r, c) \
             ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(workspaces)
        .bind(refs)
        .bind(commits)
        .execute(executor)
        .await
        .context("insert job_ref_pin entries")?;
        Ok(())
    }

    /// The commit `(workspace, git_ref)` resolved to in `job_id`, if any.
    pub async fn get(
        pool: &PgPool,
        job_id: Uuid,
        workspace: &str,
        git_ref: &str,
    ) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT commit FROM job_ref_pin WHERE job_id = $1 AND workspace = $2 AND git_ref = $3",
        )
        .bind(job_id)
        .bind(workspace)
        .bind(git_ref)
        .fetch_optional(pool)
        .await
        .context("get job_ref_pin entry")
    }

    /// First writer wins: write `commit` unless the key exists, then return
    /// the STORED commit — another resolver's, if it wrote first.
    pub async fn insert_first_wins(
        pool: &PgPool,
        job_id: Uuid,
        workspace: &str,
        git_ref: &str,
        commit: &str,
    ) -> Result<String> {
        sqlx::query(
            "INSERT INTO job_ref_pin (job_id, workspace, git_ref, commit) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(workspace)
        .bind(git_ref)
        .bind(commit)
        .execute(pool)
        .await
        .context("insert job_ref_pin entry")?;
        Self::get(pool, job_id, workspace, git_ref)
            .await?
            .context("job_ref_pin entry missing right after its insert")
    }

    /// Every entry of `job_id`, ordered by workspace then ref.
    pub async fn list_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobRefPinRow>> {
        sqlx::query_as::<_, JobRefPinRow>(
            "SELECT workspace, git_ref, commit FROM job_ref_pin \
             WHERE job_id = $1 ORDER BY workspace, git_ref",
        )
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("list job_ref_pin entries")
    }
}
```

`crates/stroem-db/src/repos/mod.rs`: add `pub mod job_ref_pin;` after `pub mod job_artifact;`.
`crates/stroem-db/src/lib.rs`: add `pub use repos::job_ref_pin::{JobRefPinRepo, JobRefPinRow};` after the `repos::job::{…}` re-export.

- [ ] **Step 4: Extend the keep-set**

In `JobRepo::pin_keep_set` (`crates/stroem-db/src/repos/job.rs:897`), add a fourth source to the doc list (`/// 4. every \`job_ref_pin\` commit of a non-terminal job (spec 2026-10-15 § 5.2), so a resolved commit stays in the PinStore until its step is claimed.`) and insert into the SQL, before the final `UNION SELECT workspace, revision FROM job WHERE status = 'failed' …`:

```sql
             UNION \
             SELECT p.workspace, p.commit FROM job_ref_pin p \
               JOIN job j ON j.job_id = p.job_id \
              WHERE j.status IN ('pending', 'running') \
```

- [ ] **Step 5: Run the tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration templated_refs_test:: tarball_keep_revisions_test::`
Expected: PASS (the keep-set tests in `tarball_keep_revisions_test.rs` still pass).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-db --all-targets -- -D warnings
git add crates/stroem-db
git commit -m "feat(db): job_ref_pin repository; the PinStore keep-set keeps memo commits of live jobs"
```

---

### Task 5: Deferred-step DB primitives, the sweep clause, the EXPLAIN check

Spec § 5.3 steps 0 and 7, § 5.5, § 5.8, § 8; contract deviations 1, 6, 7.

**Files:**
- Modify: `crates/stroem-db/src/repos/job_step.rs` — new methods in `impl JobStepRepo` (after `release_claim`, `:1024`); `get_unmatched_ready_steps` (`:1336-1351`)
- Modify: `crates/stroem-db/src/repos/job.rs` — new method after `get_stalled_pinned_jobs` (`:1419`)
- Modify: `crates/stroem-db/src/lib.rs` — re-export `DeferredWrite`
- Test: `crates/stroem-db/tests/templated_refs_test.rs`

**Interfaces:**
- Consumes: `NewJobStep` (Task 3).
- Produces:
  ```rust
  // stroem_db
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum DeferredWrite { Applied, JobTerminal, NotApplied }
  impl JobStepRepo {
      pub async fn take_resolve_lease(pool: &PgPool, job_id: Uuid, step_name: &str, token: Uuid, lease_secs: f64) -> Result<bool>;
      pub async fn renew_resolve_lease(pool: &PgPool, job_id: Uuid, step_name: &str, token: Uuid, lease_secs: f64) -> Result<bool>;
      pub async fn get_ready_deferred_steps(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>>;
      pub async fn resolve_deferred_tx(pool: &PgPool, job_id: Uuid, step_name: &str, token: Uuid, resolved: &NewJobStep) -> Result<DeferredWrite>;
      pub async fn fail_deferred_tx(pool: &PgPool, job_id: Uuid, step_name: &str, token: Uuid, error: &str) -> Result<DeferredWrite>;
      pub async fn stamp_task_pin(pool: &PgPool, job_id: Uuid, step_name: &str, workspace: &str, git_ref: &str, commit: &str) -> Result<bool>;
  }
  impl JobRepo {
      pub async fn get_jobs_with_stranded_deferred_steps(pool: &PgPool, older_than_secs: f64, after: Option<Uuid>, limit: i64) -> Result<Vec<Uuid>>;
  }
  ```
  `stamp_task_pin` is used by Task 13; it is here so every guarded `job_step` write of the feature is tested in one place.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-db/tests/templated_refs_test.rs` (extend the `use stroem_db::{…}` line with `DeferredWrite, WorkerRepo`; add `use sqlx::PgPool;`):

```rust
async fn lease_owner(pool: &PgPool, job_id: Uuid, step: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT resolve_lease_owner FROM job_step WHERE job_id = $1 AND step_name = $2")
        .bind(job_id)
        .bind(step)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn expire_lease(pool: &PgPool, job_id: Uuid, step: &str) {
    sqlx::query(
        "UPDATE job_step SET resolve_lease_until = NOW() - interval '1 second' \
         WHERE job_id = $1 AND step_name = $2",
    )
    .bind(job_id)
    .bind(step)
    .execute(pool)
    .await
    .unwrap();
}

/// What the resolution pass writes: a script action at etl@release/2.3.
fn resolved(job_id: Uuid, name: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "hello".to_string(),
        action_type: "script".to_string(),
        action_spec: Some(serde_json::json!({"type": "script", "script": "echo v1"})),
        status: "ready".to_string(),
        required_ability: "script".to_string(),
        runner: "local".to_string(),
        max_retries: Some(2),
        retry_backoff_secs: Some(1),
        retry_strategy: Some("fixed".to_string()),
        action_workspace: Some("etl".to_string()),
        action_revision: Some(C1.to_string()),
        action_ref: Some("release/2.3".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn resolution_lease_is_exclusive_until_it_expires() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", a, 60.0).await?);
    assert!(!JobStepRepo::take_resolve_lease(&pool, job_id, "run", b, 60.0).await?);
    assert!(JobStepRepo::renew_resolve_lease(&pool, job_id, "run", a, 60.0).await?);
    assert!(!JobStepRepo::renew_resolve_lease(&pool, job_id, "run", b, 60.0).await?);
    expire_lease(&pool, job_id, "run").await;
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", b, 60.0).await?);
    assert_eq!(lease_owner(&pool, job_id, "run").await, Some(b));
    // The old holder can no longer renew: its lease is lost.
    assert!(!JobStepRepo::renew_resolve_lease(&pool, job_id, "run", a, 60.0).await?);
    Ok(())
}

#[tokio::test]
async fn resolve_deferred_tx_writes_once_and_only_for_the_lease_holder() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let (holder, other) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", holder, 60.0).await?);
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;

    // Token fence: not the holder → nothing.
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", other, &resolved(job_id, "run")).await?,
        DeferredWrite::NotApplied
    );
    let before = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("SELECT NOW()")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", holder, &resolved(job_id, "run")).await?,
        DeferredWrite::Applied
    );
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.status, "ready");
    assert_eq!(row.action_type, "script");
    assert_eq!(row.required_ability, "script");
    assert_eq!(row.runner, "local");
    assert_eq!(row.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(row.action_revision.as_deref(), Some(C1));
    assert_eq!(row.max_retries, Some(2));
    assert_eq!(row.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert_eq!(lease_owner(&pool, job_id, "run").await, None);
    let ready_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT ready_at FROM job_step WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await?;
    assert!(ready_at >= before, "ready_at restarts at resolution");
    // A second write (a lost race) changes nothing.
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", holder, &resolved(job_id, "run")).await?,
        DeferredWrite::NotApplied
    );
    Ok(())
}

#[tokio::test]
async fn deferred_writes_on_a_cancelled_job_write_nothing() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let token = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", token, 60.0).await?);
    assert!(JobRepo::cancel(&pool, job_id).await?);
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", token, &resolved(job_id, "run")).await?,
        DeferredWrite::JobTerminal
    );
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "run", token, "[ref] step 'run': x").await?,
        DeferredWrite::JobTerminal
    );
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!((row.status.as_str(), row.action_type.as_str()), ("ready", "deferred"));
    Ok(())
}

#[tokio::test]
async fn fail_deferred_tx_never_overwrites_a_resolved_claimed_or_completed_row() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let token = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", token, 60.0).await?);
    // Another resolver resolved it (simulated by the holder's own write).
    JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", token, &resolved(job_id, "run")).await?;
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "run", token, "late").await?,
        DeferredWrite::NotApplied
    );
    // Claimed by a worker.
    let worker = Uuid::new_v4();
    WorkerRepo::register(&pool, worker, "w-df", &["script".to_string()], &[], false, None).await?;
    let claimed = JobStepRepo::claim_ready_step(&pool, &["script".to_string()], &[], false, worker).await?;
    assert_eq!(claimed.map(|s| s.step_name).as_deref(), Some("run"));
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "run", token, "late").await?,
        DeferredWrite::NotApplied
    );
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.status, "running");
    assert!(row.error_message.is_none());

    // The happy failure path: a deferred row, the holder → failed.
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "other")]).await?;
    let t2 = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "other", t2, 60.0).await?);
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "other", t2, "[ref] step 'other': the rendered ref is empty").await?,
        DeferredWrite::Applied
    );
    let row = JobStepRepo::get_step(&pool, job_id, "other").await?.unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_message.as_deref(), Some("[ref] step 'other': the rendered ref is empty"));
    assert!(row.completed_at.is_some());
    Ok(())
}

#[tokio::test]
async fn a_worker_with_every_ability_never_claims_a_deferred_step() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let caps: Vec<String> = ["script", "docker", "kubernetes", "agent"].iter().map(|s| s.to_string()).collect();
    let worker = Uuid::new_v4();
    WorkerRepo::register(&pool, worker, "w-all", &caps, &[], false, None).await?;
    assert!(JobStepRepo::claim_ready_step(&pool, &caps, &[], false, worker).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn unmatched_sweep_and_stranded_listing_treat_deferred_rows_apart() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    let mut script = deferred_step(job_id, "plain");
    script.action_type = "script".to_string();
    script.required_ability = "script".to_string();
    script.runner = "local".to_string();
    script.ref_template = None;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run"), script]).await?;
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;

    // No worker at all: the script step is unmatched, the deferred one is not.
    let unmatched = JobStepRepo::get_unmatched_ready_steps(&pool, 30.0).await?;
    let names: Vec<&str> = unmatched.iter().filter(|s| s.job_id == job_id).map(|s| s.step_name.as_str()).collect();
    assert_eq!(names, vec!["plain"]);

    // Stranded: an old, unleased deferred row.
    let listed = JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100).await?;
    assert!(listed.contains(&job_id));
    // A live lease hides it.
    let token = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", token, 600.0).await?);
    assert!(!JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100).await?.contains(&job_id));
    // An expired one shows it again.
    expire_lease(&pool, job_id, "run").await;
    assert!(JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100).await?.contains(&job_id));
    // Too young: not listed.
    assert!(!JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 7200.0, None, 100).await?.contains(&job_id));
    // A terminal job: not listed.
    JobRepo::cancel(&pool, job_id).await?;
    assert!(!JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100).await?.contains(&job_id));
    Ok(())
}

#[tokio::test]
async fn stamp_task_pin_only_stamps_a_ready_unstamped_step() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    let mut task_step = deferred_step(job_id, "call");
    task_step.action_type = "task".to_string();
    task_step.required_ability = String::new();
    task_step.ref_template = None;
    JobStepRepo::create_steps(&pool, &[task_step]).await?;
    assert!(JobStepRepo::stamp_task_pin(&pool, job_id, "call", "etl", "release/2.3", C1).await?);
    assert!(!JobStepRepo::stamp_task_pin(&pool, job_id, "call", "etl", "main", C2).await?);
    let row = JobStepRepo::get_step(&pool, job_id, "call").await?.unwrap();
    assert_eq!(
        (row.task_workspace.as_deref(), row.task_ref.as_deref(), row.task_revision.as_deref()),
        (Some("etl"), Some("release/2.3"), Some(C1))
    );
    Ok(())
}

/// Spec § 8, contract deviation 1: the claim and the unmatched sweep keep
/// using a partial `status = 'ready'` index after the sweep gains its
/// `<> 'deferred'` conjunct. The WHERE clauses are copies of the two
/// queries (`claim_ready_step`, `get_unmatched_ready_steps`) with literal
/// values — keep them in sync. `enable_seqscan = off` because a test table is
/// small enough that the planner would scan it whatever the indexes are.
#[tokio::test]
async fn claim_and_unmatched_sweep_still_use_a_ready_partial_index() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, \
                               required_ability, runner, ready_at) \
         SELECT $1, 's' || g, 'a', 'script', \
                CASE WHEN g % 100 = 0 THEN 'ready' ELSE 'completed' END, 'script', 'local', NOW() \
         FROM generate_series(1, 5000) g",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;
    sqlx::query("ANALYZE job_step").execute(&pool).await?;

    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off").execute(&mut *tx).await?;
    let claim: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN SELECT job_id, step_name FROM job_step \
         WHERE status = 'ready' \
           AND '[\"script\"]'::jsonb @> to_jsonb(required_ability) \
           AND required_tags::jsonb <@ '[]'::jsonb \
           AND (NOT false OR '[]'::jsonb <@ required_tags::jsonb) \
           AND action_type NOT IN ('task', 'approval', 'event_source') \
           AND (retry_at IS NULL OR retry_at <= NOW()) \
         ORDER BY random() FOR UPDATE SKIP LOCKED LIMIT 1",
    )
    .fetch_all(&mut *tx)
    .await?;
    let sweep: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN SELECT js.job_id, js.step_name FROM job_step js \
         WHERE js.status = 'ready' \
           AND js.action_type NOT IN ('task', 'approval', 'event_source') \
           AND js.action_type <> 'deferred' \
           AND js.ready_at < NOW() - make_interval(secs => 30::double precision)",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (name, plan) in [("claim", claim), ("sweep", sweep)] {
        let text: String = plan.into_iter().map(|r| r.0).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("Seq Scan on job_step"), "{name}: {text}");
        assert!(
            text.contains("idx_job_step_ready_ability") || text.contains("idx_job_step_ready_claim"),
            "{name} must use a partial status = 'ready' index: {text}"
        );
    }
    tx.rollback().await?;
    Ok(())
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration templated_refs_test::`
Expected: compile error — the new methods and `DeferredWrite` are missing.

- [ ] **Step 3: Implement the step primitives**

In `crates/stroem-db/src/repos/job_step.rs`, next to `ReleaseOutcome` (`:311`), add:

```rust
/// Result of a guarded write to a deferred step (spec 2026-10-15 § 5.3 step 7,
/// § 5.5): [`JobStepRepo::resolve_deferred_tx`] and [`JobStepRepo::fail_deferred_tx`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredWrite {
    /// The row was written.
    Applied,
    /// The job is no longer `pending`/`running` (e.g. cancelled): nothing written.
    JobTerminal,
    /// The row is no longer this lease holder's deferred `ready` row — resolved
    /// by another resolver, cancelled, claimed, or the lease was lost: nothing written.
    NotApplied,
}
```

In `impl JobStepRepo`, after `release_claim`, add:

```rust
    /// Take the resolution lease of a deferred `ready` step (spec 2026-10-15
    /// § 5.3 step 0, D12). `false`: another resolver holds a live lease, or the
    /// step is no longer a deferred `ready` row. The DB clock decides expiry.
    pub async fn take_resolve_lease(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        token: Uuid,
        lease_secs: f64,
    ) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE job_step \
             SET resolve_lease_owner = $3, \
                 resolve_lease_until = NOW() + make_interval(secs => $4::double precision) \
             WHERE job_id = $1 AND step_name = $2 AND status = 'ready' AND action_type = 'deferred' \
               AND (resolve_lease_until IS NULL OR resolve_lease_until < NOW())",
        )
        .bind(job_id)
        .bind(step_name)
        .bind(token)
        .bind(lease_secs)
        .execute(pool)
        .await
        .context("take resolve lease")?;
        Ok(r.rows_affected() == 1)
    }

    /// Extend a held lease. `false`: the lease is lost (taken over after
    /// expiry) or the step moved on — the holder must not write.
    pub async fn renew_resolve_lease(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        token: Uuid,
        lease_secs: f64,
    ) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE job_step \
             SET resolve_lease_until = NOW() + make_interval(secs => $4::double precision) \
             WHERE job_id = $1 AND step_name = $2 AND resolve_lease_owner = $3 \
               AND status = 'ready' AND action_type = 'deferred'",
        )
        .bind(job_id)
        .bind(step_name)
        .bind(token)
        .bind(lease_secs)
        .execute(pool)
        .await
        .context("renew resolve lease")?;
        Ok(r.rows_affected() == 1)
    }

    /// The job's deferred `ready` steps, by name.
    pub async fn get_ready_deferred_steps(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>> {
        sqlx::query_as::<_, JobStepRow>(concat!(
            "SELECT ",
            step_columns!(),
            " FROM job_step WHERE job_id = $1 AND status = 'ready' AND action_type = 'deferred' \
              ORDER BY step_name"
        ))
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("get ready deferred steps")
    }

    /// Write a deferred step's resolved columns (spec 2026-10-15 § 5.3 step 7),
    /// in ONE transaction: the job row `FOR SHARE` first (the lock order of
    /// [`Self::release_claim`], which serialises with `JobRepo::cancel`), nothing
    /// unless the job is `pending`/`running`, then the step only while it is
    /// still this lease holder's deferred `ready` row. `ready_at` restarts: the
    /// step becomes claimable now. `status`, `input`, `timeout_secs`, `when`
    /// and the loop columns are not touched.
    pub async fn resolve_deferred_tx(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        token: Uuid,
        resolved: &NewJobStep,
    ) -> Result<DeferredWrite> {
        let mut tx = pool.begin().await.context("begin resolve_deferred_tx")?;
        if let Some(outcome) = Self::deferred_job_gate(&mut tx, job_id).await? {
            tx.rollback().await.ok();
            return Ok(outcome);
        }
        let required_tags =
            serde_json::to_value(&resolved.required_tags).context("serialize required_tags")?;
        let r = sqlx::query(
            "UPDATE job_step SET \
                 action_name = $4, action_type = $5, action_image = $6, action_spec = $7, \
                 required_ability = $8, required_tags = $9, runner = $10, \
                 max_retries = $11, retry_backoff_secs = $12, retry_strategy = $13, retry_jitter = $14, \
                 action_workspace = $15, action_revision = $16, action_ref = $17, \
                 task_workspace = $18, task_ref = $19, task_revision = $20, \
                 ready_at = NOW(), resolve_lease_owner = NULL, resolve_lease_until = NULL \
             WHERE job_id = $1 AND step_name = $2 AND status = 'ready' \
               AND action_type = 'deferred' AND resolve_lease_owner = $3",
        )
        .bind(job_id)
        .bind(step_name)
        .bind(token)
        .bind(&resolved.action_name)
        .bind(&resolved.action_type)
        .bind(&resolved.action_image)
        .bind(&resolved.action_spec)
        .bind(&resolved.required_ability)
        .bind(required_tags)
        .bind(&resolved.runner)
        .bind(resolved.max_retries)
        .bind(resolved.retry_backoff_secs)
        .bind(&resolved.retry_strategy)
        .bind(resolved.retry_jitter)
        .bind(&resolved.action_workspace)
        .bind(&resolved.action_revision)
        .bind(&resolved.action_ref)
        .bind(&resolved.task_workspace)
        .bind(&resolved.task_ref)
        .bind(&resolved.task_revision)
        .execute(&mut *tx)
        .await
        .context("write resolved deferred step")?;
        let outcome = if r.rows_affected() == 1 {
            DeferredWrite::Applied
        } else {
            DeferredWrite::NotApplied
        };
        tx.commit().await.context("commit resolve_deferred_tx")?;
        Ok(outcome)
    }

    /// Fail a deferred step whose resolution failed (spec 2026-10-15 § 5.5).
    /// Same transaction shape and token fence as [`Self::resolve_deferred_tx`];
    /// never `mark_failed`, which matches on job and step name alone. A failure
    /// that loses to a cancellation writes nothing. No retry attempt is
    /// consumed: the step never ran.
    pub async fn fail_deferred_tx(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        token: Uuid,
        error: &str,
    ) -> Result<DeferredWrite> {
        let mut tx = pool.begin().await.context("begin fail_deferred_tx")?;
        if let Some(outcome) = Self::deferred_job_gate(&mut tx, job_id).await? {
            tx.rollback().await.ok();
            return Ok(outcome);
        }
        let r = sqlx::query(
            "UPDATE job_step \
             SET status = 'failed', error_message = $4, completed_at = NOW(), \
                 resolve_lease_owner = NULL, resolve_lease_until = NULL \
             WHERE job_id = $1 AND step_name = $2 AND status = 'ready' \
               AND action_type = 'deferred' AND resolve_lease_owner = $3",
        )
        .bind(job_id)
        .bind(step_name)
        .bind(token)
        .bind(error)
        .execute(&mut *tx)
        .await
        .context("fail deferred step")?;
        let outcome = if r.rows_affected() == 1 {
            DeferredWrite::Applied
        } else {
            DeferredWrite::NotApplied
        };
        tx.commit().await.context("commit fail_deferred_tx")?;
        Ok(outcome)
    }

    /// The job-row half of both deferred writes: lock the job `FOR SHARE`;
    /// `Some(outcome)` means stop — the job is gone or terminal.
    async fn deferred_job_gate(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job_id: Uuid,
    ) -> Result<Option<DeferredWrite>> {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM job WHERE job_id = $1 FOR SHARE")
                .bind(job_id)
                .fetch_optional(&mut **tx)
                .await
                .context("lock job row for a deferred-step write")?;
        Ok(match status.as_deref() {
            None => Some(DeferredWrite::NotApplied),
            Some("pending") | Some("running") => None,
            Some(_) => Some(DeferredWrite::JobTerminal),
        })
    }

    /// Stamp the task pin of a `type: task` step whose templated `ref:` was
    /// resolved at dispatch (spec 2026-10-15 § 5.6), only while the step is
    /// still `ready` and unstamped. `false`: another dispatcher got there first.
    pub async fn stamp_task_pin(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        workspace: &str,
        git_ref: &str,
        commit: &str,
    ) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE job_step SET task_workspace = $3, task_ref = $4, task_revision = $5 \
             WHERE job_id = $1 AND step_name = $2 AND status = 'ready' AND task_ref IS NULL",
        )
        .bind(job_id)
        .bind(step_name)
        .bind(workspace)
        .bind(git_ref)
        .bind(commit)
        .execute(pool)
        .await
        .context("stamp task pin")?;
        Ok(r.rows_affected() == 1)
    }
```

In `get_unmatched_ready_steps`, below `AND js.action_type NOT IN ('task', 'approval', 'event_source')`, add:

```sql
              -- A deferred step waits for the server, never for a worker
              -- (spec 2026-10-15 § 8). A separate conjunct keeps the list
              -- above identical to the claim SQL's.
              AND js.action_type <> 'deferred'
```

`crates/stroem-db/src/lib.rs`: add `DeferredWrite` to the `repos::job_step::{…}` re-export.

- [ ] **Step 4: Implement the recovery listing**

In `crates/stroem-db/src/repos/job.rs`, after `get_stalled_pinned_jobs`:

```rust
    /// Recovery Phase 4.6 (spec 2026-10-15 § 5.8): non-terminal jobs with a
    /// deferred `ready` step that has waited longer than `older_than_secs`
    /// and holds no live resolution lease — left by a crash or a dead
    /// advance. Cursor pages like [`Self::get_stalled_pinned_jobs`].
    pub async fn get_jobs_with_stranded_deferred_steps(
        pool: &PgPool,
        older_than_secs: f64,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        sqlx::query_scalar(
            r#"
            SELECT j.job_id
            FROM job j
            WHERE j.status IN ('pending', 'running')
              AND EXISTS (
                  SELECT 1 FROM job_step s
                  WHERE s.job_id = j.job_id
                    AND s.status = 'ready'
                    AND s.action_type = 'deferred'
                    AND s.ready_at < NOW() - make_interval(secs => $1::double precision)
                    AND (s.resolve_lease_until IS NULL OR s.resolve_lease_until < NOW())
              )
            ORDER BY ($2::uuid IS NOT NULL AND j.job_id <= $2), j.job_id
            LIMIT $3
            "#,
        )
        .bind(older_than_secs)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("Failed to list jobs with stranded deferred steps")
    }
```

- [ ] **Step 5: Run the tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration templated_refs_test::`
Expected: PASS. If the EXPLAIN test fails because the planner chose another `status`-index (for example `idx_job_step_status`), record the plan text in the commit message, keep the "no Seq Scan" assertion, and widen the name assertion to `idx_job_step_` — do not change any index.

- [ ] **Step 6: Run the whole crate**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-db --all-targets -- -D warnings
git add crates/stroem-db
git commit -m "feat(db): deferred-step lease, guarded resolve/fail writes, stranded listing, sweep ignores deferred"
```

---
## Part C — server foundations

### Task 6: `claim_job` rejects a capability outside `KNOWN_ABILITIES`

Spec D9, § 8. The claim SQL matches the capabilities the CLAIM REQUEST carries; `/worker/register` validates them, `claim_job` does not. Without this check a client holding the worker token could send `["deferred"]` and claim a deferred row.

**Files:**
- Modify: `crates/stroem-server/src/web/worker_api/jobs.rs` — `claim_job` (between the empty-capabilities check `:841-846` and `claim_ready_step` `:848`); unit test in the test module (next to `test_migrate_legacy_register_splits_tags_into_capabilities`, `:2016`)
- Create: `crates/stroem-server/tests/git_refs_deferred_test.rs`; Modify: `crates/stroem-server/tests/main.rs`

**Interfaces:**
- Produces: the integration-test module `git_refs_deferred_test` that Tasks 8–14 extend.

- [ ] **Step 1: Write the failing tests**

Add to the test module of `crates/stroem-server/src/web/worker_api/jobs.rs`:

```rust
    /// A deferred step carries `required_ability = 'deferred'` (spec
    /// 2026-10-15 D9). It must never become a known ability, or a worker
    /// could claim a step whose action is not resolved yet.
    #[test]
    fn known_abilities_never_contain_deferred() {
        assert!(!KNOWN_ABILITIES.contains(&"deferred"));
        assert!(!KNOWN_ABILITIES.contains(&stroem_common::models::job::DEFERRED_ACTION_TYPE));
    }
```

Create `crates/stroem-server/tests/git_refs_deferred_test.rs`:

```rust
//! Templated refs (`docs/superpowers/specs/2026-10-15-templated-refs-design.md`):
//! deferred steps, their resolution at promotion, the ref memo, the lease,
//! privacy of the rendered value, `type: task` and webhook templated refs.

use crate::common::pinned::*;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// A claim request may only carry known abilities (spec D9): `deferred` (or any
/// other unknown token) is a 400 before the claim SQL runs.
#[tokio::test]
async fn claim_with_an_unknown_capability_is_a_400() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    for caps in [json!(["deferred"]), json!(["script", "deferred"]), json!(["scipt"])] {
        let (status, body) = worker_req(
            &fx.router,
            "POST",
            "/worker/jobs/claim",
            Some(json!({"worker_id": Uuid::new_v4().to_string(), "capabilities": caps})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{caps}: {body}");
        assert!(body.to_string().contains("unknown capability"), "{caps}: {body}");
    }
    Ok(())
}
```

Add `mod git_refs_deferred_test;` to `crates/stroem-server/tests/main.rs` (alphabetical, before `mod git_refs_read_paths_test;`).

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::claim_with_an_unknown_capability`
Expected: FAIL — the claim answers 200 (no work), not 400.

- [ ] **Step 3: Implement**

In `claim_job`, after the `if capabilities.is_empty() { … }` block:

```rust
    // The claim SQL matches the capabilities this REQUEST carries, so an
    // unknown token could claim rows no worker should — a deferred step
    // carries `required_ability = 'deferred'` (spec 2026-10-15 D9). Same
    // check and message as /worker/register; a real worker always sends its
    // registered (validated) capabilities.
    if let Some(c) = capabilities
        .iter()
        .find(|c| !KNOWN_ABILITIES.contains(&c.as_str()))
    {
        return Err(AppError::BadRequest(format!(
            "unknown capability '{c}'. Known: {KNOWN_ABILITIES:?}"
        )));
    }
```

- [ ] **Step 4: Run the tests**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib web::worker_api::jobs
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test:: git_refs_claim_test::
```
Expected: PASS (the existing claim tests send known capabilities).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/web/worker_api/jobs.rs crates/stroem-server/tests/git_refs_deferred_test.rs crates/stroem-server/tests/main.rs
git commit -m "fix(worker-api): claim rejects capabilities outside KNOWN_ABILITIES"
```

---

### Task 7: Render-context scopes `StepRef`, `TaskActionRef` and `webhook_ref`

Spec § 4.1 table, § 4.2 rule 2, § 10.4. No ref context carries `secret`.

**Files:**
- Modify: `crates/stroem-server/src/render_context.rs` — `Scope` (`:177-196`); `build_entries` (`:254-368`); a new constructor after `build` (`:402`); tests (helpers `:425-478`, the anti-regression test `:488-541`)

**Interfaces:**
- Produces:
  ```rust
  // crate::render_context
  pub enum Scope<'a> { /* existing */ StepRef, TaskActionRef { ref_input: &'a Value } }
  pub fn webhook_ref(input: &Value) -> RenderContext;
  ```
  - `StepRef`: `input` = job input, NO `secret`, `state`, `global_state`, `job`, step entries, `each` for a loop instance. The same insertion order as every scope.
  - `TaskActionRef { ref_input }`: exactly `input` = `ref_input` and `job` (§ 4.1). No `secret`, no state, no step entries, no `each`.
  - `webhook_ref(input)`: exactly `input`.

- [ ] **Step 1: Write the failing tests**

In `mod tests` of `crates/stroem-server/src/render_context.rs`, add after `all_scopes`:

```rust
    /// The ref scopes (spec 2026-10-15 § 4.2 rule 2): none carries `secret`.
    fn ref_scopes<'a>(ref_input: &'a serde_json::Value) -> Vec<Scope<'a>> {
        vec![Scope::StepRef, Scope::TaskActionRef { ref_input }]
    }

    /// Forces every new `Scope` variant into exactly one of the two lists
    /// above: the match is exhaustive, so a new variant does not compile here.
    #[allow(dead_code)]
    fn every_scope_is_listed(scope: Scope<'_>) -> &'static str {
        match scope {
            Scope::StepInput
            | Scope::ActionBody { .. }
            | Scope::AgentPrompt
            | Scope::Condition
            | Scope::ChildTaskInput
            | Scope::ApprovalMessage { .. } => "all_scopes",
            Scope::StepRef | Scope::TaskActionRef { .. } => "ref_scopes",
        }
    }

    #[test]
    fn ref_scopes_never_carry_secret() {
        let job_input = json!({"branch": "release/2.3"});
        let caller = secrets(&[("token", "s3cr3t-value")]);
        let sn = snaps(None, None);
        let ref_input = json!({"branch": "main"});
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        for scope in ref_scopes(&ref_input) {
            let label = format!("{scope:?}");
            let v = build(&job, &[], None, scope);
            assert!(v.as_value().get("secret").is_none(), "{label} carries secret");
            assert!(!v.as_value().to_string().contains("s3cr3t-value"), "{label}");
        }
        let v = webhook_ref(&json!({"body": {"ref": "main"}}));
        assert!(v.as_value().get("secret").is_none());
    }

    #[test]
    fn step_ref_carries_job_input_state_steps_and_each_in_order() {
        let job_input = json!({"branch": "release/2.3"});
        let caller = secrets(&[("c", "1")]);
        let sn = snaps(Some(json!({"s": 1})), Some(json!({"g": 1})));
        let rows = vec![row("pick", "completed", Some(json!({"branch": "main"})))];
        let views = views(&rows);
        let inst = instance("run[0]", "run", 0, json!("release/2.3"));
        let slot = LoopSlot::of(&inst);
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: Some("rev"),
            job_ref: None,
        };
        let ordered = build_ordered(&job, &views, slot, Scope::StepRef);
        let keys: Vec<&str> = ordered.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["input", "state", "global_state", "job", "pick", "each"]);
        let v = build(&job, &views, slot, Scope::StepRef);
        assert_eq!(v.as_value()["input"]["branch"], "release/2.3");
        assert_eq!(v.as_value()["pick"]["output"]["branch"], "main");
        assert_eq!(v.as_value()["each"]["item"], "release/2.3");
        // No loop slot → no `each`.
        let v = build(&job, &views, None, Scope::StepRef);
        assert!(v.as_value().get("each").is_none());
    }

    #[test]
    fn task_action_ref_carries_only_the_projection_and_job() {
        let job_input = json!({"branch": "release/2.3"});
        let caller = secrets(&[("c", "1")]);
        let sn = snaps(Some(json!({"s": 1})), None);
        let rows = vec![row("pick", "completed", Some(json!({"x": 1})))];
        let views = views(&rows);
        let inst = instance("call[0]", "call", 0, json!("i"));
        let ref_input = json!({"branch": "main"});
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: Some("rev"),
            job_ref: Some("release/2.3"),
        };
        let ordered = build_ordered(
            &job,
            &views,
            LoopSlot::of(&inst),
            Scope::TaskActionRef { ref_input: &ref_input },
        );
        let keys: Vec<&str> = ordered.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["input", "job"]);
        assert_eq!(ordered[0].1, json!({"branch": "main"}));
        assert_eq!(ordered[1].1["ref"], "release/2.3");
    }

    #[test]
    fn webhook_ref_carries_only_the_webhook_input() {
        let input = json!({"body": {"ref": "main"}, "method": "POST"});
        let v = webhook_ref(&input);
        assert_eq!(v.as_value(), &json!({"input": input}));
        assert!(v.collisions().is_empty());
    }

    #[test]
    fn step_ref_records_collisions_like_every_scope() {
        let caller = secrets(&[]);
        let sn = snaps(None, None);
        let rows = vec![row("input", "completed", Some(json!(1)))];
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&rows), None, Scope::StepRef);
        assert!(v.collisions().iter().any(|c| c.step == "input" && c.key == "input"));
    }
```

`every_scope_carries_every_framework_key_in_order` keeps iterating `all_scopes` only; it stays unchanged.

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib render_context::tests`
Expected: compile error — `Scope::StepRef`, `Scope::TaskActionRef`, `webhook_ref` not found.

- [ ] **Step 3: Implement**

Add to `pub enum Scope<'a>` (after `ApprovalMessage`):

```rust
    /// A flow step's templated `ref:` (spec 2026-10-15 § 4.2 rule 2). Job
    /// input and NO `secret`: a ref is an identifier and is never masked.
    /// `each` for a loop instance.
    StepRef,
    /// A `type: task` action's templated `ref:` (§ 5.6): `input` is the
    /// schema-free ref projection of the caller's step input; `job`; nothing
    /// else (§ 4.1) — no `secret`, no state, no step entries, no `each`.
    TaskActionRef { ref_input: &'a Value },
```

In `build_entries`, at the very top (after `let mut collisions = Vec::new();`):

```rust
    // § 4.1: a task action's ref sees its projection and `job`, nothing else.
    if let Scope::TaskActionRef { ref_input } = scope {
        upsert(&mut ctx, "input", ref_input.clone());
        upsert(&mut ctx, "job", job_context(job.job_revision, job.job_ref));
        return (ctx, collisions);
    }
```

Replace the `(input, secrets)` match and the unconditional `secret` upsert with:

```rust
    // The two real axes (spec §3.2). The ref scopes carry no `secret`
    // (spec 2026-10-15 D10).
    let (input, secrets): (Option<&Value>, Option<&HashMap<String, Value>>) = match scope {
        Scope::ActionBody { prepared_input } => (prepared_input, Some(job.owner_secrets)),
        // TaskActionRef returned above.
        Scope::StepRef | Scope::TaskActionRef { .. } => (job.job_input, None),
        Scope::StepInput
        | Scope::AgentPrompt
        | Scope::Condition
        | Scope::ChildTaskInput
        | Scope::ApprovalMessage { .. } => (job.job_input, Some(job.caller_secrets)),
    };

    if let Some(v) = input {
        upsert(&mut ctx, "input", v.clone());
    }
    if let Some(secrets) = secrets {
        upsert(
            &mut ctx,
            "secret",
            serde_json::to_value(secrets).unwrap_or_else(|_| json!({})),
        );
    }
```

The `each` condition (`if !matches!(scope, Scope::Condition)`) stays: `StepRef` gets `each` from `loop_slot`, and `TaskActionRef` has already returned.

After `build`, add:

```rust
/// The context of a webhook trigger's templated `ref:` (spec 2026-10-15 § 6):
/// the webhook job input (`body`, `headers`, `method`, `query` and the
/// trigger's `input` defaults) and nothing else — no job exists yet, so
/// [`build`], which takes a [`JobContext`], does not fit.
pub fn webhook_ref(input: &Value) -> RenderContext {
    let mut ctx = Map::new();
    ctx.insert("input".to_string(), input.clone());
    RenderContext {
        value: Secret::new(Value::Object(ctx)),
        collisions: Vec::new(),
    }
}
```

Also update the module doc's sentence "The only constructor is [`build`]" on `RenderContext` (`:205`) to "The only constructors are [`build`] and [`webhook_ref`]".

- [ ] **Step 4: Run the tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib render_context::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/render_context.rs
git commit -m "feat(render): StepRef / TaskActionRef scopes and webhook_ref context, none carrying secret"
```

---

### Task 8: Privacy — the resolve span, the private pin load, `RenderedRefError`, the classifier

Spec § 5.5 "Changes" 1–3, § 10.3; contract deviations 3, 11, 14.

**Files:**
- Create: `crates/stroem-server/src/templated_ref.rs`; Modify: `crates/stroem-server/src/lib.rs` (add `pub mod templated_ref;` next to `pub mod refs;`)
- Modify: `crates/stroem-server/src/workspace/pins.rs` — `PinStore::resolve`'s `#[tracing::instrument]` (`:760`)
- Modify: `crates/stroem-server/src/workspace/mod.rs` — new `pinned_config_private` after `config_for_user` (`:540`)
- Modify: `crates/stroem-server/src/job_creator.rs` — a `MissingAtRef` type next to `OwnerSideRender` (`:379-405`); `action_at` (`:1147-1163`) and `task_named_at` (`:1167-1183`) return it
- Modify: `crates/stroem-server/src/web/api/mod.rs` — `classify_execute_error` (`:420`) + tests (`classify_execute_error_tests`, `:478`)
- Create: `crates/stroem-server/tests/common/capture.rs`; Modify: `crates/stroem-server/tests/common/mod.rs`
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: Task 1's `RenderedRefRejection`.
- Produces:
  ```rust
  // crate::templated_ref
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum RenderedRefKind { Empty, Invalid, SecretValue, NotFound { short_sha: bool }, NotGit, NoAction,
                             AgentAction, LoadWithheld, Unavailable, Render, Unresolvable }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct RenderedRefError { pub kind: RenderedRefKind, pub workspace: String, /* private */ detail: Option<String> }
  impl RenderedRefError {
      pub fn new(kind: RenderedRefKind, workspace: &str) -> Self;
      pub fn from_rejection(r: stroem_common::git_ref::RenderedRefRejection, workspace: &str) -> Self;
      pub fn from_error(err: &anyhow::Error, workspace: &str) -> Self;
  }
  // impl Display (fixed sentence per kind) + std::error::Error (no source)
  // crate::workspace::WorkspaceManager
  pub async fn pinned_config_private(&self, ws: &str, commit: &str) -> anyhow::Result<Arc<Pinned>>;
  // crate::job_creator
  pub(crate) struct MissingAtRef { .. }      // Display: "{kind} '{written}': workspace '{owner}' has no {kind} '{local}' at ref '{git_ref}'"
  // tests/common/capture.rs
  pub struct Captured; impl Captured { pub fn text(&self) -> String; pub fn span_fields(&self, name: &str) -> Vec<String>; }
  pub fn capture() -> (Captured, tracing::subscriber::DefaultGuard);
  ```

- [ ] **Step 1: Write the failing unit tests for `RenderedRefError`**

Create `crates/stroem-server/src/templated_ref.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefResolveError;
    use crate::workspace::pins::{PinError, PinLoadWithheld, PinRef};

    const VALUE: &str = "feature/zz-private-4711";

    fn kind_of(err: anyhow::Error) -> RenderedRefKind {
        RenderedRefError::from_error(&err, "etl").kind
    }

    #[test]
    fn errors_are_classified_by_type() {
        let ws = "etl".to_string();
        assert_eq!(
            kind_of(PinError::RefNotFound { workspace: ws.clone(), git_ref: VALUE.into() }.into()),
            RenderedRefKind::NotFound { short_sha: false }
        );
        assert_eq!(
            kind_of(PinError::RefNotFound { workspace: ws.clone(), git_ref: "3f2a9c0".into() }.into()),
            RenderedRefKind::NotFound { short_sha: true }
        );
        assert_eq!(
            kind_of(PinError::PinUnavailable { workspace: ws.clone(), message: format!("ref '{VALUE}' x") }.into()),
            RenderedRefKind::Unavailable
        );
        assert_eq!(
            kind_of(PinError::PinLoadFailed { workspace: ws.clone(), commit: "c".into(), message: "m".into() }.into()),
            RenderedRefKind::LoadWithheld
        );
        assert_eq!(kind_of(PinError::NotGit { workspace: ws.clone() }.into()), RenderedRefKind::NotGit);
        let pin = PinRef { git_ref: VALUE.into(), commit: "3f2a9c0e".into() };
        assert_eq!(kind_of(PinLoadWithheld::new("etl", &pin).into()), RenderedRefKind::LoadWithheld);
        assert_eq!(
            kind_of(RefResolveError::AgentAction("ask".into()).into()),
            RenderedRefKind::AgentAction
        );
        assert_eq!(kind_of(RefResolveError::NotGit("docs".into()).into()), RenderedRefKind::NotGit);
        assert_eq!(
            kind_of(anyhow::Error::new(crate::job_creator::MissingAtRef::new_for_test()).context("outer")),
            RenderedRefKind::NoAction
        );
        assert_eq!(kind_of(anyhow::anyhow!("anything {VALUE}")), RenderedRefKind::Unresolvable);
        let te = stroem_common::template::render_ref_template("{{ nope }}", &serde_json::json!({}))
            .unwrap_err();
        let e = RenderedRefError::from_error(&te, "etl");
        assert_eq!(e.kind, RenderedRefKind::Render);
        assert!(e.to_string().starts_with("the ref template could not be rendered: "), "{e}");
    }

    #[test]
    fn no_message_carries_the_value_and_none_has_a_source() {
        let errors: Vec<anyhow::Error> = vec![
            PinError::RefNotFound { workspace: "etl".into(), git_ref: VALUE.into() }.into(),
            PinError::PinUnavailable { workspace: "etl".into(), message: format!("ref '{VALUE}' is not in the last listing") }.into(),
            anyhow::anyhow!("whatever {VALUE}"),
        ];
        for err in errors {
            let e = RenderedRefError::from_error(&err, "etl");
            let shown = format!("{:#}", anyhow::Error::new(e.clone()));
            assert!(!shown.contains("zz-private"), "{shown}");
            assert!(std::error::Error::source(&e).is_none());
        }
        assert_eq!(
            RenderedRefError::new(RenderedRefKind::NotFound { short_sha: false }, "billing").to_string(),
            "the rendered ref was not found in workspace 'billing'"
        );
    }

    #[test]
    fn rejections_map_one_to_one() {
        use stroem_common::git_ref::RenderedRefRejection as R;
        assert_eq!(RenderedRefError::from_rejection(R::Empty, "etl").kind, RenderedRefKind::Empty);
        assert_eq!(RenderedRefError::from_rejection(R::SecretValue, "etl").kind, RenderedRefKind::SecretValue);
        assert_eq!(RenderedRefError::from_rejection(R::Invalid, "etl").kind, RenderedRefKind::Invalid);
        assert_eq!(
            RenderedRefError::from_rejection(R::Empty, "etl").to_string(),
            "the rendered ref is empty"
        );
    }
}
```

Add `pub mod templated_ref;` to `crates/stroem-server/src/lib.rs`.

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib templated_ref`
Expected: compile error — `RenderedRefError`, `MissingAtRef` not found.

- [ ] **Step 3: Add `MissingAtRef` to `job_creator.rs`**

Next to `OwnerSideRender` (`:379-405`):

```rust
/// A name missing from a pinned config. `Display` is the text `action_at` /
/// `task_named_at` always produced, so `classify_execute_error`'s
/// "has no action" / "has no task" phrases still answer 400; the templated-ref
/// paths classify it by TYPE (spec 2026-10-15, `templated_ref::RenderedRefKind::NoAction`).
#[derive(Debug)]
pub(crate) struct MissingAtRef {
    kind: &'static str,
    written: String,
    owner: String,
    local: String,
    git_ref: String,
}

impl std::fmt::Display for MissingAtRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} '{}': workspace '{}' has no {} '{}' at ref '{}'",
            self.kind, self.written, self.owner, self.kind, self.local, self.git_ref
        )
    }
}

impl std::error::Error for MissingAtRef {}

#[cfg(test)]
impl MissingAtRef {
    pub(crate) fn new_for_test() -> Self {
        Self {
            kind: "action",
            written: "hello".into(),
            owner: "etl".into(),
            local: "hello".into(),
            git_ref: "main".into(),
        }
    }
}
```

`action_at`: replace its `ok_or_else(|| anyhow::anyhow!(…))` with

```rust
    cfg.actions.get(local).cloned().ok_or_else(|| {
        anyhow::Error::new(MissingAtRef {
            kind: "action",
            written: written.to_string(),
            owner: owner.to_string(),
            local: local.to_string(),
            git_ref: git_ref.to_string(),
        })
    })
```

and `task_named_at` likewise with `kind: "task"`, `written: task_ref.to_string()`, `owner: ws.to_string()`.

- [ ] **Step 4: Implement `RenderedRefError`**

Put this above the test module in `crates/stroem-server/src/templated_ref.rs`:

```rust
//! Templated refs (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`
//! § 5.5 "Changes" 3): the ONE error a resolution of a rendered ref leaves
//! with. A rendered value is private until it passes both secret checks
//! (§ 10.3), so the error is classified by TYPE from whatever failed and
//! carries a fixed sentence per kind, the owner workspace (a configured
//! name), and — for a render error only — the value-free `TemplateError`
//! text. Never the value, never the replaced error: it is a leaf with no
//! `source()`, because `AppError::Internal` and the webhook handler print
//! `{:#}` chains and the replaced error can quote the value.

use std::fmt;

use stroem_common::git_ref::{GitRefError, RenderedRefRejection};
use stroem_common::template_error::TemplateError;

use crate::job_creator::MissingAtRef;
use crate::refs::RefResolveError;
use crate::workspace::pins::{PinError, PinLoadWithheld};

/// Why a rendered ref could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedRefKind {
    Empty,
    Invalid,
    SecretValue,
    NotFound { short_sha: bool },
    NotGit,
    NoAction,
    AgentAction,
    LoadWithheld,
    /// Transient (D7: it still fails the step; `classify_execute_error` → 500).
    Unavailable,
    Render,
    Unresolvable,
}

/// The value-free error of a templated-ref resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedRefError {
    pub kind: RenderedRefKind,
    /// The owner workspace the ref names (a configured name — an identifier).
    pub workspace: String,
    /// `Render` only: the `TemplateError` text (category + position).
    detail: Option<String>,
}

impl RenderedRefError {
    pub fn new(kind: RenderedRefKind, workspace: &str) -> Self {
        Self {
            kind,
            workspace: workspace.to_string(),
            detail: None,
        }
    }

    pub fn from_rejection(r: RenderedRefRejection, workspace: &str) -> Self {
        let kind = match r {
            RenderedRefRejection::Empty => RenderedRefKind::Empty,
            RenderedRefRejection::SecretValue => RenderedRefKind::SecretValue,
            RenderedRefRejection::Invalid => RenderedRefKind::Invalid,
        };
        Self::new(kind, workspace)
    }

    /// Classify `err` by type. `err` itself is dropped: its text may carry
    /// the rendered value.
    pub fn from_error(err: &anyhow::Error, workspace: &str) -> Self {
        if let Some(t) = err.downcast_ref::<TemplateError>() {
            return Self {
                kind: RenderedRefKind::Render,
                workspace: workspace.to_string(),
                detail: Some(t.to_string()),
            };
        }
        Self::new(classify(err), workspace)
    }
}

fn classify(err: &anyhow::Error) -> RenderedRefKind {
    if let Some(p) = err.downcast_ref::<PinError>() {
        return match p {
            PinError::NotGit { .. } => RenderedRefKind::NotGit,
            PinError::RefNotFound { git_ref, .. } => RenderedRefKind::NotFound {
                short_sha: stroem_common::git_ref::short_sha_hint(git_ref).is_some(),
            },
            PinError::CommitNotFound { .. } => RenderedRefKind::NotFound { short_sha: false },
            PinError::PinLoadFailed { .. } => RenderedRefKind::LoadWithheld,
            PinError::PinUnavailable { .. } => RenderedRefKind::Unavailable,
        };
    }
    if err.downcast_ref::<PinLoadWithheld>().is_some() {
        return RenderedRefKind::LoadWithheld;
    }
    if let Some(r) = err.downcast_ref::<RefResolveError>() {
        return match r {
            RefResolveError::NotGit(_) => RenderedRefKind::NotGit,
            RefResolveError::AgentAction(_) => RenderedRefKind::AgentAction,
            RefResolveError::LibraryItem(_) | RefResolveError::UnknownWorkspace(_) => {
                RenderedRefKind::Unresolvable
            }
        };
    }
    if err.downcast_ref::<GitRefError>().is_some() {
        return RenderedRefKind::Invalid;
    }
    if err.downcast_ref::<MissingAtRef>().is_some() {
        return RenderedRefKind::NoAction;
    }
    RenderedRefKind::Unresolvable
}

impl fmt::Display for RenderedRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ws = &self.workspace;
        match self.kind {
            RenderedRefKind::Empty => f.write_str("the rendered ref is empty"),
            RenderedRefKind::Invalid => {
                f.write_str("the rendered ref is not a valid branch, tag or full commit SHA")
            }
            RenderedRefKind::SecretValue => f.write_str("the rendered ref contains a secret value"),
            RenderedRefKind::NotFound { short_sha } => {
                write!(f, "the rendered ref was not found in workspace '{ws}'")?;
                if short_sha {
                    f.write_str(
                        " (short commit SHAs are not supported; use the full 40-character SHA)",
                    )?;
                }
                Ok(())
            }
            RenderedRefKind::NotGit => {
                write!(f, "workspace '{ws}' is not a git workspace; `ref` needs a git owner")
            }
            RenderedRefKind::NoAction => {
                write!(f, "workspace '{ws}' has no such action or task at the rendered ref")
            }
            RenderedRefKind::AgentAction => f.write_str(
                "the action at the rendered ref is an agent action; agent actions cannot be referenced with `ref` yet",
            ),
            RenderedRefKind::LoadWithheld => write!(
                f,
                "workspace '{ws}' at the rendered ref cannot be loaded: its configuration does not load"
            ),
            RenderedRefKind::Unavailable => {
                write!(f, "workspace '{ws}' is not available at the rendered ref yet")
            }
            RenderedRefKind::Render => match &self.detail {
                Some(d) => write!(f, "the ref template could not be rendered: {d}"),
                None => f.write_str("the ref template could not be rendered"),
            },
            RenderedRefKind::Unresolvable => {
                write!(f, "the rendered ref could not be resolved in workspace '{ws}'")
            }
        }
    }
}

impl std::error::Error for RenderedRefError {}
```

- [ ] **Step 5: Run the unit tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib templated_ref`
Expected: PASS.

- [ ] **Step 6: Classifier branch (test first)**

Add to `classify_execute_error_tests` in `crates/stroem-server/src/web/api/mod.rs`:

```rust
    #[test]
    fn rendered_ref_errors_answer_by_kind_with_their_fixed_sentence() {
        use crate::templated_ref::{RenderedRefError, RenderedRefKind};
        let unavailable = anyhow::Error::new(RenderedRefError::new(RenderedRefKind::Unavailable, "etl"));
        assert!(matches!(classify_execute_error(unavailable), AppError::Internal(_)));
        let not_found = anyhow::Error::new(RenderedRefError::new(
            RenderedRefKind::NotFound { short_sha: false },
            "etl",
        ));
        match classify_execute_error(not_found) {
            AppError::BadRequest(msg) => {
                assert_eq!(msg, "the rendered ref was not found in workspace 'etl'")
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }
```

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib classify_execute_error_tests::rendered_ref`
Expected: FAIL (the NotFound case is classified as Internal).

Then add at the top of `classify_execute_error`, before the `PinError` branch:

```rust
    // Templated refs (spec 2026-10-15 § 5.5): one value-free typed error,
    // checked first. Its kind decides: transient → 500, else → 400.
    if let Some(r) = e.downcast_ref::<crate::templated_ref::RenderedRefError>() {
        return match r.kind {
            crate::templated_ref::RenderedRefKind::Unavailable => AppError::Internal(e),
            _ => AppError::BadRequest(r.to_string()),
        };
    }
```

Run it again. Expected: PASS.

- [ ] **Step 7: The resolve span and the private pin load**

`crates/stroem-server/src/workspace/pins.rs:760` — change the attribute of `PinStore::resolve` to:

```rust
    // `git_ref` is not a span field: a templated ref's value is private until
    // it passes both secret checks (spec 2026-10-15 § 5.5 change 1). No
    // caller read the field.
    #[tracing::instrument(skip_all, fields(workspace = %ws))]
```

`crates/stroem-server/src/workspace/mod.rs`, after `config_for_user`:

```rust
    /// Load `ws` at `commit` for a ref whose value is still private (spec
    /// 2026-10-15 § 5.5 change 2): every label this path writes names the
    /// commit only, `ws (sha7)`, never the ref. A `PinLoadFailed` is withheld
    /// as [`pin_error_for_user`](Self::pin_error_for_user) does — scrubbed
    /// chain to `tracing::error!`, fixed sentence returned — under that
    /// commit-only label. Every other `PinError` passes through (its text
    /// names the workspace and, at most, the commit).
    #[tracing::instrument(skip_all, fields(workspace = %ws, commit = %commit))]
    pub async fn pinned_config_private(&self, ws: &str, commit: &str) -> Result<Arc<Pinned>> {
        match self.pins.ensure(ws, commit).await {
            Ok(pinned) => Ok(pinned),
            Err(err @ PinError::PinLoadFailed { .. }) => {
                let withheld = pins::PinLoadWithheld::for_commit(ws, commit);
                let detail = self.scrub_live(ws, &err.to_string()).await;
                tracing::error!("{withheld}: {detail}");
                Err(withheld.into())
            }
            Err(err) => Err(err.into()),
        }
    }
```

- [ ] **Step 8: The capture helper**

Create `crates/stroem-server/tests/common/capture.rs`:

```rust
//! Capture `tracing` output in a test: every span field (at creation and on
//! `record`) and every formatted event, so a test can assert that a value
//! never reached the server log. `set_default` is thread-local: use it on a
//! `current_thread` runtime (the `#[tokio::test]` default) and keep the guard
//! alive across the awaited calls. Work on other threads (`spawn_blocking`)
//! is not captured.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
pub struct Captured {
    fields: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<u8>>>,
}

impl Captured {
    /// Every captured span field (`name=value`) and event line.
    pub fn text(&self) -> String {
        let mut out = self.fields.lock().unwrap().join("\n");
        out.push('\n');
        out.push_str(&String::from_utf8_lossy(&self.events.lock().unwrap()));
        out
    }

    /// The captured span fields called `name`, as `name=value`.
    pub fn span_fields(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name}=");
        self.fields
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f.starts_with(&prefix))
            .cloned()
            .collect()
    }
}

struct Visitor<'a>(&'a mut Vec<String>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={:?}", field.name(), value));
    }
}

struct FieldLayer(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FieldLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        attrs.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }

    fn on_record(
        &self,
        _id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        values.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }
}

#[derive(Clone)]
struct Writer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
    type Writer = Writer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Start capturing on this thread until the guard drops.
pub fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry()
        .with(FieldLayer(captured.fields.clone()))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(Writer(captured.events.clone()))
                .with_ansi(false),
        );
    let guard = tracing::subscriber::set_default(subscriber);
    (captured, guard)
}
```

Add `pub mod capture;` to `crates/stroem-server/tests/common/mod.rs`.

- [ ] **Step 9: Integration tests for the span and the private load**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs` (add `use crate::common::capture::capture;` and `use stroem_server::workspace::pins::PinLoadWithheld;` to the imports):

```rust
/// A commit whose config does not load: its connection renders an undefined
/// secret, so the loader itself fails.
pub(crate) const DF_BROKEN: &str = r#"
secrets:
  token: s3cr3t-token-value
connections:
  db:
    host: "{{ secret.token }}-{{ secret.nope }}"
actions:
  hello:
    type: script
    runner: local
    script: echo broken
"#;

/// Spec § 5.5 change 1: `PinStore::resolve` no longer records the ref.
#[tokio::test]
async fn pin_store_resolve_records_no_git_ref_span_field() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let (captured, _guard) = capture();
    fx.mgr().pins().resolve("etl", "release/2.3").await?;
    assert!(captured.span_fields("git_ref").is_empty(), "{}", captured.text());
    assert!(!captured.span_fields("workspace").is_empty(), "{}", captured.text());
    Ok(())
}

/// Spec § 5.5 change 2: the private load labels a withheld load by commit only.
#[tokio::test]
async fn private_pin_load_labels_a_withheld_load_by_commit_only() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let broken = fx
        .etl
        .commit("zz-sentinel-71c3", "main", &[("workflow.yaml", DF_BROKEN)]);
    let (captured, _guard) = capture();
    let err = fx
        .mgr()
        .pinned_config_private("etl", &broken)
        .await
        .err()
        .expect("the broken commit does not load");
    assert!(err.downcast_ref::<PinLoadWithheld>().is_some(), "{err:#}");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "[pin] etl ({}) cannot be loaded: its configuration does not load",
            &broken[..7]
        )
    );
    let text = captured.text();
    assert!(text.contains(&format!("[pin] etl ({})", &broken[..7])), "{text}");
    assert!(!text.contains("zz-sentinel-71c3"), "{text}");
    assert!(!text.contains("s3cr3t-token-value"), "{text}");
    Ok(())
}
```

- [ ] **Step 10: Run the tests**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib templated_ref classify_execute_error_tests
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test:: git_refs_creation_test:: git_refs_scheduler_test::
```
Expected: PASS. The creation and scheduler suites prove the `MissingAtRef` texts and the classifier did not change for static refs.

- [ ] **Step 11: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server
git commit -m "feat(server): value-free RenderedRefError, private pin load, resolve span without the ref"
```

---
## Part D — creation and resolution

### Task 9: Job creation — `resolve_step_row`, deferred rows, the memo persisted

Spec § 4.2 rule 4, § 5.1, § 5.2, § 5.3 step 5, § 5.6 (creation half), § 7 (creation syntax check); contract deviation 4.

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs`:
  - imports (`:1-22`);
  - `create_job_for_task_inner`: the step loop (`:594-718`) and the creation transaction (`:721-790`);
  - `build_step` (the `fn build_step` after `TaskPinStamp`);
  - `RefMemo` (`impl RefMemo`, after `struct RefMemo`);
  - `resolve_step_action` (its signature and its first `if let`).
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: Task 1 (`classify_ref`, `RefKind`, `check_ref_template_syntax`, `DEFERRED_ACTION_TYPE`), Task 3 (`NewJobStep.ref_template`), Task 4 (`JobRefPinRepo`, `JobRefPinRow`).
- Produces:
  ```rust
  // crate::job_creator
  pub(crate) struct StepRowCtx<'a> {
      pub workspaces: &'a WorkspaceManager,
      pub job_id: Uuid,
      pub workspace_name: &'a str,
      pub workspace_config: &'a WorkspaceConfig,
      pub task_name: &'a str,
      pub job_pin: Option<&'a PinRef>,
      pub world: &'a RefWorld<'a>,
      pub memo: &'a RefMemo,
      pub ws_set: &'a WorkspaceSet<'a>,
      pub defaults: JobDefaults,
  }
  pub(crate) async fn resolve_step_row(ctx: &StepRowCtx<'_>, step_name: &str, flow_step: &FlowStep,
                                       status: StepStatus, git_ref_override: Option<&str>) -> Result<NewJobStep>;
  impl RefMemo {
      pub(crate) fn from_rows(rows: &[stroem_db::JobRefPinRow]) -> Self;
      pub(crate) fn entries(&self) -> Vec<stroem_db::JobRefPinRow>;
  }
  ```

- [ ] **Step 1: Write the shared test fixture and the failing creation tests**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs` (extend the imports with `use sqlx::PgPool;`, `use stroem_db::{JobRefPinRepo, JobStepRepo, JobStepRow};`, `use stroem_server::config::JobDefaults;`, `use stroem_server::job_creator::create_job_for_task_detailed;` — every helper and import must be used in the task that adds it: `clippy --all-targets -D warnings` fails on dead code in the test binary):

```rust
// ─── Fixture ───────────────────────────────────────────────────────────────

/// `etl` main gains a `type: task` action whose own `ref:` is a template.
const DF_MAIN_ACTIONS: &str = "actions:\n  call-nightly-at:\n    type: task\n    task: nightly\n    ref: \"{{ input.branch }}\"\n  hello:\n";

/// Templated-ref tasks, appended to `ETL_MAIN`'s `tasks:` section (its last key).
const DF_TASKS: &str = r#"
  tr-input:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      run:
        action: hello
        ref: "{{ input.branch }}"
  tr-later:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      first:
        action: hello
      later:
        action: hello
        ref: "{{ input.branch }}"
        depends_on: [first]
  tr-output:
    flow:
      pick:
        action: hello
      run:
        action: hello
        ref: "{{ pick.output.branch }}"
        depends_on: [pick]
  tr-each:
    flow:
      run:
        action: hello
        ref: "{{ each.item }}"
        for_each: ["release/2.3", "main", "release/2.3"]
  tr-each-objects:
    flow:
      run:
        action: hello
        ref: "{{ each.item }}"
        for_each: [{"b": "release/2.3"}]
  tr-memo:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      a:
        action: hello
        ref: release/2.3
      b:
        action: hello
        ref: "{{ input.branch }}"
        depends_on: [a]
  tr-memo-two:
    flow:
      x:
        action: hello
      y:
        action: hello
        ref: "{{ 'release/' ~ '2.3' }}"
        depends_on: [x]
      z:
        action: hello
        depends_on: [y]
      w:
        action: hello
        ref: "release/{{ '2.3' }}"
        depends_on: [z]
  tr-task:
    flow:
      run:
        action: call-local
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-gate:
    flow:
      run:
        action: gate
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-agent:
    flow:
      run:
        action: ask
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-missing-action:
    flow:
      run:
        action: nope
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-retry:
    flow:
      run:
        action: flaky
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-billing:
    flow:
      run:
        action: billing.export
        ref: "{{ input.tag }}"
  tr-docs:
    flow:
      run:
        action: docs.render
        ref: "{{ input.r }}"
  tr-bad-syntax:
    flow:
      run:
        action: hello
        ref: "{{ input.branch "
  tr-chain:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      a:
        action: hello
        ref: "{{ input.branch }}"
      b:
        action: hello
        depends_on: [a]
  tr-task-action:
    flow:
      call:
        action: call-nightly-at
        input:
          branch: release/2.3
  tr-task-action-later:
    flow:
      first:
        action: hello
      call:
        action: call-nightly-at
        depends_on: [first]
        input:
          branch: release/2.3
"#;

/// `etl` main with the templated-ref actions and tasks.
pub(crate) fn df_etl_main() -> String {
    format!(
        "{}{DF_TASKS}",
        ETL_MAIN.replace("actions:\n  hello:\n", DF_MAIN_ACTIONS)
    )
}

/// `etl` release/2.3 plus `flaky`, an action with its own `retry:`
/// (Review Focus 3 and 4).
pub(crate) fn df_etl_release() -> String {
    ETL_RELEASE.replace(
        "actions:\n  hello:\n",
        "actions:\n  flaky:\n    type: script\n    runner: local\n    script: exit 1\n    retry:\n      max_attempts: 3\n      delay: 1s\n  hello:\n",
    )
}

pub(crate) async fn df_fixture() -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(df_etl_main()),
        etl_release: Some(df_etl_release()),
        ..Default::default()
    })
    .await
}

/// A job of `task` in the live `etl` config, created like the execute API does.
pub(crate) async fn df_create(fx: &PinnedFixture, task: &str, input: serde_json::Value) -> Result<Uuid> {
    let cfg = fx.mgr().get_config("etl").await.expect("etl loaded");
    create_job_for_task_detailed(
        fx.mgr(),
        &fx.pool,
        &cfg,
        "etl",
        task,
        input,
        "api",
        None,
        Some(&fx.commits.etl_main),
        None,
        None,
        JobDefaults::default(),
    )
    .await
    .map(|c| c.job_id)
}

pub(crate) async fn df_step(pool: &PgPool, job_id: Uuid, name: &str) -> JobStepRow {
    JobStepRepo::get_step(pool, job_id, name)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("step {name} exists"))
}

async fn df_job_count(pool: &PgPool, task: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE task_name = $1")
        .bind(task)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ─── Creation (spec § 5.1, § 5.2, § 7) ─────────────────────────────────────

#[tokio::test]
async fn templated_step_ref_creates_a_deferred_row() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-later", json!({})).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.status, "pending");
    assert_eq!(later.action_type, "deferred");
    assert_eq!(later.required_ability, "deferred");
    assert_eq!(later.runner, "none");
    assert_eq!(later.required_tags, json!([]));
    assert_eq!(later.action_workspace.as_deref(), Some("etl"));
    assert_eq!(later.action_name, "hello");
    assert_eq!(later.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert!(later.action_spec.is_none() && later.action_ref.is_none() && later.action_revision.is_none());
    let first = df_step(&fx.pool, job_id, "first").await;
    assert_eq!(first.action_type, "script");
    assert!(first.ref_template.is_none());
    Ok(())
}

#[tokio::test]
async fn creation_persists_the_ref_memo() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-memo", json!({})).await?;
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    assert!(
        memo.iter().any(|e| e.workspace == "etl"
            && e.git_ref == "release/2.3"
            && e.commit == fx.commits.etl_release),
        "{memo:?}"
    );
    // A pinned job's own pin seeds its memo.
    let pinned = fx.create_pinned_etl_job("nightly").await?;
    let memo = JobRefPinRepo::list_for_job(&fx.pool, pinned).await?;
    assert!(
        memo.iter().any(|e| e.workspace == "etl"
            && e.git_ref == "release/2.3"
            && e.commit == fx.commits.etl_release),
        "{memo:?}"
    );
    Ok(())
}

#[tokio::test]
async fn templated_ref_syntax_error_is_a_400_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    let (status, body) = execute_task(&fx.router, "etl", "tr-bad-syntax", json!({}), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("invalid `ref` template"), "{body}");
    assert_eq!(df_job_count(&fx.pool, "tr-bad-syntax").await, 0);
    Ok(())
}

/// Contract deviation 4 (spec § 4.2 rule 4): the owner comes from the action
/// name, so a non-git owner is known — and refused — at creation.
#[tokio::test]
async fn templated_ref_on_a_non_git_owner_is_a_400_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    let (status, body) =
        execute_task(&fx.router, "etl", "tr-docs", json!({"r": "main"}), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("is not a git workspace"), "{body}");
    assert_eq!(df_job_count(&fx.pool, "tr-docs").await, 0);
    Ok(())
}

/// § 5.6 at creation: a `type: task` action whose own `ref:` is a template is
/// an ordinary task step with no task pin yet; the template is on the row
/// inside `action_spec`.
#[tokio::test]
async fn templated_task_action_ref_leaves_the_task_pin_empty_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-task-action-later", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "pending");
    assert_eq!(call.action_type, "task");
    assert!(call.task_workspace.is_none() && call.task_ref.is_none() && call.task_revision.is_none());
    assert_eq!(call.action_spec.unwrap()["ref"], json!("{{ input.branch }}"));
    assert!(call.ref_template.is_none());
    Ok(())
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::`
Expected: the new creation tests FAIL — creation answers ``step '…': invalid ref name: `ref` '{{ … }}' is a template`` (an `Err` from `df_create`, a 400 for the bad-syntax case but with the wrong text, a 400 "invalid ref name" for `tr-docs`).

- [ ] **Step 3: `RefMemo` gains `from_rows` and `entries`**

In `impl RefMemo` (job_creator.rs), make `get` and `insert` `pub(crate)`, and add:

```rust
    /// A memo holding a job's persisted entries (`job_ref_pin`), for the
    /// deferred-step pass (spec 2026-10-15 § 5.3 step 5).
    pub(crate) fn from_rows(rows: &[stroem_db::JobRefPinRow]) -> Self {
        let memo = Self::new();
        for row in rows {
            memo.insert(
                &row.workspace,
                &row.git_ref,
                PinRef {
                    git_ref: row.git_ref.clone(),
                    commit: row.commit.clone(),
                },
            );
        }
        memo
    }

    /// Every entry, to persist (`job_ref_pin`, spec 2026-10-15 § 5.2).
    pub(crate) fn entries(&self) -> Vec<stroem_db::JobRefPinRow> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .map(|((workspace, git_ref), pin)| stroem_db::JobRefPinRow {
                workspace: workspace.clone(),
                git_ref: git_ref.clone(),
                commit: pin.commit.clone(),
            })
            .collect()
    }
```

- [ ] **Step 4: `resolve_step_action` takes the ref to use**

Change its signature to add `step_ref: Option<&str>` after `flow_step: &FlowStep`, and its first line from `if let Some(step_ref) = flow_step.git_ref.as_deref() {` to `if let Some(step_ref) = step_ref {`. Update its doc comment: "`step_ref` is the step's own `ref:`, or — for a deferred step — the rendered value (spec 2026-10-15 § 5.3 step 5)."

- [ ] **Step 5: Extract the shared column helpers and refactor `build_step`**

Add next to `build_step`:

```rust
/// `timeout_secs` of a step: its own timeout, else the server default.
fn step_timeout(flow_step: &FlowStep, defaults: JobDefaults) -> Option<i32> {
    flow_step
        .timeout
        .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32"))
        .or(defaults.step_timeout_secs)
}

/// `for_each_expr`: the raw string of a template, compact JSON of a literal array.
fn for_each_text(flow_step: &FlowStep) -> Option<String> {
    flow_step.for_each.as_ref().map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// The four retry columns. `max_attempts` counts executions; the column counts
/// retries, so it stores `max_attempts - 1` (validation: `max_attempts >= 1`).
struct RetryCols {
    max_retries: Option<i32>,
    backoff_secs: Option<i32>,
    strategy: Option<String>,
    jitter: bool,
}

fn retry_cols(retry: Option<&stroem_common::models::workflow::RetryConfig>) -> RetryCols {
    RetryCols {
        max_retries: retry
            .map(|r| i32::try_from(r.max_attempts - 1).expect("max_attempts fits i32")),
        backoff_secs: retry
            .map(|r| i32::try_from(r.delay.as_secs()).expect("retry delay fits i32")),
        strategy: retry.map(|r| {
            if r.backoff == BackoffStrategy::Exponential {
                "exponential".to_string()
            } else {
                "fixed".to_string()
            }
        }),
        jitter: retry.is_some_and(|r| r.jitter),
    }
}
```

In `build_step`, replace the `timeout_secs`, `for_each_expr` and four retry expressions with the helpers:

```rust
    let retry = retry_cols(resolve_step_retry_config(flow_step, action).as_ref());
    // …
        timeout_secs: step_timeout(flow_step, defaults),
        when_condition: flow_step.when.clone(),
        for_each_expr: for_each_text(flow_step),
        // …
        max_retries: retry.max_retries,
        retry_backoff_secs: retry.backoff_secs,
        retry_strategy: retry.strategy,
        retry_jitter: retry.jitter,
```

(remove the old `let retry = resolve_step_retry_config(flow_step, action);`). `build_step` keeps its signature (the hook path calls it too) and its `ref_template: None` from Task 3.

- [ ] **Step 6: `StepRowCtx`, `resolve_step_row`, `deferred_row`, `initial_status`**

Imports: add `use stroem_common::git_ref::{classify_ref, RefKind};`, `use stroem_common::models::job::DEFERRED_ACTION_TYPE;`, `use stroem_common::template::check_ref_template_syntax;` and extend `use stroem_db::{…}` with `JobRefPinRepo`.

Add after `build_step`:

```rust
/// What every flow step of one job resolves against (spec 2026-10-15 § 5.3
/// step 5): the job's own config and pin, the ref world, the memo and the
/// job's connection set. Built once per creation and once per deferred-step
/// resolution.
pub(crate) struct StepRowCtx<'a> {
    pub workspaces: &'a WorkspaceManager,
    pub job_id: Uuid,
    pub workspace_name: &'a str,
    pub workspace_config: &'a WorkspaceConfig,
    pub task_name: &'a str,
    pub job_pin: Option<&'a PinRef>,
    pub world: &'a RefWorld<'a>,
    pub memo: &'a RefMemo,
    pub ws_set: &'a WorkspaceSet<'a>,
    pub defaults: JobDefaults,
}

/// The status a flow step is created with: a root step with no `when` and no
/// `for_each` is `ready`; every other step waits for the cascade.
fn initial_status(flow_step: &FlowStep) -> StepStatus {
    if flow_step.for_each.is_some() {
        // For-each steps always start pending — expanded at promotion time
        StepStatus::Pending
    } else if flow_step.depends_on.is_empty() && flow_step.when.is_none() {
        StepStatus::Ready
    } else {
        // Steps with `when` conditions start as pending even if they
        // have no deps — the post-creation promote loop evaluates them.
        StepStatus::Pending
    }
}

/// One flow step's row (spec 2026-10-15 § 5.3 step 5): its action resolved
/// (git-refs § 4.3), the creation checks run, the columns built — the SAME
/// code for job creation (static refs) and for the deferred-step pass
/// (`git_ref_override` = the rendered, checked value).
pub(crate) async fn resolve_step_row(
    ctx: &StepRowCtx<'_>,
    step_name: &str,
    flow_step: &FlowStep,
    status: StepStatus,
    git_ref_override: Option<&str>,
) -> Result<NewJobStep> {
    let StepAction {
        action,
        workspace: action_workspace,
        revision: action_revision,
        name: action_name,
        owner_cfg,
        pin: action_pin,
    } = resolve_step_action(
        ctx.workspaces,
        ctx.workspace_name,
        ctx.workspace_config,
        ctx.job_pin,
        step_name,
        flow_step,
        git_ref_override.or(flow_step.git_ref.as_deref()),
        ctx.world,
        ctx.memo,
    )
    .await?;

    // Fail fast (400) on literal connection references the worker would
    // otherwise reject at claim time. Templated values cannot be checked here.
    let mut task_pin: Option<TaskPinStamp> = None;
    if action.action_type == "task" {
        let task_ref = action
            .task
            .as_deref()
            .context("type: task action missing task field")?;
        match action.git_ref.as_deref() {
            // Spec 2026-10-15 § 5.6: a templated task ref resolves at
            // dispatch. Only its syntax is checked now; no task pin.
            Some(r) if classify_ref(r) == RefKind::Template => {
                check_ref_template_syntax(r)
                    .map_err(anyhow::Error::new)
                    .with_context(|| {
                        format!(
                            "step '{step_name}': action '{action_name}' has an invalid `ref` template"
                        )
                    })?;
            }
            task_git_ref => {
                let base_ws: &str = action_workspace.as_deref().unwrap_or(ctx.workspace_name);
                let base_cfg: &WorkspaceConfig =
                    owner_cfg.as_deref().unwrap_or(ctx.workspace_config);
                // The pin of the config this action is written in (§ 4.3 base
                // pin): the step's action pin, else — for an action of the
                // job's own config — the job's. A live foreign action has none.
                let base_pin: Option<PinRef> = match (&action_pin, &action_workspace) {
                    (Some(p), _) => Some(p.clone()),
                    (None, None) => ctx.job_pin.cloned(),
                    (None, Some(_)) => None,
                };
                let (resolved, pin) = resolve_task_for_step(
                    ctx.workspaces,
                    base_ws,
                    base_cfg,
                    base_pin.as_ref(),
                    task_ref,
                    task_git_ref,
                    ctx.world,
                    ctx.memo,
                )
                .await?;
                // A self-reference is the same task at the same pin; the same
                // task at another commit is a different version (bounded by
                // MAX_TASK_DEPTH like any nesting).
                if resolved.workspace == ctx.workspace_name
                    && resolved.task_name == ctx.task_name
                    && pin.as_ref() == ctx.job_pin
                {
                    bail!(
                        "task '{}' is a self-reference to '{}/{}' (invalid)",
                        task_ref,
                        ctx.workspace_name,
                        ctx.task_name
                    );
                }
                precheck_task_step_literals(
                    step_name,
                    flow_step,
                    &resolved,
                    ctx.workspaces,
                    RoleConfig {
                        workspace: ctx.workspace_name,
                        config: ctx.workspace_config,
                    },
                    base_cfg,
                )
                .await?;
                task_pin = pin.map(|p| TaskPinStamp {
                    workspace: resolved.workspace.clone(),
                    git_ref: p.git_ref,
                    commit: p.commit,
                });
            }
        }
    } else {
        // Role-scoped (spec § 7.4): the job's own config is the caller, the
        // action's owner config (pinned or live) its owner — the same roles
        // the claim path resolves with.
        let roles = RoleScope {
            caller: RoleConfig {
                workspace: ctx.workspace_name,
                config: ctx.workspace_config,
            },
            action_owner: action_workspace.as_deref().map(|ws| RoleConfig {
                workspace: ws,
                config: owner_cfg.as_deref().unwrap_or(ctx.workspace_config),
            }),
            task_owner: None,
            others: ctx.ws_set,
        };
        precheck_literal_connection_inputs(step_name, flow_step, &action, &roles)?;
    }

    Ok(build_step(
        ctx.job_id,
        step_name,
        action_name,
        flow_step,
        &action,
        Some(serde_json::to_value(&flow_step.input).unwrap_or_default()),
        status,
        ctx.defaults,
        action_workspace,
        action_revision,
        action_pin.map(|p| p.git_ref),
        task_pin,
    ))
}

/// A flow step whose `ref:` is a template (spec 2026-10-15 § 5.1): the owner
/// is decided now from the action name (D11, § 4.2 rule 4 — an unknown, a
/// library or a non-git owner is a 400 here), the template is checked for
/// syntax (§ 7), and everything derived from the action is left for the
/// resolution pass. Retry columns come from the flow step's own `retry:`
/// when present; the action's are filled at resolution.
fn deferred_row(
    ctx: &StepRowCtx<'_>,
    step_name: &str,
    flow_step: &FlowStep,
    status: StepStatus,
) -> Result<NewJobStep> {
    let template = flow_step
        .git_ref
        .as_deref()
        .expect("deferred_row is called only for a templated ref");
    check_ref_template_syntax(template)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("step '{step_name}': invalid `ref` template"))?;
    let RefPlan::AtRef { target, .. } = plan_reference(
        ctx.workspace_name,
        None,
        &flow_step.action,
        Some(template),
        ctx.world,
    )?
    else {
        unreachable!("plan_reference returns AtRef whenever a ref is given")
    };
    let retry = retry_cols(flow_step.retry.as_ref());
    Ok(NewJobStep {
        job_id: ctx.job_id,
        step_name: step_name.to_string(),
        action_name: target.local_name,
        action_type: DEFERRED_ACTION_TYPE.to_string(),
        action_image: None,
        action_spec: None,
        input: Some(serde_json::to_value(&flow_step.input).unwrap_or_default()),
        status: status.to_string(),
        required_ability: DEFERRED_ACTION_TYPE.to_string(),
        required_tags: Vec::new(),
        runner: "none".to_string(),
        timeout_secs: step_timeout(flow_step, ctx.defaults),
        when_condition: flow_step.when.clone(),
        for_each_expr: for_each_text(flow_step),
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        max_retries: retry.max_retries,
        retry_backoff_secs: retry.backoff_secs,
        retry_strategy: retry.strategy,
        retry_jitter: retry.jitter,
        action_workspace: Some(target.owner),
        action_revision: None,
        action_ref: None,
        task_workspace: None,
        task_ref: None,
        task_revision: None,
        ref_template: Some(template.to_string()),
    })
}
```

- [ ] **Step 7: The creation loop and the memo write**

In `create_job_for_task_inner`, replace the whole `for (step_name, flow_step) in &task.flow { … }` loop (`:594-718`) with:

```rust
        let ctx = StepRowCtx {
            workspaces,
            job_id,
            workspace_name,
            workspace_config,
            task_name,
            job_pin: job_pin.as_ref(),
            world: &world,
            memo: &memo,
            ws_set: &ws_set,
            defaults,
        };
        for (step_name, flow_step) in &task.flow {
            let status = initial_status(flow_step);
            let step = match flow_step.git_ref.as_deref().map(classify_ref) {
                // Spec 2026-10-15 § 5.1: resolved when the step becomes ready.
                Some(RefKind::Template) => deferred_row(&ctx, step_name, flow_step, status)?,
                _ => resolve_step_row(&ctx, step_name, flow_step, status, None).await?,
            };
            new_steps.push(step);
        }
```

Inside the creation transaction, directly after `JobStepRepo::create_steps_tx(&mut *tx, &new_steps)…?;`, add:

```rust
        // The job's ref memo (spec 2026-10-15 § 5.2): its own pin and every
        // static ref this creation resolved. Deferred steps resolve against it.
        JobRefPinRepo::insert_many_tx(&mut *tx, job_id, &memo.entries())
            .await
            .context("persist the job's ref memo")?;
```

- [ ] **Step 8: Run the new tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::`
Expected: PASS. (`tr-later`'s `later` stays `pending` because nothing resolves deferred steps yet; that comes in Task 10.)

- [ ] **Step 9: Run the creation regression suites**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib job_creator
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_creation_test:: git_refs_claim_test:: git_refs_scheduler_test:: pinned_rerun_restart_test:: restart_integration_test:: rerun_integration_test::
```
Expected: PASS — the refactor changes no behaviour for static refs.

- [ ] **Step 10: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/tests/git_refs_deferred_test.rs
git commit -m "feat(creation): templated flow-step refs create deferred rows; resolve_step_row shared; ref memo persisted"
```

---
### Task 10: The resolution pass, wired into `advance` and `init`

Spec § 5.3 (steps 0–7), § 5.4, § 5.5, § 10.3; contract deviations 5, 8, 11.

**Files:**
- Create: `crates/stroem-server/src/settlement/deferred.rs`
- Modify: `crates/stroem-server/src/settlement/mod.rs` — `pub mod deferred;` (module list, `:6-11`); `advance` step 3 (between `cascade_and_settle` and `dispatch::handle_task_steps`)
- Modify: `crates/stroem-server/src/settlement/dispatch.rs` — `init` (between `crate::cascade::execute` and `handle_task_steps`)
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: Task 1 (`check_rendered_ref`, `render_ref_template`), Task 4 (`JobRefPinRepo`), Task 5 (lease + guarded writes), Task 7 (`Scope::StepRef`), Task 8 (`RenderedRefError`, `pinned_config_private`), Task 9 (`StepRowCtx`, `resolve_step_row`, `RefMemo::{from_rows, entries}`).
- Produces:
  ```rust
  // crate::settlement::deferred
  pub const DEFERRED_LEASE_SECS: u64 = 60;
  pub const DEFERRED_LEASE_RENEW_SECS: u64 = 20;
  #[doc(hidden)] #[derive(Debug, Clone, Copy)] pub struct LeaseTiming { pub lease: Duration, pub renew: Duration } // Default = 60 s / 20 s
  pub async fn resolve_deferred_steps(workspaces: &WorkspaceManager, pool: &PgPool, workspace_config: &WorkspaceConfig,
      workspace_name: &str, job_id: Uuid, task: &TaskDef, defaults: JobDefaults, snapshots: &Snapshots) -> Result<()>;
  #[doc(hidden)] pub async fn resolve_deferred_steps_with(/* same */, timing: LeaseTiming) -> Result<()>;
  pub(crate) async fn ref_secret_set(workspaces: &WorkspaceManager, pool: &PgPool, job: &JobRow)
      -> Result<std::result::Result<Vec<String>, RenderedRefError>>;
  pub(crate) async fn select_commit(workspaces: &WorkspaceManager, pool: &PgPool, job_id: Uuid, owner: &str, value: &str)
      -> Result<std::result::Result<PinRef, RenderedRefError>>;
  ```
  The two `pub(crate)` helpers are reused by Task 13 (§ 5.6). Their outer `Result` is an escaping DB error; the inner one fails the step.

- [ ] **Step 1: Write the failing integration tests**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs` (extend the imports: `JobRepo` in the `stroem_db::{…}` list, and `use stroem_server::job_creator::create_child_job_for_task_detailed;`):

```rust
// ─── Resolution at promotion (spec § 5.3) ─────────────────────────────────

/// release/2.3 one commit later: `hello` now echoes v2.
pub(crate) fn df_release_v2(fx: &PinnedFixture) -> String {
    fx.etl.commit(
        "release/2.3",
        "release/2.3",
        &[("workflow.yaml", &df_etl_release().replace("echo v1", "echo v2"))],
    )
}

/// The headline case: a ROOT step with `ref: "{{ input.branch }}"` is
/// resolved by `init`, inside the creating request — no recovery wait.
#[tokio::test]
async fn root_templated_ref_is_resolved_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    // Review Focus 1: a trailing newline (a pasted form value) is trimmed.
    for branch in ["release/2.3", "release/2.3\n"] {
        let (status, body) =
            execute_task(&fx.router, "etl", "tr-input", json!({"branch": branch}), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
        let run = df_step(&fx.pool, job_id, "run").await;
        assert_eq!(run.status, "ready", "{branch:?}");
        assert_eq!(run.action_type, "script");
        assert_eq!(run.required_ability, "script");
        assert_eq!(run.runner, "local");
        assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));
        assert_eq!(run.action_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
        assert_eq!(run.action_spec.as_ref().unwrap()["script"], json!("echo v1"));
        assert_eq!(run.ref_template.as_deref(), Some("{{ input.branch }}"));
    }
    // The claim returns the branch's action and commit.
    let worker = register_worker(&fx.router, &["script"]).await;
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["revision"], json!(fx.commits.etl_release));
    assert_eq!(claimed["action_spec"]["script"], json!("echo v1"));
    Ok(())
}

#[tokio::test]
async fn ref_read_from_an_earlier_steps_output() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-output", json!({})).await?;
    assert_eq!(df_step(&fx.pool, job_id, "run").await.action_type, "deferred");
    let worker = register_worker(&fx.router, &["script"]).await;
    claim_and_complete(&fx, &worker, job_id, "pick", json!({"output": {"branch": "release/2.3"}}))
        .await;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "ready");
    assert_eq!(run.action_type, "script");
    assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(run.action_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    Ok(())
}

/// § 5.4: each instance renders its own `each`; equal values share a commit.
#[tokio::test]
async fn for_each_instances_resolve_their_own_values_and_share_equal_ones() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-each", json!({})).await?;
    for (i, want) in [
        (0, &fx.commits.etl_release),
        (1, &fx.commits.etl_main),
        (2, &fx.commits.etl_release),
    ] {
        let s = df_step(&fx.pool, job_id, &format!("run[{i}]")).await;
        assert_eq!(s.action_type, "script", "run[{i}]");
        assert_eq!(s.action_revision.as_deref(), Some(want.as_str()), "run[{i}]");
    }
    let placeholder = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(placeholder.action_type, "deferred", "the placeholder never runs");
    assert_eq!(placeholder.status, "running");
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    let etl: Vec<&str> = memo
        .iter()
        .filter(|e| e.workspace == "etl")
        .map(|e| e.git_ref.as_str())
        .collect();
    assert_eq!(etl, vec!["main", "release/2.3"]);
    Ok(())
}

/// Success criterion 4: a static ref and a template rendering the same value
/// get ONE commit, even after the branch moves between creation and
/// promotion; so do two promotions of the same rendered value.
#[tokio::test]
async fn one_commit_per_ref_survives_a_branch_move() -> Result<()> {
    let fx = df_fixture().await?;
    let worker = register_worker(&fx.router, &["script"]).await;

    let job_id = df_create(&fx, "tr-memo", json!({})).await?;
    let v2 = df_release_v2(&fx);
    assert_ne!(v2, fx.commits.etl_release);
    claim_and_complete(&fx, &worker, job_id, "a", json!({"output": {}})).await;
    let b = df_step(&fx.pool, job_id, "b").await;
    assert_eq!(b.action_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    // A claim takes ANY ready step: finish job 1 so job 2's claims are exact.
    claim_and_complete(&fx, &worker, job_id, "b", json!({"output": {}})).await;

    let job2 = df_create(&fx, "tr-memo-two", json!({})).await?;
    claim_and_complete(&fx, &worker, job2, "x", json!({"output": {}})).await;
    let y = df_step(&fx.pool, job2, "y").await;
    assert_eq!(y.action_revision.as_deref(), Some(v2.as_str()), "y resolved at the tip");
    let v3 = fx.etl.commit(
        "release/2.3",
        "release/2.3",
        &[("workflow.yaml", &df_etl_release().replace("echo v1", "echo v3"))],
    );
    claim_and_complete(&fx, &worker, job2, "y", json!({"output": {}})).await;
    claim_and_complete(&fx, &worker, job2, "z", json!({"output": {}})).await;
    let w = df_step(&fx.pool, job2, "w").await;
    assert_eq!(w.action_revision.as_deref(), Some(v2.as_str()), "w reuses y's commit, not {v3}");
    Ok(())
}

/// Spec § 5.5: each resolution error fails ONLY that step with a fixed,
/// value-free sentence; the job settles; the execute call still returns the
/// job (D8: no pre-check); nothing failed reaches the memo.
#[tokio::test]
async fn resolution_failures_fail_the_step_with_a_value_free_message() -> Result<()> {
    let fx = df_fixture().await?;
    let cases: &[(&str, serde_json::Value, &str)] = &[
        ("tr-input", json!({"branch": "  "}), "[ref] step 'run': the rendered ref is empty"),
        (
            "tr-input",
            json!({"branch": "release/9.9"}),
            "[ref] step 'run': the rendered ref was not found in workspace 'etl'",
        ),
        (
            "tr-input",
            json!({"branch": "a..b"}),
            "[ref] step 'run': the rendered ref is not a valid branch, tag or full commit SHA",
        ),
        (
            "tr-missing-action",
            json!({}),
            "[ref] step 'run': workspace 'etl' has no such action or task at the rendered ref",
        ),
        (
            "tr-agent",
            json!({}),
            "[ref] step 'run': the action at the rendered ref is an agent action; agent actions cannot be referenced with `ref` yet",
        ),
        (
            "tr-billing",
            json!({"tag": "v9.9.9"}),
            "[ref] step 'run': the rendered ref was not found in workspace 'billing'",
        ),
    ];
    for (task, input, want) in cases {
        let (status, body) = execute_task(&fx.router, "etl", task, input.clone(), None).await;
        assert_eq!(status, StatusCode::OK, "{task} {input}: {body}");
        let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
        let run = df_step(&fx.pool, job_id, "run").await;
        assert_eq!(run.status, "failed", "{task} {input}");
        assert_eq!(run.error_message.as_deref(), Some(*want), "{task} {input}");
        assert_eq!(run.action_type, "deferred", "a failed resolution keeps the deferred row");
        assert_eq!(JobRepo::get(&fx.pool, job_id).await?.unwrap().status, "failed");
        let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
        assert!(
            memo.iter().all(|e| e.git_ref != "release/9.9" && e.git_ref != "v9.9.9"),
            "{memo:?}"
        );
    }
    // The owner comes from the action name: `billing.export` at a tag.
    let job_id = df_create(&fx, "tr-billing", json!({"tag": "v4.1.0"})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_workspace.as_deref(), Some("billing"));
    assert_eq!(run.action_revision.as_deref(), Some(fx.commits.billing_tag.as_str()));
    Ok(())
}

#[tokio::test]
async fn a_rendered_ref_holding_a_live_secret_is_refused() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(format!("secrets:\n  deploy_token: tok-9f8e7d6c5b\n{}", df_etl_main())),
        etl_release: Some(df_etl_release()),
        ..Default::default()
    })
    .await?;
    let job_id = df_create(&fx, "tr-input", json!({"branch": "feature-tok-9f8e7d6c5b"})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "failed");
    assert_eq!(
        run.error_message.as_deref(),
        Some("[ref] step 'run': the rendered ref contains a secret value")
    );
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    assert!(memo.iter().all(|e| !e.git_ref.contains("tok-")), "{memo:?}");
    Ok(())
}

/// D7: a temporary git failure at promotion fails the step (no wait).
#[tokio::test]
async fn a_temporary_git_failure_fails_the_step() -> Result<()> {
    let fx = df_fixture().await?;
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let (status, body) = execute_task(
        &replica.router,
        "etl",
        "tr-input",
        json!({"branch": "release/2.3"}),
        None,
    )
    .await;
    fx.etl.restore_remote();
    assert_eq!(status, StatusCode::OK, "{body}");
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "failed");
    assert_eq!(
        run.error_message.as_deref(),
        Some("[ref] step 'run': workspace 'etl' is not available at the rendered ref yet")
    );
    Ok(())
}

/// After resolution, a `type: task` step is dispatched in the same `init`,
/// its child pinned to the commit the template selected.
#[tokio::test]
async fn a_resolved_task_action_dispatches_a_pinned_child() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-task", json!({})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_type, "task");
    assert_eq!(run.status, "running", "dispatched after the pass");
    assert_eq!(run.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(run.task_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    let child: Uuid = sqlx::query_scalar("SELECT job_id FROM job WHERE parent_job_id = $1")
        .bind(job_id)
        .fetch_one(&fx.pool)
        .await?;
    let child = JobRepo::get(&fx.pool, child).await?.unwrap();
    assert_eq!(child.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(child.revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    Ok(())
}

#[tokio::test]
async fn a_resolved_approval_step_suspends() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-gate", json!({})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_type, "approval");
    assert_eq!(run.status, "suspended");
    Ok(())
}

/// An agent task-tool child whose only root step fails resolution at `init`
/// is born terminal (spec § 5.3 consequences).
#[tokio::test]
async fn an_agent_tool_child_whose_root_fails_resolution_is_born_terminal() -> Result<()> {
    let fx = df_fixture().await?;
    let parent = df_create(&fx, "tr-later", json!({})).await?;
    let cfg = fx.mgr().get_config("etl").await.expect("etl loaded");
    let created = create_child_job_for_task_detailed(
        fx.mgr(),
        &fx.pool,
        &cfg,
        "etl",
        "tr-input",
        json!({"branch": "release/9.9"}),
        "agent_tool",
        None,
        parent,
        "first",
        Some(&fx.commits.etl_main),
        JobDefaults::default(),
        None,
    )
    .await?;
    assert!(fx.state.settlement().agent_child_created(created).await.is_err());
    Ok(())
}

/// Review Focus 2: an object item renders JSON text (or a render error);
/// either way the instance fails and its message carries none of it.
#[tokio::test]
async fn for_each_over_objects_fails_each_instance_without_echoing_it() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-each-objects", json!({})).await?;
    let inst = df_step(&fx.pool, job_id, "run[0]").await;
    assert_eq!(inst.status, "failed");
    let msg = inst.error_message.unwrap();
    assert!(
        msg == "[ref] step 'run[0]': the rendered ref is not a valid branch, tag or full commit SHA"
            || msg.starts_with("[ref] step 'run[0]': the ref template could not be rendered"),
        "{msg}"
    );
    assert!(!msg.contains("release/2.3") && !msg.contains('{'), "{msg}");
    Ok(())
}

/// Review Focus 3: the flow step has no `retry:`; the resolved action's own
/// retry fills the row.
#[tokio::test]
async fn a_resolved_action_brings_its_own_retry() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-retry", json!({})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_type, "script");
    assert_eq!(run.max_retries, Some(2));
    assert_eq!(run.retry_backoff_secs, Some(1));
    assert_eq!(run.retry_strategy.as_deref(), Some("fixed"));
    Ok(())
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::`
Expected: the new tests FAIL — every templated step stays `deferred` (nothing resolves it).

- [ ] **Step 3: Write the pass**

Create `crates/stroem-server/src/settlement/deferred.rs`:

```rust
//! The deferred-step pass (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`
//! § 5.3). A flow step whose `ref:` is a template waits at `ready` as
//! `action_type = 'deferred'` until this pass renders the template, resolves
//! the commit (one per `(job, owner, ref)`, table `job_ref_pin`), checks the
//! value against the job's ref secret set before and after the commit's
//! config loads, and writes the step's real columns with the code job
//! creation uses. Runs in `Settlement::advance` and `dispatch::init`, after
//! the cascade and before task dispatch. `cascade::run` stays pure.
//!
//! A rendered value is private until both secret checks pass: no span, log
//! line or error text carries it (§ 5.5). Every step failure is a
//! `RenderedRefError`; only DB errors escape.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use stroem_common::git_ref::check_rendered_ref;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_common::template::render_ref_template;
use stroem_db::{DeferredWrite, JobRefPinRepo, JobRepo, JobRow, JobStepRepo, JobStepRow, NewJobStep};
use uuid::Uuid;

use crate::config::JobDefaults;
use crate::job_creator::{ref_world_sets, resolve_step_row, RefMemo, StepRowCtx};
use crate::refs::RefWorld;
use crate::render_context::{self, JobContext, LoopSlot, Scope, Snapshots};
use crate::templated_ref::{RenderedRefError, RenderedRefKind};
use crate::workspace::pins::PinRef;
use crate::workspace::WorkspaceManager;
use crate::workspace_set::{collect_redaction_values, WorkspaceSet};

/// Lease length (D12): a resolver whose process dies loses its lease in this time.
pub const DEFERRED_LEASE_SECS: u64 = 60;
/// Renewal period of a held lease.
pub const DEFERRED_LEASE_RENEW_SECS: u64 = 20;

/// Lease length and renewal period. Production uses the default; tests
/// shorten both.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct LeaseTiming {
    pub lease: Duration,
    pub renew: Duration,
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(DEFERRED_LEASE_SECS),
            renew: Duration::from_secs(DEFERRED_LEASE_RENEW_SECS),
        }
    }
}

/// One step's step-failure-or-row: the inner `Err` fails the step.
type Resolution<T> = std::result::Result<T, RenderedRefError>;

/// Owns the renewal task of one resolution lease. `Drop` aborts the task, so
/// renewal stops on EVERY exit — success, error, panic, or the resolver future
/// being dropped (a cancelled HTTP request during `init`). A detached renewal
/// task would keep a `ready` row leased forever, out of Phase 4.6's reach.
struct LeaseGuard {
    token: Uuid,
    lost: Arc<AtomicBool>,
    renew: tokio::task::JoinHandle<()>,
}

impl LeaseGuard {
    /// § 5.3 step 0. `None`: another resolver holds a live lease, or the step
    /// is no longer a deferred `ready` row.
    async fn take(pool: &PgPool, job_id: Uuid, step: &str, timing: LeaseTiming) -> Result<Option<Self>> {
        let token = Uuid::new_v4();
        let secs = timing.lease.as_secs_f64();
        if !JobStepRepo::take_resolve_lease(pool, job_id, step, token, secs).await? {
            return Ok(None);
        }
        let lost = Arc::new(AtomicBool::new(false));
        let (pool, step, flag) = (pool.clone(), step.to_string(), Arc::clone(&lost));
        let renew = tokio::spawn(async move {
            let mut tick = tokio::time::interval(timing.renew);
            tick.tick().await; // the first tick completes at once
            loop {
                tick.tick().await;
                match JobStepRepo::renew_resolve_lease(&pool, job_id, &step, token, secs).await {
                    Ok(true) => {}
                    Ok(false) => {
                        flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    Err(e) => tracing::warn!(%job_id, step = %step, "renewing a resolution lease failed: {e:#}"),
                }
            }
        });
        Ok(Some(Self { token, lost, renew }))
    }

    /// A renewal changed no row: another resolver took the lease over. The
    /// token fence would refuse this holder's write anyway.
    fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.renew.abort();
    }
}

/// Resolve every deferred `ready` step of `job_id` (spec § 5.3). Loops like
/// `handle_task_steps`: a failure re-cascades, which can promote more
/// deferred steps; bounded by the flow size. Only DB errors escape.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(workspaces, pool, workspace_config, task, snapshots))]
pub async fn resolve_deferred_steps(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
) -> Result<()> {
    resolve_deferred_steps_with(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        job_id,
        task,
        defaults,
        snapshots,
        LeaseTiming::default(),
    )
    .await
}

/// [`resolve_deferred_steps`] with an explicit lease timing (tests).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn resolve_deferred_steps_with(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
    timing: LeaseTiming,
) -> Result<()> {
    // Every failing pass retires at least one step, like `handle_task_steps`.
    for _ in 0..(task.flow.len() + 1) {
        let failed_any = resolve_pass(
            workspaces,
            pool,
            workspace_config,
            workspace_name,
            job_id,
            task,
            defaults,
            snapshots,
            timing,
        )
        .await?;
        if !failed_any {
            break;
        }
    }
    Ok(())
}

/// One pass over the deferred `ready` steps. `true` if a step was failed.
#[allow(clippy::too_many_arguments)]
async fn resolve_pass(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
    timing: LeaseTiming,
) -> Result<bool> {
    let deferred = JobStepRepo::get_ready_deferred_steps(pool, job_id).await?;
    if deferred.is_empty() {
        return Ok(false);
    }
    let Some(job) = JobRepo::get(pool, job_id).await? else {
        return Ok(false);
    };
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let mut failed_any = false;
    for step in &deferred {
        let Some(lease) = LeaseGuard::take(pool, job_id, &step.step_name, timing).await? else {
            continue;
        };
        let outcome = resolve_one(
            workspaces,
            pool,
            workspace_config,
            workspace_name,
            &job,
            &steps,
            step,
            task,
            defaults,
            snapshots,
        )
        .await?;
        if lease.is_lost() {
            continue;
        }
        match outcome {
            Ok(row) => {
                if JobStepRepo::resolve_deferred_tx(pool, job_id, &step.step_name, lease.token, &row)
                    .await?
                    == DeferredWrite::Applied
                {
                    tracing::info!(%job_id, step = %step.step_name, "[ref] deferred step resolved");
                }
            }
            Err(err) => {
                let message = format!("[ref] step '{}': {}", step.step_name, err);
                if JobStepRepo::fail_deferred_tx(pool, job_id, &step.step_name, lease.token, &message)
                    .await?
                    == DeferredWrite::Applied
                {
                    tracing::warn!(%job_id, "{message}");
                    // The `fail_task_step` pattern: re-cascade so dependents are
                    // skipped and the job can settle.
                    if let Err(e) = crate::settlement::cascade_and_settle(
                        pool,
                        job_id,
                        task,
                        workspace_config,
                        snapshots,
                    )
                    .await
                    {
                        tracing::error!(
                            "Failed to orchestrate after deferred step '{}' failed in job {}: {:#}",
                            step.step_name,
                            job_id,
                            e
                        );
                    }
                    failed_any = true;
                }
            }
        }
    }
    Ok(failed_any)
}

/// § 5.3 steps 1–6 for one step.
#[allow(clippy::too_many_arguments)]
async fn resolve_one(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job: &JobRow,
    steps: &[JobStepRow],
    step: &JobStepRow,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
) -> Result<Resolution<NewJobStep>> {
    // The owner was decided at creation from the action name (D11).
    let owner = step.action_workspace.as_deref().unwrap_or(workspace_name);
    let unresolvable = || RenderedRefError::new(RenderedRefKind::Unresolvable, owner);
    let Some(template) = step.ref_template.as_deref() else {
        return Ok(Err(unresolvable()));
    };
    let flow_key = step.loop_source.as_deref().unwrap_or(&step.step_name);
    let Some(flow_step) = task.flow.get(flow_key) else {
        return Ok(Err(unresolvable()));
    };

    // 1. Job input, no `secret`, `each` for an instance.
    let job_ctx = JobContext {
        job_id: job.job_id,
        job_input: job.input.as_ref(),
        caller_secrets: &workspace_config.secrets,
        owner_secrets: &workspace_config.secrets,
        snapshots,
        job_revision: job.revision.as_deref(),
        job_ref: job.git_ref.as_deref(),
    };
    let ctx = render_context::build(
        &job_ctx,
        &render_context::views(steps),
        LoopSlot::of(step),
        Scope::StepRef,
    );

    // 2. Render, then the post-render check against the ref secret set.
    let rendered = match render_ref_template(template, ctx.as_value()) {
        Ok(r) => r,
        Err(e) => return Ok(Err(RenderedRefError::from_error(&e, owner))),
    };
    let secrets = match ref_secret_set(workspaces, pool, job).await? {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let value = match check_rendered_ref(&rendered, &secrets) {
        Ok(v) => v,
        Err(r) => return Ok(Err(RenderedRefError::from_rejection(r, owner))),
    };

    // 3–4. The commit: memo, else resolve; loaded privately; checked against
    // that commit's own secrets; only then in the memo.
    if let Err(e) = select_commit(workspaces, pool, job.job_id, owner, &value).await? {
        return Ok(Err(e));
    }

    // 5–6. `value` passed both checks: it is an identifier from here on. The
    // row is built by the code creation uses, from the row's own owner and
    // action name (decided at creation) and the job config's flow step.
    let mut written = flow_step.clone();
    written.action = if owner == workspace_name {
        step.action_name.clone()
    } else {
        format!("{owner}.{}", step.action_name)
    };
    build_row(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        job,
        &step.step_name,
        &written,
        &value,
        owner,
        defaults,
    )
    .await
}

/// The ref secret set of a step before resolution (spec § 10.3): the live set
/// plus the pin secrets of the job's own pin and of every commit in its
/// `job_ref_pin`. A pin that cannot be loaded fails the step (D7).
pub(crate) async fn ref_secret_set(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    job: &JobRow,
) -> Result<Resolution<Vec<String>>> {
    let set = WorkspaceSet::load(workspaces, &job.workspace, None).await;
    let mut values = collect_redaction_values(&set);
    let mut pins: BTreeSet<(String, String)> = BTreeSet::new();
    if let Some(p) = PinRef::of_job(job) {
        pins.insert((job.workspace.clone(), p.commit));
    }
    for row in JobRefPinRepo::list_for_job(pool, job.job_id).await? {
        pins.insert((row.workspace, row.commit));
    }
    for (ws, commit) in pins {
        let pinned = match workspaces.pinned_config_private(&ws, &commit).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(RenderedRefError::from_error(&e, &ws))),
        };
        values.extend(workspaces.pin_redaction_values(&ws, &pinned).await);
    }
    Ok(Ok(values))
}

/// § 5.3 steps 3–4 / § 5.2: the commit `owner@value` runs at in `job_id`.
/// Looked up in the memo, else resolved; loaded through the private path;
/// the value checked against THAT commit's own secrets; only then written to
/// the memo, first writer wins — if another resolver wrote a different
/// commit first, that one is loaded and checked instead.
pub(crate) async fn select_commit(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    job_id: Uuid,
    owner: &str,
    value: &str,
) -> Result<Resolution<PinRef>> {
    let fail = |e: anyhow::Error| RenderedRefError::from_error(&e, owner);
    let mut commit = match JobRefPinRepo::get(pool, job_id, owner, value).await? {
        Some(c) => c,
        None => match workspaces.pins().resolve(owner, value).await {
            Ok(pin) => pin.commit,
            Err(e) => return Ok(Err(fail(e.into()))),
        },
    };
    for _ in 0..3 {
        let pinned = match workspaces.pinned_config_private(owner, &commit).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(fail(e))),
        };
        let own = workspaces.pin_redaction_values(owner, &pinned).await;
        if let Err(r) = check_rendered_ref(value, &own) {
            return Ok(Err(RenderedRefError::from_rejection(r, owner)));
        }
        let stored = JobRefPinRepo::insert_first_wins(pool, job_id, owner, value, &commit).await?;
        if stored == commit {
            return Ok(Ok(PinRef {
                git_ref: value.to_string(),
                commit,
            }));
        }
        commit = stored;
    }
    Ok(Err(RenderedRefError::new(RenderedRefKind::Unresolvable, owner)))
}

/// § 5.3 steps 5–6 through `job_creator::resolve_step_row`, with nested
/// static refs kept in the job's memo too (contract deviation 8): the memo is
/// seeded from `job_ref_pin`, every entry the row met is written first-writer-
/// wins, and the row is rebuilt (at most 3 times) when one disagrees.
#[allow(clippy::too_many_arguments)]
async fn build_row(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job: &JobRow,
    step_name: &str,
    written: &FlowStep,
    value: &str,
    owner: &str,
    defaults: JobDefaults,
) -> Result<Resolution<NewJobStep>> {
    let (library_names, configured, git) = ref_world_sets(workspaces);
    let world = RefWorld {
        library_names: &library_names,
        configured: &configured,
        git: &git,
    };
    let ws_set = WorkspaceSet::load(workspaces, workspace_name, Some(workspace_config)).await;
    let job_pin = PinRef::of_job(job);
    for _ in 0..3 {
        let memo = RefMemo::from_rows(&JobRefPinRepo::list_for_job(pool, job.job_id).await?);
        let ctx = StepRowCtx {
            workspaces,
            job_id: job.job_id,
            workspace_name,
            workspace_config,
            task_name: &job.task_name,
            job_pin: job_pin.as_ref(),
            world: &world,
            memo: &memo,
            ws_set: &ws_set,
            defaults,
        };
        let row = match resolve_step_row(&ctx, step_name, written, StepStatus::Ready, Some(value)).await {
            Ok(row) => row,
            Err(e) => return Ok(Err(RenderedRefError::from_error(&e, owner))),
        };
        let mut agreed = true;
        for entry in memo.entries() {
            let stored = JobRefPinRepo::insert_first_wins(
                pool,
                job.job_id,
                &entry.workspace,
                &entry.git_ref,
                &entry.commit,
            )
            .await?;
            agreed &= stored == entry.commit;
        }
        if agreed {
            return Ok(Ok(row));
        }
    }
    Ok(Err(RenderedRefError::new(RenderedRefKind::Unresolvable, owner)))
}
```

Add `pub mod deferred;` to the module list in `crates/stroem-server/src/settlement/mod.rs`.

- [ ] **Step 4: Wire the pass into `advance` and `init`**

In `Settlement::advance` (`crates/stroem-server/src/settlement/mod.rs`), directly after `cascade_and_settle(&self.pool, job_id, task, workspace, &snapshots).await?;`:

```rust
            // Deferred steps (spec 2026-10-15 § 5.3): resolve every templated
            // `ref:` this cascade promoted, before task / approval dispatch
            // reads the real action types. An escaping error is a DB error:
            // logged like a task-dispatch error, never propagated.
            if let Err(e) = deferred::resolve_deferred_steps(
                &self.workspaces,
                &self.pool,
                workspace,
                &job.workspace,
                job_id,
                task,
                self.defaults,
                &snapshots,
            )
            .await
            {
                tracing::error!("Failed to resolve deferred steps for job {}: {:#}", job_id, e);
                self.server_log(
                    job_id,
                    &format!("[orchestration] Failed to resolve deferred steps: {:#}", e),
                )
                .await;
            }
```

In `settlement::dispatch::init` (`crates/stroem-server/src/settlement/dispatch.rs`), directly after the `crate::cascade::execute(…).await.context("creation-time step cascade")?;` call:

```rust
    // Deferred root steps (spec 2026-10-15 § 5.3): a root step with a
    // templated `ref:` is inserted `ready`, so without this call it would
    // wait for recovery. A resolution failure fails only its step; an
    // escaping (DB) error takes the `[creation] initialisation failed` path.
    crate::settlement::deferred::resolve_deferred_steps(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        job_id,
        task,
        defaults,
        &snapshots,
    )
    .await
    .context("resolve deferred steps")?;
```

- [ ] **Step 5: Run the tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::`
Expected: PASS. If `one_commit_per_ref_survives_a_branch_move` fails because `y` resolved at the OLD tip, the PinStore served a cached listing: the fixture's `poll_interval` is 0, so every `resolve` re-lists — check that `select_commit` calls `pins().resolve` on a memo miss.

- [ ] **Step 6: Run the settlement and creation regressions**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib settlement
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration orchestrator_test:: git_refs_creation_test:: git_refs_claim_test:: propagate_to_parent_test::
```
Expected: PASS — a job without deferred steps returns from the pass after one query.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/settlement crates/stroem-server/tests/git_refs_deferred_test.rs
git commit -m "feat(settlement): resolve deferred steps at promotion — lease, memo, secret checks, guarded writes"
```

---
### Task 11: Lease, concurrency, cancel, privacy, retry and restart

Spec § 5.3 step 0 and 7, § 5.5, § 5.7, § 10.3, § 12 (lease, concurrency, cancel, private path, ref secret set); Review Focus 4 and 5. Mostly tests of Task 10's code; fix the code if a test finds a defect.

**Files:**
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: `settlement::deferred::{resolve_deferred_steps_with, LeaseTiming}`, `PinStore::hold_loads_for_test`, `crate::common::capture::capture`, Task 9's fixture.

- [ ] **Step 1: Add the helpers**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs` (extend the imports with `use std::time::Duration;`, `use stroem_server::render_context::Snapshots;`, `use stroem_server::settlement::deferred::{resolve_deferred_steps_with, LeaseTiming};`, `use stroem_server::state::AppState;`):

```rust
// ─── Lease, concurrency, cancel (spec § 5.3 steps 0 and 7, D12) ───────────

/// A `tr-later` job whose `later` step is `ready` but NOT resolved — what a
/// crash between the cascade and the pass leaves (§ 5.8). No advance runs.
pub(crate) async fn df_promoted_unresolved(fx: &PinnedFixture, branch: &str) -> Result<Uuid> {
    let job_id = df_create(fx, "tr-later", json!({"branch": branch})).await?;
    sqlx::query(
        "UPDATE job_step SET status = 'completed', output = $2, completed_at = NOW() \
         WHERE job_id = $1 AND step_name = 'first'",
    )
    .bind(job_id)
    .bind(json!({}))
    .execute(&fx.pool)
    .await?;
    sqlx::query(
        "UPDATE job_step SET status = 'ready', ready_at = NOW() \
         WHERE job_id = $1 AND step_name = 'later'",
    )
    .bind(job_id)
    .execute(&fx.pool)
    .await?;
    sqlx::query("UPDATE job SET status = 'running' WHERE job_id = $1")
        .bind(job_id)
        .execute(&fx.pool)
        .await?;
    Ok(job_id)
}

/// Run the pass for a `tr-later` job on `state`'s replica.
pub(crate) async fn df_resolve_on(state: &AppState, job_id: Uuid, timing: LeaseTiming) -> Result<()> {
    let cfg = state.workspaces.get_config("etl").await.expect("etl loaded");
    let task = cfg.tasks.get("tr-later").expect("tr-later").clone();
    resolve_deferred_steps_with(
        &state.workspaces,
        &state.pool,
        &cfg,
        "etl",
        job_id,
        &task,
        JobDefaults::default(),
        &Snapshots::default(),
        timing,
    )
    .await
}

pub(crate) async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not reached within 10 s");
}

async fn df_lease_live(pool: &PgPool, job_id: Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT COALESCE(resolve_lease_until > NOW(), false) FROM job_step \
         WHERE job_id = $1 AND step_name = 'later'",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

const SHORT: LeaseTiming = LeaseTiming {
    lease: Duration::from_secs(2),
    renew: Duration::from_millis(300),
};
```

- [ ] **Step 2: Write the concurrency and lease tests**

```rust
/// Two concurrent resolvers: exactly one takes the lease and resolves; the
/// other skips the step; one commit.
#[tokio::test]
async fn two_concurrent_resolvers_resolve_once() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let first = tokio::spawn(async move { df_resolve_on(&state, job_id, LeaseTiming::default()).await });
    wait_until(|| hold.entered() > 0).await;
    df_resolve_on(&fx.state, job_id, LeaseTiming::default()).await?;
    assert_eq!(df_step(&fx.pool, job_id, "later").await.action_type, "deferred");
    drop(hold);
    first.await??;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.action_type, "script");
    assert_eq!(later.action_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    Ok(())
}

/// A resolve held past the lease length keeps its lease by renewal, and the
/// Phase 4.6 listing does not offer the step to a second resolver.
#[tokio::test]
async fn a_held_resolve_keeps_its_lease_by_renewal() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1")
        .bind(job_id)
        .execute(&fx.pool)
        .await?;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, SHORT).await });
    wait_until(|| hold.entered() > 0).await;
    tokio::time::sleep(Duration::from_secs(4)).await; // twice the lease
    assert!(df_lease_live(&fx.pool, job_id).await, "renewal kept the lease");
    assert!(!JobRepo::get_jobs_with_stranded_deferred_steps(&fx.pool, 30.0, None, 100)
        .await?
        .contains(&job_id));
    drop(hold);
    resolver.await??;
    assert_eq!(df_step(&fx.pool, job_id, "later").await.action_type, "script");
    Ok(())
}

/// The token fence: a resolver whose lease was taken over writes nothing,
/// neither the resolved row nor a failure.
#[tokio::test]
async fn a_resolver_whose_lease_was_taken_over_cannot_write() -> Result<()> {
    let fx = df_fixture().await?;
    fx.etl.commit("zz-broken", "main", &[("workflow.yaml", DF_BROKEN)]);
    for branch in ["release/2.3", "zz-broken"] {
        let job_id = df_promoted_unresolved(&fx, branch).await?;
        let replica = fx.second_replica().await?;
        let hold = replica.state.workspaces.pins().hold_loads_for_test();
        let state = replica.state.clone();
        let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, LeaseTiming::default()).await });
        wait_until(|| hold.entered() > 0).await;
        sqlx::query(
            "UPDATE job_step SET resolve_lease_owner = $2, \
                                 resolve_lease_until = NOW() + interval '1 hour' \
             WHERE job_id = $1 AND step_name = 'later'",
        )
        .bind(job_id)
        .bind(Uuid::new_v4())
        .execute(&fx.pool)
        .await?;
        drop(hold);
        resolver.await??;
        let later = df_step(&fx.pool, job_id, "later").await;
        assert_eq!(
            (later.status.as_str(), later.action_type.as_str()),
            ("ready", "deferred"),
            "{branch}"
        );
        assert!(later.error_message.is_none(), "{branch}");
    }
    Ok(())
}

/// The lease guard: dropping the resolver future (a cancelled request during
/// `init`) stops renewal, so the lease expires.
#[tokio::test]
async fn dropping_the_resolver_stops_renewal() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, SHORT).await });
    wait_until(|| hold.entered() > 0).await;
    resolver.abort();
    let _ = resolver.await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!df_lease_live(&fx.pool, job_id).await, "no renewal after the drop");
    drop(hold);
    Ok(())
}

/// A cancellation that commits while a resolution runs wins: the resolution
/// (success or failure) writes nothing and the step ends `cancelled`.
#[tokio::test]
async fn a_cancel_during_resolution_wins() -> Result<()> {
    let fx = df_fixture().await?;
    fx.etl.commit("zz-broken", "main", &[("workflow.yaml", DF_BROKEN)]);
    for branch in ["release/2.3", "zz-broken"] {
        let job_id = df_promoted_unresolved(&fx, branch).await?;
        let replica = fx.second_replica().await?;
        let hold = replica.state.workspaces.pins().hold_loads_for_test();
        let state = replica.state.clone();
        let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, LeaseTiming::default()).await });
        wait_until(|| hold.entered() > 0).await;
        fx.state.settlement().cancel(job_id).await?;
        drop(hold);
        resolver.await??;
        let later = df_step(&fx.pool, job_id, "later").await;
        assert_eq!(later.status, "cancelled", "{branch}");
        assert_eq!(later.action_type, "deferred", "{branch}: nothing written after the cancel");
    }
    Ok(())
}
```

- [ ] **Step 3: Write the privacy tests**

```rust
// ─── Privacy of the rendered value (spec § 5.5, § 10.3) ───────────────────

/// A BRANCH value equal to a secret that exists only in the commit it selects
/// is refused after that commit's config loads; nothing is written; no
/// captured span, log line or error text carries the value.
#[tokio::test]
async fn a_value_equal_to_a_secret_of_the_selected_commit_is_refused_privately() -> Result<()> {
    const SNEAKY: &str = "zz-sekrit-5150";
    let fx = df_fixture().await?;
    fx.etl.commit(
        SNEAKY,
        "release/2.3",
        &[("workflow.yaml", &format!("secrets:\n  hidden: {SNEAKY}\n{}", df_etl_release()))],
    );
    let job_id = df_promoted_unresolved(&fx, SNEAKY).await?;
    let (captured, _guard) = capture();
    fx.state.settlement().advance(job_id).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.status, "failed");
    assert_eq!(
        later.error_message.as_deref(),
        Some("[ref] step 'later': the rendered ref contains a secret value")
    );
    assert!(JobRefPinRepo::list_for_job(&fx.pool, job_id).await?.is_empty());
    let text = captured.text();
    assert!(!text.contains(SNEAKY), "{text}");
    Ok(())
}

/// A load failure of a rendered ref's commit is labelled `ws (sha)` only.
#[tokio::test]
async fn a_rendered_refs_load_failure_names_the_commit_only() -> Result<()> {
    const BRANCH: &str = "zz-sentinel-9001";
    let fx = df_fixture().await?;
    let broken = fx.etl.commit(BRANCH, "main", &[("workflow.yaml", DF_BROKEN)]);
    let job_id = df_promoted_unresolved(&fx, BRANCH).await?;
    let (captured, _guard) = capture();
    fx.state.settlement().advance(job_id).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(
        later.error_message.as_deref(),
        Some("[ref] step 'later': workspace 'etl' at the rendered ref cannot be loaded: its configuration does not load")
    );
    let text = captured.text();
    assert!(text.contains(&format!("[pin] etl ({})", &broken[..7])), "{text}");
    assert!(!text.contains(BRANCH), "{text}");
    Ok(())
}

/// A full-SHA value is the accepted residual of § 10.3: it resolves, the
/// resolve span never names it, and only `commit` fields carry it.
#[tokio::test]
async fn a_full_sha_ref_resolves_and_only_commit_fields_name_it() -> Result<()> {
    let fx = df_fixture().await?;
    let sha = fx.commits.etl_release.clone();
    let job_id = df_promoted_unresolved(&fx, &sha).await?;
    let (captured, _guard) = capture();
    fx.state.settlement().advance(job_id).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.action_revision.as_deref(), Some(sha.as_str()));
    assert!(captured.span_fields("git_ref").is_empty(), "{}", captured.text());
    for line in captured.text().lines().filter(|l| l.contains(&sha)) {
        assert!(line.contains("commit="), "only commit fields may name the SHA: {line}");
    }
    Ok(())
}
```

- [ ] **Step 4: Write the Review Focus 4 and 5 tests**

```rust
// ─── After resolution (spec § 5.7) ────────────────────────────────────────

/// Review Focus 4: a resolved step that fails retries in place on the same
/// commit; it is never deferred again.
#[tokio::test]
async fn a_resolved_step_retries_on_its_commit_without_re_resolution() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-retry", json!({})).await?;
    let worker = register_worker(&fx.router, &["script"]).await;
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["step_name"], json!("run"), "{claimed}");
    let (st, body) = worker_req(
        &fx.router,
        "POST",
        &format!("/worker/jobs/{job_id}/steps/run/complete"),
        Some(json!({"exit_code": 1, "error": "boom"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "ready", "retried in place");
    assert_eq!(run.retry_attempt, 1);
    assert_eq!(run.action_type, "script");
    assert_eq!(run.action_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    assert!(JobStepRepo::get_ready_deferred_steps(&fx.pool, job_id).await?.is_empty());
    tokio::time::sleep(Duration::from_millis(1200)).await; // the action's 1 s delay
    let again = claim_once(&fx.router, &worker).await;
    assert_eq!(again["step_name"], json!("run"), "{again}");
    assert_eq!(again["revision"], json!(fx.commits.etl_release));
    Ok(())
}

/// Review Focus 5 (spec § 5.7): restart from the templated step resolves it
/// again (a newer commit); a carried templated row stays terminal and keeps
/// its deferred kind — the pass never touches it.
#[tokio::test]
async fn restart_re_resolves_a_templated_step_and_leaves_a_carried_one() -> Result<()> {
    let fx = df_fixture().await?;
    let worker = register_worker(&fx.router, &["script"]).await;
    let source = df_create(&fx, "tr-chain", json!({})).await?;
    claim_and_complete(&fx, &worker, source, "a", json!({"output": {}})).await;
    claim_and_complete(&fx, &worker, source, "b", json!({"output": {}})).await;
    assert_eq!(JobRepo::get(&fx.pool, source).await?.unwrap().status, "completed");
    let v2 = df_release_v2(&fx);

    let (st, body) = api_req(
        &fx.router,
        "POST",
        &format!("/api/jobs/{source}/restart"),
        None,
        Some(json!({"from_step": "b"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let carried: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let a = df_step(&fx.pool, carried, "a").await;
    assert_eq!((a.status.as_str(), a.action_type.as_str()), ("completed", "deferred"));
    assert!(a.carried_over);
    assert!(a.action_ref.is_none());

    let (st, body) = api_req(
        &fx.router,
        "POST",
        &format!("/api/jobs/{source}/restart"),
        None,
        Some(json!({"from_step": "a"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let rerun: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let a = df_step(&fx.pool, rerun, "a").await;
    assert_eq!(a.action_type, "script");
    assert_eq!(a.action_revision.as_deref(), Some(v2.as_str()));
    Ok(())
}
```

- [ ] **Step 5: Run the tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::`
Expected: PASS. If a privacy test finds the value in the captured text, find the span or event that carries it (the captured line shows the field name) and remove the value from that span on the private path — never weaken the assertion. If `hold.entered()` stays 0 in a lease test, the cold replica's first blocking unit is not reached: check that the replica is created after `df_promoted_unresolved` and that the branch exists on the remote.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server
git commit -m "test(templated-refs): lease, token fence, cancel, privacy, retry and restart of deferred steps"
```

---

### Task 12: Recovery Phase 4.6 — re-advance stranded deferred steps

Spec § 5.8.

**Files:**
- Modify: `crates/stroem-server/src/recovery.rs` — `sweep` (after the Phase 4.5 call `readvance_stalled_pinned_jobs(state, StalledSweepBounds::default()).await;`); a new function after `readvance_stalled_pinned_jobs`
- Modify: `crates/stroem-server/src/state.rs` — a cursor field next to `stalled_pinned_cursor` (`:141-143`), initialised in `AppState::new` (`:197`)
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: `JobRepo::get_jobs_with_stranded_deferred_steps` (Task 5), `StalledSweepBounds`.
- Produces: `pub async fn readvance_deferred_steps(state: &AppState, bounds: StalledSweepBounds)`; `AppState.deferred_steps_cursor: Arc<std::sync::Mutex<Option<Uuid>>>`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs` (extend the imports with `use stroem_server::recovery::{readvance_deferred_steps, StalledSweepBounds};`):

```rust
// ─── Recovery Phase 4.6 (spec § 5.8) ──────────────────────────────────────

/// Age `later` past the unmatched timeout; with `lease_secs`, give it a lease
/// of another (dead or live) resolver ending that many seconds from now.
async fn df_age_and_lease(pool: &PgPool, job_id: Uuid, lease_secs: Option<f64>) {
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1 AND step_name = 'later'")
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
    if let Some(secs) = lease_secs {
        sqlx::query(
            "UPDATE job_step SET resolve_lease_owner = gen_random_uuid(), \
                                 resolve_lease_until = NOW() + make_interval(secs => $2::double precision) \
             WHERE job_id = $1 AND step_name = 'later'",
        )
        .bind(job_id)
        .bind(secs)
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn phase_4_6_resolves_stranded_and_expired_but_not_leased_steps() -> Result<()> {
    let fx = df_fixture().await?;
    let stranded = df_promoted_unresolved(&fx, "release/2.3").await?;
    let expired = df_promoted_unresolved(&fx, "release/2.3").await?;
    let leased = df_promoted_unresolved(&fx, "release/2.3").await?;
    df_age_and_lease(&fx.pool, stranded, None).await;
    df_age_and_lease(&fx.pool, expired, Some(-1.0)).await;
    df_age_and_lease(&fx.pool, leased, Some(3600.0)).await;

    readvance_deferred_steps(&fx.state, StalledSweepBounds::default()).await;

    assert_eq!(df_step(&fx.pool, stranded, "later").await.action_type, "script");
    assert_eq!(df_step(&fx.pool, expired, "later").await.action_type, "script");
    assert_eq!(df_step(&fx.pool, leased, "later").await.action_type, "deferred");
    Ok(())
}

#[tokio::test]
async fn phase_4_6_is_bounded_and_resumes_on_the_next_sweep() -> Result<()> {
    let fx = df_fixture().await?;
    let one = df_promoted_unresolved(&fx, "release/2.3").await?;
    let two = df_promoted_unresolved(&fx, "release/2.3").await?;
    df_age_and_lease(&fx.pool, one, None).await;
    df_age_and_lease(&fx.pool, two, None).await;
    let bounds = StalledSweepBounds {
        max_jobs: 10,
        budget: Duration::ZERO,
    };
    readvance_deferred_steps(&fx.state, bounds).await;
    let resolved = |rows: [JobStepRow; 2]| rows.iter().filter(|r| r.action_type == "script").count();
    let after_one = resolved([
        df_step(&fx.pool, one, "later").await,
        df_step(&fx.pool, two, "later").await,
    ]);
    assert_eq!(after_one, 1, "the budget stops after the first job");
    readvance_deferred_steps(&fx.state, bounds).await;
    let after_two = resolved([
        df_step(&fx.pool, one, "later").await,
        df_step(&fx.pool, two, "later").await,
    ]);
    assert_eq!(after_two, 2, "the next sweep resumes after the cursor");
    Ok(())
}

/// A resolver dropped mid-resolve (Task 11) leaves a leased row; once the
/// lease expires, Phase 4.6 resolves it.
#[tokio::test]
async fn phase_4_6_resolves_the_row_of_a_dropped_resolver_after_expiry() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    df_age_and_lease(&fx.pool, job_id, None).await;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, SHORT).await });
    wait_until(|| hold.entered() > 0).await;
    resolver.abort();
    let _ = resolver.await;
    drop(hold);
    tokio::time::sleep(Duration::from_secs(3)).await;
    readvance_deferred_steps(&fx.state, StalledSweepBounds::default()).await;
    assert_eq!(df_step(&fx.pool, job_id, "later").await.action_type, "script");
    Ok(())
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::phase_4_6`
Expected: compile error — `readvance_deferred_steps` not found.

- [ ] **Step 3: Implement**

`crates/stroem-server/src/state.rs`, next to `stalled_pinned_cursor`:

```rust
    /// Recovery Phase 4.6 cursor: the last job with a stranded deferred step
    /// a bounded sweep advanced (spec 2026-10-15 § 5.8).
    pub deferred_steps_cursor: Arc<std::sync::Mutex<Option<Uuid>>>,
```

and in `AppState::new`, next to `stalled_pinned_cursor: …`: `deferred_steps_cursor: Arc::new(std::sync::Mutex::new(None)),`.

`crates/stroem-server/src/recovery.rs`, in `sweep`, directly after the Phase 4.5 call:

```rust
    // Phase 4.6 (templated refs, spec 2026-10-15 § 5.8): re-advance jobs whose
    // deferred step was left `ready` with no live resolution lease.
    readvance_deferred_steps(state, StalledSweepBounds::default()).await;
```

and after `readvance_stalled_pinned_jobs`:

```rust
/// Phase 4.6 (spec 2026-10-15 § 5.8): `advance` every non-terminal job with a
/// deferred `ready` step that has waited longer than
/// `unmatched_step_timeout_secs` and holds no live resolution lease — left by
/// a crash or an advance that died after the cascade committed. The unmatched
/// sweep ignores deferred rows and Phase 4.5 skips jobs with a `ready` step,
/// so nothing else re-enters such a job. Only a lease holder resolves (§ 5.3
/// step 0): a slow resolve keeps renewing its lease and is not listed. Bounded
/// and resumable like Phase 4.5 (its cursor moves only past advanced jobs);
/// one beat per job. A failed listing is logged and the sweep continues.
pub async fn readvance_deferred_steps(state: &AppState, bounds: StalledSweepBounds) {
    let after = *state
        .deferred_steps_cursor
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let older_than = state.config.recovery.unmatched_step_timeout_secs as f64;
    let jobs = match JobRepo::get_jobs_with_stranded_deferred_steps(
        &state.pool,
        older_than,
        after,
        bounds.max_jobs,
    )
    .await
    {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!("Failed to list jobs with stranded deferred steps: {:#}", e);
            return;
        }
    };
    let started = std::time::Instant::now();
    for (i, job_id) in jobs.iter().enumerate() {
        if i > 0 && started.elapsed() >= bounds.budget {
            break;
        }
        *state
            .deferred_steps_cursor
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(*job_id);
        state.background_tasks.recovery_beat.beat();
        if let Err(e) = state.settlement().advance(*job_id).await {
            tracing::error!("Failed to re-advance job {} with a stranded deferred step: {:#}", job_id, e);
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test:: git_refs_creation_test::stalled_pinned
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib recovery
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/recovery.rs crates/stroem-server/src/state.rs crates/stroem-server/tests/git_refs_deferred_test.rs
git commit -m "feat(recovery): Phase 4.6 re-advances jobs with stranded deferred steps"
```

---
## Part E — the other two places, API, UI, docs

### Task 13: A `type: task` action's templated `ref:`, resolved at dispatch

Spec § 5.6 (dispatch half), § 4.1 row 2; contract deviation 13.

**Files:**
- Modify: `crates/stroem-common/src/template.rs` — `render_ref_projection` after `render_input_map` (`:476-530`); tests in `mod tests`
- Modify: `crates/stroem-server/src/settlement/dispatch.rs` — imports (`:6-26`); `handle_task_steps_pass`: replace `let task_stamp = PinRef::of_step_task(step);` (step 2 of the pass); a new private helper after `resolve_stamped_task`
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: Task 5 (`stamp_task_pin`), Task 7 (`Scope::TaskActionRef`), Task 8 (`RenderedRefError`), Task 10 (`deferred::{ref_secret_set, select_commit}`).
- Produces: `pub fn stroem_common::template::render_ref_projection(input_map: &HashMap<String, serde_json::Value>, context: &serde_json::Value) -> anyhow::Result<serde_json::Value>`.

- [ ] **Step 1: Write the failing projection unit tests**

Append to `mod tests` in `crates/stroem-common/src/template.rs`:

```rust
    fn map_of(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn ref_projection_top_level_is_exactly_render_input_map() {
        let ctx = json!({"input": {"b": "release/2.3", "conn": {"host": "h"}}});
        let map = map_of(&[
            ("branch", json!("{{ input.b }}")),
            ("db", json!("{{ input.conn }}")),
            ("n", json!(3)),
        ]);
        assert_eq!(
            render_ref_projection(&map, &ctx).unwrap(),
            render_input_map(&map, &ctx).unwrap()
        );
    }

    #[test]
    fn ref_projection_renders_nested_leaves_as_strings_keeping_order() {
        let ctx = json!({"input": {"b": "release/2.3", "n": 7}});
        let map = map_of(&[(
            "cfg",
            json!({"branch": "{{ input.b }}", "list": ["a", "{{ input.n }}", 1, null]}),
        )]);
        assert_eq!(
            render_ref_projection(&map, &ctx).unwrap(),
            json!({"cfg": {"branch": "release/2.3", "list": ["a", "7", 1, null]}})
        );
    }

    #[test]
    fn ref_projection_fails_on_any_nested_error_naming_only_the_field() {
        let ctx = json!({"input": {}});
        for bad in [json!({"host": "{{ bad"}), json!({"b": "{{ input.nope }}"})] {
            let err = render_ref_projection(&map_of(&[("conn", bad.clone())]), &ctx).unwrap_err();
            let shown = format!("{err:#}");
            assert!(shown.contains("input field 'conn'"), "{shown}");
            assert!(!shown.contains("{{"), "{shown}");
        }
        // The documented escape (spec § 5.6 known limitation).
        let raw = map_of(&[("conn", json!({"host": "{% raw %}{{ bad{% endraw %}"}))]);
        assert_eq!(
            render_ref_projection(&raw, &ctx).unwrap(),
            json!({"conn": {"host": "{{ bad"}})
        );
    }
```

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib template::tests::ref_projection`
Expected: compile error — `render_ref_projection` not found.

- [ ] **Step 2: Implement `render_ref_projection`**

After `render_input_map` in `crates/stroem-common/src/template.rs`:

```rust
/// The schema-free ref input of a `type: task` step (spec 2026-10-15 § 5.6
/// step 1): the top level exactly as [`render_input_map`] renders it (a simple
/// variable reference keeps a structured value; a top-level object or array is
/// copied), and inside every copied object or array each string leaf rendered
/// as a string, array length and order kept. ANY render error is returned:
/// no ref is ever built from text that did not render (a filter could strip
/// the delimiters of a failed leaf, and a dropped leaf would let `default`
/// pick a fallback). Errors name the input field, never its value.
pub fn render_ref_projection(
    input_map: &HashMap<String, serde_json::Value>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let serde_json::Value::Object(top) = render_input_map(input_map, context)? else {
        unreachable!("render_input_map returns an object")
    };
    let mut out = serde_json::Map::new();
    for (key, value) in top {
        let value = match input_map.get(&key) {
            Some(serde_json::Value::Object(_)) | Some(serde_json::Value::Array(_)) => {
                render_string_leaves(&value, context).with_context(|| {
                    format!("Failed to render a template inside input field '{key}'")
                })?
            }
            _ => value,
        };
        out.insert(key, value);
    }
    Ok(serde_json::Value::Object(out))
}

fn render_string_leaves(
    value: &serde_json::Value,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    Ok(match value {
        serde_json::Value::String(s) => serde_json::Value::String(render_template(s, context)?),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| render_string_leaves(v, context))
                .collect::<Result<_>>()?,
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), render_string_leaves(v, context)?);
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    })
}
```

Run the unit tests again. Expected: PASS.

- [ ] **Step 3: Write the failing dispatch tests**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs`:

```rust
// ─── A `type: task` action's templated ref (spec § 5.6) ───────────────────

/// main: actions whose own `ref:` is a template, and tasks that call them.
fn df_task_action_main() -> String {
    format!(
        "{}{}",
        df_etl_main().replace(
            "actions:\n",
            "actions:\n  call-json-at:\n    type: task\n    task: takes-json\n    ref: \"{{ input.branch }}\"\n  call-default-at:\n    type: task\n    task: nightly\n    ref: \"{{ input.branch }}\"\n    input:\n      branch:\n        type: string\n        default: release/2.3\n",
        ),
        r#"
  tr-json:
    input:
      payload:
        type: json
        default: {"a": 1}
    flow:
      call:
        action: call-json-at
        input:
          branch: release/2.3
          cfg: "{{ input.payload }}"
  tr-default:
    flow:
      call:
        action: call-default-at
  tr-nested-bad:
    flow:
      call:
        action: call-nightly-at
        input:
          branch: release/2.3
          conn:
            host: "{{ bad"
  tr-nested-raw:
    flow:
      call:
        action: call-nightly-at
        input:
          branch: release/2.3
          conn:
            host: "{% raw %}{{ bad{% endraw %}"
  tr-cross:
    flow:
      call:
        action: billing.call-at
"#
    )
}

/// release/2.3 plus `takes-json` (a `json` input) at the end of `tasks:`.
fn df_task_action_release() -> String {
    format!(
        "{}  takes-json:\n    input:\n      cfg:\n        type: json\n    flow:\n      s:\n        action: hello\n",
        df_etl_release()
    )
}

async fn df_task_action_fixture() -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(df_task_action_main()),
        etl_release: Some(df_task_action_release()),
        billing_main: Some(BILLING_MAIN.replace(
            "actions:\n",
            "actions:\n  call-at:\n    type: task\n    task: nightly\n    ref: \"{{ input.nope }}\"\n",
        )),
        ..Default::default()
    })
    .await
}

async fn df_child_of(pool: &PgPool, parent: Uuid) -> stroem_db::JobRow {
    let id: Uuid = sqlx::query_scalar("SELECT job_id FROM job WHERE parent_job_id = $1")
        .bind(parent)
        .fetch_one(pool)
        .await
        .unwrap();
    JobRepo::get(pool, id).await.unwrap().unwrap()
}

#[tokio::test]
async fn templated_task_action_ref_resolves_at_dispatch_and_stamps_the_pin() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-task-action", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "running", "{:?}", call.error_message);
    assert_eq!(call.task_workspace.as_deref(), Some("etl"));
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(call.task_revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    let child = df_child_of(&fx.pool, job_id).await;
    assert_eq!(child.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(child.revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    assert!(memo.iter().any(|e| e.git_ref == "release/2.3"), "{memo:?}");
    Ok(())
}

/// The ref reads the caller's step input as strings; the child's `json`
/// field is still a native value (T's schema decides the real render).
#[tokio::test]
async fn the_ref_input_is_schema_free_and_the_child_keeps_native_json() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-json", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "running", "{:?}", call.error_message);
    let child = df_child_of(&fx.pool, job_id).await;
    assert_eq!(child.input.unwrap()["cfg"], json!({"a": 1}));
    Ok(())
}

/// Action defaults are not visible to the ref (spec § 5.6 consequences).
#[tokio::test]
async fn action_defaults_are_not_in_the_ref_context() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-default", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    let msg = call.error_message.unwrap();
    assert!(msg.starts_with("[ref] step 'call': the ref template could not be rendered"), "{msg}");
    Ok(())
}

/// A nested leaf that does not render fails the step, naming the field only;
/// the `{% raw %}` escape lets it run (spec § 5.6 known limitation).
#[tokio::test]
async fn a_nested_render_error_fails_the_step_and_raw_escapes_it() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-nested-bad", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    let msg = call.error_message.unwrap();
    assert!(msg.contains("input field 'conn'"), "{msg}");
    assert!(!msg.contains("{{ bad"), "{msg}");
    let job_id = df_create(&fx, "tr-nested-raw", json!({})).await?;
    assert_eq!(df_step(&fx.pool, job_id, "call").await.status, "running");
    Ok(())
}

/// The template is the ACTION OWNER's: its render error is withheld when the
/// owner (billing) is not the job's workspace (etl).
#[tokio::test]
async fn an_owner_side_ref_render_error_is_withheld_across_workspaces() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-cross", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    assert_eq!(
        call.error_message.as_deref(),
        Some(
            "Failed to prepare input for task step 'call': rendering in workspace 'billing' failed \
             (details withheld from this job; see the server log or validate the owner workspace)"
        )
    );
    Ok(())
}
```

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::`
Expected: the new dispatch tests FAIL — dispatch ignores `action_spec.ref` and resolves `nightly` live in main, where it does not exist ("Task 'nightly' not found").

- [ ] **Step 4: Implement the dispatch branch**

Imports in `crates/stroem-server/src/settlement/dispatch.rs`: add

```rust
use stroem_common::git_ref::{check_rendered_ref, classify_ref, RefKind};
use stroem_common::template::{render_ref_projection, render_ref_template};

use crate::job_creator::ref_world_sets;
use crate::refs::{plan_reference, RefPlan, RefWorld};
use crate::settlement::deferred::{ref_secret_set, select_commit};
use crate::templated_ref::{RenderedRefError, RenderedRefKind};
```

After `resolve_stamped_task`, add:

```rust
/// What dispatch made of a `type: task` action's templated `ref:`.
enum TemplatedTaskRef {
    /// Resolved, checked and stamped on the parent step: `(T, pin)`.
    Stamped(String, PinRef),
    /// Another dispatcher stamped the step first: leave it alone.
    Moved,
    /// Fail the step with `message`; persist `withheld` instead when set.
    Failed {
        message: String,
        withheld: Option<String>,
    },
}

/// Spec 2026-10-15 § 5.6 steps 1–4, in this order because the TASK's schema
/// (which decides the real step-input render) is unknown until the ref is.
/// The outer `Err` is a DB error.
#[allow(clippy::too_many_arguments)]
async fn resolve_templated_task_ref(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    job: &JobRow,
    job_ctx: &JobContext<'_>,
    step_views: &[render_context::StepView<'_>],
    step: &stroem_db::JobStepRow,
    template: &str,
    task_ref: &str,
    base_ws: &str,
    workspace_name: &str,
) -> Result<TemplatedTaskRef> {
    let fail = |e: RenderedRefError| TemplatedTaskRef::Failed {
        message: format!("[ref] step '{}': {}", step.step_name, e),
        withheld: None,
    };

    // 1. The ref input: a schema-free projection of the caller's step input,
    //    in the step-input context (caller secrets, `vals`). Caller-side
    //    (bucket C, never withheld); used only to build the ref context.
    let input_ctx = render_context::build(job_ctx, step_views, LoopSlot::of(step), Scope::ChildTaskInput);
    let map: HashMap<String, serde_json::Value> = step
        .input
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let ref_input = match render_ref_projection(&map, input_ctx.as_value()) {
        Ok(v) => v,
        Err(e) => {
            return Ok(TemplatedTaskRef::Failed {
                message: format!(
                    "Failed to render input for task step '{}' (ref input): {:#}",
                    step.step_name, e
                ),
                withheld: None,
            })
        }
    };

    // 2. The ref: no `secret`, no `vals`. The template is the action owner
    //    O's own: its render error is withheld across the boundary (O != A).
    let ref_ctx = render_context::build(
        job_ctx,
        step_views,
        None,
        Scope::TaskActionRef { ref_input: &ref_input },
    );
    let rendered = match render_ref_template(template, ref_ctx.as_value()) {
        Ok(r) => r,
        Err(e) => {
            let err = RenderedRefError::from_error(&e, base_ws);
            return Ok(TemplatedTaskRef::Failed {
                message: format!("[ref] step '{}': {}", step.step_name, err),
                withheld: (base_ws != workspace_name)
                    .then(|| withheld_owner_render_error(&step.step_name, base_ws)),
            });
        }
    };

    // 3. T syntactically, from `task` relative to O; the commit; the private
    //    load; T's secrets; the memo — the deferred-step helpers.
    let (libraries, configured, git) = ref_world_sets(workspaces);
    let world = RefWorld {
        library_names: &libraries,
        configured: &configured,
        git: &git,
    };
    let target = match plan_reference(base_ws, None, task_ref, Some(template), &world) {
        Ok(RefPlan::AtRef { target, .. }) => target,
        Ok(RefPlan::Today { .. }) => {
            unreachable!("plan_reference returns AtRef whenever a ref is given")
        }
        Err(e) => return Ok(fail(RenderedRefError::from_error(&e.into(), base_ws))),
    };
    let secrets = match ref_secret_set(workspaces, pool, job).await? {
        Ok(s) => s,
        Err(e) => return Ok(fail(e)),
    };
    let value = match check_rendered_ref(&rendered, &secrets) {
        Ok(v) => v,
        Err(r) => return Ok(fail(RenderedRefError::from_rejection(r, &target.owner))),
    };
    let pin = match select_commit(workspaces, pool, job.job_id, &target.owner, &value).await? {
        Ok(p) => p,
        Err(e) => return Ok(fail(e)),
    };

    // 4. The task must exist at that commit; then the stamp the existing
    //    path reads (and a re-dispatch finds instead of rendering again).
    let has_task = match workspaces.pinned_config_private(&target.owner, &pin.commit).await {
        Ok(p) => p.config.tasks.contains_key(&target.local_name),
        Err(e) => return Ok(fail(RenderedRefError::from_error(&e, &target.owner))),
    };
    if !has_task {
        return Ok(fail(RenderedRefError::new(RenderedRefKind::NoAction, &target.owner)));
    }
    if !JobStepRepo::stamp_task_pin(pool, job.job_id, &step.step_name, &target.owner, &pin.git_ref, &pin.commit).await? {
        return Ok(TemplatedTaskRef::Moved);
    }
    Ok(TemplatedTaskRef::Stamped(target.owner, pin))
}
```

In `handle_task_steps_pass`, replace the line `let task_stamp = PinRef::of_step_task(step);` (in "2. Task owner T") with:

```rust
        let mut task_stamp = PinRef::of_step_task(step);
        // Spec 2026-10-15 § 5.6: an unstamped step whose action carries a
        // templated `ref:` is resolved now, and stamped.
        if task_stamp.is_none() {
            if let Some(template) = action_spec
                .get("ref")
                .and_then(|v| v.as_str())
                .filter(|r| classify_ref(r) == RefKind::Template)
            {
                match resolve_templated_task_ref(
                    workspaces,
                    pool,
                    &job,
                    &job_ctx,
                    &step_views,
                    step,
                    template,
                    task_ref,
                    base_ws,
                    workspace_name,
                )
                .await?
                {
                    TemplatedTaskRef::Stamped(t_ws, pin) => task_stamp = Some((t_ws, pin)),
                    TemplatedTaskRef::Moved => continue,
                    TemplatedTaskRef::Failed { message, withheld } => {
                        fail_task_step(
                            pool,
                            job_id,
                            &step.step_name,
                            &message,
                            task,
                            workspace_config,
                            snapshots,
                            &scrub,
                            withheld.as_deref(),
                        )
                        .await?;
                        failed_any = true;
                        continue;
                    }
                }
            }
        }
```

The rest of the pass reads `task_stamp` unchanged: `resolve_stamped_task` loads T at the stamped pin (now an identifier), renders the step input with T's schema, merges the action defaults (bucket D, rendered once), resolves connections by provenance, persists the parent input, and creates the child pinned.

- [ ] **Step 5: Run the tests**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common --lib template::tests
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test:: git_refs_creation_test::
```
Expected: PASS. If the cross-workspace test fails at creation instead of dispatch, check that `resolve_step_row` takes the template branch for a live foreign `type: task` action (it must not call `resolve_task_for_step` for a template).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-common -p stroem-server --all-targets -- -D warnings
git add crates/stroem-common/src/template.rs crates/stroem-server/src/settlement/dispatch.rs crates/stroem-server/tests/git_refs_deferred_test.rs
git commit -m "feat(dispatch): resolve a type: task action's templated ref at dispatch from a schema-free ref input"
```

---

### Task 14: A webhook trigger's templated `ref:`

Spec § 6, § 4.1 row 3.

**Files:**
- Modify: `crates/stroem-server/src/trigger_target.rs` — a new function after `resolve_trigger_target`
- Modify: `crates/stroem-server/src/web/hooks.rs` — `webhook_handler`, the `resolve_trigger_target` call (after `let source_id = …`)
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`

**Interfaces:**
- Consumes: Task 7 (`render_context::webhook_ref`), Task 8 (`RenderedRefError`, `pinned_config_private`).
- Produces: `pub(crate) async fn resolve_templated_trigger_target(workspaces: &WorkspaceManager, defining_ws: &str, defining_cfg: &Arc<WorkspaceConfig>, task_ref: &str, template: &str, input: &serde_json::Value) -> std::result::Result<ResolvedTarget, RenderedRefError>`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs`:

```rust
// ─── Webhook trigger ref (spec § 6) ───────────────────────────────────────

const DF_HOOK: &str = r#"
triggers:
  on-push:
    type: webhook
    name: push-hook
    task: nightly
    ref: "{{ input.body.ref }}"
"#;

async fn df_hook_fixture(main_prefix: &str) -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(format!("{main_prefix}{}{DF_HOOK}", df_etl_main())),
        etl_release: Some(df_etl_release()),
        ..Default::default()
    })
    .await
}

async fn df_webhook_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE source_type = 'webhook'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn webhook_templated_ref_creates_a_pinned_job() -> Result<()> {
    let fx = df_hook_fixture("").await?;
    let (status, body) = api_req(
        &fx.router,
        "POST",
        "/hooks/push-hook",
        None,
        Some(json!({"ref": "release/2.3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let job = JobRepo::get(&fx.pool, job_id).await?.unwrap();
    assert_eq!(job.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(job.revision.as_deref(), Some(fx.commits.etl_release.as_str()));
    assert_eq!(job.task_folder.as_deref(), Some("nightlies"));
    Ok(())
}

#[tokio::test]
async fn webhook_templated_ref_errors_are_value_free_400s_and_create_nothing() -> Result<()> {
    let fx = df_hook_fixture("secrets:\n  hook_token: hk-77aa66bb99\n").await?;
    let cases = [
        (json!({"ref": "  "}), "the rendered ref is empty"),
        (json!({"ref": "release/9.9"}), "the rendered ref was not found in workspace 'etl'"),
        (json!({"ref": "main"}), "workspace 'etl' has no such action or task at the rendered ref"),
        (json!({"ref": "x-hk-77aa66bb99"}), "the rendered ref contains a secret value"),
        (json!({}), "the ref template could not be rendered"),
    ];
    for (body_in, want) in cases {
        let (status, body) =
            api_req(&fx.router, "POST", "/hooks/push-hook", None, Some(body_in.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body_in}: {body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.starts_with(want), "{body_in}: {error}");
    }
    assert_eq!(df_webhook_jobs(&fx.pool).await, 0);
    Ok(())
}

/// A temporary git failure is a 500, and neither the response nor the server
/// log carries the rendered value.
#[tokio::test]
async fn webhook_unavailable_is_a_500_and_the_log_carries_no_value() -> Result<()> {
    const BRANCH: &str = "zz-hook-4242";
    let fx = df_hook_fixture("").await?;
    fx.etl.commit(BRANCH, "release/2.3", &[("workflow.yaml", &df_etl_release())]);
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let (captured, guard) = capture();
    let (status, body) = api_req(
        &replica.router,
        "POST",
        "/hooks/push-hook",
        None,
        Some(json!({"ref": BRANCH})),
    )
    .await;
    drop(guard);
    fx.etl.restore_remote();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(!body.to_string().contains(BRANCH), "{body}");
    let text = captured.text();
    assert!(text.contains("is not available at the rendered ref yet"), "{text}");
    assert!(!text.contains(BRANCH), "{text}");
    assert_eq!(df_webhook_jobs(&fx.pool).await, 0);
    Ok(())
}
```

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::webhook`
Expected: FAIL — the handler answers 400 "invalid ref name" (`resolve_trigger_target` parses the template).

- [ ] **Step 2: Implement the private webhook resolution**

In `crates/stroem-server/src/trigger_target.rs`, extend the imports with

```rust
use stroem_common::git_ref::check_rendered_ref;
use stroem_common::template::render_ref_template;

use crate::refs::{plan_reference, RefPlan, RefWorld};
use crate::templated_ref::{RenderedRefError, RenderedRefKind};
use crate::workspace_set::{collect_redaction_values, WorkspaceSet};
```

and add after `resolve_trigger_target`:

```rust
/// A webhook trigger whose `ref:` is a template (spec 2026-10-15 § 6): render
/// it from the webhook job input, check it against the live set, resolve it
/// and load the commit through the private path, check it against the
/// selected commit's secrets — only then is it an identifier, and the target
/// is exactly a static-ref one. Every error is a value-free
/// `RenderedRefError`; nothing is written here.
pub(crate) async fn resolve_templated_trigger_target(
    workspaces: &WorkspaceManager,
    defining_ws: &str,
    defining_cfg: &Arc<WorkspaceConfig>,
    task_ref: &str,
    template: &str,
    input: &serde_json::Value,
) -> std::result::Result<ResolvedTarget, RenderedRefError> {
    let _ = defining_cfg; // the owner is syntactic with a ref (git-refs § 4.3)
    let (libraries, configured, git) = crate::job_creator::ref_world_sets(workspaces);
    let world = RefWorld {
        library_names: &libraries,
        configured: &configured,
        git: &git,
    };
    let target = match plan_reference(defining_ws, None, task_ref, Some(template), &world) {
        Ok(RefPlan::AtRef { target, .. }) => target,
        Ok(RefPlan::Today { .. }) => {
            unreachable!("plan_reference returns AtRef whenever a ref is given")
        }
        Err(e) => return Err(RenderedRefError::from_error(&e.into(), defining_ws)),
    };
    let owner = target.owner.clone();
    let fail = |e: anyhow::Error| RenderedRefError::from_error(&e, &owner);

    let ctx = crate::render_context::webhook_ref(input);
    let rendered = render_ref_template(template, ctx.as_value()).map_err(fail)?;
    let live = collect_redaction_values(&WorkspaceSet::load(workspaces, defining_ws, None).await);
    let value = check_rendered_ref(&rendered, &live)
        .map_err(|r| RenderedRefError::from_rejection(r, &owner))?;
    let pin = workspaces
        .pins()
        .resolve(&owner, &value)
        .await
        .map_err(|e| fail(e.into()))?;
    let pinned = workspaces
        .pinned_config_private(&owner, &pin.commit)
        .await
        .map_err(fail)?;
    let own = workspaces.pin_redaction_values(&owner, &pinned).await;
    check_rendered_ref(&value, &own).map_err(|r| RenderedRefError::from_rejection(r, &owner))?;
    let task = pinned
        .config
        .tasks
        .get(&target.local_name)
        .cloned()
        .ok_or_else(|| RenderedRefError::new(RenderedRefKind::NoAction, &owner))?;
    Ok(ResolvedTarget {
        workspace: owner,
        task_name: target.local_name,
        pin: Some(PinRef {
            git_ref: value,
            commit: pin.commit,
        }),
        task_folder: task.folder.clone(),
        config: ConfigHandle::Pinned(pinned),
    })
}
```

Remove the `let _ = defining_cfg;` line and the parameter if clippy prefers; keep the parameter if it keeps the call symmetric with `resolve_trigger_target` (then name it `_defining_cfg`).

- [ ] **Step 3: Use it in the handler**

In `crates/stroem-server/src/web/hooks.rs`, add `use stroem_common::git_ref::{classify_ref, RefKind};` and replace the `let target = match crate::trigger_target::resolve_trigger_target(…) { … };` block with:

```rust
    // Spec 2026-10-15 § 6: a templated `ref:` renders from this request's
    // input through the private path; its errors are value-free
    // `RenderedRefError`s before they reach this log or the classifier.
    let resolved = match wh.git_ref.as_deref() {
        Some(t) if classify_ref(t) == RefKind::Template => {
            crate::trigger_target::resolve_templated_trigger_target(
                &state.workspaces,
                &wh.ws_name,
                &defining_config,
                &wh.task,
                t,
                &input_value,
            )
            .await
            .map_err(anyhow::Error::new)
        }
        static_ref => {
            crate::trigger_target::resolve_trigger_target(
                &state.workspaces,
                &wh.ws_name,
                &defining_config,
                &wh.task,
                static_ref,
            )
            .await
        }
    };
    let target = match resolved {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Webhook '{}' could not resolve its target: {:#}", name, e);
            return crate::web::api::classify_execute_error(e).into_response();
        }
    };
```

- [ ] **Step 4: Run the tests**

Run:
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::webhook git_refs_read_paths_test:: git_refs_scheduler_test::
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib trigger_target
```
Expected: PASS (static-ref webhooks and scheduler triggers unchanged).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/trigger_target.rs crates/stroem-server/src/web/hooks.rs crates/stroem-server/tests/git_refs_deferred_test.rs
git commit -m "feat(webhook): templated trigger ref rendered from the request through the private path"
```

---
### Task 15: `ref_template` in the job API and the UI label

Spec § 9.

**Files:**
- Modify: `crates/stroem-server/src/web/api/jobs.rs` — the step `json!` in `get_job` (`:313-348`)
- Modify: `ui/src/lib/types.ts` (`JobStep`, `:137-177`), `ui/src/lib/job-ref.ts`, `ui/src/components/step-timeline.tsx` (`:98`, badge row `:146-178`)
- Test: `crates/stroem-server/tests/git_refs_deferred_test.rs`, `ui/src/lib/__tests__/job-ref.test.ts`, `ui/src/components/__tests__/step-timeline.test.tsx`

**Interfaces:**
- Produces: job-detail step field `ref_template: string | null`; `export function deferredRefLabel(step: Pick<JobStep, "action_type" | "ref_template">): string | null` in `ui/src/lib/job-ref.ts`.

- [ ] **Step 1: Write the failing API test**

Append to `crates/stroem-server/tests/git_refs_deferred_test.rs`:

```rust
// ─── API (spec § 9) ───────────────────────────────────────────────────────

/// Job detail returns the template and the deferred kind of an unresolved
/// step; `ref_template` is content, not an identifier key (redacted as such).
#[tokio::test]
async fn job_detail_shows_the_template_of_a_deferred_step() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-later", json!({})).await?;
    let (status, detail) = api_req(&fx.router, "GET", &format!("/api/jobs/{job_id}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let later = detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["step_name"] == "later")
        .expect("later")
        .clone();
    assert_eq!(later["action_type"], json!("deferred"));
    assert_eq!(later["ref_template"], json!("{{ input.branch }}"));
    assert!(!stroem_server::redaction::STEP_IDENTIFIER_KEYS.contains(&"ref_template"));
    Ok(())
}
```

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration git_refs_deferred_test::job_detail_shows`
Expected: FAIL — `ref_template` is `null` (absent from the JSON).

- [ ] **Step 2: Add the field to job detail**

In `get_job`'s step `json!({ … })`, after `"task_revision": step.task_revision,` add `"ref_template": step.ref_template,`. Do not add it to `STEP_IDENTIFIER_KEYS`.

Run the test again. Expected: PASS.

- [ ] **Step 3: Write the failing UI tests**

Append to `ui/src/lib/__tests__/job-ref.test.ts` (import `deferredRefLabel` too):

```ts
describe("deferredRefLabel", () => {
  it("labels a deferred step with its template", () => {
    expect(deferredRefLabel({ action_type: "deferred", ref_template: "{{ input.branch }}" })).toBe(
      "ref: {{ input.branch }}",
    );
  });
  it("is null once the step is resolved, or without a template", () => {
    expect(deferredRefLabel({ action_type: "script", ref_template: "{{ input.branch }}" })).toBeNull();
    expect(deferredRefLabel({ action_type: "deferred", ref_template: null })).toBeNull();
    expect(deferredRefLabel({ action_type: "deferred" })).toBeNull();
  });
});
```

Append inside the `describe` that holds the pin-badge tests in `ui/src/components/__tests__/step-timeline.test.tsx`:

```tsx
  it("shows the ref template on a deferred step and no pin badge", () => {
    renderTimeline([
      makeStep({
        step_name: "publish",
        action_type: "deferred",
        status: "pending",
        ref_template: "{{ find.output.branch }}",
      }),
    ]);
    expect(screen.getByTestId("step-deferred-ref-publish").textContent).toBe(
      "ref: {{ find.output.branch }}",
    );
    expect(screen.queryByTestId("step-pin-publish")).toBeNull();
  });

  it("shows the pin badge, not the template, once the step is resolved", () => {
    renderTimeline([
      makeStep({
        step_name: "publish",
        action_type: "script",
        ref_template: "{{ find.output.branch }}",
        action_ref: "release/2.3",
        action_revision: "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901",
      }),
    ]);
    expect(screen.queryByTestId("step-deferred-ref-publish")).toBeNull();
    expect(screen.getByTestId("step-pin-publish").textContent).toBe("@ release/2.3 · 3f2a9c0");
  });
```

Run: `cd ui && bunx vitest run src/lib/__tests__/job-ref.test.ts src/components/__tests__/step-timeline.test.tsx`
Expected: FAIL — `deferredRefLabel` is not exported; `ref_template` is not a `JobStep` field.

- [ ] **Step 4: Implement the UI**

`ui/src/lib/types.ts`, in `JobStep` after `task_revision: string | null;`:

```ts
  /** The `ref:` template of a step created deferred (templated refs). Optional: older servers omit it. */
  ref_template?: string | null;
```

`ui/src/lib/job-ref.ts`, append:

```ts
/** `ref: {{ … }}` — a step whose action waits for its templated ref to be resolved. */
export function deferredRefLabel(
  step: Pick<JobStep, "action_type" | "ref_template">,
): string | null {
  if (step.action_type !== "deferred" || !step.ref_template) return null;
  return `ref: ${step.ref_template}`;
}
```

`ui/src/components/step-timeline.tsx`: import `deferredRefLabel` with `formatPin, stepPin`; after `const pin = stepPin(step);` add `const deferredRef = deferredRefLabel(step);`; after the `{pin && ( … )}` badge add:

```tsx
                  {deferredRef && (
                    <span
                      data-testid={`step-deferred-ref-${step.step_name}`}
                      title="The branch, tag or commit is chosen when the step starts"
                      className="rounded bg-violet-100 px-1.5 py-0.5 font-mono text-[10px] font-medium text-violet-700 dark:bg-violet-900/30 dark:text-violet-400"
                    >
                      {deferredRef}
                    </span>
                  )}
```

A carried templated row of a restart job (contract deviation 9) keeps `action_type: "deferred"` with a terminal status; it shows the template too, which is correct (it never resolved in this job).

- [ ] **Step 5: Run the UI checks**

Run:
```bash
cd ui && bunx vitest run src/lib/__tests__/job-ref.test.ts src/components/__tests__/step-timeline.test.tsx && bun run lint && bunx tsc --noEmit
```
Expected: PASS, no lint or type errors.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy -p stroem-server --all-targets -- -D warnings
git add crates/stroem-server/src/web/api/jobs.rs crates/stroem-server/tests/git_refs_deferred_test.rs ui/src
git commit -m "feat(ui,api): ref_template on job steps; deferred steps show their ref template"
```

---

### Task 16: Redaction read-path audit and E2E

Spec § 10.2, § 12 (redaction, E2E).

**Files:**
- Modify: `crates/stroem-server/tests/read_path_audit_test.rs`
- Modify: `tests/e2e.sh` — the `refs.yaml` heredoc in section 0b (`:57-89`) and a new section after section 20 (`:886-920`)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the redaction test**

Append to `crates/stroem-server/tests/read_path_audit_test.rs`:

```rust
/// Templated refs (spec 2026-10-15 § 10.2): a step whose action was chosen
/// by a templated ref reads `action_ref` / `action_revision` only after
/// resolution, and from then on job detail masks that commit's secrets in the
/// step's output — the same closure as a static ref.
const TEMPLATED_MAIN: &str = r#"
actions:
  hello:
    type: script
    runner: local
    script: echo main
tasks:
  pick-a-branch:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      run:
        action: leak
        ref: "{{ input.branch }}"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn job_detail_masks_a_secret_of_a_templated_refs_commit() -> Result<()> {
    bounded(async {
        let release = format!(
            "secrets:\n  token: {REF_ONLY_SECRET}\n{}",
            ETL_RELEASE.replace(
                "actions:\n  hello:\n",
                "actions:\n  leak:\n    type: script\n    runner: local\n    script: echo leak\n  hello:\n",
            )
        );
        let fx = pinned_workspace_fixture(PinnedFixtureOpts {
            etl_main: Some(TEMPLATED_MAIN.to_string()),
            etl_release: Some(release),
            ..Default::default()
        })
        .await?;
        let worker = register_worker(&fx.router, &["script"]).await;
        let (st, body) = execute_task(&fx.router, "etl", "pick-a-branch", json!({}), None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let job: Uuid = body["job_id"].as_str().expect("job_id").parse()?;
        claim_and_complete(
            &fx,
            &worker,
            job,
            "run",
            json!({"output": {"said": format!("token is {REF_ONLY_SECRET}")}}),
        )
        .await;
        let detail = assert_detail_masks_ref_only_secret(&fx, job).await;
        let run = detail["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["step_name"] == "run")
            .unwrap()
            .clone();
        assert_eq!(run["action_ref"], json!("release/2.3"), "identifiers stay readable");
        assert_eq!(run["output"]["said"], json!(format!("token is {MASK}")));
        Ok(())
    })
    .await
}
```

`REF_ONLY_SECRET`, `MASK`, `assert_detail_masks_ref_only_secret` and `bounded` already exist in this file / the fixture. If `REF_ONLY_SECRET` is shorter than 4 characters (it is not today), the mask floor (contract deviation 10) would skip it — keep a long value.

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --test integration read_path_audit_test::`
Expected: PASS (resolution stamped `action_ref` / `action_revision`, which the closure already reads).

- [ ] **Step 2: Add a templated task to the E2E workspace**

In `tests/e2e.sh`, inside the `refs.yaml` heredoc, after the `release-matrix` task (still under `tasks:`), add:

```yaml
    pick-version:
      mode: distributed
      input:
        branch:
          type: string
          default: main
      flow:
        run:
          action: which-version
          ref: "{{ input.branch }}"
```

The heredoc is quoted (`<<'YAML'`), so the shell leaves `{{ … }}` alone. The `release/1` branch is cut after this commit, so the task exists on every ref.

- [ ] **Step 3: Add the E2E section**

After section 20 (before section 21), add:

```bash
# --- 20b. Templated ref: the Run-form input chooses the branch (spec 2026-10-15) ---
info "Triggering pick-version with branch=release/1 (templated ref:)..."
EXEC_RESP_PV=$(acurl -X POST "$BASE_URL/api/workspaces/refs/tasks/pick-version/execute" \
    -H "Content-Type: application/json" -d '{"input": {"branch": "release/1"}}')
PV_JOB_ID=$(echo "$EXEC_RESP_PV" | jq -r '.job_id')
[ -n "$PV_JOB_ID" ] && [ "$PV_JOB_ID" != "null" ] || fail "pick-version execute failed: $EXEC_RESP_PV"
PV_POLLED=0; PV_STATUS="pending"
while [ "$PV_STATUS" != "completed" ] && [ "$PV_STATUS" != "failed" ]; do
    sleep 2; PV_POLLED=$((PV_POLLED + 2))
    [ "$PV_POLLED" -lt "$MAX_POLL" ] || { acurl "$BASE_URL/api/jobs/$PV_JOB_ID" | jq .; fail "pick-version did not finish"; }
    PV_DETAIL=$(acurl "$BASE_URL/api/jobs/$PV_JOB_ID"); PV_STATUS=$(echo "$PV_DETAIL" | jq -r '.status'); printf "."
done; echo ""
[ "$PV_STATUS" = "completed" ] || { echo "$PV_DETAIL" | jq .; fail "pick-version failed"; }
PV_STEP() { echo "$PV_DETAIL" | jq -r --arg f "$1" '.steps[] | select(.step_name=="run") | .[$f]'; }
[ "$(echo "$PV_DETAIL" | jq -r '.steps[] | select(.step_name=="run") | .output.version')" = "branch-r1" ] \
    || fail "pick-version ran '$(echo "$PV_DETAIL" | jq -r '.steps[0].output.version')', expected branch-r1"
[ "$(PV_STEP action_ref)" = "release/1" ] || fail "pick-version action_ref is '$(PV_STEP action_ref)'"
[ "$(PV_STEP action_revision)" = "$REFS_R1_SHA" ] || fail "pick-version pinned $(PV_STEP action_revision), expected $REFS_R1_SHA"
[ "$(PV_STEP ref_template)" = "{{ input.branch }}" ] || fail "pick-version ref_template is '$(PV_STEP ref_template)'"
pass "templated ref: the input chose release/1 ($REFS_R1_SHA)"
```

- [ ] **Step 4: Run the E2E suite**

Run: `./tests/e2e.sh`
Expected: every section passes, including 20 and 20b. (It builds the images; check free disk first — `df -h /` — and run `scripts/test-clean.sh` if the host is near full.)

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/tests/read_path_audit_test.rs tests/e2e.sh
git commit -m "test(templated-refs): redaction of a templated ref's commit secrets; e2e input chooses the branch"
```

---

### Task 17: Documentation

Spec § 13, § 14; contract deviation 1 (CLAUDE.md correction).

**Files:**
- Modify: `docs/src/content/docs/guides/git-refs.md` (`:70-71`, a new section before `## Ref forms`, `## Not yet supported` `:439`)
- Modify: `docs/src/content/docs/guides/templating.md` (a new section before `## Step name rules`, `:118`)
- Create: `docs/src/content/docs/operations/migration-051-052.md`; Modify: `docs/astro.config.mjs` (Operations group, after the `migration-049-050` entry, `:88-91`)
- Modify: `docs/scripts/generate-llms-txt.ts` (`git-refs` entry `:129-130`); regenerate `docs/public/llms.txt`
- Modify: `CLAUDE.md` (§ Capabilities `:170`, § Git refs `:211`, § Worker Recovery `:539`, § Tera Templating "Single owner"), `CONTEXT.md` (`:28` and two new entries), `docs/internal/TODO.md` (a new section after `## Git refs follow-ups`, `:1105`), `docs/internal/stroem-v2-plan.md` (after `**6d.` `:866`)
- Modify: `docs/superpowers/specs/2026-10-06-tera-2-upgrade-design.md` (§ 3.4 render-path table, `:449-457`), `docs/superpowers/specs/2026-10-15-templated-refs-design.md` (status line `:3`)

- [ ] **Step 1: The user guide**

In `docs/src/content/docs/guides/git-refs.md`, replace

```md
A ref is a fixed string. A value containing `{{` or `{%` is rejected: refs
are never templated.
```

with

```md
A ref is a fixed string, or a template on a flow step, a `type: task`
action or a webhook trigger — see [Templated refs](#templated-refs).
```

Insert before `## Ref forms`:

````md
## Templated refs

A `ref:` can be a Tera template, so the person who runs a task, the system
that calls a webhook, or an earlier step chooses the branch, tag or commit:

```yaml
tasks:
  deploy:
    input:
      branch: { type: string, default: main }
    flow:
      build:
        action: build
        ref: "{{ input.branch }}"          # chosen in the Run form
      find:
        action: find-release
      publish:
        action: billing.publish
        ref: "{{ find.output.branch }}"    # chosen by an earlier step
        depends_on: [find]

triggers:
  on-push:
    type: webhook
    name: on-push
    task: build
    ref: "{{ input.body.ref }}"            # chosen by the caller
```

A `ref:` is a template when it contains `{{` or `{%`.

| Place | Resolved | The template can read |
|---|---|---|
| Flow-step `ref:` | when the step becomes ready | `input` (job input), `state`, `global_state`, `job`, earlier steps' `output` / `error`, `each` (in a `for_each` instance) |
| `type: task` action `ref:` | when the step is dispatched | `input` (the step's `input:` rendered as strings, without the task's schema and without the action's input defaults), `job` |
| Webhook trigger `ref:` | when the webhook fires | `input` (`body`, `headers`, `method`, `query` and the trigger's `input` defaults) |

No ref template can read `secret` or use `vals`. A template is a
`stroem validate` error on scheduler triggers, hooks, event sources, agent
task tools and actions other than `type: task`.

**The owner.** The action name decides the owner workspace when the job is
created (`billing.publish` → `billing`); the template chooses only the
commit. The action name itself is never a template. An owner that is not a
git workspace, an unknown workspace or a library is a `400` at job creation.

**When.** Until it is resolved, a flow step is *deferred*: job detail shows
`action_type: deferred` and the UI shows `ref: {{ … }}` on the step. A root
step (no `depends_on`, `when` or `for_each`) is resolved while the job is
created. Each `for_each` instance renders its own `each.item`.

**One commit per ref.** Inside one job, every step that uses the same owner
and the same ref value runs the same commit — static and templated refs
alike — even if the branch moves while the job runs. A task retry, a re-run
and a restart resolve again.

**Errors.** The rendered value is trimmed. These fail the step — never the
job creation — with a message that never shows the value:

| Rendered value | Step error |
|---|---|
| empty | `[ref] step '…': the rendered ref is empty` |
| not a branch, tag or full SHA | `… is not a valid branch, tag or full commit SHA` |
| contains a secret value | `… contains a secret value` |
| no such branch or tag | `… was not found in workspace '…'` |
| the action or task is missing at that commit | `workspace '…' has no such action or task at the rendered ref` |
| git is unreachable | `workspace '…' is not available at the rendered ref yet` |
| the template fails to render | `the ref template could not be rendered: …` |

A failed resolution uses no retry attempt. A template syntax error or an
unknown filter is a `400` at job creation. A webhook answers `400` for these
errors (`500` when git is unreachable) and creates no job.

**`type: task` actions.** The ref reads the step's `input:` rendered as
strings, before the task is known. A string nested inside an object of the
step input must render too, even if the task never uses it: escape literal
template text there with `{% raw %}…{% endraw %}`. The step input is rendered
twice — once for the ref, once for the child job — so a value that changes
between renders (`now()`, a `vals` lookup) can differ.

**Tools.** `stroem validate` checks a templated ref's syntax and skips the
action or task lookup with a warning. `stroem run` ignores `ref:` and runs
the local action.

:::caution[Who can choose the commit]
There is no allow-list. Anyone who can set a value the template reads — a
user with `Run` on the task, any caller of the webhook (anyone at all when it
has no `secret`), or an earlier step's output — can make the server run any
branch, tag or commit of the owner workspace with that commit's secrets and
connections. With push access to the owner repository, that runs any code.
Never pass a secret into a value a ref template reads: a rendered ref is an
identifier and is never masked. The server rejects a rendered ref that
contains a known secret value of 4 or more characters, but not a transformed
one (`| upper`, `| replace`).
:::

Before you merge YAML with a templated `ref:`, every server replica must run
a release that supports it — see
[Migrations 051 and 052](/operations/migration-051-052/).
````

In `## Not yet supported`, add:

```md
- A templated `ref` on scheduler triggers, hooks, event sources and agent
  task tools; an empty rendered value meaning "no ref"; an allow-list of the
  refs a template may choose.
```

- [ ] **Step 2: The templating guide**

Insert before `## Step name rules` in `docs/src/content/docs/guides/templating.md`:

```md
## Templated refs

A `ref:` on a flow step, a `type: task` action or a webhook trigger can be a
template ([Git refs → Templated refs](/guides/git-refs/#templated-refs)). Its
context is narrower than a step input's:

| `ref:` on | Context |
|---|---|
| a flow step | `input`, `state`, `global_state`, `job`, earlier steps, `each` |
| a `type: task` action | `input` (the step's input rendered as strings), `job` |
| a webhook trigger | `input` (the webhook job input) |

None of them has `secret`, and `vals` is an unknown filter there.
```

- [ ] **Step 3: The upgrade note**

Create `docs/src/content/docs/operations/migration-051-052.md`:

```md
---
title: Migrations 051 and 052 — templated refs
description: Schema changes and the rollout rule for templated ref values
---

Migrations `051_templated_refs.sql` and `052_templated_refs_validate.sql`
ship with [templated refs](/guides/git-refs/#templated-refs).

## The migrations

**051** is additive:

| Table | Change |
|---|---|
| `job_step` | `ref_template`, `resolve_lease_owner`, `resolve_lease_until` (nullable) |
| `job_ref_pin` | new: one commit per job, owner workspace and ref |
| `job_step` | the action-type check accepts `deferred`, added `NOT VALID` (no table scan) |

**052** runs `ALTER TABLE job_step VALIDATE CONSTRAINT job_step_action_type_check`.
It scans `job_step` under a `SHARE UPDATE EXCLUSIVE` lock, so writes continue.
They are two migrations because each runs in its own transaction: a
validation inside 051 would hold 051's exclusive lock for the whole scan.

## Rollout rule

**Merge no workflow YAML with a templated `ref:` before every server replica
runs this release.**

- An older replica refuses a templated ref at job creation (`400`).
- But a deferred step created by a new replica can reach an older leader:
  its unmatched-step sweep fails the step after
  `recovery.unmatched_step_timeout_secs`, and an older replica never
  resolves it.
- Workers do not change: no claim ever returns a deferred step.

## What changes without templated refs

- `POST /worker/jobs/claim` answers `400` for a capability outside `script`,
  `docker`, `kubernetes` and `agent`, as `/worker/register` always did. Strøm
  workers send their registered capabilities, so only a custom client that
  sends an unknown token sees a change.
- The PinStore span of a ref resolution no longer records the ref (server
  logs only).
- The PinStore keeps the commits a live job has resolved through templated
  refs; recovery has a new phase (4.6) that re-advances a deferred step left
  `ready` by a crash.
```

In `docs/astro.config.mjs`, after the `migration-049-050` entry, add:

```js
              {
                label: "Migrations 051–052 — Templated Refs",
                slug: "operations/migration-051-052",
              },
```

- [ ] **Step 4: llms.txt**

In `docs/scripts/generate-llms-txt.ts`, change the `git-refs` entry's `desc` to `"ref: on actions, type: task and triggers — branch/tag/SHA, templated refs, pinned jobs, per-ref state"`. Then run `cd docs && bun run generate-llms && bun run build`. Expected: the build passes and `docs/public/llms.txt` contains "## Templated refs".

- [ ] **Step 5: CLAUDE.md**

1. § Capabilities (`:170`): replace the sentence ``The unmatched-step sweep and the partial index `idx_job_step_ready_claim` (migration `044_claim_index_agent.sql`) must carry the identical predicate or the planner drops the index.`` with:
   ``The unmatched-step sweep keeps the claim's `NOT IN` list (plus `action_type <> 'deferred'`, templated refs). Measured 2026-10-16: neither query uses the GIN index `idx_job_step_ready_claim` (GIN `jsonb_ops` cannot serve `<@`); both use `idx_job_step_ready_ability` (partial `status = 'ready'`) — pinned by stroem-db `templated_refs_test::claim_and_unmatched_sweep_still_use_a_ready_partial_index` (TODO.md).`` Also add one sentence: ``A claim request carrying a capability outside `KNOWN_ABILITIES` is a 400 (a deferred step's `required_ability` is `deferred`).``
2. § Git refs (`:211`): replace `(branch | tag | full SHA; \`stroem_common::git_ref::parse_git_ref\`, never templated, short SHAs get a hint)` with `(branch | tag | full SHA; \`stroem_common::git_ref::parse_git_ref\`, short SHAs get a hint) — or a template on a flow step, a \`type: task\` action or a webhook trigger (next bullet)`, and add a bullet after that first bullet:
   ```md
   - **Templated refs** (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`, guide section "Templated refs", upgrade note `operations/migration-051-052.md`). `git_ref::classify_ref` (`{{` / `{%`). A flow step with a templated ref is created DEFERRED (`job_creator::deferred_row`: `action_type = required_ability = 'deferred'`, `runner = 'none'`, `ref_template`, owner from the action name at creation — D11) and resolved when it is `ready` by `settlement::deferred::resolve_deferred_steps`, called in `Settlement::advance` (after the cascade, before task dispatch; errors logged) and `dispatch::init` (errors → compensation). Per step: a renewable lease with an owner token (`take_resolve_lease`, `LeaseGuard` aborts renewal on drop; 60 s / 20 s, `LeaseTiming` for tests); render with `Scope::StepRef` (no `secret`) on the `vals`-free ref engine (`template::render_ref_template`); `check_rendered_ref` (trim, empty, ref secret set, `parse_git_ref`); the commit from `job_ref_pin` (first writer wins, seeded with the creation's `RefMemo`) or `PinStore::resolve`; `WorkspaceManager::pinned_config_private` (labels `ws (sha7)`) + a second check against the selected commit's secrets; then `job_creator::resolve_step_row` (shared with creation) and `JobStepRepo::resolve_deferred_tx` (job row `FOR SHARE`, token fence, `ready_at = NOW()`). Failures: `fail_deferred_tx` (same fence, no retry attempt) + cascade, message `[ref] step '…': <RenderedRefError>` — `templated_ref::RenderedRefError` is a value-free LEAF (no `source`), classified by type; `classify_execute_error`: `Unavailable` → 500, else 400. A rendered value is private until both checks pass (the full-SHA residual of spec § 10.3 aside): `PinStore::resolve` records no `git_ref` span field. `type: task` action templated ref: dispatch (`handle_task_steps_pass` → `resolve_templated_task_ref`: `template::render_ref_projection`, `Scope::TaskActionRef`, `stamp_task_pin`). Webhook: `trigger_target::resolve_templated_trigger_target` (`render_context::webhook_ref`). Recovery Phase 4.6 `readvance_deferred_steps`. Restart: carried templated rows stay `deferred` with a terminal status (`Seed` copies only status/output). Rollout: no templated YAML before every replica runs the release. Tests: `tests/git_refs_deferred_test.rs`, stroem-db `templated_refs_test.rs`, e2e section 20b.
   ```
3. § Worker Recovery (`:539`): after the `(4.5) …` sentence add `(4.6) \`readvance_deferred_steps\` — \`advance\` every non-terminal job with a deferred \`ready\` step older than \`unmatched_step_timeout_secs\` and no live resolution lease (bounded and resumable like 4.5, cursor \`AppState::deferred_steps_cursor\`).`
4. § Tera Templating, "Single owner" bullet: after the `Scope` sentence add `` `Scope::StepRef` and `Scope::TaskActionRef` (templated refs) carry no `secret`; `render_context::webhook_ref` is the second constructor (webhook input only). ``

- [ ] **Step 6: CONTEXT.md**

Replace ` Never templated.` in the **Ref** entry (`:28`) with ` Templated when it contains \`{{\` or \`{%\` (see *Deferred step*).` and add after the **Pin** entry:

```md
- **Deferred step** — a flow step whose `ref:` is a template: stored with `action_type = required_ability = 'deferred'` (no worker can claim it) and resolved, when it is `ready`, by `settlement::deferred::resolve_deferred_steps`, which writes the step's real action columns. Spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`.
- **Ref memo** — table `job_ref_pin`: the one commit each `(job, owner workspace, ref as written or rendered)` runs at for the job's whole life. Seeded with a creation's `RefMemo`, extended by deferred-step resolution, first writer wins.
```

- [ ] **Step 7: TODO.md and the v2 plan**

In `docs/internal/TODO.md`, after the `## Git refs follow-ups` section (before `## Bugs Found & Fixed`), add:

```md
## Templated refs follow-ups (spec 2026-10-15 § 14)

Feature: `ref: "{{ … }}"` on flow steps, `type: task` actions and webhook triggers (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`, CLAUDE.md § Git refs, guide `guides/git-refs.md` § Templated refs, `operations/migration-051-052.md`).

- [ ] An empty rendered value meaning "no ref" (D6 option A).
- [ ] A trust gate for templated refs (`ref_allow` globs, or an owner-workspace policy).
- [ ] Close the transformed-secret path (D13): a restricted ref grammar, or masking ref identifiers in every outlet.
- [ ] A `type: task` ref and its child input from ONE evaluation of the caller's step input (§ 5.6 renders it twice).
- [ ] A privacy flag through `PinStore::ensure` / `ensure_tree` for a full-SHA ref whose value a `vals` secret holds (§ 10.3).
- [ ] Wait through a temporary git failure at promotion instead of failing the step (D7).
- [ ] A creation-time pre-check for templates that read only `input` (D8).
- [ ] Per-run ref override from API / UI / MCP / CLI (GR § 16).
- [ ] The claim SQL does not read the job's status: a `ready` step can be claimed between `JobRepo::cancel` and `cancel_pending_steps` (pre-existing, every step kind).
- [ ] Idempotent `type: task` / `type: approval` dispatch, then recovery of a `ready` task / approval row left by a crash (lifecycle-hardening spec).
- [ ] Templated refs on hooks, event sources, agent task tools, scheduler triggers.
- [ ] `idx_job_step_ready_claim` (GIN on `required_tags`) is not used by the claim or the unmatched sweep — GIN `jsonb_ops` cannot serve `<@`; both use `idx_job_step_ready_ability`. Decide: drop it, or replace it with an index the claim can use (measured 2026-10-16).
- [ ] A failed literal connection pre-check at resolution reports the generic `Unresolvable` sentence; give it its own value-free kind without changing the creation-time 400 classification.
```

In `docs/internal/stroem-v2-plan.md`, after the `**6d. Git refs on references** — DONE …` paragraph, add:

```md
**6e. Templated refs** — DONE (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`). `ref:` may be a Tera template on flow steps (resolved at promotion — deferred steps, resolution pass in advance/init, lease, `job_ref_pin` memo), `type: task` actions (at dispatch) and webhook triggers (at fire time); no `secret` / `vals` in ref contexts; value-free `RenderedRefError`; recovery Phase 4.6. Migrations 051 + 052; upgrade note `docs/src/content/docs/operations/migration-051-052.md`. Follow-ups in TODO.md § Templated refs follow-ups.
```

Also mark the spec's status line (`docs/superpowers/specs/2026-10-15-templated-refs-design.md:3`) as `Status: implemented (plan docs/superpowers/plans/2026-10-16-templated-refs.md)`.

- [ ] **Step 8: The render-path table (spec § 10.4)**

CLAUDE.md § Secrets in logs requires every new render path in the table of `docs/superpowers/specs/2026-10-06-tera-2-upgrade-design.md` § 3.4 (`:449-457`). Add these rows before the `CLI \`stroem run\`` row:

```md
| Deferred step `ref:` (`settlement/deferred.rs`, `Scope::StepRef`, ref engine without `vals`) | `job_step.error_message`, `tracing::warn!` | value-free: one `RenderedRefError` per kind (spec 2026-10-15 § 5.5), the rendered value never quoted; a `Render` error carries only the `TemplateError` text |
| `type: task` action `ref:` (`settlement/dispatch.rs::resolve_templated_task_ref`, `Scope::TaskActionRef`) | job log via `fail_task_step`, step error | value-free; the ref-input projection is a caller-side step-input render (scrubbed, never withheld); the ref render is owner-side (withheld when O ≠ A) |
| Webhook trigger `ref:` (`trigger_target::resolve_templated_trigger_target`, `render_context::webhook_ref`) | HTTP 400 / 500 body, `tracing::warn!` in `web/hooks.rs`, `AppError::Internal` log | value-free: `RenderedRefError` only (a leaf with no `source`) |
```

- [ ] **Step 9: Commit**

```bash
git add docs CLAUDE.md CONTEXT.md
git commit -m "docs: templated refs — guide, templating contexts, migration 051/052 note, CLAUDE.md, CONTEXT.md, TODO"
```

---

### Task 18: Final verification

**Files:** none new.

- [ ] **Step 1: The full CI suite**

Run, in order (user rule: the full suite before any push):

```bash
cargo fmt --check --all
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --workspace --all-targets -- -D warnings
CARGO_PROFILE_DEV_DEBUG=0 cargo test --workspace
cd ui && bun run lint && bunx tsc --noEmit && bunx vitest run && cd ..
cd docs && bun run build && cd ..
```

Expected: all green. `log_storage::tests` is a known flaky under parallel load (memory `reference_flaky_tests`); re-run that module alone before treating a failure there as real.

- [ ] **Step 2: E2E**

Run: `./tests/e2e.sh`
Expected: all sections pass, including 20b.

- [ ] **Step 3: Check the spec's success criteria against the tests**

| Criterion | Test |
|---|---|
| 1. A flow-step ref reads input, step outputs, state, global_state, job, each | `root_templated_ref_is_resolved_at_creation`, `ref_read_from_an_earlier_steps_output`, `for_each_instances_resolve_their_own_values_and_share_equal_ones`, `step_ref_carries_job_input_state_steps_and_each_in_order` |
| 2. A `type: task` action ref reads the caller's step input as strings | `templated_task_action_ref_resolves_at_dispatch_and_stamps_the_pin`, `the_ref_input_is_schema_free_and_the_child_keeps_native_json` |
| 3. A webhook ref reads the webhook input | `webhook_templated_ref_creates_a_pinned_job` |
| 4. One commit per `(owner, ref)` per job | `one_commit_per_ref_survives_a_branch_move`, `restart_re_resolves_a_templated_step_and_leaves_a_carried_one` (the exception) |
| 5. Static refs unchanged | the unchanged `git_refs_*` suites, `pinned_*` suites, e2e section 20 |

- [ ] **Step 4: Codex implementation review (user rule)**

Before the finishing-a-development-branch menu, send the branch to Codex for an adversarial implementation review (`/codex:rescue`; from a worktree start a fresh thread — threads are per cwd). Present its output verbatim, verify each finding against the code, fix, and re-review until Codex gives a merge verdict.

---
