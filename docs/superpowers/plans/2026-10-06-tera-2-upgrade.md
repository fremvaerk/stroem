# Tera 1 → 2 Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `tera` 1.20 with 2.4 in Strøm so that every server-side template error is value-free by construction, Tera 1 behaviour is kept where a small shim can keep it, and the rest is documented in an upgrade guide.

**Architecture:** All Tera usage stays inside `stroem-common`. One process-wide engine (`tera_engine.rs`) holds Tera's builtins plus our compatibility filters (`tera_compat.rs`); every render clones it and registers a per-call `vals` filter. Every Tera error is converted at the crate boundary into `TemplateError` (`template_error.rs`), whose message is assembled from a category, a position and closed-set names — Tera's text is reachable only through `raw_detail()`, which only `stroem-cli` calls. Server code changes only where it builds errors from rendered values (connection resolution, `for_each`), where it logged unscrubbed render errors (hooks, event sources), and where the render context leaves `state` absent.

**Tech Stack:** Rust 2021 workspace, `tera` 2.4 (`preserve_order`), `tera-contrib` 0.3 (`json`), `serde_json`, `chrono` / `chrono-tz`, `anyhow`, sqlx/Postgres testcontainers for server integration tests.

**Spec:** `docs/superpowers/specs/2026-10-06-tera-2-upgrade-design.md` (revision 7). Research notes with Tera 2 source references: (working notes, not committed). Tera 2.4.0 source is under `~/.cargo/registry/src/*/tera-2.4.0`. Tera 1.20.1 is at `~/.cargo/registry/src/*/tera-1.20.1`.

## Global Constraints

- Worktree `/Users/ala/workspace/fremvaerk/stroem-deps`, branch `up/tera`. Never commit the working notes.
- Every cargo command is prefixed `CARGO_INCREMENTAL=0`. Never set `CARGO_TARGET_DIR` (the global target `/Users/ala/.tmp/cargo` is shared).
- Before any cargo build/test/clippy: `df -g /Users/ala | tail -1`; if the Available column is below 10, STOP and report — do not build.
- Never run `docker ... prune`, `docker rm`, `docker volume rm`, `docker rmi`. The coordinator runs `scripts/test-clean.sh`.
- Never run `cargo test --workspace` inside a task unless the task says so (Task 10 does). Run the crates/binaries the task names.
- Commits: conventional style, no `Co-Authored-By` or any AI attribution trailer. Do not push.
- `tera = { version = "2.4", features = ["preserve_order"] }`; `tera-contrib = { version = "0.3", default-features = false, features = ["json"] }`. No other tera-contrib feature (`date` pulls ICU).
- No server-side code path may format a `tera::Error`, call `TemplateError::raw_detail()`, or interpolate a rendered value, a part of one, or template source text into an error, log line, persisted message or API response (spec § 3.2–§ 3.4).
- Message wording that tests assert is copied verbatim from this plan.

## Review Focus

Inputs the spec implies but its sections do not test directly; each line names the task whose tests pin it.

1. A workspace secret whose literal value contains `{%` or `{#` (e.g. password `p{%ss`) must keep loading unchanged — secret-value detection stays `{{`-only (Task 6, test `literal_secret_with_block_marker_is_not_rendered`).
2. `{{ step.output.x | default(value='n/a') }}` where `step` was SKIPPED (output `null`) keeps rendering `n/a` (Task 3, `default_replaces_null_like_tera1`).
3. A task's first run with `{{ state.cursor | default(value=0) }}` and `{% if state.x %}` renders `0` / takes the else branch instead of failing (Task 5, `absent_snapshot_renders_state_as_null`).
4. `when: "{{ input.ratio }}"` with `ratio = 0.0` skips the step, as it did with Tera 1 (rendered `0`) (Task 4, truth table row `0.0`).
5. A connection-typed step input rendered from a secret that does not name a connection fails the step with a message naming the FIELD, never the rendered text (Task 6, `rendered_connection_name_never_in_error_chain`).

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `Cargo.toml` (workspace) | `tera` 2.4, `tera-contrib` 0.3 | 1 |
| `crates/stroem-common/Cargo.toml` | depend on `tera-contrib`, `chrono-tz` | 1, 3 |
| `crates/stroem-common/src/template_error.rs` (new) | `TemplateError`, `ValsFailure`, conversion, enrichment allow-lists | 1, 2 |
| `crates/stroem-common/src/tera_engine.rs` (new) | process-wide base engine; render engine (with `vals`); check engine | 1, 3 |
| `crates/stroem-common/src/tera_compat.rs` (new) | C1 `default`, C3 ported Tera 1 filters + `now` | 3 |
| `crates/stroem-common/src/template.rs` | render entry points, `vals` filter, `is_vals_failure`, `check_template_syntax`, `evaluate_condition`, `looks_templated`, connection resolver | 1, 4, 6 |
| `crates/stroem-common/src/validation.rs` | four template checks; `check_connection_values(label, …)` | 1, 6 |
| `crates/stroem-common/src/models/workflow.rs` | load-time secret/connection rendering (unchanged detection) | 6 |
| `crates/stroem-server/src/render_context.rs` | C2 null `state`/`global_state`; Tera keyword collisions | 5 |
| `crates/stroem-server/src/cascade.rs` | shape-only `for_each` errors | 4 |
| `crates/stroem-server/src/job_creator.rs` | both pre-checks use `looks_templated`, no names in 400s | 6 |
| `crates/stroem-server/src/settlement/hooks.rs` | scrub hook-input render failures | 7 |
| `crates/stroem-server/src/event_source.rs` | scrub env render failures | 7 |
| `crates/stroem-cli/src/local/error_report.rs` (new) | `full_report(&anyhow::Error)` with raw detail | 8 |
| `crates/stroem-cli/src/local/{run.rs,validate.rs,mod.rs}` | use `full_report`; shape-only `for_each` | 4, 8 |
| server tests (`integration_test.rs`, `git_refs_claim_test.rs`, unit tests in `cascade.rs`, `rendering.rs`, `workspace_set.rs`) | re-fixtured security tests | 9 |
| docs | upgrade guide, guides, CLAUDE.md, TODO.md, llms.txt | 11 |

---

### Task 1: Core swap — Tera 2 engine, value-free `TemplateError`, `vals`, validation

The workspace must compile at the end of this task. Many existing tests that depend on Tera 1 semantics will fail until later tasks; this task's own tests and the listed stroem-common tests must pass.

**Files:**
- Modify: `Cargo.toml` (workspace deps, `tera = "1"` line ~43)
- Modify: `crates/stroem-common/Cargo.toml`
- Create: `crates/stroem-common/src/template_error.rs`
- Create: `crates/stroem-common/src/tera_engine.rs`
- Modify: `crates/stroem-common/src/lib.rs` (module list)
- Modify: `crates/stroem-common/src/template.rs:1-116` (vals filter, `is_vals_failure`, render), `:380-406` (`render_json_strings`), `:770-800` (`render_value_deep`)
- Modify: `crates/stroem-common/src/validation.rs:280-345` (`when`, `for_each`), `:1754-1778` (`prompt`, `system_prompt`)
- Test: in-module tests of `template_error.rs`, `template.rs`, `validation.rs`

**Interfaces:**
- Produces (used by every later task):
  - `stroem_common::template_error::{TemplateError, ValsFailure, ValsFailureKind}`
  - `TemplateError::message(&self) -> &str` (value-free), `TemplateError::is_vals_failure(&self) -> bool`, `TemplateError::vals_failure_kind(&self) -> Option<ValsFailureKind>`, `TemplateError::raw_detail(&self) -> String`, `Display` = `"{message}"` or `"{message} (line L, column C)"`.
  - `pub(crate) fn TemplateError::from_tera(err: &tera::Error, src: Option<&str>, vals: Option<ValsFailure>) -> TemplateError`
  - `stroem_common::template::check_template_syntax(src: &str) -> Result<(), TemplateError>`
  - `stroem_common::tera_engine::{base, render_engine, check_engine}` (crate-private)
  - `render_template` / `render_template_with` keep their signatures (`-> anyhow::Result<String>`); their error chain is `"Failed to render template"` (or `"Failed to parse template"` for a syntax error) → `TemplateError`.

- [ ] **Step 1: Bump the dependencies**

`Cargo.toml` `[workspace.dependencies]`, replace `tera = "1"` with:

```toml
tera = { version = "2.4", features = ["preserve_order"] }
tera-contrib = { version = "0.3", default-features = false, features = ["json"] }
```

`crates/stroem-common/Cargo.toml` `[dependencies]`, next to `tera.workspace = true`, add:

```toml
tera-contrib.workspace = true
```

- [ ] **Step 2: Write `template_error.rs` with failing tests**

Create `crates/stroem-common/src/template_error.rs`:

```rust
//! Value-free template errors (spec 2026-10-06 § 3.2).
//!
//! Every Tera error is converted here. The message is assembled by us from a
//! category, a position and (Task 2) names drawn from closed sets — Tera's own
//! text, which can quote context values and template source, is kept only for
//! [`TemplateError::raw_detail`], which only `stroem-cli` may call.

use std::fmt;

/// How a `vals` call failed (recorded by the filter itself, spec § 3.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValsFailureKind {
    /// `vals eval` exited unsuccessfully; `None` when killed by a signal.
    Exited(Option<i32>),
    /// The load budget's deadline passed while `vals` ran.
    TimedOut,
    /// The `vals` binary could not be started.
    SpawnFailed,
    /// `vals` succeeded but its output was not the expected JSON.
    BadOutput,
}

/// A `vals` failure. `stderr` is arbitrary subprocess output: it is shown
/// only by [`TemplateError::raw_detail`].
#[derive(Clone)]
pub struct ValsFailure {
    pub kind: ValsFailureKind,
    pub stderr: String,
}

pub struct TemplateError {
    message: String,
    line: Option<usize>,
    column: Option<usize>,
    vals: Option<ValsFailure>,
    raw: String,
}

impl TemplateError {
    pub(crate) fn from_tera(err: &tera::Error, src: Option<&str>, vals: Option<ValsFailure>) -> Self {
        let _ = src; // used by Task 2's filter-name enrichment
        let (category, span) = match err.kind() {
            tera::ErrorKind::SyntaxError(r) => ("template syntax error", Some(r.span().clone())),
            tera::ErrorKind::RenderingError(r) => ("template rendering failed", Some(r.span().clone())),
            tera::ErrorKind::Msg(text) => (msg_category(text), None),
            tera::ErrorKind::InvalidArgument { .. } => ("a filter received a value of the wrong type", None),
            tera::ErrorKind::MissingArgument { .. } => ("a filter call is missing a required argument", None),
            tera::ErrorKind::OutOfRangeArgument { .. } => ("a number is out of range", None),
            _ => ("template rendering failed", None),
        };
        let message = match &vals {
            Some(v) => vals_category(v.kind),
            None => category.to_string(),
        };
        TemplateError {
            message,
            line: span.as_ref().map(|s| s.start_line),
            column: span.as_ref().map(|s| s.start_col + 1),
            vals,
            raw: err.to_string(),
        }
    }

    /// The value-free message, without position.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_vals_failure(&self) -> bool {
        self.vals.is_some()
    }

    pub fn vals_failure_kind(&self) -> Option<ValsFailureKind> {
        self.vals.as_ref().map(|v| v.kind)
    }

    /// Tera's original report plus any `vals` stderr. It contains template
    /// source and may contain context values. ONLY `stroem-cli` may call
    /// this (enforced by a test in Task 8).
    pub fn raw_detail(&self) -> String {
        match &self.vals {
            Some(v) if !v.stderr.trim().is_empty() => {
                format!("{}\nvals stderr:\n{}", self.raw, v.stderr.trim())
            }
            _ => self.raw.clone(),
        }
    }
}

fn msg_category(text: &str) -> &'static str {
    let first = text.lines().next().unwrap_or("");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    if first.starts_with("Unknown filter `") {
        "template uses an unknown filter"
    } else if first.starts_with("Unknown test `") {
        "template uses an unknown test"
    } else if first.starts_with("Unknown function `") {
        "template uses an unknown function"
    } else if first.starts_with("Template inheritance ({% extends %}) is not supported") {
        "{% extends %} is not supported"
    } else if first.starts_with("Blocks not supported") {
        "{% block %} is not supported"
    } else {
        "template could not be compiled"
    }
}

