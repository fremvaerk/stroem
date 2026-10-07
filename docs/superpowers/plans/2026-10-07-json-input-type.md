# `json` Input Type Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `type: json` task/action input field that holds any JSON value and keeps it structured through templates — a string that is exactly one `{{ expression }}` takes the expression's native value.

**Architecture:** A new `stroem-common` module, `json_field`, owns the rule (classify a string, evaluate a single expression through a `set`/`json_encode` wrapper, map error positions back, walk objects/arrays). `template::render_input_typed` and the two default mergers call it for fields typed `json`; every other field renders exactly as today. The server applies it at claim (schema from the step's persisted `action_spec`), task dispatch, hooks and job creation; the CLI at `stroem run`. Re-run gains an explicit `replay_fields` list; the UI gets a three-mode JSON editor.

**Tech Stack:** Rust (tokio, axum, sqlx, Tera 2.4, serde_json), React 19 + TypeScript + Vite + vitest + Playwright, bun.

**Spec:** `docs/superpowers/specs/2026-10-06-json-input-type-design.md` (revision 7). Read it with this plan; section numbers below (§ n) and decisions (Dn) refer to it.

## Global Constraints

- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` and `DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock` exported. Never set `CARGO_TARGET_DIR` (all checkouts share `/Users/ala/.tmp/cargo`). At most 2 parallel build agents.
- Commit messages: no `Co-Authored-By` line, no AI attribution (user's global rule).
- Error messages are VALUE-FREE: they may name schema keys (input field names), workspace/task/action/secret NAMES and JSON type names; never a value, never template text, never a request-supplied field name that did not match a schema key (CLAUDE.md § Secrets in logs, spec § 8).
- A NEW integration test file is a `mod` line in its crate's `tests/main.rs`; never a new test target (`stroem-common/tests/test_targets_guard.rs`).
- DB tests get Postgres only through `stroem_test_support::{test_db, test_pool}`; never call `testcontainers` directly.
- The type name is exactly `json` (D1). The wrapper is exactly `{% set __stroem_v = ` + inner + ` %}{% if __stroem_v is undefined %}{{ __stroem_v }}{% endif %}{{ __stroem_v | json_encode() }}` (spec § 4.2).
- Redaction is NOT changed (D7, revision 7): no numeric collection, no number masking.
- `replay_fields` errors: `UnknownField` → `replay_fields names a field the task does not declare` (names no field); `AlsoInInput` → `field '{f}' is both in input and in replay_fields`; `MissingRequired` → `the source job has no value for required field '{f}'`; no source → `replay_fields requires source_job_id` (spec § 7).
- Fixed claim failure for an unreadable persisted schema: `the step's persisted action definition has an unreadable input schema` (spec § 6).

## Review Focus

1. A `json` value written as a YAML block scalar (`cfg: |` then `{{ x }}` on the next line) carries a trailing newline — it must classify as a single expression, not Mixed. (Task 2 test `yaml_block_scalar_trailing_newline_is_single`.)
2. `{{ typo }}` and `{{ obj.missing }}` in a `json` field must FAIL like in any other field (both are undefined in Tera 2, spec revision 8), while `{{ obj.missing | default(value=none) }}` is `null`. (Task 2 tests `undefined_top_level_variable_is_an_error_not_null` and `single_expressions_keep_native_values`.)
3. Re-run in the UI of a `json` field whose source value happens to equal the task default must REPLAY the source value (value mode, sent), never silently fall back to the default. (Task 10 test `re-run with a source value equal to the default still sends it`.)
4. A `for_each` instance passing `{{ each.item }}` (an object) and `{{ each.index }}` into `json` fields must get an object and a number in every instance. (Task 5 test `for_each_instances_get_native_item_and_index`.)
5. An action whose `json` field is retyped to a connection type after job creation must not make claim try to resolve the native value as a connection name. (Task 5 test `claim_ignores_a_live_retype_to_a_connection_type`.)

---

### Task 1: Register the `json` type and its field-option rules

**Files:**
- Modify: `crates/stroem-common/src/template.rs:610-613` (`PRIMITIVE_TYPES`)
- Modify: `crates/stroem-common/src/validation.rs:759-772` (`validate_connections` reserved names), `:868-884` (`check_input_field_options`), `:956-960` (`validate_connection_inputs` primitives), `:1812` (`validate_approval_action`)
- Modify: `crates/stroem-common/src/models/workflow.rs:88-91` (doc comment)
- Modify: `ui/src/components/task/constants.ts:6-15`
- Test: `crates/stroem-common/src/validation.rs` (`mod tests`)

**Interfaces:**
- Produces: `stroem_common::template::JSON_TYPE: &str = "json"`, `PRIMITIVE_TYPES` (now includes `"json"`), `RESERVED_TYPE_NAMES: &[&str]`.

- [ ] **Step 1: Write the failing tests** — append to `mod tests` in `crates/stroem-common/src/validation.rs`:

```rust
    // --- json input type (spec 2026-10-06-json-input-type § 5) ---

    fn json_cfg(input_yaml: &str) -> WorkspaceConfig {
        serde_yaml::from_str(&format!(
            "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n{input_yaml}\n\
             tasks:\n  t:\n    flow:\n      s: {{ action: a }}\n"
        ))
        .unwrap()
    }

    #[test]
    fn json_input_type_is_accepted() {
        let cfg = json_cfg("      cfg: { type: json, default: { a: 1, b: [x, y] } }");
        validate_workflow_config(&cfg).expect("json is a valid input type");
    }

    #[test]
    fn json_input_rejects_secret_options_allow_custom_and_multiple() {
        for (opt, word) in [
            ("secret: true", "secret"),
            ("options: [a, b]", "options"),
            ("allow_custom: true", "allow_custom"),
            ("multiple: true", "multiple"),
        ] {
            let cfg = json_cfg(&format!("      cfg: {{ type: json, {opt} }}"));
            let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
            assert!(
                err.contains("json") && err.contains(word),
                "{opt}: {err}"
            );
        }
    }

    #[test]
    fn connection_type_named_json_is_rejected() {
        let cfg: WorkspaceConfig =
            serde_yaml::from_str("connection_types:\n  json:\n    host:\n      type: string\n")
                .unwrap();
        let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn approval_action_rejects_json_input() {
        let cfg: WorkspaceConfig = serde_yaml::from_str(
            "actions:\n  gate:\n    type: approval\n    message: \"ok?\"\n    input:\n      \
             reason: { type: json }\n",
        )
        .unwrap();
        let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
        assert!(err.contains("approval") && err.contains("json"), "{err}");
    }

    #[test]
    fn reserved_type_names_cover_every_primitive() {
        for t in crate::template::PRIMITIVE_TYPES {
            assert!(crate::template::RESERVED_TYPE_NAMES.contains(t), "{t}");
        }
        for t in ["bool", "array", "object"] {
            assert!(crate::template::RESERVED_TYPE_NAMES.contains(&t), "{t}");
        }
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-common --lib validation::tests::json_input validation::tests::connection_type_named_json validation::tests::approval_action_rejects_json validation::tests::reserved_type_names`
Expected: FAIL — `json_input_type_is_accepted` errors with "references unknown type 'json'", `RESERVED_TYPE_NAMES` not found.

- [ ] **Step 3: Implement**

In `crates/stroem-common/src/template.rs`, replace the `PRIMITIVE_TYPES` block (lines 610-613) with:

```rust
/// The `json` input type (spec 2026-10-06-json-input-type): any JSON value; a
/// string that is exactly one `{{ expression }}` takes the expression's value.
pub const JSON_TYPE: &str = "json";

/// Primitive type names that are NOT connection type references.
pub const PRIMITIVE_TYPES: &[&str] = &[
    "string", "text", "integer", "number", "boolean", "date", "datetime", JSON_TYPE,
];

/// Names a connection type may not take: every primitive, the `bool` alias
/// (`canonicalize_field_type`) and the agent output-schema types.
pub const RESERVED_TYPE_NAMES: &[&str] = &[
    "string", "text", "integer", "number", "boolean", "date", "datetime", JSON_TYPE, "bool",
    "array", "object",
];
```

In `crates/stroem-common/src/validation.rs` `validate_connections`, delete the local `reserved_type_names` array (lines 764-772, keep its comment) and use the const:

```rust
    // Connection-type names that would collide with a primitive input-field
    // type (or one of its aliases — see canonicalize_field_type). A task
    // input written as `type: <name>` is resolved as a primitive first; if a
    // connection type shadows that name, the reference would silently miss
    // the connection registry.
    let reserved_type_names = crate::template::RESERVED_TYPE_NAMES;
```

(`valid_prop_types` at `:761-763` stays as it is — connection-type PROPERTY types, spec § 5.2.)

In `validate_connection_inputs`, replace the local `primitives` array (`:958-960`) with:

```rust
    let primitives = crate::template::PRIMITIVE_TYPES;
```

and change the inner `fn check(...)` parameter `primitives: &[&str]` — it already takes a slice, so the call site passes `primitives` unchanged.

In `check_input_field_options`, insert at the TOP of the function body (before the `if field.multiple` block, so a `json` field gets the json-specific message):

```rust
    if field.field_type == crate::template::JSON_TYPE {
        if field.secret {
            bail!("{context}: secret is not supported on json fields");
        }
        if field.multiple {
            bail!("{context}: multiple is not supported on json fields");
        }
        if field.options.is_some() {
            bail!("{context}: options are not supported on json fields");
        }
        if field.allow_custom {
            bail!("{context}: allow_custom is not supported on json fields");
        }
    }
```

In `validate_approval_action`, after the `message` check (first `if` in the function), add:

```rust
    // Approver forms render their own field types (ui approval-card.tsx); a
    // JSON editor there is a follow-up (spec D5).
    if let Some((field_name, _)) = action
        .input
        .iter()
        .find(|(_, f)| f.field_type == crate::template::JSON_TYPE)
    {
        bail!(
            "Action '{}' is type 'approval' but input '{}' is type json (not supported in approval forms)",
            action_name,
            field_name
        );
    }
```

In `crates/stroem-common/src/models/workflow.rs:88-91`, change the doc comment of `field_type` to:

```rust
    /// Canonical type names: `string`, `text`, `integer`, `number`, `boolean`,
    /// `date`, `datetime`, `json` (any JSON value), or a connection type name.
    /// Element type — value is an array when `multiple: true`. Aliases accepted
    /// on input: `bool` → `boolean` (see [`canonicalize_field_type`]).
```

In `ui/src/components/task/constants.ts`, add `"json",` after `"datetime",` in `PRIMITIVE_TYPES`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p stroem-common --lib validation::`
Expected: PASS (all validation tests, old and new).

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/template.rs crates/stroem-common/src/validation.rs crates/stroem-common/src/models/workflow.rs ui/src/components/task/constants.ts
git commit -m "feat(common): register the json input type and its field-option rules"
```

---

### Task 2: The json rule (`json_field` module) and `TemplateError` positions

**Files:**
- Create: `crates/stroem-common/src/json_field.rs`
- Modify: `crates/stroem-common/src/lib.rs` (add `pub mod json_field;`)
- Modify: `crates/stroem-common/src/template_error.rs` (add `position`, `with_position`)
- Test: in-module tests of both files

**Interfaces:**
- Consumes: `crate::template::{looks_templated, render_template}` (existing).
- Produces:
  - `pub fn json_field::render_json_value(value: &serde_json::Value, field: &str, context: &serde_json::Value) -> anyhow::Result<serde_json::Value>`
  - `pub(crate) fn json_field::render_json_value_with(value: &Value, field: &str, render: &dyn Fn(&str) -> anyhow::Result<String>) -> anyhow::Result<Value>`
  - `pub(crate) enum json_field::Classified<'a> { Literal, Single(SingleExpr<'a>), Mixed }`, `pub(crate) fn json_field::classify(s: &str) -> Classified<'_>`
  - `pub fn TemplateError::position(&self) -> Option<(usize, usize)>`, `pub(crate) fn TemplateError::with_position(self, pos: Option<(usize, usize)>) -> Self`

- [ ] **Step 1: Write the failing `TemplateError` test** — append to `mod tests` in `crates/stroem-common/src/template_error.rs`:

```rust
    #[test]
    fn with_position_replaces_only_the_position() {
        let err = crate::template::render_template("{{ x | nope }}", &serde_json::json!({}))
            .unwrap_err();
        let te = err
            .downcast::<TemplateError>()
            .expect("render errors carry a TemplateError under their context");
        let message = te.message().to_string();
        assert!(te.position().is_some(), "{te}");
        let moved = te.with_position(Some((7, 9)));
        assert_eq!(moved.position(), Some((7, 9)));
        assert_eq!(moved.message(), message);
        assert_eq!(moved.with_position(None).to_string(), message);
    }
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p stroem-common --lib template_error::tests::with_position_replaces_only_the_position`
Expected: FAIL to compile — no method `position` / `with_position`.

- [ ] **Step 3: Implement the two methods** — in `impl TemplateError`, after `pub fn message(&self)`:

```rust
    /// 1-based `(line, column)` of the error, when Tera reported one.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.line.zip(self.column)
    }

    /// The same error at another position (`None` drops it). The category,
    /// `vals` failure and raw detail are unchanged, so the message stays
    /// value-free (spec 2026-10-06-json-input-type § 4.2 step 3).
    pub(crate) fn with_position(mut self, pos: Option<(usize, usize)>) -> Self {
        self.line = pos.map(|p| p.0);
        self.column = pos.map(|p| p.1);
        self
    }
```

- [ ] **Step 4: Run it**

Run: `cargo test -p stroem-common --lib template_error::tests::with_position_replaces_only_the_position`
Expected: PASS.

- [ ] **Step 5: Write the `json_field` module with its tests** — create `crates/stroem-common/src/json_field.rs`:

```rust
//! The `json` input field rule (spec 2026-10-06-json-input-type § 4).
//!
//! Every string inside a `json` field's value is literal text (no Tera
//! delimiters), a single `{{ expression }}` — evaluated to its native JSON
//! value — or anything else, which is an error. Object keys are never
//! rendered, and a rendered value is never rendered again (R26).

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::template_error::TemplateError;

/// The wrapper around a single expression (spec § 4.2 step 1). `set` parses
/// exactly the expressions `{{ }}` parses (tera parser.rs:1529 / :1816); the
/// `is undefined` branch writes an undefined value out so that `{{ typo }}`
/// raises "Tried to render a variable that is undefined", as in any field.
const PREFIX: &str = "{% set __stroem_v = ";
const SUFFIX: &str = " %}{% if __stroem_v is undefined %}{{ __stroem_v }}{% endif %}{{ __stroem_v | json_encode() }}";

const DELIMITERS: [&str; 6] = ["{{", "}}", "{%", "%}", "{#", "#}"];

const NOT_JSON: &str = "the expression's value could not be converted to JSON";

/// What one string inside a `json` value is (spec § 4.1).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Classified<'a> {
    Literal,
    Single(SingleExpr<'a>),
    Mixed,
}

/// A string that is exactly one `{{ expression }}`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SingleExpr<'a> {
    /// The text between the delimiters (and whitespace-control `-`), verbatim.
    pub inner: &'a str,
    /// 1-based `(line, column)` of `inner`'s first character in the string.
    pub start: (usize, usize),
}

pub(crate) fn classify(s: &str) -> Classified<'_> {
    if !crate::template::looks_templated(s) {
        return Classified::Literal;
    }
    let is_ws = |c: char| c.is_ascii_whitespace();
    let lead = s.len() - s.trim_start_matches(is_ws).len();
    let trimmed = s.trim_matches(is_ws);
    let Some(body) = trimmed
        .strip_prefix("{{")
        .and_then(|b| b.strip_suffix("}}"))
    else {
        return Classified::Mixed;
    };
    let (body, open_len) = match body.strip_prefix('-') {
        Some(b) => (b, 3),
        None => (body, 2),
    };
    let inner = body.strip_suffix('-').unwrap_or(body);
    if DELIMITERS.iter().any(|d| inner.contains(d)) {
        return Classified::Mixed;
    }
    Classified::Single(SingleExpr {
        inner,
        start: position_of(s, lead + open_len),
    })
}

/// 1-based `(line, column)` of byte offset `byte`; columns count characters,
/// as Tera's lexer does (lexer.rs:270-277).
fn position_of(s: &str, byte: usize) -> (usize, usize) {
    let before = &s[..byte];
    let line = 1 + before.matches('\n').count();
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

/// Map a 1-based wrapper position to the author's string (spec § 4.2 step 3).
/// A position inside `PREFIX` or `SUFFIX` has no counterpart: `None`.
pub(crate) fn map_position(expr: &SingleExpr<'_>, pos: (usize, usize)) -> Option<(usize, usize)> {
    let p = PREFIX.chars().count();
    let (l, c) = pos;
    let newlines = expr.inner.matches('\n').count();
    let last_line = 1 + newlines;
    let last_col = if newlines == 0 {
        p + expr.inner.chars().count()
    } else {
        expr.inner.rsplit('\n').next().unwrap_or("").chars().count()
    };
    let before_start = l == 0 || (l == 1 && c < p + 1);
    let after_end = l > last_line || (l == last_line && c > last_col);
    if before_start || after_end {
        return None;
    }
    let (l0, c0) = expr.start;
    if l == 1 {
        Some((l0, c0 + c - (p + 1)))
    } else {
        Some((l0 + l - 1, c))
    }
}

/// Evaluate a single expression to its native value. `render` is called
/// exactly once; a failure keeps its value-free category and gets its
/// position mapped back — the expression is never evaluated a second time.
pub(crate) fn eval_single(
    expr: &SingleExpr<'_>,
    render: &dyn Fn(&str) -> Result<String>,
) -> Result<Value> {
    let wrapper = format!("{PREFIX}{}{SUFFIX}", expr.inner);
    let text = render(&wrapper).map_err(|e| remap(e, expr))?;
    serde_json::from_str(&text).map_err(|_| anyhow!(NOT_JSON))
}

fn remap(err: anyhow::Error, expr: &SingleExpr<'_>) -> anyhow::Error {
    // `render_template` wraps the TemplateError in one context of our own
    // ("Failed to render template" / "Failed to parse template").
    let outer = (err.chain().count() > 1).then(|| err.to_string());
    match err.downcast::<TemplateError>() {
        Ok(te) => {
            let pos = te.position().and_then(|p| map_position(expr, p));
            let e = anyhow::Error::new(te.with_position(pos));
            match outer {
                Some(o) => e.context(o),
                None => e,
            }
        }
        Err(err) => err,
    }
}

/// The json rule for one field value (spec § 4.1–4.3), recursive over
/// objects and arrays.
pub fn render_json_value(value: &Value, field: &str, context: &Value) -> Result<Value> {
    render_json_value_with(value, field, &|t| {
        crate::template::render_template(t, context)
    })
}

pub(crate) fn render_json_value_with(
    value: &Value,
    field: &str,
    render: &dyn Fn(&str) -> Result<String>,
) -> Result<Value> {
    walk(value, field, render, &mut Vec::new())
}

fn walk(
    value: &Value,
    field: &str,
    render: &dyn Fn(&str) -> Result<String>,
    path: &mut Vec<Option<usize>>,
) -> Result<Value> {
    match value {
        Value::String(s) => match classify(s) {
            Classified::Literal => Ok(value.clone()),
            Classified::Single(expr) => {
                eval_single(&expr, render).with_context(|| location(field, path))
            }
            Classified::Mixed => Err(anyhow!(
                "{}: a json field takes a literal value or a single {{{{ expression }}}}",
                location(field, path)
            )),
        },
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                path.push(None);
                let rendered = walk(v, field, render, path);
                path.pop();
                out.insert(k.clone(), rendered?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, v) in items.iter().enumerate() {
                path.push(Some(i));
                let rendered = walk(v, field, render, path);
                path.pop();
                out.push(rendered?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

/// Names the field (a schema key) and, for a string nested only in arrays,
/// its indices. An object key is author/value text and is never named.
fn location(field: &str, path: &[Option<usize>]) -> String {
    if !path.is_empty() && path.iter().all(Option::is_some) {
        let idx: String = path.iter().flatten().map(|i| format!("[{i}]")).collect();
        format!("Input field '{field}' at `{idx}`")
    } else {
        format!("Input field '{field}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    #[test]
    fn classify_literal_single_mixed() {
        assert_eq!(classify("plain"), Classified::Literal);
        assert_eq!(classify(""), Classified::Literal);
        assert_eq!(
            classify("{{ x }}"),
            Classified::Single(SingleExpr { inner: " x ", start: (1, 3) })
        );
        assert_eq!(
            classify("  {{ x }} "),
            Classified::Single(SingleExpr { inner: " x ", start: (1, 5) })
        );
        assert_eq!(
            classify("{{- x -}}"),
            Classified::Single(SingleExpr { inner: " x ", start: (1, 4) })
        );
        assert_eq!(classify("id {{ x }}"), Classified::Mixed);
        assert_eq!(classify("{{ a }}{{ b }}"), Classified::Mixed);
        assert_eq!(classify("{% if x %}1{% endif %}"), Classified::Mixed);
        assert_eq!(classify("{# note #}"), Classified::Mixed);
    }

    #[test]
    fn yaml_block_scalar_trailing_newline_is_single() {
        assert!(matches!(classify("{{ x }}\n"), Classified::Single(_)));
        assert!(matches!(classify("\n  {{ x }}\n"), Classified::Single(_)));
    }

    /// Spec § 4.1: a delimiter inside a string literal of the expression
    /// classifies as Mixed — the documented, pinned false negative.
    #[test]
    fn delimiter_inside_a_string_literal_is_the_known_false_negative() {
        assert_eq!(classify(r#"{{ x | default(value="}}") }}"#), Classified::Mixed);
    }

    #[test]
    fn single_expressions_keep_native_values() {
        let ctx = json!({
            "o": {"a": 1}, "l": [1, 2, 3], "n": 3, "f": 1.5,
            "b": true, "s": "txt", "z": null,
        });
        let r = |t: &str| render_json_value(&json!(t), "f", &ctx).unwrap();
        assert_eq!(r("{{ o }}"), json!({"a": 1}));
        assert_eq!(r("{{ l }}"), json!([1, 2, 3]));
        assert_eq!(r("{{ n }}"), json!(3));
        assert_eq!(r("{{ f }}"), json!(1.5));
        assert_eq!(r("{{ b }}"), json!(true));
        assert_eq!(r("{{ s }}"), json!("txt"));
        assert_eq!(r("{{ z }}"), Value::Null);
        assert_eq!(r("{{ l | length }}"), json!(3));
        assert_eq!(r("{{ o.missing }}"), Value::Null);
        assert_eq!(r("{{ o.missing | default(value=[]) }}"), json!([]));
        // json_encode yields the TEXT — a string (spec § 4.3).
        assert_eq!(r("{{ o | json_encode() }}"), json!(r#"{"a":1}"#));
        assert_eq!(r("plain text"), json!("plain text"));
    }

    #[test]
    fn undefined_top_level_variable_is_an_error_not_null() {
        let err = render_json_value(&json!("{{ typo }}"), "cfg", &json!({})).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("Input field 'cfg'"), "{msg}");
        assert!(msg.contains("undefined"), "{msg}");
    }

    #[test]
    fn objects_and_arrays_are_walked_and_keys_are_not_rendered() {
        let ctx = json!({"h": "db.local", "p": 5432});
        let v = json!({"{{ h }}": {"host": "{{ h }}", "port": "{{ p }}"}, "l": ["{{ p }}", 1]});
        let out = render_json_value(&v, "cfg", &ctx).unwrap();
        assert_eq!(
            out,
            json!({"{{ h }}": {"host": "db.local", "port": 5432}, "l": [5432, 1]})
        );
    }

    #[test]
    fn mixed_template_is_a_value_free_error() {
        let ctx = json!({"secret": {"T": "canary-secret-value"}});
        let err = render_json_value(&json!(["ok", "id {{ secret.T }}"]), "cfg", &ctx)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert_eq!(
            msg,
            "Input field 'cfg' at `[1]`: a json field takes a literal value or a single {{ expression }}"
        );
        let nested = render_json_value(&json!({"a": {"b": "x {{ y }}"}}), "cfg", &ctx)
            .unwrap_err();
        assert!(format!("{nested:#}").starts_with("Input field 'cfg': "), "{nested:#}");
    }

    #[test]
    fn a_failing_expression_never_leaks_the_value_or_the_template() {
        let ctx = json!({"secret": {"T": "canary-secret-value"}});
        let err = render_json_value(&json!("{{ secret.T | int }}"), "cfg", &ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains("canary"), "{msg}");
        assert!(!msg.contains("secret.T"), "{msg}");
        assert!(!msg.contains("__stroem_v"), "{msg}");
    }

    #[test]
    fn the_expression_is_rendered_exactly_once() {
        let calls = Cell::new(0);
        let failing = |_t: &str| -> Result<String> {
            calls.set(calls.get() + 1);
            Err(anyhow!("boom"))
        };
        assert!(render_json_value_with(&json!("{{ x | vals }}"), "f", &failing).is_err());
        assert_eq!(calls.get(), 1, "a failure must not trigger a second render");

        calls.set(0);
        let ok = |_t: &str| -> Result<String> {
            calls.set(calls.get() + 1);
            Ok("1".into())
        };
        assert_eq!(render_json_value_with(&json!("{{ x }}"), "f", &ok).unwrap(), json!(1));
        assert_eq!(calls.get(), 1);
    }

    fn template_position(err: &anyhow::Error) -> Option<(usize, usize)> {
        err.chain()
            .find_map(|c| c.downcast_ref::<TemplateError>())
            .and_then(TemplateError::position)
    }

    /// The json-field error points exactly where Tera points when it renders
    /// the author's own string as a plain template.
    fn assert_maps_like_plain_render(s: &str) {
        let ctx = json!({"x": "v"});
        let plain = crate::template::render_template(s, &ctx).unwrap_err();
        let expected = template_position(&plain);
        assert!(expected.is_some(), "fixture must give a positioned error: {plain:#}");
        let json_err = render_json_value(&json!(s), "f", &ctx).unwrap_err();
        assert_eq!(template_position(&json_err), expected, "{s:?}: {json_err:#}");
    }

    #[test]
    fn error_positions_point_into_the_authors_string() {
        assert_maps_like_plain_render("{{ nope_fn() }}"); // first token
        assert_maps_like_plain_render("{{ x | upper | nope }}"); // last token
        assert_maps_like_plain_render("{{ x\n  | nope }}"); // second line of inner
        assert_maps_like_plain_render("\n  {{ x | nope }}"); // leading newline + spaces
        assert_maps_like_plain_render("{{- x | nope -}}"); // whitespace control
        assert_maps_like_plain_render("{{ 'ü✓' ~ x | nope }}"); // non-ASCII inside inner
    }

    #[test]
    fn a_position_in_the_wrapper_itself_is_dropped() {
        let expr = SingleExpr { inner: " x ", start: (1, 3) };
        let p = PREFIX.chars().count();
        assert_eq!(map_position(&expr, (1, 1)), None);
        assert_eq!(map_position(&expr, (1, p)), None);
        assert_eq!(map_position(&expr, (1, p + 1)), Some((1, 3)));
        assert_eq!(map_position(&expr, (1, p + 3)), Some((1, 5)));
        assert_eq!(map_position(&expr, (1, p + 4)), None); // first SUFFIX char
        // `{{ typo }}`'s undefined error is raised in SUFFIX: no position.
        let err = render_json_value(&json!("{{ typo }}"), "f", &json!({})).unwrap_err();
        assert_eq!(template_position(&err), None, "{err:#}");
    }
}
```

Add `pub mod json_field;` to `crates/stroem-common/src/lib.rs` (alphabetical: after `pub mod git_ref;`).

- [ ] **Step 6: Run the module tests**

Run: `cargo test -p stroem-common --lib json_field::`
Expected: PASS. If `single_expressions_keep_native_values` fails on `{{ o.missing }}` with an "undefined" error, STOP and report: spec revision 6 assumes Tera returns `None` (not `Undefined`) for a missing map key (upgrade-tera-2.md:115); the suffix's `is undefined` branch would then need narrowing to top-level names.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-common/src/json_field.rs crates/stroem-common/src/lib.rs crates/stroem-common/src/template_error.rs
git commit -m "feat(common): json field rule — single-expression native values with mapped error positions"
```

---

### Task 3: Schema-aware input rendering and json default merging

**Files:**
- Modify: `crates/stroem-common/src/template.rs` (new `render_input_typed`, re-export; `merge_defaults` `:583-598`; `merge_action_defaults` `:843-855`)
- Test: `crates/stroem-common/src/template.rs` (`mod tests`, near the merge_defaults tests at `:1771`)

**Interfaces:**
- Consumes: `json_field::render_json_value` (Task 2), `JSON_TYPE` (Task 1).
- Produces: `pub fn template::render_input_typed(input_map: &HashMap<String, serde_json::Value>, schema: Option<&HashMap<String, InputFieldDef>>, context: &serde_json::Value) -> Result<serde_json::Value>`; `pub use crate::json_field::render_json_value;` from `template`.

- [ ] **Step 1: Write the failing tests** — add to `mod tests` in `template.rs` (the `field(...)` helper at `:1773` already exists):

```rust
    // --- json input type (spec 2026-10-06-json-input-type § 4.4–4.5) ---

    #[test]
    fn render_input_typed_matches_render_input_map_without_json_fields() {
        let mut input = HashMap::new();
        input.insert("obj".to_string(), json!("{{ o }}"));
        input.insert("len".to_string(), json!("{{ l | length }}"));
        input.insert("lit".to_string(), json!(7));
        input.insert("txt".to_string(), json!("a {{ s }}"));
        let ctx = json!({"o": {"a": 1}, "l": [1, 2], "s": "b"});
        let mut schema = HashMap::new();
        schema.insert("obj".to_string(), field("string", false, None));
        schema.insert("len".to_string(), field("integer", false, None));
        let plain = render_input_map(&input, &ctx).unwrap();
        assert_eq!(render_input_typed(&input, Some(&schema), &ctx).unwrap(), plain);
        assert_eq!(render_input_typed(&input, None, &ctx).unwrap(), plain);
    }

    #[test]
    fn render_input_typed_gives_json_fields_native_values() {
        let mut input = HashMap::new();
        input.insert("n".to_string(), json!("{{ l | length }}"));
        input.insert("s".to_string(), json!("{{ l | length }}"));
        let mut schema = HashMap::new();
        schema.insert("n".to_string(), field("json", false, None));
        schema.insert("s".to_string(), field("string", false, None));
        let out = render_input_typed(&input, Some(&schema), &json!({"l": [1, 2, 3]})).unwrap();
        assert_eq!(out["n"], json!(3));
        assert_eq!(out["s"], json!("3"));
    }

    #[test]
    fn merge_defaults_renders_a_json_default_natively() {
        let mut schema = HashMap::new();
        schema.insert(
            "db".to_string(),
            field("json", false, Some(json!({"host": "{{ secret.H }}", "port": "{{ secret.P }}"}))),
        );
        let ctx = json!({"secret": {"H": "db.local", "P": 5432}});
        let out = merge_defaults(&json!({}), &schema, &ctx).unwrap();
        assert_eq!(out["db"], json!({"host": "db.local", "port": 5432}));
    }

    #[test]
    fn merge_action_defaults_renders_a_json_default_natively() {
        let mut schema = HashMap::new();
        schema.insert("n".to_string(), field("json", false, Some(json!("{{ secret.P }}"))));
        let out = merge_action_defaults(&json!({}), &schema, &json!({"secret": {"P": 42}})).unwrap();
        assert_eq!(out["n"], json!(42));
    }

    /// R26 for json: a default whose single expression yields text that is
    /// itself a template is NOT rendered again.
    #[test]
    fn merge_action_defaults_renders_a_json_default_exactly_once() {
        const CANARY: &str = "owner-secret-canary";
        let mut schema = HashMap::new();
        schema.insert("copy".to_string(), field("json", false, Some(json!("{{ input.note }}"))));
        let context = json!({
            "secret": {"TOKEN": CANARY},
            "input": {"note": "{{ secret.TOKEN }}"},
        });
        let merged = merge_action_defaults(&json!({}), &schema, &context).unwrap();
        assert_eq!(merged["copy"], "{{ secret.TOKEN }}", "{merged}");
        assert!(!merged.to_string().contains(CANARY), "{merged}");
    }

    #[test]
    fn a_json_default_with_a_mixed_template_fails_without_its_text() {
        let mut schema = HashMap::new();
        schema.insert("cfg".to_string(), field("json", false, Some(json!("v={{ secret.T }}"))));
        let err = merge_defaults(&json!({}), &schema, &json!({"secret": {"T": "x"}})).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("a json field takes a literal value"), "{msg}");
        assert!(!msg.contains("secret.T"), "{msg}");
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-common --lib template::tests::render_input_typed template::tests::merge_defaults_renders_a_json template::tests::merge_action_defaults_renders_a_json template::tests::a_json_default`
Expected: FAIL to compile (`render_input_typed` not found).

- [ ] **Step 3: Implement**

In `template.rs`, right after `render_input_map` (ends near `:532`), add:

```rust
pub use crate::json_field::render_json_value;

/// Render a step/hook input map against `schema` (spec
/// 2026-10-06-json-input-type § 4.4): fields the schema types `json` by the
/// json rule, every other field exactly as [`render_input_map`] does.
pub fn render_input_typed(
    input_map: &HashMap<String, serde_json::Value>,
    schema: Option<&HashMap<String, InputFieldDef>>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let is_json =
        |k: &str| schema.and_then(|s| s.get(k)).is_some_and(|f| f.field_type == JSON_TYPE);
    let (json_fields, rest): (HashMap<String, serde_json::Value>, HashMap<String, serde_json::Value>) =
        input_map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .partition(|(k, _)| is_json(k));
    let mut out = render_input_map(&rest, context)?;
    let map = out
        .as_object_mut()
        .expect("render_input_map always returns an object");
    for (k, v) in &json_fields {
        map.insert(k.clone(), render_json_value(v, k, context)?);
    }
    Ok(out)
}
```

In `merge_defaults`, replace the `let resolved = match default_value { ... };` block (lines 584-598) with:

```rust
            let resolved = if field_def.field_type == JSON_TYPE {
                render_json_value(default_value, field_name, context).with_context(|| {
                    format!(
                        "Failed to render default template for input field '{}'",
                        field_name
                    )
                })?
            } else {
                match default_value {
                    serde_json::Value::String(s) => {
                        if s.contains("{{") {
                            let rendered = render_template(s, context).with_context(|| {
                                format!(
                                    "Failed to render default template for input field '{}'",
                                    field_name
                                )
                            })?;
                            serde_json::Value::String(rendered)
                        } else {
                            default_value.clone()
                        }
                    }
                    _ => default_value.clone(),
                }
            };
```

In `merge_action_defaults`, replace the `let rendered = match default_value { ... }` expression (lines 845-850, keep the `.with_context(...)` and `.context(...)?` that follow) with:

```rust
        let rendered = if field_def.field_type == JSON_TYPE {
            render_json_value(default_value, field_name, context)
        } else {
            match default_value {
                serde_json::Value::String(s) if s.contains("{{") => {
                    render_template(s, context).map(serde_json::Value::String)
                }
                _ => render_value_deep(default_value, context),
            }
        }
```

- [ ] **Step 4: Run all template tests**

Run: `cargo test -p stroem-common --lib template::`
Expected: PASS (new and existing, incl. the two existing R26 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/template.rs
git commit -m "feat(common): schema-aware input rendering and json default merging"
```

---

### Task 4: `stroem validate` — json template checks and the numeric-secret warning

**Files:**
- Modify: `crates/stroem-common/src/json_field.rs` (add `JsonLint`, `lint_json_value`)
- Modify: `crates/stroem-common/src/validation.rs` (new `validate_json_fields`, `warn_non_string_secrets`; wire into `validate_workflow_config_inner` after `validate_input_options` at `:686`)
- Test: in-module tests of both files

**Interfaces:**
- Consumes: `json_field::classify` (Task 2), `template::JSON_TYPE` (Task 1).
- Produces: `pub struct json_field::JsonLint { pub mixed: bool, pub encodes_to_text: bool }`, `pub fn json_field::lint_json_value(&Value) -> JsonLint`.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `json_field.rs`:

```rust
    #[test]
    fn lint_flags_mixed_and_json_encode_tails() {
        assert_eq!(lint_json_value(&json!({"a": "{{ x }}", "b": 1})), JsonLint::default());
        assert!(lint_json_value(&json!(["ok", "id {{ x }}"])).mixed);
        assert!(lint_json_value(&json!("{{ x | json_encode() }}")).encodes_to_text);
        assert!(lint_json_value(&json!("{{ x | json_encode(pretty=true) }}")).encodes_to_text);
        assert!(!lint_json_value(&json!("{{ x | upper }}")).encodes_to_text);
        assert!(!lint_json_value(&json!("{{ json_encode_count }}")).encodes_to_text);
    }
```

Append to `mod tests` in `validation.rs`:

```rust
    #[test]
    fn mixed_template_in_a_json_step_input_is_an_error() {
        let cfg: WorkspaceConfig = serde_yaml::from_str(
            "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n      \
             cfg: { type: json }\n\
             tasks:\n  t:\n    flow:\n      s:\n        action: a\n        input:\n          \
             cfg: \"id {{ x }}\"\n",
        )
        .unwrap();
        let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
        assert!(err.contains("Task 't' step 's' input 'cfg'"), "{err}");
        assert!(err.contains("a json field takes a literal value"), "{err}");
    }

    #[test]
    fn mixed_template_in_a_json_default_is_an_error() {
        let cfg = json_cfg("      cfg: { type: json, default: \"v={{ secret.X }}\" }");
        let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
        assert!(err.contains("Action 'a' input 'cfg' default"), "{err}");
    }

    #[test]
    fn json_encode_tail_in_a_json_field_is_a_warning() {
        let cfg: WorkspaceConfig = serde_yaml::from_str(
            "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n      \
             cfg: { type: json }\n\
             tasks:\n  t:\n    flow:\n      s:\n        action: a\n        input:\n          \
             cfg: \"{{ x | json_encode() }}\"\n",
        )
        .unwrap();
        let warnings = validate_workflow_config(&cfg).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("input 'cfg'") && w.contains("json_encode")),
            "{warnings:?}"
        );
    }

    #[test]
    fn task_step_input_is_checked_against_the_task_schema() {
        let cfg: WorkspaceConfig = serde_yaml::from_str(
            "actions:\n  call: { type: task, task: child }\n  a: { type: script, script: \"true\" }\n\
             tasks:\n  child:\n    input:\n      cfg: { type: json }\n    flow:\n      s: { action: a }\n  \
             parent:\n    flow:\n      c:\n        action: call\n        input:\n          cfg: \"x {{ y }}\"\n",
        )
        .unwrap();
        let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
        assert!(err.contains("Task 'parent' step 'c' input 'cfg'"), "{err}");
    }

    #[test]
    fn numeric_secrets_are_warned_about_by_name_only() {
        let cfg: WorkspaceConfig = serde_yaml::from_str(
            "secrets:\n  PORT: 5432\n  FLAG: true\n  NESTED: { pin: 1234 }\n  OK: \"5432\"\n  \
             REF: \"ref+awsssm://x\"\n\
             connection_types:\n  pg:\n    host: { type: string }\n    pw: { type: integer, secret: true }\n\
             connections:\n  db: { type: pg, host: h, pw: 991234 }\n",
        )
        .unwrap();
        let warnings = validate_workflow_config(&cfg).unwrap();
        let has = |needle: &str| warnings.iter().any(|w| w.contains(needle));
        assert!(has("secret 'PORT' is a number"), "{warnings:?}");
        assert!(has("secret 'FLAG' is a boolean"), "{warnings:?}");
        assert!(has("secret 'NESTED' is a number"), "{warnings:?}");
        assert!(has("connection 'db' secret property 'pw' is a number"), "{warnings:?}");
        assert!(!has("'OK'") && !has("'REF'"), "{warnings:?}");
        assert!(
            !warnings.iter().any(|w| w.contains("5432") || w.contains("991234") || w.contains("1234")),
            "a warning must never print a secret value: {warnings:?}"
        );
    }
```

(Check how a connection's typed values are written in YAML by reading one existing connection test in `validation.rs`; adjust the `connections:` line to that form if it differs — the assertion lines stay.)

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-common --lib json_field::tests::lint validation::tests::mixed_template validation::tests::json_encode_tail validation::tests::task_step_input validation::tests::numeric_secrets`
Expected: FAIL (lint fn missing; validators not wired).

- [ ] **Step 3: Implement**

Add to `json_field.rs` (above `#[cfg(test)]`):

```rust
/// What `stroem validate` reports about one `json` field value (spec § 5.4).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct JsonLint {
    /// A string that is neither literal nor a single expression (an error).
    pub mixed: bool,
    /// A single expression ending in `json_encode` — the field would receive
    /// JSON TEXT, not the value (a warning).
    pub encodes_to_text: bool,
}

pub fn lint_json_value(value: &Value) -> JsonLint {
    let mut lint = JsonLint::default();
    lint_walk(value, &mut lint);
    lint
}

fn lint_walk(value: &Value, lint: &mut JsonLint) {
    match value {
        Value::String(s) => match classify(s) {
            Classified::Mixed => lint.mixed = true,
            Classified::Single(expr) if ends_in_json_encode(expr.inner) => {
                lint.encodes_to_text = true
            }
            _ => {}
        },
        Value::Object(map) => map.values().for_each(|v| lint_walk(v, lint)),
        Value::Array(items) => items.iter().for_each(|v| lint_walk(v, lint)),
        _ => {}
    }
}

fn ends_in_json_encode(inner: &str) -> bool {
    let Some((_, last)) = inner.rsplit_once('|') else {
        return false;
    };
    let last = last.trim();
    last.strip_prefix("json_encode")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('('))
}
```

Add to `validation.rs` (near `validate_input_options`):

```rust
/// `json` field templates (spec 2026-10-06-json-input-type § 5.4): a string
/// that is not literal or a single `{{ expression }}` is an error; a single
/// expression ending in `json_encode` is a warning.
fn validate_json_fields(config: &WorkspaceConfig) -> Result<Vec<String>> {
    use crate::template::JSON_TYPE;
    let mut warnings = Vec::new();
    for (action_name, action) in &config.actions {
        for (name, f) in &action.input {
            if let (true, Some(d)) = (f.field_type == JSON_TYPE, &f.default) {
                lint_json(&format!("Action '{action_name}' input '{name}' default"), d, &mut warnings)?;
            }
        }
    }
    for (task_name, task) in &config.tasks {
        for (name, f) in &task.input {
            if let (true, Some(d)) = (f.field_type == JSON_TYPE, &f.default) {
                lint_json(&format!("Task '{task_name}' input '{name}' default"), d, &mut warnings)?;
            }
        }
        for (step_name, step) in &task.flow {
            let Some(schema) = step_target_schema(config, step) else {
                continue;
            };
            for (key, value) in &step.input {
                if schema.get(key).is_some_and(|f| f.field_type == JSON_TYPE) {
                    lint_json(
                        &format!("Task '{task_name}' step '{step_name}' input '{key}'"),
                        value,
                        &mut warnings,
                    )?;
                }
            }
        }
    }
    Ok(warnings)
}

/// The schema a flow step's `input:` lands in when it resolves locally: the
/// action's input, or for a `type: task` action the task's (spec D6).
/// Library (dotted) and cross-workspace names do not resolve here.
fn step_target_schema<'a>(
    config: &'a WorkspaceConfig,
    step: &'a crate::models::workflow::FlowStep,
) -> Option<&'a HashMap<String, crate::models::workflow::InputFieldDef>> {
    let action = match &step.inline_action {
        Some(inline) => inline,
        None => config.actions.get(&step.action)?,
    };
    if action.action_type == "task" {
        config.tasks.get(action.task.as_deref()?).map(|t| &t.input)
    } else {
        Some(&action.input)
    }
}

fn lint_json(context: &str, value: &serde_json::Value, warnings: &mut Vec<String>) -> Result<()> {
    let lint = crate::json_field::lint_json_value(value);
    if lint.mixed {
        bail!("{context}: a json field takes a literal value or a single {{{{ expression }}}}");
    }
    if lint.encodes_to_text {
        warnings.push(format!(
            "{context}: the expression ends in `json_encode`, so the field receives JSON text \
             (a string); drop `| json_encode()` to pass the value itself"
        ));
    }
    Ok(())
}

/// Secrets redaction can never mask (spec D7, § 5.4): redaction matches
/// strings only. Names the secret and the JSON type — never the value.
fn warn_non_string_secrets(config: &WorkspaceConfig) -> Vec<String> {
    fn kinds(v: &serde_json::Value, out: &mut Vec<&'static str>) {
        match v {
            serde_json::Value::String(_) => {}
            serde_json::Value::Number(_) => out.push("is a number"),
            serde_json::Value::Bool(_) => out.push("is a boolean"),
            serde_json::Value::Null => out.push("is null"),
            serde_json::Value::Array(a) => a.iter().for_each(|x| kinds(x, out)),
            serde_json::Value::Object(m) => m.values().for_each(|x| kinds(x, out)),
        }
    }
    const HINT: &str = "so it is never masked in job output or errors; quote it to have it masked";
    let mut warnings = Vec::new();
    for (name, value) in &config.secrets {
        let mut found = Vec::new();
        kinds(value, &mut found);
        found.dedup();
        for k in found {
            warnings.push(format!("secret '{name}' {k}, {HINT}"));
        }
    }
    for (conn_name, conn) in &config.connections {
        let Some(type_def) = conn
            .connection_type
            .as_ref()
            .and_then(|t| config.connection_types.get(t))
        else {
            continue;
        };
        for (prop, def) in &type_def.properties {
            if !def.secret {
                continue;
            }
            let Some(value) = conn.values.get(prop) else {
                continue;
            };
            let mut found = Vec::new();
            kinds(value, &mut found);
            found.dedup();
            for k in found {
                warnings.push(format!(
                    "connection '{conn_name}' secret property '{prop}' {k}, {HINT}"
                ));
            }
        }
    }
    warnings
}
```

Wire both into `validate_workflow_config_inner`, right after `warnings.extend(validate_input_options(config)?);` (`:686`):

```rust
    // json field templates (spec 2026-10-06-json-input-type § 5.4)
    warnings.extend(validate_json_fields(config)?);

    // Secrets redaction cannot mask (spec D7)
    warnings.extend(warn_non_string_secrets(config));
```

(`HashMap` and `bail!` are already imported in `validation.rs`; if `HashMap` is not, add `use std::collections::HashMap;`.)

- [ ] **Step 4: Run all validation and json_field tests**

Run: `cargo test -p stroem-common --lib validation:: json_field::`
Expected: PASS. If a pre-existing validation test now fails only because it asserts an EXACT warnings list and a fixture contains a numeric secret, quote that fixture's secret value (the warning is the intended new behaviour).

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/json_field.rs crates/stroem-common/src/validation.rs
git commit -m "feat(validate): json field template checks and a warning for unmaskable secrets"
```

---

### Task 5: Claim — classify by the persisted action schema

**Files:**
- Modify: `crates/stroem-server/src/web/worker_api/rendering.rs:1-6` (imports), `:11-28` (`PrepareContext`), `:55-88` (`render_step_input`), `:96-172` (`prepare_step_action_input`)
- Modify: `crates/stroem-server/src/web/worker_api/jobs.rs:1026-1052` (compute schema; `PrepareContext` literal)
- Create: `crates/stroem-server/tests/json_input_test.rs`
- Modify: `crates/stroem-server/tests/main.rs` (add `mod json_input_test;`)

**Interfaces:**
- Consumes: `template::render_input_typed` (Task 3).
- Produces: `pub const rendering::UNREADABLE_INPUT_SCHEMA: &str`, `pub fn rendering::step_input_schema(step: &JobStepRow) -> anyhow::Result<Option<HashMap<String, InputFieldDef>>>`, `PrepareContext.input_schema: Option<&'a HashMap<String, InputFieldDef>>`. Test helpers in `json_input_test.rs` reused by Tasks 6 and 7: `app`, `app_with`, `execute`, `claim`, `complete`, `step_row`, `call`, `api`, `worker`, `workspace`.

- [ ] **Step 1: Create the test file with helpers and the claim tests** — `crates/stroem-server/tests/json_input_test.rs`:

```rust
//! The `json` input type end to end (spec 2026-10-06-json-input-type).

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{JobStepRepo, JobStepRow, WorkerRepo};
use stroem_server::config::{
    AclConfig, AuthConfig, DbConfig, LogStorageConfig, RetentionConfig, ServerConfig,
    WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const WORKER_TOKEN: &str = "json-input-test-worker-token";

struct App {
    router: Router,
    pool: PgPool,
    mgr: Arc<WorkspaceManager>,
    _tmp: TempDir,
}

fn workspace(yaml: &str) -> WorkspaceConfig {
    serde_yaml::from_str(yaml).expect("workspace yaml")
}

async fn app(yaml: &str) -> Result<App> {
    app_with(yaml, None, None).await
}

async fn app_with(yaml: &str, auth: Option<AuthConfig>, acl: Option<AclConfig>) -> Result<App> {
    app_ws(&[("default", yaml)], auth, acl).await
}

/// One server over several in-memory workspaces (cross-workspace tests).
async fn app_ws(
    workspaces: &[(&str, &str)],
    auth: Option<AuthConfig>,
    acl: Option<AclConfig>,
) -> Result<App> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: test_db.url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: workspaces
            .iter()
            .map(|(name, _)| {
                (
                    name.to_string(),
                    WorkspaceSourceDef::Folder {
                        triggers: true,
                        path: tmp.path().to_string_lossy().to_string(),
                    },
                )
            })
            .collect(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: WORKER_TOKEN.to_string(),
        auth,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        pin_store: None,
    };
    let mgr = WorkspaceManager::from_configs(
        workspaces
            .iter()
            .map(|(name, yaml)| (name.to_string(), workspace(yaml), Some("rev-1".to_string())))
            .collect(),
    );
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let mgr = Arc::clone(&state.workspaces);
    let router = build_router(state, CancellationToken::new());
    Ok(App { router, pool, mgr, _tmp: tmp })
}

fn api(method: &str, uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(t) = token {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn worker(method: &str, uri: &str, body: Value) -> Request<Body> {
    api(method, uri, body, Some(WORKER_TOKEN))
}

async fn call(app: &App, req: Request<Body>) -> Result<(StatusCode, Value)> {
    let resp = app.router.clone().oneshot(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes).to_string()}))
    };
    Ok((status, body))
}

async fn execute(app: &App, task: &str, body: Value) -> Result<(StatusCode, Value)> {
    call(
        app,
        api("POST", &format!("/api/workspaces/default/tasks/{task}/execute"), body, None),
    )
    .await
}

async fn claim(app: &App) -> Result<(StatusCode, Value)> {
    let id = Uuid::new_v4();
    WorkerRepo::register(&app.pool, id, "json-test-worker", &["script".to_string()], &[], false, None)
        .await?;
    call(
        app,
        worker("POST", "/worker/jobs/claim", json!({"worker_id": id, "capabilities": ["script"]})),
    )
    .await
}

async fn complete(app: &App, job_id: &str, step: &str, output: Value) -> Result<StatusCode> {
    let (status, _) = call(
        app,
        worker(
            "POST",
            &format!("/worker/jobs/{job_id}/steps/{step}/complete"),
            json!({"output": output, "exit_code": 0}),
        ),
    )
    .await?;
    Ok(status)
}

async fn step_row(app: &App, job_id: &str, step: &str) -> Result<JobStepRow> {
    let job_id: Uuid = job_id.parse()?;
    let steps = JobStepRepo::get_steps_for_job(&app.pool, job_id).await?;
    Ok(steps.into_iter().find(|s| s.step_name == step).expect("step row"))
}

// ─── Claim (spec § 6, D10) ───────────────────────────────────────────────────

const PIPELINE: &str = r#"
actions:
  emit: { type: script, script: "true" }
  use:
    type: script
    script: "true"
    input:
      cfg: { type: json }
      count: { type: json }
      label: { type: string }
tasks:
  t:
    flow:
      a: { action: emit }
      b:
        action: use
        depends_on: [a]
        input:
          cfg: "{{ a.output.cfg }}"
          count: "{{ a.output.items | length }}"
          label: "{{ a.output.items | length }}"
"#;

#[tokio::test]
async fn claim_gives_json_fields_native_values() -> Result<()> {
    let app = app(PIPELINE).await?;
    let (s, body) = execute(&app, "t", json!({"input": {}})).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let job = body["job_id"].as_str().unwrap().to_string();

    let (s, a) = claim(&app).await?;
    assert_eq!((s, a["step_name"].as_str()), (StatusCode::OK, Some("a")), "{a}");
    let out = json!({"cfg": {"region": "eu", "replicas": 3}, "items": [1, 2, 3]});
    assert_eq!(complete(&app, &job, "a", out).await?, StatusCode::OK);

    let (s, b) = claim(&app).await?;
    assert_eq!((s, b["step_name"].as_str()), (StatusCode::OK, Some("b")), "{b}");
    assert_eq!(b["input"]["cfg"], json!({"region": "eu", "replicas": 3}), "{b}");
    assert_eq!(b["input"]["count"], json!(3), "json field gets a number: {b}");
    assert_eq!(b["input"]["label"], json!("3"), "string field unchanged: {b}");
    Ok(())
}

#[tokio::test]
async fn claim_fails_a_mixed_template_in_a_json_field() -> Result<()> {
    let app = app(&PIPELINE.replace(
        "count: \"{{ a.output.items | length }}\"",
        "count: \"n={{ a.output.items | length }}\"",
    ))
    .await?;
    let (_, body) = execute(&app, "t", json!({"input": {}})).await?;
    let job = body["job_id"].as_str().unwrap().to_string();
    claim(&app).await?;
    complete(&app, &job, "a", json!({"cfg": {}, "items": [1]})).await?;

    let (s, _) = claim(&app).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let b = step_row(&app, &job, "b").await?;
    assert_eq!(b.status, "failed");
    let err = b.error_message.unwrap_or_default();
    assert!(err.contains("Input field 'count'"), "{err}");
    assert!(err.contains("a json field takes a literal value"), "{err}");
    Ok(())
}

const SINGLE: &str = r#"
actions:
  use:
    type: script
    script: "true"
    input:
      n: { type: json }
      extra: { type: string, default: persisted }
tasks:
  t:
    input:
      cfg: { type: json }
    flow:
      b:
        action: use
        input:
          n: "{{ input.cfg.items | length }}"
"#;

#[tokio::test]
async fn claim_uses_the_persisted_schema_after_a_live_retype() -> Result<()> {
    let app = app(SINGLE).await?;
    let (_, body) = execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    assert!(body["job_id"].is_string(), "{body}");
    // After creation: `n` becomes a string and the default changes.
    app.mgr
        .replace_config_for_test(
            "default",
            workspace(
                &SINGLE
                    .replace("n: { type: json }", "n: { type: string }")
                    .replace("default: persisted", "default: live"),
            ),
        )
        .await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["input"]["n"], json!(3), "persisted json type wins: {b}");
    assert_eq!(b["input"]["extra"], json!("persisted"), "persisted default wins: {b}");
    Ok(())
}

#[tokio::test]
async fn claim_uses_the_persisted_schema_after_the_action_is_deleted() -> Result<()> {
    let app = app(SINGLE).await?;
    execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    let mut live = workspace(SINGLE);
    live.actions.remove("use");
    app.mgr.replace_config_for_test("default", live).await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["input"]["n"], json!(3), "{b}");
    assert_eq!(b["input"]["extra"], json!("persisted"), "{b}");
    Ok(())
}

#[tokio::test]
async fn claim_keeps_a_persisted_string_type_after_a_live_retype_to_json() -> Result<()> {
    let yaml = SINGLE.replace("n: { type: json }", "n: { type: string }");
    let app = app(&yaml).await?;
    execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    app.mgr.replace_config_for_test("default", workspace(SINGLE)).await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["input"]["n"], json!("3"), "persisted string type wins: {b}");
    Ok(())
}

#[tokio::test]
async fn claim_ignores_a_live_retype_to_a_connection_type() -> Result<()> {
    let app = app(SINGLE).await?;
    execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    let live = format!(
        "connection_types:\n  pg:\n    host: {{ type: string }}\n{}",
        SINGLE.replace("n: { type: json }", "n: { type: pg }")
    );
    app.mgr.replace_config_for_test("default", workspace(&live)).await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "no connection resolution of a json value: {b}");
    assert_eq!(b["input"]["n"], json!(3), "{b}");
    Ok(())
}

#[tokio::test]
async fn claim_fails_an_unreadable_persisted_schema_through_retry() -> Result<()> {
    let yaml = SINGLE.replace(
        "        action: use\n",
        "        action: use\n        retry: { max_attempts: 2, delay: 1s }\n",
    );
    let app = app(&yaml).await?;
    let (_, body) = execute(&app, "t", json!({"input": {"cfg": {"items": [1]}}})).await?;
    let job = body["job_id"].as_str().unwrap().to_string();
    let job_id: Uuid = job.parse()?;
    sqlx::query(
        "UPDATE job_step SET action_spec = jsonb_set(action_spec, '{input}', '\"garbage-canary\"'::jsonb) \
         WHERE job_id = $1 AND step_name = 'b'",
    )
    .bind(job_id)
    .execute(&app.pool)
    .await?;

    let (s, _) = claim(&app).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let b = step_row(&app, &job, "b").await?;
    assert_eq!(b.status, "ready", "first failure is retried, not released");
    assert_eq!(b.retry_attempt, 1);

    sqlx::query("UPDATE job_step SET retry_at = NOW() - INTERVAL '1 second' WHERE job_id = $1 AND step_name = 'b'")
        .bind(job_id)
        .execute(&app.pool)
        .await?;
    let (s, _) = claim(&app).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let b = step_row(&app, &job, "b").await?;
    assert_eq!(b.status, "failed");
    let err = b.error_message.unwrap_or_default();
    assert!(
        err.contains("the step's persisted action definition has an unreadable input schema"),
        "{err}"
    );
    assert!(!err.contains("garbage-canary"), "{err}");
    Ok(())
}

#[tokio::test]
async fn for_each_instances_get_native_item_and_index() -> Result<()> {
    let app = app(
        r#"
actions:
  use:
    type: script
    script: "true"
    input:
      item: { type: json }
      idx: { type: json }
tasks:
  t:
    flow:
      each:
        action: use
        for_each: [{ n: 1 }, { n: 2 }]
        input:
          item: "{{ each.item }}"
          idx: "{{ each.index }}"
"#,
    )
    .await?;
    execute(&app, "t", json!({"input": {}})).await?;
    for _ in 0..2 {
        let (s, c) = claim(&app).await?;
        assert_eq!(s, StatusCode::OK, "{c}");
        let idx = c["input"]["idx"].as_u64().expect("index is a number");
        assert_eq!(c["input"]["item"], json!({"n": idx + 1}), "{c}");
    }
    Ok(())
}

/// Spec § 8: an owner-side json error (the owner's own default) is WITHHELD
/// at claim; a caller-side one (the caller's step input) is shown.
#[tokio::test]
async fn cross_workspace_json_errors_follow_the_withholding_rule() -> Result<()> {
    let owner = r#"
secrets:
  S: "owner-secret-canary"
actions:
  bad-default:
    type: script
    script: "true"
    input:
      d: { type: json, default: "x {{ secret.S }}" }
  ok:
    type: script
    script: "true"
    input:
      c: { type: json }
"#;
    let caller = r#"
tasks:
  owner-side:
    flow:
      s: { action: B.bad-default }
  caller-side:
    flow:
      s:
        action: B.ok
        input:
          c: "n {{ 1 }}"
"#;
    let app = app_ws(&[("A", caller), ("B", owner)], None, None).await?;
    for (task, withheld) in [("owner-side", true), ("caller-side", false)] {
        let (s, body) = call(
            &app,
            api("POST", &format!("/api/workspaces/A/tasks/{task}/execute"), json!({"input": {}}), None),
        )
        .await?;
        assert_eq!(s, StatusCode::OK, "{body}");
        let job = body["job_id"].as_str().unwrap().to_string();
        let (s, _) = claim(&app).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{task}");
        let err = step_row(&app, &job, "s").await?.error_message.unwrap_or_default();
        assert!(!err.contains("owner-secret-canary"), "{task}: {err}");
        if withheld {
            assert!(err.contains("details withheld"), "{task}: {err}");
            assert!(!err.contains("a json field takes"), "{task}: {err}");
        } else {
            assert!(err.contains("Input field 'c'"), "{task}: {err}");
            assert!(err.contains("a json field takes a literal value"), "{task}: {err}");
        }
    }
    Ok(())
}
```

Add `mod json_input_test;` to `crates/stroem-server/tests/main.rs` (keep the list alphabetical).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-server --test integration json_input_test::`
Expected: FAIL — e.g. `claim_gives_json_fields_native_values` gets `"3"` for `count`; the persisted-schema tests get live types.

- [ ] **Step 3: Implement `rendering.rs`**

Update the imports at the top:

```rust
use anyhow::{Context, Result};
use std::collections::HashMap;
use stroem_common::models::workflow::{InputFieldDef, WorkspaceConfig};
use stroem_common::template::{
    prepare_action_input_roles, render_env_map, render_input_typed, render_json_strings,
    render_string_opt, ProvenanceBucket, ProvenanceError, RoleConfig, RoleScope,
};
use stroem_db::JobStepRow;
```

Add a field at the end of `PrepareContext`:

```rust
    /// The action's input schema as persisted at job creation
    /// (`action_spec.input`, spec 2026-10-06-json-input-type D10): decides
    /// which fields are `json`, which defaults merge and which fields are
    /// connection-typed. `None` = no schema.
    pub input_schema: Option<&'a HashMap<String, InputFieldDef>>,
```

Add, below `is_owner_side_prepare_error`:

```rust
/// Fixed, value-free claim failure for a persisted input schema that does
/// not deserialise (spec 2026-10-06-json-input-type § 6). No serde text: it
/// can quote the stored value.
pub const UNREADABLE_INPUT_SCHEMA: &str =
    "the step's persisted action definition has an unreadable input schema";

/// The input schema of the action this step runs, as persisted at job
/// creation (spec D10). `Ok(None)` when absent (no `action_spec`, no `input`
/// key, or `null`); `Err` when present but not an input schema — never a
/// fallback to "no schema", which would skip defaults and connection
/// resolution.
pub fn step_input_schema(step: &JobStepRow) -> Result<Option<HashMap<String, InputFieldDef>>> {
    let Some(spec) = step.action_spec.as_ref() else {
        return Ok(None);
    };
    match spec.get("input") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|_| anyhow::anyhow!(UNREADABLE_INPUT_SCHEMA)),
    }
}
```

In `render_step_input`, replace the last render call:

```rust
    let rendered = render_input_typed(&flow_step.input, prep.input_schema, ctx.as_value())
        .context("Failed to render step input template")?;
    Ok(Some(rendered))