fn vals_category(kind: ValsFailureKind) -> String {
    match kind {
        ValsFailureKind::Exited(Some(code)) => format!("vals failed (exit status {code})"),
        ValsFailureKind::Exited(None) => "vals failed (killed by a signal)".to_string(),
        ValsFailureKind::TimedOut => "vals timed out".to_string(),
        ValsFailureKind::SpawnFailed => "vals could not be started".to_string(),
        ValsFailureKind::BadOutput => "vals returned invalid output".to_string(),
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(l), Some(c)) => write!(f, "{} (line {l}, column {c})", self.message),
            _ => write!(f, "{}", self.message),
        }
    }
}

impl fmt::Debug for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TemplateError")
            .field("message", &self.message)
            .field("line", &self.line)
            .field("column", &self.column)
            .field("vals", &self.vals.as_ref().map(|v| v.kind))
            .finish()
    }
}

impl std::error::Error for TemplateError {}

#[cfg(test)]
mod tests {
    use crate::template::render_template;
    use serde_json::json;

    const CONTEXT_CANARY: &str = "ctx-canary-7f3a";
    const SOURCE_CANARY: &str = "src-canary-91be";

    fn template_error(err: &anyhow::Error) -> &super::TemplateError {
        err.chain()
            .find_map(|c| c.downcast_ref::<super::TemplateError>())
            .expect("chain holds a TemplateError")
    }

    #[test]
    fn render_error_carries_category_and_position_only() {
        let ctx = json!({ "secret": { "X": CONTEXT_CANARY } });
        let tpl = format!("{{{{ secret.X | upper | int }}}} {SOURCE_CANARY}");
        let err = render_template(&tpl, &ctx).unwrap_err();
        let te = template_error(&err);
        assert!(te.raw_detail().contains(&CONTEXT_CANARY.to_uppercase()));
        for text in [format!("{err:#}"), format!("{err:?}"), te.to_string()] {
            assert!(!text.contains(CONTEXT_CANARY), "{text}");
            assert!(!text.contains(&CONTEXT_CANARY.to_uppercase()), "{text}");
            assert!(!text.contains(SOURCE_CANARY), "{text}");
        }
        assert_eq!(te.message(), "template rendering failed");
        assert!(te.to_string().contains("(line 1, column"), "{te}");
    }

    #[test]
    fn syntax_error_carries_no_source() {
        let tpl = format!("{{{{ 'x' + }}}} {SOURCE_CANARY}");
        let err = render_template(&tpl, &json!({})).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains(SOURCE_CANARY), "{text}");
        assert_eq!(template_error(&err).message(), "template syntax error");
    }

    #[test]
    fn unknown_filter_msg_never_echoes_the_name() {
        let tpl = format!("{{{{ x | {SOURCE_CANARY_IDENT} }}}}", SOURCE_CANARY_IDENT = "srccanary91be");
        let err = render_template(&tpl, &json!({"x": 1})).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("srccanary91be"), "{text}");
        assert_eq!(template_error(&err).message(), "template uses an unknown filter");
    }

    #[test]
    fn blocks_are_rejected_with_fixed_text() {
        let err = render_template("{% block b %}x{% endblock %}", &json!({})).unwrap_err();
        assert_eq!(template_error(&err).message(), "{% block %} is not supported");
    }
}
```

Add to `crates/stroem-common/src/lib.rs` (alphabetical with the others):

```rust
pub mod template_error;
pub(crate) mod tera_engine;
```

- [ ] **Step 3: Write `tera_engine.rs`**

```rust
//! The one configured Tera (spec 2026-10-06 § 3.1).

use crate::budget::LoadBudget;
use crate::template_error::ValsFailure;
use std::sync::{Arc, LazyLock, Mutex};

/// Builtins plus the filters we promise users. Cloned per render.
static BASE: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut t = tera::Tera::default();
    t.register_filter("json_encode", tera_contrib::json::json_encode);
    t
});

pub(crate) fn base() -> &'static tera::Tera {
    &BASE
}

/// Engine for rendering: `vals` resolves `ref+` references under `budget`
/// and records a failure in `slot`.
pub(crate) fn render_engine(budget: LoadBudget, slot: Arc<Mutex<Option<ValsFailure>>>) -> tera::Tera {
    let mut t = BASE.clone();
    t.register_filter(
        crate::template::VALS_FILTER,
        move |v: &tera::Value, _kw: tera::Kwargs, _st: &tera::State| {
            crate::template::vals_filter_with(v, budget, &slot)
        },
    );
    t
}

/// Engine for compile checks: `vals` is the identity, so validation never
/// starts a subprocess.
pub(crate) fn check_engine() -> tera::Tera {
    let mut t = BASE.clone();
    t.register_filter(
        crate::template::VALS_FILTER,
        |v: &tera::Value, _kw: tera::Kwargs, _st: &tera::State| -> tera::TeraResult<tera::Value> {
            Ok(v.clone())
        },
    );
    t
}
```

If `tera::Tera` does not implement `Clone`, stop and report — the design depends on it (the research notes say it does).

- [ ] **Step 4: Port the `vals` filter, render functions and `is_vals_failure` in `template.rs`**

Replace `template.rs` lines 1–116 (imports through `render_template_with`) with:

```rust
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::budget::{is_deadline_exceeded, run_with_deadline, LoadBudget};
use crate::models::workflow::{ConnectionDef, InputFieldDef, WorkspaceConfig};
use crate::template_error::{TemplateError, ValsFailure, ValsFailureKind};

/// Name the `vals` filter is registered under.
pub(crate) const VALS_FILTER: &str = "vals";

/// Tera filter that resolves `ref+` secret references via the vals CLI.
///
/// Usage in templates: `{{ secret.KEY | vals }}`
/// - Non-string values and strings not starting with `ref+` pass through.
/// - `ref+` strings are resolved via `vals eval`, killed if `budget` expires.
///
/// A failure is recorded in `slot` (kind + stderr) and reported to Tera with
/// a fixed message: neither the reference nor the stderr enters Tera's text.
pub(crate) fn vals_filter_with(
    value: &tera::Value,
    budget: LoadBudget,
    slot: &Mutex<Option<ValsFailure>>,
) -> tera::TeraResult<tera::Value> {
    let fail = |kind: ValsFailureKind, stderr: String| {
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(ValsFailure { kind, stderr });
        tera::Error::message("vals failed")
    };
    let s = match value.as_str() {
        Some(s) => s,
        None => return Ok(value.clone()),
    };
    if !s.starts_with("ref+") {
        return Ok(value.clone());
    }

    let input_str = serde_json::to_string(&serde_json::json!({ "_v": s }))
        .map_err(|_| fail(ValsFailureKind::BadOutput, String::new()))?;
    let mut cmd = std::process::Command::new("vals");
    cmd.args(["eval", "-f", "-", "-o", "json"]);
    let output = run_with_deadline(cmd, Some(input_str.as_bytes()), &budget).map_err(|e| {
        if is_deadline_exceeded(&e) {
            fail(ValsFailureKind::TimedOut, String::new())
        } else {
            fail(ValsFailureKind::SpawnFailed, format!("{e:#}"))
        }
    })?;
    if !output.status.success() {
        return Err(fail(
            ValsFailureKind::Exited(output.status.code()),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let resolved: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| fail(ValsFailureKind::BadOutput, String::new()))?;
    match resolved.get("_v").and_then(|v| v.as_str()) {
        Some(resolved_str) => Ok(tera::Value::from(resolved_str)),
        None => Err(fail(ValsFailureKind::BadOutput, String::new())),
    }
}

/// True when `err` is, or wraps, a failure of the `vals` filter (typed: the
/// filter records it in a side channel, spec § 3.7).
pub fn is_vals_failure(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<TemplateError>()
            .is_some_and(TemplateError::is_vals_failure)
    })
}

/// Renders a single Tera template string against a JSON context
pub fn render_template(template: &str, context: &serde_json::Value) -> Result<String> {
    render_template_with(template, context, &LoadBudget::unbounded())
}

/// [`render_template`] whose `vals` filter honours `budget`.
pub fn render_template_with(
    template: &str,
    context: &serde_json::Value,
    budget: &LoadBudget,
) -> Result<String> {
    let slot = Arc::new(Mutex::new(None));
    let tera = crate::tera_engine::render_engine(*budget, slot.clone());
    let ctx = tera::Context::from_serialize(context)
        .map_err(|e| anyhow::Error::new(TemplateError::from_tera(&e, None, None)))
        .context("Failed to convert JSON to Tera context")?;
    tera.render_str(template, &ctx, false).map_err(|e| {
        let vals = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        let outer = if matches!(e.kind(), tera::ErrorKind::SyntaxError(_)) {
            "Failed to parse template"
        } else {
            "Failed to render template"
        };
        anyhow::Error::new(TemplateError::from_tera(&e, Some(template), vals)).context(outer)
    })
}

/// Compile `src` exactly as rendering would (`render_str`'s one-off
/// restrictions included) without running anything that has a side effect.
/// A runtime error (undefined variable, filter failure on the empty context)
/// means the template compiled.
pub fn check_template_syntax(src: &str) -> std::result::Result<(), TemplateError> {
    let tera = crate::tera_engine::check_engine();
    match tera.render_str(src, &tera::Context::new(), false) {
        Ok(_) => Ok(()),
        Err(e) if matches!(e.kind(), tera::ErrorKind::RenderingError(_)) => Ok(()),
        Err(e) => Err(TemplateError::from_tera(&e, Some(src), None)),
    }
}
```

In `render_json_strings` (was `template.rs:386-387`) replace the context with:

```rust
            let rendered = render_template(s, context)
                .context("Failed to render a template in a JSON string")?;
```

In `render_value_deep` (was `template.rs:777-778`) replace the context with:

```rust
                let rendered = render_template(s, context)
                    .context("Failed to render a template in a value")?;
```

- [ ] **Step 5: Switch the four validation checks**

`validation.rs` `when` check (was lines 295–309) becomes:

```rust
            if let Some(ref when_expr) = step.when {
                if let Err(e) = crate::template::check_template_syntax(when_expr) {
                    return Err(anyhow::Error::new(e).context(format!(
                        "Task '{}' step '{}' has an invalid when expression",
                        task_name, step_name
                    )));
                }
            }
```

`for_each` string branch (was lines 323–336):

```rust
                    serde_json::Value::String(expr) => {
                        if let Err(e) = crate::template::check_template_syntax(expr) {
                            return Err(anyhow::Error::new(e).context(format!(
                                "Task '{}' step '{}' has an invalid for_each expression",
                                task_name, step_name
                            )));
                        }
                    }
```

`prompt` / `system_prompt` (was lines 1754–1778):

```rust
    if let Some(ref prompt) = action.prompt {
        if let Err(e) = crate::template::check_template_syntax(prompt) {
            return Err(anyhow::Error::new(e)
                .context(format!("Action '{}' has an invalid prompt template", action_name)));
        }
    }
    if let Some(ref sp) = action.system_prompt {
        if let Err(e) = crate::template::check_template_syntax(sp) {
            return Err(anyhow::Error::new(e).context(format!(
                "Action '{}' has an invalid system_prompt template",
                action_name
            )));
        }
    }
```

Rewrite the comment block above the `when` check (was lines 280–294): delete the "Known limitation — unknown Tera filters" paragraph and replace it with:

```rust
            // Unknown filters, tests and functions are rejected here: Tera 2
            // checks every reference when the template compiles, including
            // branches that never run (spec 2026-10-06 § 3.1).
```

- [ ] **Step 6: Add the engine/validation unit tests to `template.rs`'s test module**

```rust
    #[test]
    fn check_template_syntax_accepts_vals_and_json_encode() {
        assert!(check_template_syntax("{{ 'ref+vault://x' | vals }}").is_ok());
        assert!(check_template_syntax("{{ x | json_encode() }}").is_ok());
        assert!(check_template_syntax("{{ undefined_var.field }}").is_ok());
    }

    #[test]
    fn check_template_syntax_rejects_unknown_filter_blocks_and_extends() {
        assert_eq!(
            check_template_syntax("{% if false %}{{ x | nope }}{% endif %}").unwrap_err().message(),
            "template uses an unknown filter"
        );
        assert_eq!(
            check_template_syntax("{% block b %}{% endblock %}").unwrap_err().message(),
            "{% block %} is not supported"
        );
        assert_eq!(
            check_template_syntax("{% extends \"x\" %}").unwrap_err().message(),
            "{% extends %} is not supported"
        );
    }

    #[test]
    fn vals_passes_non_ref_values_through() {
        assert_eq!(render_template("{{ 'plain' | vals }}", &json!({})).unwrap(), "plain");
        assert_eq!(render_template("{{ 42 | vals }}", &json!({})).unwrap(), "42");
    }

    #[test]
    fn vals_spawn_failure_is_typed_and_value_free() {
        // PATH without vals: the binary cannot start.
        let _guard = crate::test_env::PathGuard::empty();
        let err = render_template("{{ 'ref+vault://secret/path#key' | vals }}", &json!({})).unwrap_err();
        assert!(is_vals_failure(&err));
        let text = format!("{err:#} {err:?}");
        assert!(text.contains("vals could not be started"), "{text}");
        assert!(!text.contains("vault://secret/path"), "{text}");
    }

    #[test]
    fn non_vals_render_error_is_not_a_vals_failure() {
        let err = render_template("{{ 'x' | int }}", &json!({})).unwrap_err();
        assert!(!is_vals_failure(&err));
    }

    #[test]
    fn json_encode_keeps_sorted_key_order() {
        let ctx = json!({"m": {"b": 1, "a": 2, "c": 3}});
        assert_eq!(render_template("{{ m | json_encode() }}", &ctx).unwrap(), r#"{"a":2,"b":1,"c":3}"#);
    }
```

`crate::test_env::PathGuard` — check whether the existing vals tests in `template.rs` (search `fn test_vals_` and `PATH`) already have a helper for running without `vals` on PATH; reuse it. If none exists, replace this one test's guard with `if which::which("vals").is_ok() { return; }` is NOT allowed (`which` is not a stroem-common dependency) — instead call `vals_filter_with` directly with a budget and assert on the slot:

```rust
    #[test]
    fn vals_records_failure_kind_in_slot() {
        let slot = std::sync::Mutex::new(None);
        let r = vals_filter_with(&tera::Value::from("ref+bogus-backend://x"), LoadBudget::unbounded(), &slot);
        assert!(r.is_err());
        let kind = slot.lock().unwrap().as_ref().map(|v| v.kind);
        assert!(matches!(kind, Some(ValsFailureKind::SpawnFailed) | Some(ValsFailureKind::Exited(_))), "{kind:?}");
    }
```

Port the existing `vals` tests (search `vals_filter(` in the test module — the `#[cfg(test)] fn vals_filter` wrapper is deleted in Step 4): each becomes a `render_template("{{ '<input>' | vals }}", …)` call or a direct `vals_filter_with(&tera::Value::from(…), LoadBudget::unbounded(), &slot)` call; compare strings, not `json!` values. Delete the old `is_vals_failure_*` test that constructed a Tera 1 `CallFilter` error; the two tests above replace it.

- [ ] **Step 7: Compile the workspace and run this task's tests**

```bash
df -g /Users/ala | tail -1
CARGO_INCREMENTAL=0 cargo build --workspace --all-targets 2>&1 | grep -E '^(error|warning)' -A6 | head -60
CARGO_INCREMENTAL=0 cargo test -p stroem-common template_error:: 2>&1 | tail -15
CARGO_INCREMENTAL=0 cargo test -p stroem-common template::tests::check_template_syntax template::tests::vals template::tests::json_encode template::tests::non_vals 2>&1 | tail -15
```

Expected: the workspace builds; the listed tests pass. Then run `CARGO_INCREMENTAL=0 cargo test -p stroem-common 2>&1 | grep -E '^test .* FAILED' | sort > /tmp/../task1-failures.txt` (use the session scratchpad path) and list the failing stroem-common tests in your report — they are expected to be fixed by Tasks 2–6 and 10; do not fix them here unless they are compile errors.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock crates/stroem-common
git commit -m "feat(template)!: port to Tera 2 with value-free template errors

Tera 2.4 (preserve_order) + tera-contrib json_encode. One engine for
rendering and validation; every Tera error becomes a TemplateError whose
message is a category and a position — Tera's text (which can quote
context values and template source) is kept only for raw_detail(). The
vals filter records its failure kind and stderr in a side channel.
Validation checks when/for_each/prompts through render_str and rejects
unknown filters at load."
```

---

### Task 2: Enrichment allow-lists, drift tests and the value-free corpus

**Files:**
- Modify: `crates/stroem-common/src/template_error.rs`
- Test: `template_error.rs` test module

**Interfaces:**
- Consumes: Task 1's `TemplateError::from_tera(err, src, vals)`.
- Produces: `pub const TERA1_BUILTIN_NAMES: &[&str]`, `pub(crate) const REGISTERED_FILTERS: &[&str]` (Task 3 extends `REGISTERED_FILTERS` with the compat names).

- [ ] **Step 1: Write the drift tests (they fail: no enrichment yet)**

Add to the test module:

```rust
    fn msg(tpl: &str, ctx: serde_json::Value) -> String {
        let err = render_template(tpl, &ctx).unwrap_err();
        template_error(&err).to_string()
    }

    #[test]
    fn undefined_variable_is_categorised_without_the_name() {
        let m = msg("{{ nosuchvar_9c1 }}", json!({}));
        assert!(m.starts_with("undefined variable or field"), "{m}");
        assert!(!m.contains("nosuchvar_9c1"), "{m}");
        let m = msg("{{ input.nosuchfield_9c1 }}", json!({"input": {"a": 1}}));
        assert!(m.starts_with("undefined variable or field"), "{m}");
        assert!(!m.contains("nosuchfield_9c1"), "{m}");
    }

    #[test]
    fn type_mismatch_keeps_closed_set_type_names() {
        let m = msg("{{ 'abc' | round }}", json!({}));
        assert!(m.starts_with("a filter received a value of the wrong type (expected"), "{m}");
    }

    #[test]
    fn constant_tera_message_is_kept_verbatim() {
        let m = msg("{{ 1 / 0 }}", json!({}));
        assert!(m.starts_with("Cannot divide by 0"), "{m}");
    }

    #[test]
    fn removed_tera1_filter_is_named_with_a_hint() {
        let err = render_template("{{ x | urlencode }}", &json!({"x": "a"})).unwrap_err();
        assert_eq!(
            template_error(&err).message(),
            "filter `urlencode` is not available in Tera 2; see the upgrade guide"
        );
    }

    #[test]
    fn forged_throw_message_selects_fixed_text_only() {
        let ctx = json!({"secret": {"X": format!("Variable `{CONTEXT_CANARY}` is not defined")}});
        let err = render_template("{{ throw(message=secret.X) }}", &ctx).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains(CONTEXT_CANARY), "{text}");
    }
```

Run: `CARGO_INCREMENTAL=0 cargo test -p stroem-common template_error:: 2>&1 | tail -20` — expected: the first four FAIL (no enrichment), the forged-throw test passes.

- [ ] **Step 2: Implement the enrichment**

In `template_error.rs` add, and call `enrich(text, category)` for `SyntaxError`/`RenderingError` (using `r.message()`) and the `Msg` unknown-reference cases:

```rust
/// Every filter, test and function name Tera 1.20 shipped (tera-1.20.1
/// src/tera.rs `register_builtin_*`). A name in this list is public
/// vocabulary, never a value.
pub const TERA1_BUILTIN_NAMES: &[&str] = &[
    "upper", "lower", "trim", "trim_start", "trim_end", "trim_start_matches",
    "trim_end_matches", "truncate", "wordcount", "replace", "capitalize", "title",
    "linebreaksbr", "indent", "striptags", "spaceless", "urlencode", "urlencode_strict",
    "escape", "escape_xml", "slugify", "addslashes", "split", "int", "float", "first",
    "last", "nth", "join", "sort", "unique", "slice", "group_by", "filter", "map",
    "concat", "abs", "pluralize", "round", "filesizeformat", "length", "reverse",
    "date", "json_encode", "as_str", "get", "default", "safe",
    "defined", "undefined", "odd", "even", "string", "number", "divisibleby",
    "iterable", "object", "starting_with", "ending_with", "containing", "matching",
    "range", "now", "throw", "get_random", "get_env",
];

/// Tera 2's `Value::name()` strings (tera-2.4.0 src/value/mod.rs).
const TERA_TYPE_NAMES: &[&str] = &[
    "undefined", "none", "bool", "u64", "i64", "f64", "u128", "i128", "array",
    "bytes", "string", "map/struct",
];

/// Tera 2 messages with no placeholder: safe to show verbatim.
const TERA_CONSTANT_MESSAGES: &[&str] = &[
    "Cannot divide by 0",
    "Slicing step cannot be 0",
    "Slice step is undefined",
    "Slice start is undefined",
    "Slice end is undefined",
    "Not a valid key type",
    "Tried to escape an undefined value",
    "Function `range` was called with arguments that overflow i128",
];

/// Names our engine registers as filters: Tera 2 builtins
/// (tera-2.4.0 src/tera.rs `register_builtin_filters`) + ours.
pub(crate) const REGISTERED_FILTERS: &[&str] = &[
    "safe", "default", "upper", "lower", "wordcount", "escape", "escape_html",
    "escape_xml", "newlines_to_br", "pluralize", "trim", "trim_start", "trim_end",
    "replace", "capitalize", "title", "truncate", "indent", "str", "int", "float",
    "length", "reverse", "split", "abs", "round", "first", "last", "nth", "join",
    "sort", "unique", "get", "values", "keys", "pairs", "group_by",
    "json_encode", "vals",
];

/// Value-free detail for a Tera message: fixed text or closed-set members
/// only. A filter error can forge any message shape (`throw`), so nothing
/// free-form is ever captured.
fn enrich(tera_message: &str) -> Option<String> {
    let m = tera_message.trim();
    if TERA_CONSTANT_MESSAGES.contains(&m) {
        return Some(m.to_string());
    }
    if (m.starts_with("Variable `") && (m.contains("` is not defined") || m.contains("` exists but its value is undefined")))
        || (m.starts_with("Field `") && m.contains("` is not defined"))
    {
        return Some("undefined variable or field".to_string());
    }
    if let Some(rest) = m.strip_prefix("Invalid type for the value, expected `") {
        let mut parts = rest.split('`');
        let expected = parts.next()?;
        let actual = parts.nth(1)?;
        if TERA_TYPE_NAMES.contains(&expected) && TERA_TYPE_NAMES.contains(&actual) {
            return Some(format!(
                "a filter received a value of the wrong type (expected {expected}, got {actual})"
            ));
        }
        return Some("a filter received a value of the wrong type".to_string());
    }
    None
}