```

In `prepare_step_action_input`, keep the task and flow-step guards (they return the input unchanged, F13) but drop the live action lookup. Replace everything from `// For loop instance steps, fall back to looking up by loop_source` down to and including the `if action.input.is_empty() { return Ok(rendered_input); }` block with:

```rust
    // For loop instance steps, fall back to looking up by loop_source. The
    // flow step is only a guard here: when rendering passed the stored input
    // through (task / flow step gone, F13), preparation does too.
    let has_flow_step = task.flow.contains_key(&ctx.step.step_name)
        || ctx
            .step
            .loop_source
            .as_ref()
            .is_some_and(|src| task.flow.contains_key(src));
    if !has_flow_step {
        return Ok(rendered_input);
    }
    // The schema is the PERSISTED one (spec D10); the live action is not
    // looked up. Connection VALUES still resolve against the owner's config.
    let schema = match ctx.input_schema {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(rendered_input),
    };
```

Then, in the rest of the function, replace `action.input.keys()` with `schema.keys()` and `&action.input` with `schema`.

- [ ] **Step 4: Implement `jobs.rs`** — immediately before `// Render step input and apply action defaults` (`:1028`), add:

```rust
    // Spec 2026-10-06-json-input-type D10: one schema, read once, from the
    // persisted action definition. An unreadable one fails the claim.
    let input_schema = match rendering::step_input_schema(&step) {
        Ok(s) => s,
        Err(e) => {
            return Ok(fail_claimed_step_with_collisions(
                &state,
                step.job_id,
                &step.step_name,
                &e.to_string(),
                &ws_set,
                &ClaimFailure {
                    claim,
                    pin_secrets: &pin_secrets,
                    withheld: None,
                },
                std::mem::take(&mut collision_lines),
            )
            .await);
        }
    };
```