/// `filter `name` is not available in Tera 2…` when an unknown-reference
/// report names a Tera 1 builtin; `None` otherwise.
fn removed_builtin_hint(msg_text: &str) -> Option<String> {
    let first = msg_text.lines().next()?.strip_prefix("error: ").unwrap_or(msg_text.lines().next()?);
    for (prefix, kind) in [("Unknown filter `", "filter"), ("Unknown test `", "test"), ("Unknown function `", "function")] {
        if let Some(rest) = first.strip_prefix(prefix) {
            let name = rest.split('`').next()?;
            if TERA1_BUILTIN_NAMES.contains(&name) {
                return Some(format!("{kind} `{name}` is not available in Tera 2; see the upgrade guide"));
            }
            return None;
        }
    }
    None
}

/// Best effort: a registered filter name that appears as `| name` inside the
/// error's span text. Can only ever return a member of REGISTERED_FILTERS.
fn failing_filter(src: Option<&str>, span: &tera::Span) -> Option<&'static str> {
    let text = src?.get(span.range.clone())?;
    let after_pipe = text.rsplit('|').next()?.trim_start();
    let ident: String = after_pipe.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    REGISTERED_FILTERS.iter().copied().find(|f| *f == ident)
}
```

Wire it into `from_tera`: for `SyntaxError(r)` / `RenderingError(r)`, `message = enrich(r.message()).unwrap_or(category)`; then, for `RenderingError` only, if `failing_filter(src, r.span())` returns `Some(f)` and the message is still the bare category, use ``format!("filter `{f}` failed")``. For `Msg(text)`, `message = removed_builtin_hint(text).unwrap_or(msg_category(text))`. `vals` still overrides last.

If, while making `type_mismatch_keeps_closed_set_type_names` pass, the observed Tera message for `'abc' | round` does not have the `Invalid type for the value, expected ` prefix, print it with a temporary `eprintln!("{}", te.raw_detail())`, adjust ONLY the prefix constant to Tera's real wording, and remove the `eprintln!`. Do the same for the undefined-variable wording. Never capture free text.

- [ ] **Step 3: Add the corpus tests**

```rust
    /// Value-bearing rendering cases: Tera's raw MESSAGE (not the report,
    /// which contains the source line) carries the context canary — the
    /// fixture is real — and our text carries nothing.
    #[test]
    fn corpus_value_bearing_cases_are_value_free() {
        let ctx = json!({"secret": {"X": CONTEXT_CANARY, "N": "0x1f-not-a-number"}});
        let cases = [
            "{{ 1 | round(method=secret.X) }}",
            "{{ {} | get(key=secret.X) }}",
            "{{ secret.X | upper | int }}",
            "{{ secret.X | float }}",
            "{{ secret.N | int(base=16) }}",
            "{{ throw(message=secret.X) }}",
        ];
        for tpl in cases {
            let tpl = format!("{tpl} {SOURCE_CANARY}");
            let err = render_template(&tpl, &ctx).unwrap_err();
            let te = template_error(&err);
            let raw = te.raw_detail();
            assert!(
                raw.contains(CONTEXT_CANARY) || raw.contains(&CONTEXT_CANARY.to_uppercase()) || raw.contains("1f-not-a-number"),
                "fixture not real for {tpl}: {raw}"
            );
            for text in [format!("{err:#}"), format!("{err:?}"), te.to_string()] {
                for needle in [CONTEXT_CANARY.to_string(), CONTEXT_CANARY.to_uppercase(), SOURCE_CANARY.to_string(), "1f-not-a-number".to_string()] {
                    assert!(!text.contains(&needle), "{tpl}: {text}");
                }
            }
        }
    }

    /// Cases with no ReportError or no context evaluation: exact fallback.
    #[test]
    fn corpus_compile_and_conversion_cases_use_fixed_text() {
        let cases: [(&str, &str); 4] = [
            ("{{ 'a' ~ }}", "template syntax error"),
            ("{{ x | srccanary91be }}", "template uses an unknown filter"),
            ("{% if x is srccanary91be %}{% endif %}", "template uses an unknown test"),
            ("{{ srccanary91be() }}", "template uses an unknown function"),
        ];
        for (tpl, expected) in cases {
            let err = render_template(&format!("{tpl} {SOURCE_CANARY}"), &json!({"x": 1})).unwrap_err();
            assert_eq!(template_error(&err).message(), expected, "{tpl}");
            let text = format!("{err:#} {err:?}");
            assert!(!text.contains("srccanary91be") && !text.contains(SOURCE_CANARY), "{tpl}: {text}");
        }
        // A non-map context is a Context conversion failure (Msg).
        let err = render_template("{{ x }}", &json!([1, 2])).unwrap_err();
        assert_eq!(template_error(&err).message(), "template could not be compiled");
    }
```

If a corpus case's raw message turns out NOT to contain the canary under Tera 2.4 (for example `get` returns none instead of failing), delete that case and say so in the task report — every remaining case must prove its fixture.

- [ ] **Step 4: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-common template_error:: 2>&1 | tail -20
git add crates/stroem-common/src/template_error.rs
git commit -m "feat(template): value-free error detail from closed sets, drift + corpus tests"
```

Expected: all `template_error::` tests pass.

---

### Task 3: Compatibility filters — C1 `default`, C3 Tera 1 ports, `now`

**Files:**
- Create: `crates/stroem-common/src/tera_compat.rs`
- Modify: `crates/stroem-common/src/tera_engine.rs` (register), `crates/stroem-common/src/template_error.rs` (`REGISTERED_FILTERS` gains the compat names), `crates/stroem-common/src/lib.rs`, `crates/stroem-common/Cargo.toml` (`chrono-tz.workspace = true` if not present)
- Test: `tera_compat.rs` test module

**Interfaces:**
- Consumes: `tera_engine::BASE` construction.
- Produces: `pub(crate) fn tera_compat::register(t: &mut tera::Tera)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/stroem-common/src/tera_compat.rs` with only the test module first:

```rust
//! Tera 1 behaviour kept under Tera 2 (spec 2026-10-06 § 3.6 C1, C3).
//! The ported filters reproduce tera 1.20.1 (MIT, Keats) output exactly;
//! they convert through serde_json so the Tera 1 logic applies unchanged.

#[cfg(test)]
mod tests {
    use crate::template::render_template;
    use serde_json::json;

    fn r(tpl: &str, ctx: serde_json::Value) -> String {
        render_template(tpl, &ctx).unwrap()
    }

    #[test]
    fn default_replaces_null_like_tera1() {
        let ctx = json!({"step": {"output": null}, "x": null});
        assert_eq!(r("{{ x | default(value='d') }}", ctx.clone()), "d");
        assert_eq!(r("{{ step.output.items | default(value='n/a') }}", ctx.clone()), "n/a");
        assert_eq!(r("{{ missing | default(value=1) }}", ctx.clone()), "1");
        assert_eq!(r("{{ '' | default(value='d', boolean=true) }}", ctx.clone()), "d");
        assert_eq!(r("{{ 'v' | default(value='d') }}", ctx), "v");
    }

    #[test]
    fn as_str_renders_like_tera1() {
        let ctx = json!({"o": {"a": 1}, "a": ["x", 2, null, 2.0], "f": 2.0});
        assert_eq!(r("{{ o | as_str }}", ctx.clone()), "[object]");
        assert_eq!(r("{{ a | as_str }}", ctx.clone()), "[x, 2, , 2]");
        assert_eq!(r("{{ f | as_str }}", ctx), "2");
    }

    #[test]
    fn trim_matches_unescape_newline_and_tab_patterns() {
        assert_eq!(r(r#"{{ "\n\nx" | trim_start_matches(pat="\\n") }}"#, json!({})), "x");
        assert_eq!(r(r#"{{ "x--" | trim_end_matches(pat="-") }}"#, json!({})), "x");
    }

    #[test]
    fn linebreaksbr_handles_crlf_and_lf_only() {
        let ctx = json!({"s": "a\r\nb\nc\rd"});
        assert_eq!(r("{{ s | linebreaksbr }}", ctx), "a<br>b<br>c\rd");
    }

    #[test]
    fn map_filter_concat_slice_like_tera1() {
        let ctx = json!({"xs": [{"n": "a", "k": 1}, {"n": "b", "k": 2}, {"k": 3}]});
        assert_eq!(r("{{ xs | map(attribute='n') | join(sep=',') }}", ctx.clone()), "a,b");
        assert_eq!(r("{{ xs | filter(attribute='k', value=2) | map(attribute='n') | join(sep=',') }}", ctx.clone()), "b");
        assert_eq!(r("{{ [1, 2] | concat(with=3) | join(sep=',') }}", json!({})), "1,2,3");
        assert_eq!(r("{{ [1, 2] | concat(with=[3, 4]) | join(sep=',') }}", json!({})), "1,2,3,4");
        assert_eq!(r("{{ [1, 2, 3, 4] | slice(start=1, end=-1) | join(sep=',') }}", json!({})), "2,3");
    }

    #[test]
    fn date_like_tera1() {
        assert_eq!(r("{{ 0 | date }}", json!({})), "1970-01-01");
        assert_eq!(r("{{ '2026-10-06T12:30:00+02:00' | date(format='%H:%M', timezone='UTC') }}", json!({})), "10:30");
        assert_eq!(r("{{ '2026-10-06' | date(format='%d/%m') }}", json!({})), "06/10");
    }

    #[test]
    fn now_returns_rfc3339_or_timestamp() {
        let s = r("{{ now(utc=true) }}", json!({}));
        assert!(chrono::DateTime::parse_from_rfc3339(&s).is_ok(), "{s}");
        let ts: i64 = r("{{ now(timestamp=true) }}", json!({})).parse().unwrap();
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn compat_errors_are_value_free() {
        let err = render_template("{{ secret.X | date }}", &json!({"secret": {"X": "not-a-date-canary"}})).unwrap_err();
        assert!(!format!("{err:#} {err:?}").contains("not-a-date-canary"));
    }
}
```

Add `pub(crate) mod tera_compat;` to `lib.rs`. Run `CARGO_INCREMENTAL=0 cargo test -p stroem-common tera_compat:: 2>&1 | tail -20` — expected: FAIL (`unknown filter`, `default` keeps null).

- [ ] **Step 2: Implement the filters**

Above the test module:

```rust
use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::fmt::Write as _;
use tera::{Kwargs, State, TeraResult, Value};

fn to_json(v: &Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

fn from_json(v: &serde_json::Value) -> Value {
    Value::from_serializable(v)
}

fn err(msg: &str) -> tera::Error {
    tera::Error::message(msg.to_string())
}

/// C1: `null` and undefined both take `value` (Tera 1 replaced null too).
fn default(val: Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let default_val = kwargs.must_get::<Value>("value")?;
    if kwargs.get::<bool>("boolean")?.unwrap_or(false) {
        return Ok(if val.is_truthy() { val } else { default_val });
    }
    Ok(if val.is_undefined() || val.is_none() { default_val } else { val })
}

fn tera1_render(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => out.push_str(s),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else if let Some(f) = n.as_f64() {
                let _ = write!(out, "{f}");
            }
        }
        serde_json::Value::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        serde_json::Value::Null => {}
        serde_json::Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                tera1_render(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(_) => out.push_str("[object]"),
    }
}

fn as_str(val: &Value, _: Kwargs, _: &State) -> Value {
    let mut s = String::new();
    tera1_render(&to_json(val), &mut s);
    Value::from(s)
}

fn unescape_pat(kwargs: &Kwargs, filter: &str) -> TeraResult<String> {
    let pat = kwargs
        .get::<&str>("pat")?
        .ok_or_else(|| err(&format!("Filter `{filter}` expected an arg called `pat`")))?;
    Ok(pat.replace("\\n", "\n").replace("\\t", "\t"))
}

fn trim_start_matches(val: &str, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    let pat = unescape_pat(&kwargs, "trim_start_matches")?;
    Ok(val.trim_start_matches(pat.as_str()).to_string())
}

fn trim_end_matches(val: &str, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    let pat = unescape_pat(&kwargs, "trim_end_matches")?;
    Ok(val.trim_end_matches(pat.as_str()).to_string())
}

fn linebreaksbr(val: &str, _: Kwargs, _: &State) -> String {
    val.replace("\r\n", "<br>").replace('\n', "<br>")
}

/// tera 1 `dotted_pointer`, without its quoted-segment syntax.
fn dotted<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(v, |cur, seg| match cur {
        serde_json::Value::Object(m) => m.get(seg),
        serde_json::Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

fn as_array(val: &Value, filter: &str) -> TeraResult<Vec<serde_json::Value>> {
    match to_json(val) {
        serde_json::Value::Array(a) => Ok(a),
        _ => Err(err(&format!("The `{filter}` filter expects an array"))),
    }
}

fn map(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let arr = as_array(val, "map")?;
    let attribute = kwargs
        .get::<&str>("attribute")?
        .ok_or_else(|| err("The `map` filter has to have an `attribute` argument"))?;
    let out: Vec<serde_json::Value> = arr
        .iter()
        .filter_map(|v| dotted(v, attribute).filter(|x| !x.is_null()).cloned())
        .collect();
    Ok(from_json(&serde_json::Value::Array(out)))
}

fn filter(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let arr = as_array(val, "filter")?;
    let key = kwargs
        .get::<&str>("attribute")?
        .ok_or_else(|| err("The `filter` filter has to have an `attribute` argument"))?;
    let wanted = kwargs.get::<Value>("value")?.map(|v| to_json(&v)).unwrap_or(serde_json::Value::Null);
    let out: Vec<serde_json::Value> = arr
        .into_iter()
        .filter(|v| {
            let got = dotted(v, key).unwrap_or(&serde_json::Value::Null);
            if wanted.is_null() { !got.is_null() } else { *got == wanted }
        })
        .collect();
    Ok(from_json(&serde_json::Value::Array(out)))
}

fn concat(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let mut arr = as_array(val, "concat")?;
    let with = kwargs
        .get::<Value>("with")?
        .ok_or_else(|| err("The `concat` filter has to have a `with` argument"))?;
    match to_json(&with) {
        serde_json::Value::Array(more) => arr.extend(more),
        other => arr.push(other),
    }
    Ok(from_json(&serde_json::Value::Array(arr)))
}

fn slice(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let arr = as_array(val, "slice")?;
    if arr.is_empty() {
        return Ok(from_json(&serde_json::Value::Array(arr)));
    }
    let index = |i: f64| if i >= 0.0 { i as usize } else { (arr.len() as f64 + i) as usize };
    let start = kwargs.get::<f64>("start")?.map(index).unwrap_or(0);
    let end = kwargs.get::<f64>("end")?.map(index).unwrap_or(arr.len()).min(arr.len());
    let out = if start >= end { Vec::new() } else { arr[start..end].to_vec() };
    Ok(from_json(&serde_json::Value::Array(out)))
}

fn date(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    use chrono::format::{Item, StrftimeItems};
    let format = kwargs.get::<&str>("format")?.unwrap_or("%Y-%m-%d").to_string();
    if StrftimeItems::new(&format).any(|i| matches!(i, Item::Error)) {
        return Err(err("Invalid date format"));
    }
    let tz: Option<Tz> = match kwargs.get::<&str>("timezone")? {
        Some(t) => Some(t.parse().map_err(|_| err("Error parsing the timezone"))?),
        None => None,
    };
    let formatted = match to_json(val) {
        serde_json::Value::Number(n) => {
            let i = n.as_i64().ok_or_else(|| err("Filter `date` was invoked on a float"))?;
            let naive = DateTime::<Utc>::from_timestamp(i, 0)
                .ok_or_else(|| err("timestamp out of range"))?
                .naive_utc();
            match tz {
                Some(tz) => tz.from_utc_datetime(&naive).format(&format).to_string(),
                None => naive.format(&format).to_string(),
            }
        }
        serde_json::Value::String(s) if s.contains('T') => match s.parse::<DateTime<FixedOffset>>() {
            Ok(d) => match tz {
                Some(tz) => d.with_timezone(&tz).format(&format).to_string(),
                None => d.format(&format).to_string(),
            },
            Err(_) => match s.parse::<NaiveDateTime>() {
                Ok(n) => DateTime::<Utc>::from_naive_utc_and_offset(n, Utc).format(&format).to_string(),
                Err(_) => return Err(tera::Error::message(format!("Error parsing `{s:?}` as rfc3339 date or naive datetime"))),
            },
        },
        serde_json::Value::String(s) => match NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
            Ok(d) => DateTime::<Utc>::from_naive_utc_and_offset(d.and_hms_opt(0, 0, 0).unwrap_or_default(), Utc)
                .format(&format)
                .to_string(),
            Err(_) => return Err(tera::Error::message(format!("Error parsing `{s:?}` as YYYY-MM-DD date"))),
        },
        _ => return Err(err("Filter `date` received an incorrect type: expected i64|u64|String")),
    };
    Ok(formatted)
}

fn now(kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let utc = kwargs.get::<bool>("utc")?.unwrap_or(false);
    let timestamp = kwargs.get::<bool>("timestamp")?.unwrap_or(false);
    Ok(match (utc, timestamp) {
        (_, true) if utc => Value::from(Utc::now().timestamp()),
        (_, true) => Value::from(Local::now().timestamp()),
        (true, false) => Value::from(Utc::now().to_rfc3339()),
        (false, false) => Value::from(Local::now().to_rfc3339()),
    })
}

/// Register C1 and C3 on the base engine.
pub(crate) fn register(t: &mut tera::Tera) {
    t.register_filter("default", default);
    t.register_filter("as_str", as_str);
    t.register_filter("trim_start_matches", trim_start_matches);
    t.register_filter("trim_end_matches", trim_end_matches);
    t.register_filter("linebreaksbr", linebreaksbr);
    t.register_filter("map", map);
    t.register_filter("filter", filter);
    t.register_filter("concat", concat);
    t.register_filter("slice", slice);
    t.register_filter("date", date);
    t.register_function("now", now);
}
```

The two `date` errors that quote the input stay: Tera's text never leaves `stroem-common` (Task 1), and `compat_errors_are_value_free` proves it. If a `kwargs.get::<&str>` call does not compile (lifetime of `ArgFromValue`), use `kwargs.get::<String>` and `.as_deref()`.

In `tera_engine.rs` `BASE`, after `json_encode`: `crate::tera_compat::register(&mut t);`

In `template_error.rs` append to `REGISTERED_FILTERS`: `"as_str", "trim_start_matches", "trim_end_matches", "linebreaksbr", "map", "filter", "concat", "slice", "date"`.

Ensure `crates/stroem-common/Cargo.toml` has `chrono-tz.workspace = true` (add if missing).

- [ ] **Step 3: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-common tera_compat:: template_error:: 2>&1 | tail -20
git add crates/stroem-common
git commit -m "feat(template): keep Tera 1 default/null, as_str, map, filter, concat, slice, date, now"
```

Expected: all pass.

---

### Task 4: `when:` falsiness (C4) and shape-only `for_each` errors

**Files:**
- Modify: `crates/stroem-common/src/template.rs` (`evaluate_condition`, was lines 362–376)
- Modify: `crates/stroem-server/src/cascade.rs:96-126` (`render_for_each_template`) and its tests near `:2584` (`test_for_each_object_rendering_suggests_json_encode`)
- Modify: `crates/stroem-cli/src/local/run.rs` `evaluate_for_each` (~line 646)
- Test: `template.rs`, `cascade.rs` test modules

**Interfaces:**
- Produces: `evaluate_condition` unchanged signature; `pub fn stroem_common::template::json_type_name(&serde_json::Value) -> &'static str` (make the existing private fn `pub`).

- [ ] **Step 1: Failing truth-table test (template.rs tests)**

```rust
    #[test]
    fn when_truth_table_follows_rendered_text() {
        let ctx = json!({"z": 0.0, "e": [], "m": {}, "s": "false", "n": null, "one": 1});
        let cases = [
            ("{{ z }}", false), ("{{ e }}", false), ("{{ m }}", false),
            ("{{ s }}", false), ("{{ n }}", false), ("", false),
            ("0", false), ("-0.0", false), ("None", false), ("NULL", false),
            ("{{ one }}", true), ("{{ [0] }}", true), ("x", true), ("0.5", true),
            ("{{ e and one }}", false), ("{{ one and e }}", false), ("{{ e or one }}", true),
        ];
        for (tpl, expected) in cases {
            assert_eq!(evaluate_condition(tpl, &ctx).unwrap(), expected, "{tpl}");
        }
    }
```

Run: `CARGO_INCREMENTAL=0 cargo test -p stroem-common when_truth_table 2>&1 | tail -8` — expected FAIL (`0.0`, `[]`, `{}` truthy today).

- [ ] **Step 2: Implement C4**

```rust
/// Evaluate a `when` condition template against a JSON context.
///
/// The rendered text is false when it is empty or, case-insensitively,
/// `false`, `null`, `none`, `[]`, `{}`, or a number equal to zero (`0`,
/// `0.0`, `-0.0`) — Strøm's convention over Tera 2's rendered output
/// (spec 2026-10-06 § 3.6 C4). Render errors propagate as `Err`.
pub fn evaluate_condition(template: &str, context: &serde_json::Value) -> Result<bool> {
    let rendered = render_template(template, context)?;
    let trimmed = rendered.trim();
    let lower = trimmed.to_lowercase();
    if matches!(lower.as_str(), "" | "false" | "null" | "none" | "[]" | "{}") {
        return Ok(false);
    }
    if trimmed.parse::<f64>().is_ok_and(|n| n == 0.0) {
        return Ok(false);
    }
    Ok(true)
}
```

Make `json_type_name` `pub` (it stays in `template.rs`).

- [ ] **Step 3: Shape-only `for_each` errors in cascade.rs**

Replace the body after `render_template` in `render_for_each_template`:

```rust
    let value: serde_json::Value = serde_json::from_str(&rendered).map_err(|e| {
        anyhow::anyhow!(
            "for_each must render a JSON array; the rendered text ({} bytes) is not valid JSON \
             ({:?} error at line {}, column {}). Render arrays and objects with `| json_encode()`, \
             e.g. {{{{ step.output.items | json_encode() }}}}",
            rendered.len(),
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    match value {
        serde_json::Value::Array(arr) => Ok(arr),
        other => bail!(
            "for_each must render a JSON array, got a JSON {}",
            stroem_common::template::json_type_name(&other)
        ),
    }
```

Replace `test_for_each_object_rendering_suggests_json_encode` (the `[object]` case cannot happen under Tera 2) with:

```rust
    #[test]
    fn for_each_errors_never_contain_rendered_content() {
        let ctx = json!({"secret": {"X": "for-each-canary"}});
        let err = render_for_each_template("{{ secret.X | upper }}", &ctx).unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("FOR-EACH-CANARY") && !text.contains("for-each-canary"), "{text}");
        assert!(text.contains("is not valid JSON"), "{text}");
        let err = render_for_each_template("{{ secret | json_encode() }}", &ctx).unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("for-each-canary"), "{text}");
        assert!(text.contains("got a JSON object"), "{text}");
    }
```