and add `input_schema: input_schema.as_ref(),` to the `rendering::PrepareContext { ... }` literal. Then find every other `PrepareContext {` literal and add the field:

Run: `grep -rn "PrepareContext {" crates/stroem-server/` — for each hit outside `jobs.rs:1029`, add `input_schema: None,` (tests) or derive it the same way (production code).

- [ ] **Step 5: Run the claim tests and the existing claim suites**

Run: `cargo test -p stroem-server --test integration json_input_test:: git_refs_claim_test:: integration_test::test_cross_workspace`
Expected: PASS. A failure in an existing test that edits an ACTION's defaults after job creation and expects the live default at claim is the intended D10 change — update that test's expectation to the persisted default and say so in the commit message.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/web/worker_api/rendering.rs crates/stroem-server/src/web/worker_api/jobs.rs crates/stroem-server/tests/json_input_test.rs crates/stroem-server/tests/main.rs
git commit -m "feat(server): claim prepares step input from the persisted action schema"
```

---

### Task 6: Typed rendering at task dispatch and in hooks

**Files:**
- Modify: `crates/stroem-server/src/settlement/dispatch.rs:359` (bucket C render)
- Modify: `crates/stroem-server/src/settlement/hooks.rs:650-657` (hook input render)
- Test: `crates/stroem-server/tests/json_input_test.rs` (append)

**Interfaces:**
- Consumes: `template::render_input_typed` (Task 3); test helpers from Task 5.

- [ ] **Step 1: Write the failing tests** — append to `json_input_test.rs`:

```rust
// ─── Dispatch and hooks (spec § 6) ───────────────────────────────────────────