- [ ] **Step 4: Same rule in the CLI**

In `crates/stroem-cli/src/local/run.rs` `evaluate_for_each`, apply the identical two error messages (copy the `map_err` and `bail!` from Step 3; `bail!` needs `anyhow::bail`).

- [ ] **Step 5: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-common when_truth_table 2>&1 | tail -5
CARGO_INCREMENTAL=0 cargo test -p stroem-server --lib cascade:: 2>&1 | grep -E 'test result|FAILED' | head
CARGO_INCREMENTAL=0 cargo test -p stroem-cli 2>&1 | grep -E 'test result|FAILED' | head
git add -A crates/stroem-common/src/template.rs crates/stroem-server/src/cascade.rs crates/stroem-cli/src/local/run.rs
git commit -m "feat(template): when: falsiness for [] {} and numeric zero; shape-only for_each errors"
```

Expected: the new tests pass. Report any OTHER cascade test failure by name (Task 10 owns semantic fallout).

---

### Task 5: Null `state` / `global_state` (C2) and Tera keyword collisions

**Files:**
- Modify: `crates/stroem-server/src/render_context.rs:273-278` (state insertion), `:21` (constants), `:190-221` (`Collision`, `log_lines`), `:313-318` (collision check)
- Test: `render_context.rs` test module (the assertion at `:775` `get("state").is_none()` changes)

**Interfaces:**
- Produces: `pub const TERA_KEYWORDS: [&str; 15]` in `render_context.rs`.

- [ ] **Step 1: Failing tests**

Replace the existing test `secret_always_present_state_only_when_json_present` (near line 760) with:

```rust
    #[test]
    fn secret_always_present_state_null_without_json() {
        let caller = secrets(&[]);
        let sn = snaps(None, Some(json!({"g": 1})));
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &[], None, Scope::StepInput);
        assert_eq!(v.as_value()["secret"], json!({}));
        assert_eq!(v.as_value().get("state"), Some(&Value::Null));
        assert_eq!(v.as_value()["global_state"]["g"], 1);
    }

    #[test]
    fn absent_snapshot_renders_state_as_null() {
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let ctx = build(&job, &[], None, Scope::StepInput);
        let v = ctx.as_value();
        let r = |t: &str| stroem_common::template::render_template(t, v).unwrap();
        assert_eq!(r("{{ state.cursor | default(value=0) }}"), "0");
        assert_eq!(r("{{ global_state.last | default(value='never') }}"), "never");
        assert_eq!(r("{% if state.x %}a{% else %}b{% endif %}"), "b");
        assert_eq!(r("{{ not state }}"), "true");
    }

    #[test]
    fn step_named_after_a_tera_keyword_is_reported() {
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let rows = vec![row("none", "completed", Some(json!("x")))];
        let v = build(&job, &views(&rows), None, Scope::StepInput);
        let lines = v.log_lines();
        assert!(
            lines.iter().any(|l| l
                == "[render] step 'none' is a Tera keyword and cannot be referenced in templates"),
            "{lines:?}"
        );
    }
```

Run: `CARGO_INCREMENTAL=0 cargo test -p stroem-server --lib render_context:: 2>&1 | tail -15` — expected FAIL (state absent; no keyword collision).

- [ ] **Step 2: Implement**

Replace the two snapshot insertions:

```rust
    // C2 (spec 2026-10-06 § 3.6): `null`, not absent, when there is no
    // snapshot — Tera 2 errors on `state.x | default(..)` when `state` is
    // undefined but not when it is null.
    let task_state = job.snapshots.task.as_ref().and_then(|s| s.json.clone());
    upsert(&mut ctx, "state", task_state.unwrap_or(Value::Null));
    let global_state = job.snapshots.global.as_ref().and_then(|s| s.json.clone());
    upsert(&mut ctx, "global_state", global_state.unwrap_or(Value::Null));
```

Below `FRAMEWORK_KEYS`:

```rust
/// Tera 2 keywords: a step with one of these (sanitized) names cannot be
/// referenced in a template (spec 2026-10-06 § 3.8).
pub const TERA_KEYWORDS: [&str; 15] = [
    "none", "null", "self", "loop", "break", "continue", "true", "false", "and", "or",
    "not", "is", "in", "if", "else",
];
```

In the step loop, after the `FRAMEWORK_KEYS` check:

```rust
        if let Some(key) = TERA_KEYWORDS.iter().find(|k| **k == name) {
            collisions.push(Collision { step: s.step_name.to_string(), key });
        }
```

In `log_lines`:

```rust
            .map(|c| {
                if TERA_KEYWORDS.contains(&c.key) {
                    format!("[render] step '{}' is a Tera keyword and cannot be referenced in templates", c.step)
                } else {
                    format!("[render] step '{}' shadows template variable '{}'", c.step, c.key)
                }
            })
```

- [ ] **Step 3: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-server --lib render_context:: 2>&1 | grep -E 'test result|FAILED'
git add crates/stroem-server/src/render_context.rs
git commit -m "feat(render): state/global_state are null without a snapshot; report Tera keyword step names"
```

---

### Task 6: Value-free connection resolution and pre-checks (§ 3.3.2)

**Files:**
- Modify: `crates/stroem-common/src/template.rs:191-360` (`found_config`, `resolve_connection_ref`), `:610-709` (`resolve_connection_inputs_scoped`)
- Modify: `crates/stroem-common/src/validation.rs:736-776` (`check_connection_values`) and its caller `:825`, tests `:2232-2244`
- Modify: `crates/stroem-server/src/job_creator.rs:1311-1313` and `:1359`
- Test: `template.rs`, `validation.rs`, `models/workflow.rs`, `job_creator.rs` tests

**Interfaces:**
- Produces:
  - `pub enum ConnectionRefError { NotFound, UnknownWorkspace, WorkspaceUnavailable, NotShared, OfflineCrossWorkspace }` with `impl Display` (fixed sentences below) and `impl std::error::Error`.
  - `resolve_connection_ref(conn_ref: &str, scope: &ResolveScope) -> std::result::Result<ResolvedConnection<'a>, ConnectionRefError>`.
  - `pub fn looks_templated(s: &str) -> bool`.
  - `check_connection_values(label: &str, values, type_name, type_def)` — `label` is a full noun phrase such as `connection 'db'` or `input field 'db'`.

- [ ] **Step 1: Failing tests**

`template.rs` tests — fixtures `make_ws_with_connection()`, `three_workspaces()`, `schema_of(..)`, `field(..)` already exist in the module:

```rust
    #[test]
    fn rendered_connection_name_never_in_error_chain() {
        // (a) the name does not resolve
        let ws = make_ws_with_connection();
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));
        let err = resolve_connection_inputs(
            &json!({"db": "CONNCANARY"}),
            &schema,
            &SingleWorkspace { name: "local", config: &ws },
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("CONNCANARY"), "{text}");
        assert!(text.contains("Input field 'db': no connection with that name exists"), "{text}");

        // (b) resolves to a foreign-typed connection missing a required field
        let mut multi = three_workspaces();
        multi.configs.get_mut("jobs").unwrap().connection_types.insert(
            "clickhouse".to_string(),
            ConnectionTypeDef {
                properties: HashMap::from([(
                    "host".to_string(),
                    crate::models::workflow::ConnectionPropertyDef {
                        property_type: "string".into(),
                        required: true,
                        default: None,
                        secret: false,
                    },
                )]),
            },
        );
        multi.configs.get_mut("infra").unwrap().connections.insert(
            "CONNCANARY".to_string(),
            ConnectionDef {
                connection_type: Some("jobs.clickhouse".into()),
                shared: true,
                values: HashMap::new(),
            },
        );
        let err = resolve_connection_inputs(
            &json!({"ch": "infra.CONNCANARY"}),
            &schema_of("jobs.clickhouse"),
            &multi,
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("CONNCANARY"), "{text}");
        assert!(text.contains("input field 'ch' is missing required field 'host'"), "{text}");

        // (c) resolves to a connection of the wrong type
        let mut ws = make_ws_with_connection();
        ws.connection_types.insert("redis".to_string(), ConnectionTypeDef { properties: HashMap::new() });
        let prod = ws.connections["prod_db"].clone();
        ws.connections.insert("CONNCANARY".to_string(), prod);
        let mut schema = HashMap::new();
        schema.insert("cache".to_string(), field("redis", false, None));
        let err = resolve_connection_inputs(
            &json!({"cache": "CONNCANARY"}),
            &schema,
            &SingleWorkspace { name: "local", config: &ws },
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("CONNCANARY"), "{text}");
        assert!(text.contains("Input field 'cache' expects type"), "{text}");
    }

    #[test]
    fn looks_templated_recognises_all_three_markers() {
        assert!(looks_templated("{{ x }}"));
        assert!(looks_templated("{% if x %}a{% endif %}"));
        assert!(looks_templated("{# c #}a"));
        assert!(!looks_templated("plain"));
    }
```

Existing tests that assert the old wording change with this task: `test_resolve_connection_inputs_missing_connection` (asserts `err.contains("nonexistent")` and `"does not exist"` → assert `!err.contains("nonexistent")` and `err.contains("no connection with that name exists")`), `test_resolve_connection_inputs_type_mismatch` (`"is type"` → `"the connection it names is type"`), `test_resolve_foreign_typed_connection_gets_type_defaults_and_is_checked` (`"missing required field 'host'"` still matches), and the provenance test asserting `msg.contains("missing-ch")` (~line 2560): an OWNER DEFAULT is config text, but the rule is uniform — assert `msg.contains("Input field 'ch'")` instead. Search the module for other `contains(` on a connection name and convert them the same way; list them in the report.

`models/workflow.rs` tests:

```rust
    #[test]
    fn literal_secret_with_block_marker_is_not_rendered() {
        let mut cfg: WorkspaceConfig = serde_yaml::from_str("secrets:\n  PW: \"p{%ss{#x\"\n").unwrap();
        cfg.render_secrets().unwrap();
        assert_eq!(cfg.secrets["PW"], serde_json::json!("p{%ss{#x"));
    }
```

Pre-check tests: find the existing tests of `precheck_literal_connection_inputs` and `precheck_task_step_literals` (`grep -rn "precheck_literal_connection_inputs\|precheck_task_step_literals\|literal connection" crates/stroem-server/tests crates/stroem-server/src/job_creator.rs`). For EACH pre-check, copy its nearest "unknown literal connection → 400" test twice: (1) set the flow-step input to `"{% if true %}CONNCANARY{% endif %}"` and assert job creation SUCCEEDS (the value is a template, skipped); (2) keep a literal `"CONNCANARY"` and assert the error's `{:#}` does NOT contain `CONNCANARY` and DOES contain `Input field '`.

Run the new tests — expected FAIL.

- [ ] **Step 2: Typed resolver errors**

In `template.rs`:

```rust
/// Why a connection reference did not resolve. Value-free: never holds the
/// reference, which may have been rendered from a secret (spec § 3.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRefError {
    NotFound,
    UnknownWorkspace,
    WorkspaceUnavailable,
    NotShared,
    OfflineCrossWorkspace,
}

impl std::fmt::Display for ConnectionRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "no connection with that name exists",
            Self::UnknownWorkspace => "the connection names an unknown workspace",
            Self::WorkspaceUnavailable => "the workspace that owns the connection is not available",
            Self::NotShared => "the connection exists but is not shared (set `shared: true` on it)",
            Self::OfflineCrossWorkspace => {
                "cross-workspace connection references require a server (run this task through `stroem-api trigger`)"
            }
        })
    }
}

impl std::error::Error for ConnectionRefError {}

pub fn looks_templated(s: &str) -> bool {
    s.contains("{{") || s.contains("{%") || s.contains("{#")
}
```

Rewrite `resolve_connection_ref` to return `Result<ResolvedConnection<'a>, ConnectionRefError>`, mapping each existing `bail!` by meaning: lookup of `scope.value_ws` → `Unknown`→`UnknownWorkspace`, `Unavailable`→`WorkspaceUnavailable`; qualified `Some(_)` not shared → `NotShared`; qualified `None` → `NotFound`; `Unknown if offline` → `OfflineCrossWorkspace`; `Unknown` → `UnknownWorkspace`; `Unavailable` → `WorkspaceUnavailable`; fallback `Some(_)` not shared → `NotShared`; final → `NotFound`. Keep `found_config` for its other callers (its `what` strings come from config — e.g. `"connection type"` — never pass a reference into it).

In `resolve_connection_inputs_scoped`, replace the resolve call and everything that prints `conn_name` or `resolved.workspace`/`resolved.name`:

```rust
        let label = format!("Input field '{}'", field_name);
        let resolved = resolve_connection_ref(conn_name, scope)
            .map_err(|kind| anyhow::anyhow!("{label}: {kind}"))?;
        let values = match resolved.def.connection_type {
            None => resolved.def.values.clone(),
            Some(ref declared) => {
                let conn_ct = canonical_type_ref(declared, &resolved.workspace, scope.lookup)
                    .with_context(|| format!("{label}: its connection's type"))?;
                if conn_ct != field_ct {
                    bail!("{label} expects type '{field_ct}' but the connection it names is type '{conn_ct}'");
                }
                if conn_ct.workspace != resolved.workspace {
                    let type_cfg = found_config(scope.lookup, &conn_ct.workspace, "connection type")?;
                    let type_def = type_cfg
                        .connection_types
                        .get(&conn_ct.name)
                        .with_context(|| format!("connection type '{}' vanished", conn_ct))?;
                    let with_defaults = resolved.def.values_with_type_defaults(type_def);
                    let field_label = format!("input field '{}'", field_name);
                    let warnings = crate::validation::check_connection_values(
                        &field_label,
                        &with_defaults,
                        &conn_ct.to_string(),
                        type_def,
                    )?;
                    for w in warnings {
                        tracing::warn!(field = %field_name, "{}", w);
                    }
                    with_defaults
                } else {
                    resolved.def.values.clone()
                }
            }
        };
```

`canonical_type_ref(declared, …)`: `declared` is the connection's YAML `type:` (config) — safe; its errors name type refs only.

- [ ] **Step 3: `check_connection_values` takes a label**

```rust
pub fn check_connection_values(
    label: &str,
    values: &HashMap<String, serde_json::Value>,
    type_name: &str,
    type_def: &ConnectionTypeDef,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (prop_name, prop_def) in &type_def.properties {
        if prop_def.required && prop_def.default.is_none() && !values.contains_key(prop_name) {
            bail!("{label} is missing required field '{prop_name}' (type '{type_name}')");
        }
    }
    for key in values.keys() {
        if !type_def.properties.contains_key(key) {
            warnings.push(format!("{label} has field '{key}' not defined in type '{type_name}'"));
        }
    }
    for (key, value) in values {
        if value.as_str().is_some_and(str::is_empty) {
            bail!("{label} field '{key}' has an empty value");
        }
    }
    Ok(warnings)
}
```

The load-time caller (`validation.rs:825`) passes `&format!("Connection '{}'", conn_name)` so its messages are byte-identical to today (`Connection 'x' is missing …`). Update the three unit tests at `:2232-2244` to call with `"Connection 'c'"`.

- [ ] **Step 4: Pre-checks**

`job_creator.rs:1312`: `if !s.contains("{{") {` → `if !stroem_common::template::looks_templated(s) {`.
`job_creator.rs:1359`: `Some(serde_json::Value::String(s)) if s.contains("{{") => {}` → `Some(serde_json::Value::String(s)) if stroem_common::template::looks_templated(s) => {}`.

Both pre-checks route their resolution errors through `resolve_connection_inputs_scoped` / `resolve_task_input_by_provenance_roles`, so their 400 text is now the field-labelled sentence. Search the pre-check functions for any additional `format!` that interpolates the value (`conn`, `name`, `s`) and replace it with the field name.

Do NOT change `models/workflow.rs:1160`, `web/worker_api/rendering.rs:287`, `template.rs:550` (`merge_defaults`) or `:776` (`render_value_deep`) — they keep `{{`-only detection (spec § 3.3.2).

- [ ] **Step 5: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-common rendered_connection_name looks_templated literal_secret_with_block validation::tests::check_connection 2>&1 | grep -E 'test result|FAILED|panicked'
CARGO_INCREMENTAL=0 cargo test -p stroem-common 2>&1 | grep -E '^test .*(connection|resolve).* FAILED'
git add -A crates/stroem-common crates/stroem-server/src/job_creator.rs
git commit -m "fix(connections): value-free resolver errors; pre-checks skip {% %} and {# #} templates"
```

Fix any existing resolver test that asserted the old `connection 'x' …` wording by asserting the new field-labelled sentence instead (list them in the report).

---

### Task 7: Scrub hook-input and event-source env render failures

**Files:**
- Modify: `crates/stroem-server/src/settlement/hooks.rs:305-336` (the `fire_single_hook` error branch)
- Modify: `crates/stroem-server/src/event_source.rs:419-429`
- Test: `hooks.rs` and `event_source.rs` test modules

**Interfaces:**
- Consumes: `crate::workspace_set::{collect_config_secret_values, redact_secrets_in_str}`.

- [ ] **Step 1: Failing tests**

`hooks.rs` tests:

```rust
    #[test]
    fn hook_error_text_is_scrubbed_with_workspace_secrets() {
        let mut cfg = stroem_common::models::workflow::WorkspaceConfig::new();
        cfg.secrets.insert("T".into(), serde_json::json!("hook-raw-canary"));
        let text = super::scrub_hook_error("failed: hook-raw-canary", &cfg);
        assert!(!text.contains("hook-raw-canary"), "{text}");
    }
```

`event_source.rs` tests:

```rust
    #[test]
    fn env_error_text_is_scrubbed_with_workspace_secrets() {
        let mut cfg = stroem_common::models::workflow::WorkspaceConfig::new();
        cfg.secrets.insert("T".into(), serde_json::json!("env-raw-canary"));
        let text = super::scrub_env_error("failed: env-raw-canary", &cfg);
        assert!(!text.contains("env-raw-canary"), "{text}");
    }
```

Run both — expected: compile error (helpers missing).

- [ ] **Step 2: Implement**

`hooks.rs`:

```rust
/// Defence in depth (spec § 3.4): template errors are value-free, but the
/// chain also carries our own contexts; scrub with the workspace's secrets.
fn scrub_hook_error(text: &str, cfg: &WorkspaceConfig) -> String {
    crate::workspace_set::redact_secrets_in_str(
        text,
        &crate::workspace_set::collect_config_secret_values(cfg),
    )
}
```

and in the error branch format once, scrub, then use the scrubbed text in BOTH the `tracing::error!` and the `server_log` line:

```rust
        {
            let detail = scrub_hook_error(&format!("{e:#}"), workspace_config);
            tracing::error!("Failed to fire hook {}[{}] for job {}: {}", hook_type, i, job.job_id, detail);
            s.server_log(
                job.job_id,
                &format!("[hooks] Failed to fire hook {}[{}] for action '{}': {}", hook_type, i, hook.action, detail),
            )
            .await;
        }
```

`event_source.rs`: add `fn scrub_env_error(text: &str, cfg: &WorkspaceConfig) -> String` with the same body and use `scrub_env_error(&format!("{e:#}"), config)` in the `tracing::warn!` (pass whichever `WorkspaceConfig` the surrounding loop holds for `ws_name`).

- [ ] **Step 3: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-server --lib settlement::hooks event_source 2>&1 | grep -E 'test result|FAILED'
git add crates/stroem-server/src/settlement/hooks.rs crates/stroem-server/src/event_source.rs
git commit -m "fix(hooks,event-source): scrub render failures before logging"
```

---

### Task 8: CLI raw detail and the `raw_detail` guard

**Files:**
- Create: `crates/stroem-cli/src/local/error_report.rs`
- Modify: `crates/stroem-cli/src/local/mod.rs` (module), `run.rs` (error print sites at ~270, ~283, ~354, ~422, ~429), `validate.rs:67`
- Create: `crates/stroem-common/tests/raw_detail_guard.rs`

**Interfaces:**
- Produces: `pub fn error_report::full_report(err: &anyhow::Error) -> String`.

- [ ] **Step 1: Failing tests**

`error_report.rs`:

```rust
//! The operator's view of an error: our value-free chain plus Tera's full
//! report (the operator holds every secret, spec § 3.2.3).

use stroem_common::template_error::TemplateError;

pub fn full_report(err: &anyhow::Error) -> String {
    let mut out = format!("{err:#}");
    for cause in err.chain() {
        if let Some(te) = cause.downcast_ref::<TemplateError>() {
            out.push_str("\n  Tera detail:\n");
            for line in te.raw_detail().lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn full_report_appends_tera_detail() {
        let err = stroem_common::template::render_template("{{ nosuchvar_cli }}", &serde_json::json!({})).unwrap_err();
        let r = super::full_report(&err);
        assert!(r.contains("undefined variable or field"), "{r}");
        assert!(r.contains("nosuchvar_cli"), "{r}");
    }
}
```

`crates/stroem-common/tests/raw_detail_guard.rs`:

```rust
//! Only stroem-cli may call TemplateError::raw_detail (spec § 3.2.3).

use std::path::Path;

fn visit(dir: &Path, hits: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            visit(&p, hits);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            if text.contains("raw_detail(") {
                hits.push(p.display().to_string());
            }
        }
    }
}

#[test]
fn only_the_cli_and_template_error_mention_raw_detail() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut hits = Vec::new();
    visit(crates, &mut hits);
    let allowed = |p: &String| {
        p.contains("/stroem-cli/") || p.ends_with("stroem-common/src/template_error.rs")
            || p.ends_with("stroem-common/tests/raw_detail_guard.rs")
    };
    let offenders: Vec<_> = hits.iter().filter(|p| !allowed(p)).collect();
    assert!(offenders.is_empty(), "raw_detail used outside stroem-cli: {offenders:?}");
}

#[test]
fn template_rs_contexts_never_interpolate_template_text() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/template.rs"),
    )
    .unwrap();
    for (i, line) in src.lines().enumerate() {
        let l = line.trim();
        if (l.contains("context(") || l.contains("with_context(")) && l.contains("format!") {
            for var in ["{}\", s", "{s}", "template)", "{template}", "src)", "{src}"] {
                assert!(!l.contains(var), "template.rs:{}: context interpolates template text: {l}", i + 1);
            }
        }
    }
}
```