#[tokio::test]
async fn task_step_passes_native_values_to_the_child() -> Result<()> {
    let app = app(
        r#"
secrets:
  S: "s-value"
actions:
  run-child:
    type: task
    task: child
    input:
      extra: { type: json, default: { a: "{{ secret.S }}", n: 1 } }
  noop: { type: script, script: "true" }
tasks:
  child:
    input:
      info: { type: json }
      n: { type: json }
      extra: { type: json }
    flow:
      s: { action: noop }
  parent:
    input:
      payload: { type: json }
    flow:
      call:
        action: run-child
        input:
          info: "{{ input.payload }}"
          n: "{{ input.payload.items | length }}"
"#,
    )
    .await?;
    let (s, body) =
        execute(&app, "parent", json!({"input": {"payload": {"items": [1, 2]}}})).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let parent: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let child = stroem_db::JobRepo::get_child_jobs(&app.pool, parent)
        .await?
        .pop()
        .expect("child job");
    let input = child.input.expect("child input");
    assert_eq!(input["info"], json!({"items": [1, 2]}), "{input}");
    assert_eq!(input["n"], json!(2), "{input}");
    assert_eq!(input["extra"], json!({"a": "s-value", "n": 1}), "{input}");
    Ok(())
}

#[tokio::test]
async fn hook_inputs_are_typed_by_their_target_schema() -> Result<()> {
    let app = app(
        r#"
actions:
  noop: { type: script, script: "true" }
  notify:
    type: script
    script: "true"
    input:
      count: { type: json }
  child-hook: { type: task, task: hooked }
tasks:
  hooked:
    input:
      count: { type: json }
    flow:
      s: { action: noop }
  t:
    flow:
      s: { action: noop }
    on_success:
      - action: notify
        input: { count: "{{ hook.status | length }}" }
      - action: child-hook
        input: { count: "{{ hook.status | length }}" }
"#,
    )
    .await?;
    let (_, body) = execute(&app, "t", json!({"input": {}})).await?;
    let job = body["job_id"].as_str().unwrap().to_string();
    claim(&app).await?;
    assert_eq!(complete(&app, &job, "s", json!({})).await?, StatusCode::OK);

    let job_id: Uuid = job.parse()?;
    let mut inputs = Vec::new();
    for _ in 0..50 {
        inputs = sqlx::query_scalar::<_, Value>(
            "SELECT j.input FROM job j WHERE j.source_job_id = $1 AND j.source_type = 'hook'",
        )
        .bind(job_id)
        .fetch_all(&app.pool)
        .await?;
        if inputs.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(inputs.len(), 2, "both hook jobs created: {inputs:?}");
    for input in inputs {
        assert_eq!(input["count"], json!(9), "\"completed\" has 9 chars: {input}");
    }
    Ok(())
}

/// Spec § 9 / D7: redaction is unchanged — a string secret inside a json
/// value is masked; a number is returned as a number.
#[tokio::test]
async fn job_detail_masks_string_secrets_inside_json_values_only() -> Result<()> {
    let app = app(
        r#"
secrets:
  H: "db-secret-host-value"
  P: 5432
actions:
  noop: { type: script, script: "true" }
tasks:
  t:
    input:
      db: { type: json, default: { host: "{{ secret.H }}", port: "{{ secret.P }}" } }
    flow:
      s: { action: noop }
"#,
    )
    .await?;
    let (_, body) = execute(&app, "t", json!({"input": {}})).await?;
    let job = body["job_id"].as_str().unwrap();
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job}"))
        .body(Body::empty())?;
    let (s, detail) = call(&app, req).await?;
    assert_eq!(s, StatusCode::OK, "{detail}");
    assert_eq!(detail["input"]["db"]["host"], json!("••••••"), "{detail}");
    assert_eq!(detail["input"]["db"]["port"], json!(5432), "{detail}");
    Ok(())
}
```

(Both hook kinds store their rendered input in `job.input`: the single-step hook job at `settlement/hooks.rs:752-757`, the `type: task` hook through `create_job_for_task_inner`.)

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-server --test integration json_input_test::task_step json_input_test::hook_inputs`
Expected: FAIL — `n` and `count` arrive as strings.

- [ ] **Step 3: Implement**

`dispatch.rs`: change the import from `render_input_map` to `render_input_typed` (keep `render_input_map` if still used elsewhere in the file — `:720`, approval steps, stays untyped) and replace the bucket-C call at `:359`:

```rust
                    // Bucket C lands in task T's input: T's schema decides
                    // which fields are `json` (spec 2026-10-06-json-input-type D6).
                    match render_input_typed(&map, Some(&resolved.task.input), context_value) {
```

`hooks.rs` (`fire_single_hook`): directly after `let action = ...?;` and before the template context is built, add:

```rust
    // The schema the hook input lands in (spec 2026-10-06-json-input-type
    // § 6): a `type: task` hook's task, else the hook action's own input.
    let hook_schema = if action.action_type == ActionType::Task.as_ref() {
        action
            .task
            .as_deref()
            .and_then(|t| workspace_config.tasks.get(t))
            .map(|t| &t.input)
    } else {
        Some(&action.input)
    };
```

and replace the render call:

```rust
        render_input_typed(&hook.input, hook_schema, &template_context)
            .context("Failed to render hook input templates")?
```

(update the `use stroem_common::template::...` import in `hooks.rs` accordingly).

- [ ] **Step 4: Run dispatch, hook and settlement suites**

Run: `cargo test -p stroem-server --test integration json_input_test:: orchestrator_test:: integration_test::test_xws`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/settlement/dispatch.rs crates/stroem-server/src/settlement/hooks.rs crates/stroem-server/tests/json_input_test.rs
git commit -m "feat(server): typed json rendering for task-step dispatch and hook inputs"
```

---

### Task 7: Re-run — same-task rule and `replay_fields`

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs:30-62` (`CreationMode::Rerun`), `:87-114` (`create_job_for_task_detailed`), `:357-386` (Rerun branch); add `ReplayFieldsError`, `apply_replay_fields`, `create_job_for_rerun`
- Modify: `crates/stroem-server/src/web/api/tasks.rs:102-107` (`ExecuteTaskRequest`), `:454` (no-source check), `:484-498` (same-task check), `:505-519` (unpinned creation), `:647-649` (pinned `CreationMode::Rerun`)
- Modify: `crates/stroem-server/src/web/api/mod.rs:420-440` (`classify_execute_error` typed tier)
- Test: `crates/stroem-server/tests/json_input_test.rs` (append); unit tests for `apply_replay_fields` in `job_creator.rs`

**Interfaces:**
- Produces: `pub enum job_creator::ReplayFieldsError { UnknownField, AlsoInInput { field: String }, MissingRequired { field: String } }`; `CreationMode::Rerun { source_job_id: Uuid, replay_fields: &'a [String] }`; `pub async fn job_creator::create_job_for_rerun(...)`; request JSON field `replay_fields: string[]`.

- [ ] **Step 1: Write the failing unit tests** — add to `job_creator.rs`'s `mod tests`:

```rust
    #[test]
    fn apply_replay_fields_copies_the_source_value_whole() {
        let mut schema = HashMap::new();
        schema.insert("cfg".to_string(), serde_yaml::from_str::<InputFieldDef>("type: json").unwrap());
        let mut input = json!({});
        apply_replay_fields(&mut input, &["cfg".into()], &json!({"cfg": {"a": "••••••x"}}), &schema)
            .unwrap();
        assert_eq!(input, json!({"cfg": {"a": "••••••x"}}));
    }

    #[test]
    fn apply_replay_fields_rejects_bad_requests_without_echoing_unknown_names() {
        let mut schema = HashMap::new();
        schema.insert("cfg".to_string(), serde_yaml::from_str::<InputFieldDef>("type: json").unwrap());
        schema.insert(
            "req".to_string(),
            serde_yaml::from_str::<InputFieldDef>("{ type: json, required: true }").unwrap(),
        );
        let src = json!({});
        let unknown = apply_replay_fields(&mut json!({}), &["canary-name".into()], &src, &schema)
            .unwrap_err();
        assert!(matches!(unknown, ReplayFieldsError::UnknownField));
        assert!(!unknown.to_string().contains("canary"), "{unknown}");
        let both = apply_replay_fields(&mut json!({"cfg": 1}), &["cfg".into()], &src, &schema)
            .unwrap_err();
        assert_eq!(both.to_string(), "field 'cfg' is both in input and in replay_fields");
        let missing = apply_replay_fields(&mut json!({}), &["req".into()], &src, &schema)
            .unwrap_err();
        assert_eq!(missing.to_string(), "the source job has no value for required field 'req'");
        let mut absent = json!({});
        apply_replay_fields(&mut absent, &["cfg".into()], &src, &schema).unwrap();
        assert_eq!(absent, json!({}), "absent in source → absent (default applies)");
    }
```

(If `job_creator.rs` has no `mod tests`, create one with `use super::*; use serde_json::json; use std::collections::HashMap;`.)

- [ ] **Step 2: Write the failing integration tests** — append to `json_input_test.rs`:

```rust
// ─── Re-run (spec § 7, D12) ──────────────────────────────────────────────────

const RERUN: &str = r#"
actions:
  noop: { type: script, script: "true" }
tasks:
  t:
    input:
      payload: { type: json }
      tok: { type: string, secret: true }
      need: { type: json, required: true }
    flow:
      s: { action: noop }
  other:
    input:
      payload: { type: json }
      tok: { type: string, secret: true }
    flow:
      s: { action: noop }
"#;

async fn source_job(app: &App) -> Result<String> {
    let (s, body) = execute(
        app,
        "t",
        json!({"input": {"payload": {"k": [1, "v"]}, "tok": "secret-tok", "need": 1}}),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    Ok(body["job_id"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn replay_fields_copies_the_stored_source_value() -> Result<()> {
    let app = app(RERUN).await?;
    let src = source_job(&app).await?;
    let (s, body) = execute(
        &app,
        "t",
        json!({"input": {"need": 2}, "source_job_id": src, "replay_fields": ["payload"]}),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let job: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let row = stroem_db::JobRepo::get(&app.pool, job).await?.unwrap();
    assert_eq!(row.input.unwrap()["payload"], json!({"k": [1, "v"]}));
    assert_eq!(row.raw_input.unwrap()["payload"], json!({"k": [1, "v"]}));
    Ok(())
}

#[tokio::test]
async fn replay_fields_rejects_bad_requests_with_400() -> Result<()> {
    let app = app(RERUN).await?;
    let src = source_job(&app).await?;
    for (body, needle) in [
        (json!({"input": {"need": 1}, "replay_fields": ["payload"]}), "requires source_job_id"),
        (
            json!({"input": {"need": 1}, "source_job_id": src, "replay_fields": ["canary-field"]}),
            "does not declare",
        ),
        (
            json!({"input": {"need": 1, "payload": 1}, "source_job_id": src, "replay_fields": ["payload"]}),
            "both in input and in replay_fields",
        ),
    ] {
        let (s, resp) = execute(&app, "t", body).await?;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{resp}");
        let text = resp.to_string();
        assert!(text.contains(needle), "{text}");
        assert!(!text.contains("canary-field"), "the unknown name is never echoed: {text}");
    }
    Ok(())
}

#[tokio::test]
async fn replay_fields_missing_required_value_is_400() -> Result<()> {
    let app = app(RERUN).await?;
    // A source without `need` (creation does not check required fields).
    let (_, body) = execute(&app, "t", json!({"input": {"payload": 1}})).await?;
    let src = body["job_id"].as_str().unwrap().to_string();
    let (s, resp) = execute(
        &app,
        "t",
        json!({"input": {}, "source_job_id": src, "replay_fields": ["need"]}),
    )
    .await?;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{resp}");
    assert!(resp.to_string().contains("no value for required field 'need'"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn masked_text_in_a_json_value_is_plain_data() -> Result<()> {
    let app = app(RERUN).await?;
    let src = source_job(&app).await?;
    let (s, body) = execute(
        &app,
        "t",
        json!({"input": {"need": 1, "payload": {"a": "••••••"}}, "source_job_id": src}),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let job: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let row = stroem_db::JobRepo::get(&app.pool, job).await?.unwrap();
    assert_eq!(row.input.unwrap()["payload"], json!({"a": "••••••"}));
    Ok(())
}

#[tokio::test]
async fn rerun_into_another_task_is_400_for_replay_and_sentinels() -> Result<()> {
    let app = app(RERUN).await?;
    let src = source_job(&app).await?;
    for body in [
        json!({"input": {}, "source_job_id": src, "replay_fields": ["payload"]}),
        json!({"input": {"tok": "••••••"}, "source_job_id": src}),
        json!({"input": {}, "source_job_id": src}),
    ] {
        let (s, resp) = execute(&app, "other", body).await?;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{resp}");
        assert!(resp.to_string().contains("is a run of task 't', not 'other'"), "{resp}");
    }
    Ok(())
}

#[tokio::test]
async fn mixed_acl_caller_cannot_replay_across_tasks() -> Result<()> {
    use stroem_db::{UserGroupRepo, UserRepo};
    use stroem_server::config::{AclAction, AclRule};
    let auth = AuthConfig {
        jwt_secret: "json-test-jwt-secret".to_string(),
        refresh_secret: "json-test-refresh-secret".to_string(),
        base_url: None,
        providers: HashMap::new(),
        initial_user: None,
        rate_limit: Default::default(),
    };
    let acl = AclConfig {
        default: AclAction::Deny,
        rules: vec![
            AclRule {
                workspace: "default".to_string(),
                tasks: vec!["t".to_string()],
                action: AclAction::View,
                groups: vec!["mixed".to_string()],
                users: vec![],
            },
            AclRule {
                workspace: "default".to_string(),
                tasks: vec!["other".to_string()],
                action: AclAction::Run,
                groups: vec!["mixed".to_string()],
                users: vec![],
            },
        ],
    };
    let app = app_with(RERUN, Some(auth), Some(acl)).await?;
    let hash = stroem_server::auth::hash_password("json-test-password-123")?;
    let admin = Uuid::new_v4();
    UserRepo::create(&app.pool, admin, "json-admin@test.com", Some(&hash), None).await?;
    UserRepo::set_admin(&app.pool, admin, true).await?;
    let user = Uuid::new_v4();
    UserRepo::create(&app.pool, user, "json-mixed@test.com", Some(&hash), None).await?;
    UserGroupRepo::add(&app.pool, user, "mixed").await?;
    let login = |email: &'static str| {
        api(
            "POST",
            "/api/auth/login",
            json!({"email": email, "password": "json-test-password-123"}),
            None,
        )
    };
    let (_, a) = call(&app, login("json-admin@test.com")).await?;
    let admin_token = a["access_token"].as_str().unwrap().to_string();
    let (_, u) = call(&app, login("json-mixed@test.com")).await?;
    let user_token = u["access_token"].as_str().unwrap().to_string();

    let (s, body) = call(
        &app,
        api(
            "POST",
            "/api/workspaces/default/tasks/t/execute",
            json!({"input": {"payload": {"k": 1}, "need": 1}}),
            Some(&admin_token),
        ),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let src = body["job_id"].as_str().unwrap();

    let (s, resp) = call(
        &app,
        api(
            "POST",
            "/api/workspaces/default/tasks/other/execute",
            json!({"input": {}, "source_job_id": src, "replay_fields": ["payload"]}),
            Some(&user_token),
        ),
    )
    .await?;
    assert_eq!(s, StatusCode::BAD_REQUEST, "View on t + Run on other must not replay: {resp}");
    Ok(())
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test -p stroem-server --lib job_creator::tests::apply_replay_fields` and `cargo test -p stroem-server --test integration json_input_test::replay json_input_test::masked json_input_test::rerun_into json_input_test::mixed_acl`
Expected: FAIL (types missing; cross-task re-run returns 200).

- [ ] **Step 4: Implement `job_creator.rs`**

Change the `Rerun` variant:

```rust
    /// User clicked Re-run: `••••••` sentinels in secret/connection fields
    /// and the fields named in `replay_fields` (spec 2026-10-06 D12) take the
    /// source's stored `raw_input` values; `source_job_id` is persisted.
    Rerun {
        source_job_id: Uuid,
        replay_fields: &'a [String],
    },
```

Add near the top-level items:

```rust
/// A `replay_fields` request that cannot be honoured (spec
/// 2026-10-06-json-input-type § 7). `classify_execute_error` maps it to 400.
/// `UnknownField` carries no name: it is request text, not a schema key.
#[derive(Debug)]
pub enum ReplayFieldsError {
    UnknownField,
    AlsoInInput { field: String },
    MissingRequired { field: String },
}

impl std::fmt::Display for ReplayFieldsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownField => f.write_str("replay_fields names a field the task does not declare"),
            Self::AlsoInInput { field } => {
                write!(f, "field '{field}' is both in input and in replay_fields")
            }
            Self::MissingRequired { field } => {
                write!(f, "the source job has no value for required field '{field}'")
            }
        }
    }
}

impl std::error::Error for ReplayFieldsError {}

/// Each field named in `replay_fields` takes the source's stored value, or
/// stays absent (its default then applies) when the source had none.
pub(crate) fn apply_replay_fields(
    input: &mut serde_json::Value,
    replay_fields: &[String],
    source_raw: &serde_json::Value,
    schema: &HashMap<String, InputFieldDef>,
) -> std::result::Result<(), ReplayFieldsError> {
    if replay_fields.is_empty() {
        return Ok(());
    }
    if !input.is_object() {
        *input = serde_json::json!({});
    }
    let map = input.as_object_mut().expect("just made an object");
    for name in replay_fields {
        let Some((key, def)) = schema.get_key_value(name.as_str()) else {
            return Err(ReplayFieldsError::UnknownField);
        };
        if map.contains_key(key) {
            return Err(ReplayFieldsError::AlsoInInput { field: key.clone() });
        }
        match source_raw.get(key) {
            Some(v) => {
                map.insert(key.clone(), v.clone());
            }
            None if def.required && def.default.is_none() => {
                return Err(ReplayFieldsError::MissingRequired { field: key.clone() });
            }
            None => {}
        }
    }
    Ok(())
}

/// Re-run of `source_job_id` (spec D12): sentinels and `replay_fields` are
/// resolved against the source's stored `raw_input`.
#[allow(clippy::too_many_arguments)]
pub async fn create_job_for_rerun(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    input: serde_json::Value,
    source_type: &str,
    source_id: Option<&str>,
    revision: Option<&str>,
    source_job_id: Uuid,
    replay_fields: &[String],
    agents_config: Option<&AgentsConfig>,
    defaults: JobDefaults,
) -> Result<CreatedJob> {
    create_job_for_task_inner(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        None,
        None,
        revision,
        CreationMode::Rerun {
            source_job_id,
            replay_fields,
        },
        agents_config,
        defaults,
        None,
    )
    .await
}
```

In `create_job_for_task_detailed`, the match arm becomes `Some(id) => CreationMode::Rerun { source_job_id: id, replay_fields: &[] },`.

In the Rerun branch of `create_job_for_task_inner`, destructure `CreationMode::Rerun { source_job_id, replay_fields } =>` and, right after `let source_raw = match ... ;` and BEFORE `resolve_rerun_sentinels`, add:

```rust
                apply_replay_fields(&mut effective_input, replay_fields, &source_raw, &task.input)
                    .map_err(anyhow::Error::new)?;
```

- [ ] **Step 5: Implement the API**

`web/api/tasks.rs` — `ExecuteTaskRequest` gains:

```rust
    /// Fields whose value is replayed from the source job's stored
    /// `raw_input` (spec 2026-10-06-json-input-type D12). Requires
    /// `source_job_id`.
    #[serde(default)]
    pub replay_fields: Vec<String>,
```

At the top of the execute handler, immediately before the `// 1b. Re-run of a PINNED source` comment:

```rust
    if !req.replay_fields.is_empty() && req.source_job_id.is_none() {
        return Err(AppError::BadRequest(
            "replay_fields requires source_job_id".into(),
        ));
    }
```

In the unpinned re-run validation (after `check_rerun_source(&source_job, &ws)?;` and the ACL check, before `effective_source_type = "rerun";`), add the pinned path's rule:

```rust
        // Same task as the source, on both paths (spec D12): a re-run copies
        // stored values only between runs of one task.
        if source_job.task_name != name {
            return Err(AppError::BadRequest(format!(
                "Source job {} is a run of task '{}', not '{}'",
                source_job.job_id, source_job.task_name, name
            )));
        }
```

Replace the unpinned `let created = create_job_for_task_detailed(...).await.map_err(classify_execute_error)?;` with:

```rust
    let created = match req.source_job_id {
        Some(src_id) => {
            crate::job_creator::create_job_for_rerun(
                &state.workspaces,
                &state.pool,
                &workspace,
                &ws,
                &name,
                input_value,
                effective_source_type,
                source_id.as_deref(),
                revision.as_deref(),
                src_id,
                &req.replay_fields,
                state.config.agents.as_ref(),
                JobDefaults::from(state.config.as_ref()),
            )
            .await
        }
        None => {
            create_job_for_task_detailed(
                &state.workspaces,
                &state.pool,
                &workspace,
                &ws,
                &name,
                input_value,
                effective_source_type,
                source_id.as_deref(),
                revision.as_deref(),
                None,
                state.config.agents.as_ref(),
                JobDefaults::from(state.config.as_ref()),
            )
            .await
        }
    }
    .map_err(classify_execute_error)?;
```

In `execute_pinned_rerun`, the mode becomes:

```rust
        CreationMode::Rerun {
            source_job_id: source_job.job_id,
            replay_fields: &req.replay_fields,
        },
```

`web/api/mod.rs` — in `classify_execute_error`'s typed tier (right after the `PinLoadWithheld`/`RefResolveError`/`GitRefError` block):

```rust
    if let Some(r) = e.downcast_ref::<crate::job_creator::ReplayFieldsError>() {
        return AppError::BadRequest(r.to_string());
    }
```

- [ ] **Step 6: Run the re-run suites**

Run: `cargo test -p stroem-server --lib job_creator::` and `cargo test -p stroem-server --test integration json_input_test:: rerun_integration_test:: pinned_rerun_restart_test::`
Expected: PASS. If an existing re-run test re-runs a source of a DIFFERENT task and expects 200, that is the intended D12 change — make it re-run the source's own task.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/src/web/api/tasks.rs crates/stroem-server/src/web/api/mod.rs crates/stroem-server/tests/json_input_test.rs
git commit -m "feat(api): replay_fields on re-run; every re-run requires the source's own task"
```

---

### Task 8: `stroem run` — typed step input

**Files:**
- Modify: `crates/stroem-cli/src/local/run.rs:509` (render call) and its imports
- Create: `crates/stroem-cli/tests/json_input.rs`
- Modify: `crates/stroem-cli/tests/main.rs` (add `mod json_input;`)

**Interfaces:**
- Consumes: `template::render_input_typed` (Task 3); validation from Task 4.

- [ ] **Step 1: Write the failing tests** — `crates/stroem-cli/tests/json_input.rs`:

```rust
use std::process::{Command, Output};