Run both — expected: `full_report` test fails to compile until the module exists; the guard passes only if no other crate calls `raw_detail` (Task 2's tests in `template_error.rs` are allowed).

- [ ] **Step 2: Wire the CLI**

Add `pub mod error_report;` to `crates/stroem-cli/src/local/mod.rs`. In `run.rs`, at every site that formats a step or template error with `{:#}` for the operator (`when condition error`, `for_each expression error`, iteration error, `Step '{}' error`, and the stored failure message at ~429), replace `format!("…{:#}", e)` with `format!("…{}", crate::local::error_report::full_report(&e))`. In `validate.rs:67` replace `eprintln!("[FAIL] {:#}", e);` with `eprintln!("[FAIL] {}", crate::local::error_report::full_report(&e));`.

- [ ] **Step 3: Run and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p stroem-cli error_report 2>&1 | tail -5
CARGO_INCREMENTAL=0 cargo test -p stroem-common --test raw_detail_guard 2>&1 | tail -5
git add crates/stroem-cli crates/stroem-common/tests/raw_detail_guard.rs
git commit -m "feat(cli): show Tera's full report locally; guard raw_detail to the CLI"
```

---

### Task 9: Re-fixture the security tests and add the load-time test

**Files:**
- Modify: `crates/stroem-server/tests/integration_test.rs` (fixtures at ~3308, ~3344, ~3366, ~3418, ~4670 doc, ~30299–30339; the PREFIX span-union fixture at ~3456–3462; hook `map(attribute=)` tests at ~32118, ~32679 — they must pass via Task 3, verify only)
- Modify: `crates/stroem-server/tests/git_refs_claim_test.rs` (~988–1085)
- Modify: unit tests in `crates/stroem-server/src/cascade.rs` (~1296–1303), `web/worker_api/rendering.rs` (~1564), `workspace_set.rs` (~568–580), `crates/stroem-common/src/template.rs` (~2543, ~4045)
- Create: `crates/stroem-server/tests/common/tera_fixtures.rs` (shared constants) — include it the way `tests/common/minio.rs` is included (`#[path = "common/tera_fixtures.rs"] mod tera_fixtures;`)
- Test: same files + a new load-time test in `crates/stroem-server/src/workspace/mod.rs` tests

**Interfaces:**
- Produces (test-only): `tera_fixtures::{QUOTING_ROUND_METHOD, QUOTING_UPPER_INT, raw_message_contains}`.

- [ ] **Step 1: Shared fixtures**

`crates/stroem-server/tests/common/tera_fixtures.rs`:

```rust
//! Templates whose RAW Tera error demonstrably carries the secret
//! (spec 2026-10-06 § 3.5): proven by `fixtures_are_real` below, so a test
//! asserting the secret is absent from our output is never vacuous.

#![allow(dead_code)]

/// Raw: `round(method=X)` quotes X verbatim.
pub fn quoting_round_method(secret_path: &str) -> String {
    format!("{{{{ 1 | round(method={secret_path}) }}}}")
}

/// Transformed: `upper` then `int` quotes the UPPER-CASED value.
pub fn quoting_upper_int(secret_path: &str) -> String {
    format!("{{{{ {secret_path} | upper | int }}}}")
}

/// True when rendering `tpl` against `ctx` fails and Tera's raw detail
/// contains `needle`.
pub fn raw_detail_contains(tpl: &str, ctx: &serde_json::Value, needle: &str) -> bool {
    let err = stroem_common::template::render_template(tpl, ctx).unwrap_err();
    err.chain()
        .find_map(|c| c.downcast_ref::<stroem_common::template_error::TemplateError>())
        .is_some_and(|te| te.raw_detail().contains(needle))
}
```

NOTE: `raw_detail_contains` calls `raw_detail()` from a server TEST file. Extend Task 8's guard `allowed` closure to also accept `p.ends_with("stroem-server/tests/common/tera_fixtures.rs")` — test code, never shipped — and say so in the commit.

- [ ] **Step 2: Re-fixture each test**

For each listed test: replace the Tera 1 fixture (`{{ secret.X | round }}` / `{{ secret.X | json_encode | round }}`) with `quoting_round_method("secret.X")` (raw case) or, where the test is about a value no exact scrub matches (`TOKEN_CHAIN` tests and the git-refs owner-default/body cases), `quoting_upper_int("secret.X")`. Before the existing absence assertion, add:

```rust
    assert!(tera_fixtures::raw_detail_contains(&TEMPLATE, &json!({"secret": {"X": SECRET}}), &EXPECTED),
            "fixture must leak through Tera's raw text, else this test is vacuous");
```

with `EXPECTED` = the secret (raw case) or `SECRET.to_uppercase()` (transformed case). Keep each test's existing assertion that the persisted error / 422 body / job log does NOT contain the secret (and, for `upper_int`, add the same for the upper-cased form). `test_xws_task_secret_scrub_same_workspace_still_masks` (~4767) asserted a `••••••` mask is PRESENT: Tera text no longer reaches the scrub, so change it to assert the secret is ABSENT and rename it `test_xws_task_secret_never_in_same_workspace_error`.

Unit tests inside `src/` cannot include `tests/common`; for `cascade.rs`, `rendering.rs`, `workspace_set.rs` and `template.rs` inline the same template strings and a local copy of `raw_detail_contains` gated `#[cfg(test)]`, and add those four paths to the Task 8 guard's `allowed` list with the comment "test modules".

The span-union PREFIX/crossing tests in `workspace_set.rs` (~568–580) test `redact_secrets_in_str` itself: replace their Tera-rendered input with a synthetic string that contains the crossing occurrences directly (the scrub function is unchanged).

- [ ] **Step 3: Load-time test (§ 3.4.1)**

In `crates/stroem-server/src/workspace/mod.rs` tests (copy the setup of the nearest test that asserts on `WorkspaceInfo.error` — search `error_field` / `load_errors` in that module): a folder workspace with

```yaml
secrets:
  PW: "load-canary-pw"
connections:
  db:
    host: "{{ secret.PW | upper | int }}"
```

Assert: the raw Tera detail contains `LOAD-CANARY-PW` (use a local `raw_detail_contains` copy on the same template and context `{"secret": {"PW": "load-canary-pw"}}`), and `WorkspaceInfo.error` (as the API returns it) contains neither `load-canary-pw` nor `LOAD-CANARY-PW`, and contains `Failed to render connection 'db' field 'host'`.

- [ ] **Step 4: Run and commit**

```bash
df -g /Users/ala | tail -1
CARGO_INCREMENTAL=0 cargo test -p stroem-server --test integration_test --test git_refs_claim_test 2>&1 | grep -E 'test result|FAILED' | head
CARGO_INCREMENTAL=0 cargo test -p stroem-server --lib 2>&1 | grep -E 'test result|FAILED' | head
CARGO_INCREMENTAL=0 cargo test -p stroem-common 2>&1 | grep -E 'test result|FAILED' | head
git add -A crates
git commit -m "test(security): re-fixture scrub/withhold tests on Tera 2 quoting paths; load-time test"
```

---

### Task 10: Full suite, remaining Tera 2 fallout, fixtures

**Files:** whatever the failures point at (tests asserting Tera 1 output: float rendering `2` vs `2.0`, `[object]`, array rendering, error wording).

- [ ] **Step 1: Full run**

```bash
df -g /Users/ala | tail -1
CARGO_INCREMENTAL=0 cargo fmt --all
CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E '^(error|warning)' -A6 | head -40
CARGO_INCREMENTAL=0 cargo test --workspace --no-fail-fast > "$SCRATCH/tera-full.log" 2>&1; grep -E '^test .* FAILED' "$SCRATCH/tera-full.log"
```

(`$SCRATCH` = (working notes, not committed).)

- [ ] **Step 2: Classify and fix each failure**

For each failing test decide: (a) it asserted Tera 1 OUTPUT that the spec lists as a documented break (§ 3.9: float `.0`, object/array rendering, and/or operands, undefined intermediate segments, error wording) → update the expectation to Tera 2's output and add a one-line comment `// Tera 2: <which § 3.9 item>`; (b) anything else → it is a bug in Tasks 1–9: fix the code, not the test. List every (a) change in the report with its § 3.9 item.

- [ ] **Step 3: Fixtures**

```bash
CARGO_INCREMENTAL=0 cargo run -q -p stroem-cli --bin stroem -- validate --path workspace
CARGO_INCREMENTAL=0 cargo run -q -p stroem-cli --bin stroem -- validate --path tests/e2e-workspace 2>/dev/null || ls tests
CARGO_INCREMENTAL=0 cargo run -q -p stroem-cli --bin stroem -- validate --path /Users/ala/workspace/allunite/jobs-playground
```

(Use the actual `stroem validate` flag syntax — check `stroem validate --help`.) Every one must print `[OK]`. A failure caused by a Tera 1 construct in a repo fixture: rewrite the fixture to Tera 2 syntax. A failure in jobs-playground: do NOT edit it — report the exact construct.

- [ ] **Step 4: Commit**

```bash
git add -A crates tests workspace
git commit -m "test: align expectations with Tera 2 output (documented breaks)"
```

---

### Task 11: Documentation

**Files:**
- Create: `docs/src/content/docs/operations/upgrade-tera-2.md`
- Modify: `docs/src/content/docs/guides/templating.md`, `conditionals.md`, `task-state.md`, `loops.md`, `secrets.md`, `action-types.md`, `workflow-basics.md`, `reference/workflow-yaml.md` (template references), `CLAUDE.md` (§ Secrets in logs, § Tera Templating), `docs/internal/TODO.md`; regenerate `docs/public/llms.txt`

- [ ] **Step 1: Upgrade guide**

Write `upgrade-tera-2.md` (Starlight frontmatter `title: Upgrading to Tera 2`, `description:`) with one section per spec § 3.9 item 1–13, each: what changed, a before/after YAML snippet, and the fix. Include C1–C4 as "kept from Tera 1" and the rollout note (§ 3.10: all server replicas together; templates render on the server and in the CLI, never on workers).

- [ ] **Step 2: Guides**

- `templating.md`: Tera links → Tera 2 docs; note C1 (`default` replaces `null`), C2 (`state`/`global_state` are `null` before the first snapshot), reserved step names, value-free errors + `stroem run` for full detail, link the upgrade guide.
- `conditionals.md`: the C4 truth table (copy the cases from Task 4's test); the error section: comparisons against a missing field are `false`; an undefined operand of `and`/`or` errors.
- `task-state.md`: remove the non-existent `| bool` filter usage; `| int` errors on unparsable input.
- `loops.md`: keep recommending `| json_encode()`; new error wording.
- `secrets.md`: replace "Tera quotes the offending value" text with "template errors never contain values"; the `{{ obj }}` connection-rendering warning (§ 3.5 accepted item).
- `action-types.md`: fix the invalid f-string example (~line 292) — render it with valid Tera 2 syntax.
- `workflow-basics.md` (~390): `stroem validate` now rejects unknown filters/tests/functions in `when`, `for_each` and agent prompts.

- [ ] **Step 3: CLAUDE.md and TODO.md**

CLAUDE.md § Secrets in logs: replace the sentences built on "Tera quotes the offending value in filter/type errors" with: template errors are value-free by construction (`stroem_common::template_error::TemplateError`; category + position + closed-set names; Tera's text only via `raw_detail()`, CLI-only, guarded by `stroem-common/tests/raw_detail_guard.rs`); post-render errors name the field, never the rendered value (`ConnectionRefError`); the render-path table rule (spec § 3.4). Keep the scrub and origin-withholding text (defence in depth). § Tera Templating: Tera 2, `tera_engine.rs` / `tera_compat.rs` / `template_error.rs`, C1–C4, `TERA_KEYWORDS`.

TODO.md: add under Security: "`vals` deadline is classified secret-class by the PinStore (`pins.rs` `is_vals_failure` before the deadline check sees a `ValsFailureKind::TimedOut`) — CLAUDE.md says a deadline is never secret-class; decide and align (spec 2026-10-06 § 3.7)". Mark nothing done that is not.

- [ ] **Step 4: Build docs and commit**

```bash
cd docs && bun run build 2>&1 | grep -E 'error|Complete|page\(s\)'; cd ..
git add docs CLAUDE.md
git commit -m "docs: Tera 2 upgrade guide, value-free template errors, guide updates"
```

Expected: the docs build passes; `llms.txt` regenerated.