fn stroem(yaml: &str, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), yaml).unwrap();
    let mut full = vec!["--path", dir.path().to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_stroem")).args(&full).output().unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

const OK: &str = r#"
secrets:
  PORT: 5432
actions:
  emit:
    type: script
    script: |
      echo 'OUTPUT: {"cfg": {"region": "eu"}, "items": [1, 2, 3]}'
  check:
    type: script
    script: |
      [ "{{ input.cfg.region }}" = "eu" ] || exit 11
      [ "{{ input.count + 1 }}" = "4" ] || exit 12
      [ "{{ input.db.port + 1 }}" = "5433" ] || exit 13
    input:
      cfg: { type: json }
      count: { type: json }
      db: { type: json, default: { port: "{{ secret.PORT }}" } }
tasks:
  t:
    flow:
      a: { action: emit }
      b:
        action: check
        depends_on: [a]
        input:
          cfg: "{{ a.output.cfg }}"
          count: "{{ a.output.items | length }}"
"#;

#[test]
fn run_passes_native_values_through_json_fields_and_defaults() {
    let o = stroem(OK, &["run", "t"]);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn run_fails_a_mixed_template_in_a_json_field() {
    let yaml = OK.replace(
        "count: \"{{ a.output.items | length }}\"",
        "count: \"n={{ a.output.items | length }}\"",
    );
    let o = stroem(&yaml, &["run", "t"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("a json field takes a literal value"), "{}", text(&o));
}

#[test]
fn validate_reports_json_errors_and_warnings() {
    let mixed = OK.replace("cfg: \"{{ a.output.cfg }}\"", "cfg: \"x {{ a.output.cfg }}\"");
    let o = stroem(&mixed, &["validate"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("Task 't' step 'b' input 'cfg'"), "{}", text(&o));

    let o = stroem(OK, &["validate"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("secret 'PORT' is a number"), "{}", text(&o));
}
```

Add `mod json_input;` to `crates/stroem-cli/tests/main.rs`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-cli --test integration json_input::`
Expected: `run_passes_native_values...` FAILS (exit 12: `count` is the string "3"); the validate tests pass already only if Task 4 is done — they must pass after this task.

- [ ] **Step 3: Implement** — in `run.rs` `execute_step`, replace:

```rust
    let rendered_input = render_input_map(&step.input, ctx)
        .with_context(|| format!("Step '{}': failed to render input", step_name))?;
```

with:

```rust
    let rendered_input =
        stroem_common::template::render_input_typed(&step.input, Some(&action.input), ctx)
            .with_context(|| format!("Step '{}': failed to render input", step_name))?;
```

and remove `render_input_map` from `run.rs`'s imports if it is no longer used there.

- [ ] **Step 4: Run the CLI suite**

Run: `cargo test -p stroem-cli`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-cli/src/local/run.rs crates/stroem-cli/tests/json_input.rs crates/stroem-cli/tests/main.rs
git commit -m "feat(cli): stroem run renders json fields natively"
```

---

### Task 9: Agent task-tool schema for `json`

**Files:**
- Modify: `crates/stroem-agent/src/tools.rs:50-77` (`input_schema_to_json_schema`), `:116-127` (`map_field_type`), tests `:210-230`
- Test: `crates/stroem-agent/src/tools.rs`, `crates/stroem-agent/src/loop_dispatch.rs` (`mod tests`)

**Interfaces:**
- Changes: `fn map_field_type(field_type: &str) -> (Option<String>, Option<String>)` — `None` type means "any JSON value" (no `type` keyword).

- [ ] **Step 1: Write the failing tests**

In `tools.rs` `mod tests`, update `test_map_field_type_coverage`'s expectations to the new `Option` shape (`(Some("string".to_string()), None)` etc.) and add:

```rust
    #[test]
    fn json_field_has_no_type_keyword() {
        assert_eq!(map_field_type("json"), (None, None));
        let mut input = HashMap::new();
        input.insert(
            "cfg".to_string(),
            serde_yaml::from_str::<InputFieldDef>("{ type: json, description: Config }").unwrap(),
        );
        let schema = input_schema_to_json_schema(&input);
        let cfg = &schema["properties"]["cfg"];
        assert!(cfg.get("type").is_none(), "{schema}");
        assert_eq!(cfg["description"], "Config Any JSON value.");
    }
```

In `loop_dispatch.rs` `mod tests`, add a wire test next to `resume_of_legacy_ask_user_state_replays_calls_and_answers`:

```rust
    /// The OpenAI wire carries a `json` tool parameter without `type`.
    #[tokio::test]
    async fn json_task_tool_parameter_reaches_the_wire_without_a_type() {
        let (base_url, server) = capture_one_request().await;
        let provider = openai_provider(format!("{base_url}/v1"));
        let action: ActionDef = serde_json::from_value(serde_json::json!({
            "type": "agent", "tools": [{"task": "deploy"}]
        }))
        .unwrap();
        let infos = vec![TaskToolInfo {
            name: "deploy".to_string(),
            description: None,
            input: std::collections::HashMap::from([(
                "cfg".to_string(),
                serde_yaml::from_str("type: json").unwrap(),
            )]),
            parameters_schema: None,
        }];
        let _ = dispatch_agent_loop(
            &NoopContext,
            Uuid::new_v4(),
            "agent",
            &action,
            &provider,
            "test-model",
            "Deploy it",
            None,
            None,
            None,
            Vec::new(),
            &infos,
        )
        .await;
        let body = server.await.unwrap().body;
        let cfg = &body["tools"][0]["function"]["parameters"]["properties"]["cfg"];
        assert!(cfg.is_object(), "{body}");
        assert!(cfg.get("type").is_none(), "{body}");
    }
```


- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p stroem-agent tools::tests loop_dispatch::tests::json_task_tool`
Expected: FAIL (type mismatch / `type: object` on the wire).

- [ ] **Step 3: Implement** — in `tools.rs`:

```rust
/// Map a Strøm field type to a (JSON Schema type, optional format) pair.
/// `json` has no type: any JSON value (spec 2026-10-06-json-input-type D9).
fn map_field_type(field_type: &str) -> (Option<String>, Option<String>) {
    let t = |s: &str| Some(s.to_string());
    match field_type {
        "string" | "text" => (t("string"), None),
        "integer" => (t("integer"), None),
        "number" => (t("number"), None),
        "boolean" => (t("boolean"), None),
        "date" => (t("string"), t("date")),
        "datetime" => (t("string"), t("date-time")),
        stroem_common::template::JSON_TYPE => (None, None),
        _ => (t("object"), None), // Connection types or unknown
    }
}
```

and in `input_schema_to_json_schema`:

```rust
        let (json_type, format) = map_field_type(&field.field_type);
        if let Some(json_type) = json_type {
            prop.insert("type".to_string(), serde_json::Value::String(json_type));
        }
        if let Some(fmt) = format {
            prop.insert("format".to_string(), serde_json::Value::String(fmt));
        }

        let is_json = field.field_type == stroem_common::template::JSON_TYPE;
        match (&field.description, is_json) {
            (Some(desc), true) => {
                prop.insert(
                    "description".to_string(),
                    serde_json::Value::String(format!("{desc} Any JSON value.")),
                );
            }
            (None, true) => {
                prop.insert(
                    "description".to_string(),
                    serde_json::Value::String("Any JSON value.".to_string()),
                );
            }
            (Some(desc), false) => {
                prop.insert("description".to_string(), serde_json::Value::String(desc.clone()));
            }
            (None, false) => {}
        }
```

(replacing the existing `if let Some(ref desc) = field.description { ... }` block). Update the doc comment list above `input_schema_to_json_schema` with `- json → no type (any JSON value)`.

- [ ] **Step 4: Run the agent suite**

Run: `cargo test -p stroem-agent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-agent/src/tools.rs crates/stroem-agent/src/loop_dispatch.rs
git commit -m "feat(agent): json task-tool parameters carry no JSON Schema type"
```

---

### Task 10: UI — the three-mode JSON field

**Files:**
- Create: `ui/src/lib/json-field.ts`, `ui/src/lib/__tests__/json-field.test.ts`
- Create: `ui/src/components/task/json-input-field.tsx`, `ui/src/components/task/json-input-field.test.tsx`
- Modify: `ui/src/lib/execute-input.ts`, `ui/src/lib/__tests__/execute-input.test.ts`
- Modify: `ui/src/lib/api.ts:281-296` (`executeTask`)
- Modify: `ui/src/components/task/input-field-row.tsx:20-37` (props, json dispatch)
- Modify: `ui/src/pages/task-detail.tsx:28` (import), `:119-138` (prefill), `:186-208` (submit), `:298-305` (row props)

**Interfaces:**
- Produces (TS): `JsonMode`, `JsonFieldState { kind: "json"; mode; text }`, `ReplaySource { value: unknown }`, `isJsonFieldState`, `initialJsonFieldState(field, source?)`, `parseJsonText(text)`, `valueNotes(value)`, `toEditorText(v)`, `hasMask(v)`, `hasTemplate(v)`; `buildExecutePayload(values, fields): { input; replayFields }`, `ExecuteFormError`; `executeTask(..., opts?: { sourceJobId?: string; replayFields?: string[] })`.

- [ ] **Step 1: Write the failing pure-logic tests** — `ui/src/lib/__tests__/json-field.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import {
  initialJsonFieldState,
  parseJsonText,
  valueNotes,
  hasMask,
  hasTemplate,
} from "../json-field";
import type { InputField } from "../types";

const plain: InputField = { type: "json", default: { a: 1 } };
const templated: InputField = { type: "json", default: { host: "{{ secret.H }}" } };
const bare: InputField = { type: "json" };

describe("initialJsonFieldState (spec § 7, first match wins)", () => {
  it("replays a masked source value whole", () => {
    expect(initialJsonFieldState(templated, { value: { t: "x••••••" } }).mode).toBe("replay");
  });
  it("re-run with a masked source AND a templated default still replays (source wins)", () => {
    expect(initialJsonFieldState(templated, { value: ["••••••"] }).mode).toBe("replay");
  });
  it("prefills an unmasked source value in value mode", () => {
    expect(initialJsonFieldState(templated, { value: { k: 2 } })).toEqual({
      kind: "json",
      mode: "value",
      text: JSON.stringify({ k: 2 }, null, 2),
    });
  });
  it("re-run with a source value equal to the default still sends it", () => {
    const s = initialJsonFieldState(plain, { value: { a: 1 } });
    expect(s.mode).toBe("value");
    expect(s.text).toBe(JSON.stringify({ a: 1 }, null, 2));
  });
  it("uses default mode for a templated default when the source lacks the field", () => {
    expect(initialJsonFieldState(templated).mode).toBe("default");
  });
  it("prefills an untemplated default in value mode", () => {
    expect(initialJsonFieldState(plain).text).toBe(JSON.stringify({ a: 1 }, null, 2));
  });
  it("starts empty without a default", () => {
    expect(initialJsonFieldState(bare)).toEqual({ kind: "json", mode: "value", text: "" });
  });
});

describe("helpers", () => {
  it("detects masks and templates at any depth", () => {
    expect(hasMask({ a: [{ b: "••••••" }] })).toBe(true);
    expect(hasMask({ a: 1 })).toBe(false);
    expect(hasTemplate(["{% if x %}"])).toBe(true);
    expect(hasTemplate("plain")).toBe(false);
  });
  it("parses JSON or reports Invalid JSON", () => {
    expect(parseJsonText('{"a": [1]}')).toEqual({ ok: true, value: { a: [1] } });
    const bad = parseJsonText('{"a": }');
    expect(bad.ok).toBe(false);
    if (!bad.ok) expect(bad.error.startsWith("Invalid JSON")).toBe(true);
  });
  it("notes masked and template text without blocking", () => {
    expect(valueNotes({ a: "••••••" })).toHaveLength(1);
    expect(valueNotes({ a: "{{ x }}" })).toHaveLength(1);
    expect(valueNotes({ a: 1 })).toEqual([]);
  });
});
```

Append to `ui/src/lib/__tests__/execute-input.test.ts`:

```ts
import { buildExecutePayload, ExecuteFormError } from "../execute-input";

describe("buildExecutePayload — json fields", () => {
  const jf: Record<string, InputField> = {
    cfg: { type: "json", default: { a: 1 } },
    need: { type: "json", required: true },
    opt: { type: "json" },
  };
  const st = (mode: "default" | "replay" | "value", text = "") => ({ kind: "json" as const, mode, text });

  it("omits default mode, lists replay mode, sends value mode parsed", () => {
    expect(buildExecutePayload({ cfg: st("default") }, jf)).toEqual({ input: {}, replayFields: [] });
    expect(buildExecutePayload({ cfg: st("replay") }, jf)).toEqual({ input: {}, replayFields: ["cfg"] });
    expect(buildExecutePayload({ cfg: st("value", '{"a": 1}') }, jf)).toEqual({
      input: { cfg: { a: 1 } },
      replayFields: [],
    });
  });
  it("sends a value equal to the default (it is the user's literal)", () => {
    expect(buildExecutePayload({ cfg: st("value", '{"a":1}') }, jf).input).toEqual({ cfg: { a: 1 } });
  });
  it("omits empty text; blocks it when required without a default", () => {
    expect(buildExecutePayload({ opt: st("value", "  ") }, jf).input).toEqual({});
    expect(() => buildExecutePayload({ need: st("value", "") }, jf)).toThrow(ExecuteFormError);
  });
  it("blocks invalid JSON", () => {
    expect(() => buildExecutePayload({ opt: st("value", "{oops") }, jf)).toThrow(/Invalid JSON/);
  });
  it("sends masked text and template text as data", () => {
    expect(buildExecutePayload({ opt: st("value", '"••••••"') }, jf).input).toEqual({ opt: "••••••" });
    expect(buildExecutePayload({ opt: st("value", '"{{ x }}"') }, jf).input).toEqual({ opt: "{{ x }}" });
  });
});
```

(Move the new `import` line to the top of the file next to the existing imports.)

- [ ] **Step 2: Run them to see them fail**

Run: `cd ui && bunx vitest run src/lib/__tests__/json-field.test.ts src/lib/__tests__/execute-input.test.ts`
Expected: FAIL (modules / exports missing).

- [ ] **Step 3: Implement `ui/src/lib/json-field.ts`**

```ts
import { REDACTED_SENTINEL } from "@/components/task/constants";
import type { InputField } from "./types";

/** How a `json` field's value reaches the server (spec D8). */
export type JsonMode = "default" | "replay" | "value";

export interface JsonFieldState {
  kind: "json";
  mode: JsonMode;
  /** Editor text — meaningful in `value` mode only. */
  text: string;
}

/** The re-run source's value for a field the source supplied. */
export interface ReplaySource {
  value: unknown;
}

export function isJsonFieldState(v: unknown): v is JsonFieldState {
  return typeof v === "object" && v !== null && (v as { kind?: unknown }).kind === "json";
}

function someString(v: unknown, pred: (s: string) => boolean): boolean {
  if (typeof v === "string") return pred(v);
  if (Array.isArray(v)) return v.some((x) => someString(x, pred));
  if (v !== null && typeof v === "object") {
    return Object.values(v as Record<string, unknown>).some((x) => someString(x, pred));
  }
  return false;
}

export const hasMask = (v: unknown) => someString(v, (s) => s.includes(REDACTED_SENTINEL));
export const hasTemplate = (v: unknown) =>
  someString(v, (s) => s.includes("{{") || s.includes("{%") || s.includes("{#"));

export function toEditorText(v: unknown): string {
  return JSON.stringify(v, null, 2);
}

/** Initial mode, first match wins (spec § 7): the source's value beats the default. */
export function initialJsonFieldState(field: InputField, source?: ReplaySource): JsonFieldState {
  if (source) {
    if (hasMask(source.value)) return { kind: "json", mode: "replay", text: "" };
    return { kind: "json", mode: "value", text: toEditorText(source.value) };
  }
  if (field.default !== undefined && hasTemplate(field.default)) {
    return { kind: "json", mode: "default", text: "" };
  }
  return {
    kind: "json",
    mode: "value",
    text: field.default === undefined ? "" : toEditorText(field.default),
  };
}

export type ParsedJson = { ok: true; value: unknown } | { ok: false; error: string };

/** Parse editor text; the error names a line and column when the engine gives a position. */
export function parseJsonText(text: string): ParsedJson {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    const lc = /line (\d+) column (\d+)/.exec(msg);
    if (lc) return { ok: false, error: `Invalid JSON: line ${lc[1]}, column ${lc[2]}` };
    const p = /position (\d+)/.exec(msg);
    if (p) {
      const pos = Number(p[1]);
      const before = text.slice(0, pos);
      const line = before.split("\n").length;
      const column = pos - before.lastIndexOf("\n");
      return { ok: false, error: `Invalid JSON: line ${line}, column ${column}` };
    }
    return { ok: false, error: "Invalid JSON" };
  }
}

/** Non-blocking notes about a value the user typed (spec § 7). */
export function valueNotes(value: unknown): string[] {
  const notes: string[] = [];
  if (hasMask(value)) {
    notes.push(`Contains ${REDACTED_SENTINEL}, which is sent as text. "Use previous value" replays the masked value instead.`);
  }
  if (hasTemplate(value)) {
    notes.push("Contains {{ … }}, which is sent as text, not evaluated.");
  }
  return notes;
}
```

- [ ] **Step 4: Implement `execute-input.ts`** — replace the file's body below the doc comment's imports with:

```ts
import { SECRET_SENTINEL } from "@/components/task/constants";
import { isJsonFieldState, parseJsonText } from "./json-field";
import type { InputField } from "./types";

/** A field the form cannot submit as it stands. */
export class ExecuteFormError extends Error {
  constructor(
    public field: string,
    message: string,
  ) {
    super(`${field}: ${message}`);
  }
}

export interface ExecutePayload {
  input: Record<string, unknown>;
  /** Fields replayed from the re-run source (spec D12). */
  replayFields: string[];
}

/**
 * Turn the execute form's values into the payload for
 * `POST /tasks/{name}/execute`. (Doc text of the old buildExecuteInput stays
 * here, unchanged.) A `json` field's wire value follows from its MODE alone:
 * default → omitted, replay → named in `replayFields`, value → parsed text.
 */
export function buildExecutePayload(
  values: Record<string, unknown>,
  fields: Record<string, InputField>,
): ExecutePayload {
  const input: Record<string, unknown> = {};
  const replayFields: string[] = [];
  for (const [key, val] of Object.entries(values)) {
    const field = fields[key];
    if (isJsonFieldState(val)) {
      if (val.mode === "default") continue;
      if (val.mode === "replay") {
        replayFields.push(key);
        continue;
      }
      if (val.text.trim() === "") {
        if (field?.required && field.default === undefined) {
          throw new ExecuteFormError(key, "a value is required");
        }
        continue;
      }
      const parsed = parseJsonText(val.text);
      if (!parsed.ok) throw new ExecuteFormError(key, parsed.error);
      input[key] = parsed.value;
      continue;
    }
    if (field?.secret && val === SECRET_SENTINEL) continue;
    if (val === "" && field?.default === undefined) continue;
    input[key] = field?.type === "number" ? Number(val) : val;
  }
  return { input, replayFields };
}

/** The `input` part of {@link buildExecutePayload}. */
export function buildExecuteInput(
  values: Record<string, unknown>,
  fields: Record<string, InputField>,
): Record<string, unknown> {
  return buildExecutePayload(values, fields).input;
}
```

(Keep the existing JSDoc paragraph about omitted empty fields on `buildExecutePayload`.)

`api.ts` — `executeTask`:

```ts
export async function executeTask(
  workspace: string,
  name: string,
  input: Record<string, unknown>,
  opts?: { sourceJobId?: string; replayFields?: string[] },
): Promise<ExecuteTaskResponse> {
  const body: Record<string, unknown> = { input };
  if (opts?.sourceJobId) body.source_job_id = opts.sourceJobId;
  if (opts?.sourceJobId && opts.replayFields?.length) body.replay_fields = opts.replayFields;
```

(rest unchanged).

- [ ] **Step 5: Run the pure tests**

Run: `cd ui && bunx vitest run src/lib/__tests__/json-field.test.ts src/lib/__tests__/execute-input.test.ts`
Expected: PASS.

- [ ] **Step 6: Write the failing component test** — `ui/src/components/task/json-input-field.test.tsx`:

```tsx
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { JsonInputField } from "./json-input-field";
import type { JsonFieldState } from "@/lib/json-field";

const value = (mode: JsonFieldState["mode"], text = ""): JsonFieldState => ({ kind: "json", mode, text });

describe("JsonInputField", () => {
  it("shows a monospace editor and an inline error for invalid JSON", () => {
    render(<JsonInputField id="i" fieldKey="cfg" field={{ type: "json" }} value={value("value", "{oops")} onChange={() => {}} />);
    expect(screen.getByLabelText("cfg").className).toContain("font-mono");
    expect(screen.getByRole("alert").textContent).toMatch(/^Invalid JSON/);
  });

  it("shows a templated default read-only; Override opens an empty editor", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField
        id="i"
        fieldKey="cfg"
        field={{ type: "json", default: { host: "{{ secret.H }}" } }}
        value={value("default")}
        onChange={onChange}
      />,
    );
    expect(screen.getByTestId("json-default-cfg").textContent).toContain("{{ secret.H }}");
    fireEvent.click(screen.getByRole("button", { name: "Override" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "value", text: "" });
  });

  it("offers Use previous value on a re-run and replays a masked source", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField
        id="i"
        fieldKey="cfg"
        field={{ type: "json" }}
        value={value("value", "{}")}
        onChange={onChange}
        replaySource={{ value: { t: "••••••" } }}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Use previous value" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "replay", text: "" });
  });
});
```

- [ ] **Step 7: Run it to see it fail**

Run: `cd ui && bunx vitest run src/components/task/json-input-field.test.tsx`
Expected: FAIL (module missing).

- [ ] **Step 8: Implement `ui/src/components/task/json-input-field.tsx`**

```tsx
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import type { InputField } from "@/lib/types";
import {
  type JsonFieldState,
  type ReplaySource,
  hasMask,
  parseJsonText,
  toEditorText,
  valueNotes,
} from "@/lib/json-field";

export interface JsonInputFieldProps {
  id: string;
  fieldKey: string;
  field: InputField;
  value: JsonFieldState;
  onChange: (v: JsonFieldState) => void;
  replaySource?: ReplaySource;
}

/** The three-mode `json` field (spec 2026-10-06-json-input-type § 7). */
export function JsonInputField({ id, fieldKey, field, value, onChange, replaySource }: JsonInputFieldProps) {
  const label = field.name ?? fieldKey;
  const override = () => onChange({ kind: "json", mode: "value", text: "" });
  const useDefault =
    field.default !== undefined ? () => onChange({ kind: "json", mode: "default", text: "" }) : undefined;
  const usePrevious = replaySource
    ? () =>
        onChange(
          hasMask(replaySource.value)
            ? { kind: "json", mode: "replay", text: "" }
            : { kind: "json", mode: "value", text: toEditorText(replaySource.value) },
        )
    : undefined;

  const header = (
    <Label htmlFor={id}>
      {label}
      {field.required && field.default === undefined && <span className="ml-1 text-destructive">*</span>}
    </Label>
  );

  if (value.mode === "default") {
    return (
      <div className="space-y-2">
        {header}
        <pre data-testid={`json-default-${fieldKey}`} className="max-h-48 overflow-auto rounded-md border bg-muted px-3 py-2 font-mono text-xs">
          {toEditorText(field.default)}
        </pre>
        <p className="text-xs text-muted-foreground">The task&apos;s default — evaluated when the job runs.</p>
        <Button type="button" variant="outline" size="sm" onClick={override}>
          Override
        </Button>
      </div>
    );
  }

  if (value.mode === "replay") {
    return (
      <div className="space-y-2">
        {header}
        <p className="text-sm text-muted-foreground">The previous run&apos;s value (contains masked secrets) is reused.</p>
        <Button type="button" variant="outline" size="sm" onClick={override}>
          Override
        </Button>
      </div>
    );
  }

  const parsed = value.text.trim() ? parseJsonText(value.text) : null;
  const notes = parsed?.ok ? valueNotes(parsed.value) : [];
  return (
    <div className="space-y-2">
      {header}
      <Textarea
        id={id}
        rows={8}
        className="font-mono text-xs"
        value={value.text}
        placeholder={fieldKey}
        onChange={(e) => onChange({ kind: "json", mode: "value", text: e.target.value })}
      />
      {parsed && !parsed.ok && (
        <p role="alert" className="text-xs text-destructive">
          {parsed.error}
        </p>
      )}
      {notes.map((n) => (
        <p key={n} className="text-xs text-muted-foreground">
          {n}
        </p>
      ))}
      {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
      {(useDefault || usePrevious) && (
        <div className="flex gap-2">
          {useDefault && (
            <Button type="button" variant="outline" size="sm" onClick={useDefault}>
              Use default
            </Button>
          )}
          {usePrevious && (
            <Button type="button" variant="outline" size="sm" onClick={usePrevious}>
              Use previous value
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 9: Wire it into the form**

`input-field-row.tsx` — add to the imports:

```tsx
import { JsonInputField } from "@/components/task/json-input-field";
import { type JsonFieldState, type ReplaySource } from "@/lib/json-field";
```

add `replaySource?: ReplaySource;` to `InputFieldRowProps` and to the destructured props, and make the FIRST statement after `const displayLabel = ...`:

```tsx
  if (field.type === "json") {
    return (
      <JsonInputField
        id={id}
        fieldKey={fieldKey}
        field={field}
        value={value as JsonFieldState}
        onChange={onChange}
        replaySource={replaySource}
      />
    );
  }
```

`task-detail.tsx`:
- imports: replace `import { buildExecuteInput } from "@/lib/execute-input";` with `import { buildExecutePayload } from "@/lib/execute-input";` and add `import { initialJsonFieldState } from "@/lib/json-field";`.
- prefill loop (`for (const [key, field] of Object.entries(data.input))`): make its first statement

```ts
            const hasSource = !!rawInput && Object.prototype.hasOwnProperty.call(rawInput, key);
            if (field.type === "json") {
              defaults[key] = initialJsonFieldState(field, hasSource ? { value: rawInput![key] } : undefined);
              continue;
            }
```

- `handleSubmit`: replace `const input = buildExecuteInput(values, task.input);` with `const { input, replayFields } = buildExecutePayload(values, task.input);` and the options argument with `sourceJobId && rawInput ? { sourceJobId, replayFields } : undefined,`.
- `<InputFieldRow ...>`: add

```tsx
                    replaySource={
                      rawInput && Object.prototype.hasOwnProperty.call(rawInput, key)
                        ? { value: rawInput[key] }
                        : undefined
                    }
```

- [ ] **Step 10: Run the UI suites, lint and typecheck**

Run: `cd ui && bunx vitest run && bun run lint && bunx tsc -b`
Expected: PASS, no lint or type errors.

- [ ] **Step 11: Commit**

```bash
git add ui/src/lib/json-field.ts ui/src/lib/__tests__/json-field.test.ts ui/src/lib/execute-input.ts ui/src/lib/__tests__/execute-input.test.ts ui/src/lib/api.ts ui/src/components/task/json-input-field.tsx ui/src/components/task/json-input-field.test.tsx ui/src/components/task/input-field-row.tsx ui/src/pages/task-detail.tsx
git commit -m "feat(ui): three-mode JSON input field with replay_fields re-run"
```

---

### Task 11: Demo workspace, E2E section and Playwright

**Files:**
- Create: `workspace/.workflows/json-input.yaml`
- Modify: `tests/e2e.sh` (new section before `# --- Summary ---`, `:922`)
- Modify: `ui/e2e/tasks.spec.ts` (two tests after the multi-select tests)

- [ ] **Step 1: Create the demo workspace** — `workspace/.workflows/json-input.yaml`:

```yaml
actions:
  emit-config:
    type: script
    script: |
      echo 'OUTPUT: {"cfg": {"region": "eu", "replicas": 3}, "items": ["a", "b", "c"]}'
  use-config:
    type: script
    script: |
      echo "REGION={{ input.cfg.region }}"
      echo "NEXT={{ input.cfg.replicas + 1 }}"
      echo "COUNT_PLUS={{ input.count + 1 }}"
      echo "PAYLOAD_KEY={{ input.payload.key }}"
    input:
      cfg: { type: json, required: true }
      count: { type: json }
      payload: { type: json }

tasks:
  json-demo:
    mode: distributed
    description: Demonstrates the json input type.
    input:
      payload:
        type: json
        description: Any JSON value
        default: { key: "from-task-default" }
    flow:
      emit:
        action: emit-config
      use:
        action: use-config
        depends_on: [emit]
        input:
          cfg: "{{ emit.output.cfg }}"
          count: "{{ emit.output.items | length }}"
          payload: "{{ input.payload }}"
```

- [ ] **Step 2: Add the E2E section** — insert before `# --- Summary ---` in `tests/e2e.sh`:

```bash
# --- 21. json input type: native values between steps, API object input ---
info "Triggering json-demo (json input type)..."
EXEC_RESP_JS=$(acurl -X POST "$BASE_URL/api/workspaces/default/tasks/json-demo/execute" \
    -H "Content-Type: application/json" -d '{"input": {"payload": {"key": "from-api"}}}')
JS_JOB_ID=$(echo "$EXEC_RESP_JS" | jq -r '.job_id')
[ -n "$JS_JOB_ID" ] && [ "$JS_JOB_ID" != "null" ] || fail "json-demo execute failed: $EXEC_RESP_JS"
JS_POLLED=0; JS_STATUS="pending"
while [ "$JS_STATUS" != "completed" ] && [ "$JS_STATUS" != "failed" ]; do
    sleep 2; JS_POLLED=$((JS_POLLED + 2))
    [ "$JS_POLLED" -lt "$MAX_POLL" ] || { acurl "$BASE_URL/api/jobs/$JS_JOB_ID" | jq .; fail "json-demo did not finish"; }
    JS_DETAIL=$(acurl "$BASE_URL/api/jobs/$JS_JOB_ID"); JS_STATUS=$(echo "$JS_DETAIL" | jq -r '.status'); printf "."
done; echo ""
[ "$JS_STATUS" = "completed" ] || { echo "$JS_DETAIL" | jq .; fail "json-demo failed"; }
JS_LOGS=$(acurl "$BASE_URL/api/jobs/$JS_JOB_ID/logs" | jq -r '.logs')
echo "$JS_LOGS" | grep -q "REGION=eu" || fail "object did not reach the json field"
echo "$JS_LOGS" | grep -q "NEXT=4" || fail "nested number was not native"
echo "$JS_LOGS" | grep -q "COUNT_PLUS=4" || fail "| length did not arrive as a number"
echo "$JS_LOGS" | grep -q "PAYLOAD_KEY=from-api" || fail "API object input did not pass through"
[ "$(echo "$JS_DETAIL" | jq -r '.steps[] | select(.step_name=="use") | .input.count')" = "3" ] \
    || fail "persisted step input count is not the number 3"
pass "json input: native object, number and API object verified end to end"
```

(Use the next free section number if `21` is taken by the time this lands.)

- [ ] **Step 3: Add the Playwright tests** — in `ui/e2e/tasks.spec.ts`, after the multi-select tests:

```ts
  test("json input submits a parsed object and re-runs it", async ({ page }) => {
    await page.goto("/workspaces/default/tasks/json-demo");
    await page.waitForLoadState("networkidle");

    const editor = page.getByLabel("payload");
    await expect(editor).toHaveValue(/from-task-default/);
    await editor.fill('{"key": "from-ui"}');

    const isExecute = (req: import("@playwright/test").Request) =>
      req.url().includes("/tasks/json-demo/execute") && req.method() === "POST";
    const first = page.waitForRequest(isExecute);
    await page.getByRole("button", { name: "Run Task" }).click();
    const body = (await first).postDataJSON() as { input?: { payload?: unknown } };
    expect(body.input?.payload).toEqual({ key: "from-ui" });

    await page.waitForURL(/\/jobs\/.+/);
    await page.getByRole("link", { name: "Re-run" }).click();
    await page.waitForURL(/\/tasks\/json-demo/);
    await expect(page.getByLabel("payload")).toHaveValue(/from-ui/);

    const second = page.waitForRequest(isExecute);
    await page.getByRole("button", { name: "Run Task" }).click();
    const rerun = (await second).postDataJSON() as {
      input?: { payload?: unknown };
      source_job_id?: string;
    };
    expect(rerun.input?.payload).toEqual({ key: "from-ui" });
    expect(rerun.source_job_id).toBeTruthy();
  });
```

- [ ] **Step 4: Run them**

Run: `./tests/e2e.sh` (needs Docker) and `docker compose -f docker-compose.yml -f docker-compose.test.yml up --build --abort-on-container-exit playwright`
Expected: the new E2E section passes; Playwright suite green.

- [ ] **Step 5: Commit**

```bash
git add workspace/.workflows/json-input.yaml tests/e2e.sh ui/e2e/tasks.spec.ts
git commit -m "test(e2e): json input type end to end and in the Run form"
```

---

### Task 12: Documentation

**Files:**
- Modify: `docs/src/content/docs/guides/input-and-output.md` (types table `:31-41`; new section after "Default values")
- Modify: `docs/src/content/docs/guides/templating.md`, `guides/secrets.md`, `guides/rerun-and-restart.md`
- Create: `docs/src/content/docs/operations/upgrade-0-19-json-input.md`; Modify: `docs/astro.config.mjs` (sidebar, after the Git Refs migration entry `:88-91`)
- Modify: `CLAUDE.md`, `CONTEXT.md`, `docs/internal/TODO.md`, `docs/internal/stroem-v2-plan.md`
- Regenerate: `docs/public/llms.txt`

- [ ] **Step 1: User guides.** In `input-and-output.md`, add a table row `| \`json\` | Any JSON value (object, array, string, number, boolean, null). Renders as a JSON editor in the UI |` and change the following sentence to "If the `type` is not one of the types above, …". Add this section after "### Default values":

````markdown
### JSON inputs

A `json` field holds any JSON value and keeps it structured through templates.

```yaml
actions:
  deploy:
    type: script
    script: echo "{{ input.cfg.region }} x{{ input.replicas + 1 }}"
    input:
      cfg: { type: json }
      replicas: { type: json }
tasks:
  release:
    input:
      targets: { type: json, default: [eu, us] }
    flow:
      plan: { action: make-plan }
      go:
        action: deploy
        depends_on: [plan]
        input:
          cfg: "{{ plan.output.cfg }}"            # an object
          replicas: "{{ plan.output.hosts | length }}"   # a number
```

Rules for every string inside a `json` value (also inside its objects and arrays):

- **Literal text** (no `{{`, `{%` or `{#`) is used as is.
- **Exactly one `{{ expression }}`** takes the expression's value — object, array, number, boolean, string or `null`. A missing field (`{{ obj.missing }}`) or undefined variable (`{{ typo }}`) is an error, as everywhere; use `{{ obj.missing | default(value=none) }}` for an optional value (gives `null`).
- **Anything else** (`"id {{ x }}"`, two expressions, `{% if %}` blocks) is an error. Build text inside one expression instead: `{{ 'id ' ~ x }}`.
- `| json_encode()` gives JSON *text* — the field then holds a string. Leave it out to pass the value; `stroem validate` warns about it.

Values sent through the API, a webhook's `body`, the CLI `--input`, MCP and agent tools are used exactly as given. A `json` field cannot be `secret`, have `options`/`allow_custom`/`multiple`, or be used in an approval form.

In the **Run task** form a `json` field is a JSON editor. A default that contains templates is shown read-only ("evaluated when the job runs"); *Override* opens an empty editor. An empty editor sends nothing (the default applies) — type `""` or `null` to send those values.
````

`templating.md`: under the section on rendering structured data, add: "In a `json` input field, a string that is exactly one `{{ expression }}` keeps the expression's native value — see [JSON inputs](/guides/input-and-output/#json-inputs)."

`secrets.md`: add a subsection "### What is masked":

```markdown
Job detail, webhook and MCP responses mask secret values that appear in **strings** — including strings inside JSON input values. Numbers, booleans and `null` are never masked:

- Quote numeric secrets (`PORT: "5432"`) so they are masked; `stroem validate` warns about unquoted ones.
- A filter that changes a secret's form — `| int`, `| upper`, `| b64encode`, a slice — produces a value that is not masked, in any field type.
- Values of 3 characters or fewer are never masked.
```

`rerun-and-restart.md`: add under Re-run: "A `json` field whose previous value contains masked secrets is replayed whole (*Use previous value*): the server copies the source job's stored value. API clients do the same with `replay_fields`: `{"source_job_id": "…", "replay_fields": ["cfg"], "input": {…}}`. A re-run must use a source job of the same task; `replay_fields` needs `source_job_id`, may only name the task's fields, may not repeat a field given in `input`, and fails when the source has no value for a required field with no default — each a `400`."

- [ ] **Step 2: Upgrade note** — `docs/src/content/docs/operations/upgrade-0-19-json-input.md`:

```markdown
---
title: Upgrading to 0.19 — JSON inputs
description: Behaviour that changes with the json input type release
---

- **Roll out servers first.** Every server replica must run 0.19 before any workflow YAML uses `type: json`; an older replica treats `json` as a connection type and fails the step. Upgrade the `stroem` CLI too — older `stroem validate` rejects `type: json`.
- **A connection type named `json` is rejected.**
- **Claim uses the action definition from job creation.** Defaults and input types of an action are read from the definition stored when the job was created; editing an action no longer changes the unclaimed steps of jobs already running (as `type: task` steps already behaved).
- **Re-run requires the source's own task.** `source_job_id` of a run of another task now answers `400` on every re-run.
- **`stroem validate` warns about numeric secrets.** Secrets written as YAML numbers were never masked; quote them to have them masked.
```

Sidebar entry in `docs/astro.config.mjs` after the "Migrations 049–050 — Git Refs" item:

```js
            {
              label: "Upgrading to 0.19 — JSON Inputs",
              slug: "operations/upgrade-0-19-json-input",
            },
```

- [ ] **Step 3: Internal docs.**
- `CLAUDE.md` § Tera Templating — add a bullet: "**`json` input fields** (spec `docs/superpowers/specs/2026-10-06-json-input-type-design.md`): `stroem_common::json_field` owns the rule — literal / exactly one `{{ expr }}` (native value via the `{% set __stroem_v = … %}…json_encode` wrapper, undefined top-level variables still error) / anything else an error. `template::render_input_typed` is the schema-aware renderer (non-`json` fields byte-for-byte `render_input_map`); `merge_defaults` / `merge_action_defaults` branch on `JSON_TYPE`. Claim reads the schema (json classification, defaults, connection-typed fields) from the step's persisted `action_spec.input` (`rendering::step_input_schema`; unreadable → fixed claim failure). Wrapper errors keep their category and get positions mapped back — never a second render. Re-run: `replay_fields` (typed `ReplayFieldsError` → 400) and every re-run needs the source's own task."
- `CLAUDE.md` § Secrets in logs — append: "Redaction is strings-only; numbers, booleans and nulls are never masked (spec 2026-10-06-json-input-type D7). `stroem validate` warns about secrets written as YAML numbers."
- `CONTEXT.md` — add a glossary entry in the existing format: **Native value** — the value a `json` input field takes from a string that is exactly one `{{ expression }}`: the expression's JSON value (object, array, number, …), not its rendered text.
- `docs/internal/TODO.md` — add each item of spec § 13 under the matching section (Frontend: templated string defaults submitted verbatim, `integer` not converted, `json` in approval forms, `secret: true` on `json`, partially masked re-run strings; Architecture: plain action hooks get no defaults, first claim after a removed flow step passes raw templates, per-config scrub set lacks connection properties, sentinel missing-required gap; Agent: per-provider fallback if a provider rejects typeless properties).
- `docs/internal/stroem-v2-plan.md` — add a status line: "json input type — complete (spec 2026-10-06-json-input-type)".

- [ ] **Step 4: Regenerate llms.txt and build the docs**

Run: `cd docs && bun run build`
Expected: build succeeds; `docs/public/llms.txt` contains "JSON inputs".

- [ ] **Step 5: Commit**

```bash
git add docs/ CLAUDE.md CONTEXT.md
git commit -m "docs: json input type — guides, upgrade note, internal docs"
```

---

### Task 13: Full CI check suite

- [ ] **Step 1: Run every check the CI runs**

```bash
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
cd ui && bun run lint && bunx tsc -b && bunx vitest run && bun run build && cd ..
```

Expected: all green. Fix any failure in the task that introduced it (amend nothing — add a `fix:` commit).

- [ ] **Step 2: Commit any fixes**

```bash
git add -A
git commit -m "fix: CI check suite findings for the json input type"
```

(Skip if there is nothing to commit.)

---

### Task 14: Codex implementation review (gate before any merge decision)

The user requires the IMPLEMENTATION to be reviewed by Codex, not only the spec (memory: Codex-gated design loop). A Claude whole-branch review does not replace this gate.

- [ ] **Step 1: Send the branch to Codex** — run the `codex:rescue` command (forwarder agent `codex:codex-rescue`) from this worktree with `--fresh` (threads are per directory; the spec thread lives here too, so `--resume` is also valid — prefer `--resume` to keep the spec findings in context):

> Adversarially review the implementation of the json input type on branch worktree-feat-json-input-type (commits after 5d239cb8) against docs/superpowers/specs/2026-10-06-json-input-type-design.md revision 7 and docs/superpowers/plans/2026-10-07-json-input-type.md. Check: every spec section is implemented; value-free errors (no value, template text or request-supplied unknown field name in any message); R26 render-once; the claim schema source (persisted action_spec.input) for defaults and connections; cross-workspace withholding unchanged; replay_fields validation and the same-task rule on both execute paths; the UI mode model; tests actually exercising the Review Focus items. Verify file:line. Report findings ranked by severity and a verdict (ready to merge: yes/no). Do not edit files.

- [ ] **Step 2: Present Codex's output verbatim to the user**, then verify each finding against the code.

- [ ] **Step 3: Fix verified findings** (one `fix:` commit per finding or coherent group), re-run Task 13's suite, and re-review in the SAME Codex thread (`--resume`) until the verdict is yes or the remaining findings are explicitly accepted by the user.

- [ ] **Step 4: Hand off** — only after Codex's "yes": use superpowers:finishing-a-development-branch.
