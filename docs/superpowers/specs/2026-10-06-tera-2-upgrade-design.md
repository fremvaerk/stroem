# Tera 1 → 2 upgrade

Status: revision 9, proposed (2026-10-06)

Part of the dependency refresh in PR #8 (`chore/dependabot-2026-10-06`). The
user chose to include Tera 2 in it, knowing it changes the template language
workflow authors write. The facts below were gathered in research notes
(working notes, not committed) with the Tera 2 file:line behind each.

## Revision history

**Revision 9 (2026-10-06, fix wave 2).** R24 — the deep-render path context names only the root field our own code
supplies (`manifest` / `args`), never object keys (author text); R26 — action defaults are rendered exactly once (a second
render in `merge_action_defaults` let an owner secret smuggle through a
caller-visible value; pre-existing, fixed in PR #8); R27 — documented behaviour change: action input defaults render exactly once (previously a default whose first render produced `{{ … }}` was evaluated again). Corrections: R20 also
accepts `indentation=`; § 3.2.3 lists the real `raw_detail()` callers
(`local/validate.rs`, the `stroem run` printers, `main`'s top-level error
print); deep-render errors name the field (`manifest`/`args`), not object keys.

**Revision 8 (2026-10-06, post-implementation corrections).** Rulings made
while implementing and in the final review, folded back into the spec:
R9 — § 3.2.1's filter-name rule is corrected to Tera 2's span semantics (a
filter span starts at the filter name, so the leading identifier is taken);
R19 — a YAML literal is template source text, so the literal-JSON `for_each`
error and the validation message for an invalid literal report only the JSON
type; R20 — `indent` (`prefix=`, also `width=` and `indentation=`) and `unique` (Tera 1
case-insensitive default, `case_sensitive=`, `attribute=`) are ported as C3
compat overrides; R21 — `spaceless`, `is matching` and `get_random` are
dropped and documented in the upgrade guide.

**Revision 7 (2026-10-06, Codex spec review round 6, verdict "no").** Two
findings, both further instances of § 3.3.2's rule, verified and fixed by
making the rule structural instead of per call site:
(1) after a SUCCESSFUL lookup the foreign-type branch passes
`"{workspace}.{name}"` of the rendered reference to
`validation::check_connection_values`, whose errors and warnings (and a
`tracing::warn!`) print it (`template.rs:665-685`, `validation.rs:740`); the
type-mismatch error two lines above prints `conn_name` too — § 3.3.2 now
says a rendered reference never travels past the resolver: everything
downstream gets a FIELD LABEL;
(2) `precheck_task_step_literals` (`job_creator.rs:1359`) has its own
`{{`-only test — both pre-checks now use one `looks_templated` helper; the
other five `contains("{{")` sites are inventoried and deliberately left
alone (§ 3.3.2).

**Revision 6 (2026-10-06, Codex spec review round 5, verdict "no"; Codex
confirmed every Tera render path and the round-4 fixes).** Four findings,
verified and fixed, all in § 3.2.2 / § 3.3.2 / § 3.7:
(1) the inner resolver `resolve_connection_ref` / `found_config` put the
reference (`conn_ref`, its `ws` and `item` parts) into its OWN errors
(`template.rs:245-355`), so an outer field-only context would not hide it in
`{:#}` — the resolver now returns a value-free typed error and the caller
names the field; the grep covers `conn_ref` / `ws` / `item`;
(2) the creation-time pre-check calls any string without `{{` "literal",
missing `{% … %}` templates (`job_creator.rs:1311`) — the pre-check now
treats `{{`, `{%` and `{#` as template markers, and its 400 no longer echoes
the name at all;
(3) the corpus' raw-message assertion applies to value-bearing rendering
cases only; syntax, `Msg` and context-conversion cases get their own
fallback tests;
(4) `ValsFailure::BadOutput` gets the public category `vals returned invalid
output`.

**Revision 5 (2026-10-06, Codex spec review round 4, verdict "no").** Six
findings, all verified, all fixed:
(1) `throw(message=…)` (and any filter/function error) can FORGE a Tera
message shape, so a captured identifier is not proof of provenance — the
undefined variable/field path is no longer echoed; matched shapes now select
fixed text only (§ 3.2.1).
(2) The connection resolver echoes a RENDERED connection name
(`template.rs:653`) into claim errors; a transformed secret passes the
scrub — new § 3.3.2: post-render errors name the input field, never the
rendered value.
(3) `vals` stderr is arbitrary subprocess output — it no longer goes to any
server-side message or log; only the CLI's raw detail shows it (§ 3.2.3).
(4) `validation.rs` interpolates the whole `when` / `for_each` expression
into its errors — § 3.3 removes that; `stroem validate` / `stroem run` opt
in to raw detail at named call sites.
(5) The § 3.2.2 corpus could pass vacuously (Tera's report contains the
source line) — distinct source and context canaries; the raw assertion is
made on `ReportError::message()`; a forged-`throw` case is added.
(6) The `vals` side channel records a structured failure kind and exit
status, not a bool (§ 3.7).

**Revision 4 (2026-10-06, Codex spec review round 3, verdict "no"; user
decision).** Round 3's four findings share one root: Tera's error TEXT can
carry a context value — raw, or transformed by an earlier filter
(`{{ secret.X | upper | int }}`) — and no exact-value scrub matches a
transformed value; the same text then reaches load errors (`{e:#}` keeps the
whole chain, `entry.rs:173`), transient pin errors (`pins.rs:1529` →
`PinUnavailable`), secret-template errors (`{{ 'literal' | int }}`) and
server logs. This is not new — Tera 1 quoted the value in every filter type
error — but it does not converge by adding scrub forms. The user chose
**value-free errors by construction**: § 3.2 now builds every server-side
error message from a category, a position and names drawn only from closed
sets; Tera's message text never leaves `stroem-common` (only `stroem run`
shows it, § 3.2.3). Round 3's findings 1, 2 and 4 and F9's numeric residue
are closed by that one rule (§ 3.4.1 now says why per path, instead of adding
a guard per path); finding 3 is the rule. Removed as no longer needed: the
extra scrub forms (trimmed / radix-prefix-stripped values), the
`LoadRenderError` detail-to-log split. Kept: the scrub and origin-based
withholding as defence in depth, the shape-only `for_each` errors (§ 3.3.1 —
rendered content is not Tera error text), the two new hook / event-source
guards.

**Revision 3 (2026-10-06, Codex spec review round 2, verdict "no").** All four
findings verified and fixed:
(1) Load-time connection templates render with every workspace secret in the
context (`models/workflow.rs:1120`) and the error reaches
`WorkspaceInfo.error`; revision 2 gave that path no guard. New § 3.4.1: a
load-time render error shows users a fixed sentence (which secret/connection
field, which error category) and sends the scrubbed detail to the server log
only — the same split as owner-side withholding.
(2) `cascade.rs::render_for_each_template` puts the whole rendered value into
its error, so a transformed secret (`| upper`) passes the scrub. New § 3.3.1:
errors about a rendered value describe its shape, never its content, with a
transformed-value canary.
(3) The `Msg` allow-list passed the unknown name through, and an
identifier-shaped credential can sit there. § 3.2: fixed message per kind; a
name is shown only when it is one of Tera 1's builtin names (a list in our
code), which is what an author upgrading actually hits.
(4) Other error kinds were passed through via `Display`, which includes
`OutOfRangeArgument`'s value. § 3.2: each kind is converted field by field;
unknown kinds get a fixed fallback. F9 gains numeric-operation quoting.
User decisions recorded: Q1 (C4, empty array/map and numeric zero falsy in
`when:`) accepted; Q2 (`urlencode`, `urlencode_strict`, `slugify`,
`filesizeformat`, `striptags`) dropped and listed in the upgrade guide.

**Revision 2 (2026-10-06, Codex spec review round 1, verdict "no").** All six
findings verified against the code and fixed:
(1) Tera 2 reports unknown filters/tests/functions as `ErrorKind::Msg` whose
text is a pre-rendered report INCLUDING the source line — § 3.2 now converts
`Msg` through an allow-list, never passes its text on.
(2) Template source also reaches errors through our own `.context()` calls
(`render_json_strings`, `render_value_deep` embed the whole template) and the
"failing expression" of revision 1 could itself hold a literal credential —
§ 3.2 drops the expression entirely (message + position only), new § 3.3
removes template text from every error context.
(3) Two render paths log unscrubbed text today (event-source env, hook input —
the latter into the SOURCE job's log): new § 3.4 inventories every render
path with its scrub/withhold point and closes both.
(4) `render_str` rejects blocks/`extends` where `add_raw_template` accepts them
— § 3.1 validation now goes through `render_str` itself; F2 no longer
overstates what validation covers.
(5) Aliases do not reproduce Tera 1 output (`as_str` on objects,
`linebreaksbr` on lone CR, escaped trim patterns) — C3 now ports Tera 1's
implementations; C4 is described as the Strøm convention it is.
(6) The quoting inventory gains our `date` / `now` ports; every affected
security fixture (claim, cross-workspace, git-refs) gets a positive canary.
Q3 (show the expression?) is resolved by (2): no.

**Revision 1 (2026-10-06).** First draft.

## 1. Goal

Move `tera` 1.20 → 2.4 with:

1. **No context value in any server-side template error.** An error message
   produced by rendering or compiling a template contains no context value
   and no template text, raw or transformed — by construction (§ 3.2), not
   by scrubbing. Scrubbing and origin-based withholding stay as defence in
   depth. Every security test proves its fixture WOULD leak through Tera's
   raw text (none passes vacuously).
2. **The smallest user-visible break we can reasonably buy.** Where Tera 1
   behaviour can be kept with a small, local shim, keep it. Where it cannot,
   document it in an upgrade guide.
3. **One engine, one compile path.** Rendering, `stroem validate` and server
   workspace validation use the same configured `Tera` and the same
   `render_str` entry point, so validation accepts exactly what rendering
   compiles.

Non-goals: adopting Tera 2's new syntax in our docs/examples; changing who
builds the render context (`render_context::build` stays the single owner);
extending validation to template fields it does not check today.

## 2. Facts (verified in the tera 2.4.0 source)

- F1. The Rust surface is in `stroem-common` only: `template.rs` (render, the
  `vals` filter, `is_vals_failure`) and `validation.rs` (four syntax checks:
  `when`, `for_each`, agent `prompt`, agent `system_prompt`). No `tera` type
  crosses the crate's public API.
- F2. Tera 2 checks that every filter/test/function a template uses exists
  when the template is compiled (`validate_template_references`). Today we
  add the template and then register `vals`, so every `| vals` template would
  fail; the bare `Tera::default()` / `Tera::one_off` checks in
  `validation.rs` would reject those four fields when they use `| vals` or
  `| json_encode`.
- F3. `register_filter` overwrites an existing filter of the same name.
  `default` is an ordinary builtin filter that receives a `Value`, including
  an undefined tail (the VM passes it to filters); it substitutes only
  `ValueKind::Undefined`. Tera 1 also substituted `null`.
- F4. Path resolution (`Instruction::LoadPath`): only the LAST segment may be
  undefined. A missing intermediate segment, or a path rooted at an undefined
  variable, is a hard error even under `| default`. A `null` root/segment is
  not undefined: `none.x` resolves to an undefined tail. Today
  `render_context` leaves `state` / `global_state` ABSENT when there is no
  snapshot.
- F5. `and` / `or` return an operand (`{{ [] and x }}` renders `[]`); an
  undefined operand that ends up rendered is an error.
- F6. Rendering: maps render as `{"k": v}` (Tera 1: `[object]`), string arrays
  as `["a", "b"]` (Tera 1: `[a, b]`), floats keep `.0`, `/` always yields a
  float.
- F7. Removed from core: `json_encode`, `date`, `now`, `urlencode(_strict)`,
  `slugify`, `filesizeformat`, `striptags` (moved to `tera-contrib`); `map`,
  `filter`, `concat`, `slice`, `addslashes`, `get_env` (gone); renamed
  `as_str` → `str`, `trim_start_matches` / `trim_end_matches` →
  `trim_start(pat=)` / `trim_end(pat=)`, `linebreaksbr` → `newlines_to_br`,
  each with different edge-case output (see § 3.6 C3). Tests take keyword
  arguments only; `divisibleby` → `divisible_by`, `object` → `map`.
- F8. Errors.
  - The VM flattens every filter/function/test error into
    `RenderingError(format!("{err}"))` — no `CallFilter` kind, no source chain.
  - `SyntaxError` / `RenderingError` carry a `ReportError` with a structured
    message and span; their `Display` is a multi-line report including the
    template SOURCE LINE; `Debug` includes the whole source.
  - Unknown filter/test/function errors from `render_str` are
    `ErrorKind::Msg(reports.join("\n\n"))`: pre-rendered reports, source lines
    included, no structured span (`tera.rs:1201-1207`, `reporting.rs:59-72`).
    `render_str` also returns fixed `Msg`s for `{% extends %}` and blocks.
  - "Field `x` is not defined. Available fields: …" lists the parent map's
    keys; "Available variables" lists context keys.
- F9. Value quoting in errors (full list in the research notes § 3.1). Type
  errors no longer quote the value (`{{ secret.X | round }}` → "expected `f64`
  but got `string`"). Still quoted raw: `round(method=X)`, `get(key=X)`,
  `throw(message=X)`. Quoted transformed: `int` / `float` trim the string,
  `int(base=2|8|16)` strips the `0b`/`0o`/`0b` prefix. Our `vals` quotes its
  stderr. Our `date` port (§ 3.6 C3) quotes an unparsable input, as Tera 1's
  did. (tera-contrib's `date` also quotes input and kwargs; we do not use it.)
  Numeric operations quote operands as numbers ("Unable to perform {lhs} +
  {rhs}", `OutOfRangeArgument { value }`): a number derived from a context
  value appears in its numeric `Display` form, which an exact-string scrub
  matches only when it equals the source string (`"1234"` yes, `"007"` no).
  None of this text reaches a server-side error (§ 3.2).
- F10. Without the `preserve_order` feature Tera's map is a `HashMap`, so
  `json_encode` key order would vary per process (Tera 1: sorted).

## 3. Design

### 3.1 One engine, one compile path

`stroem_common::template` owns a process-wide `LazyLock<Tera>`:
`Tera::default()` plus everything in § 3.6. Each render clones it (filters are
`Arc`ed), registers the per-call `vals` closure (it captures the
`LoadBudget` and the § 3.7 side channel) BEFORE compiling, then calls
`render_str(template, &ctx, autoescape = false)`.

`check_template_syntax(src)` calls `render_str` too — on the same engine with
`vals` registered as a side-effect-free identity — with an empty context, and
classifies the outcome by error kind: `SyntaxError` or `Msg` (parse error,
unknown filter/test/function, `extends`, blocks) is a validation error;
`RenderingError` or success means the template compiled. This exercises
exactly the one-off restrictions rendering applies. No registered filter has a
side effect in that engine (`vals` is the identity; `now`/`date` read the
clock). The four checks in `validation.rs` call it; the separate `one_off`
pass goes away.

Validation becomes STRICTER for those four fields: an unknown filter, test or
function is a validation error (also in a branch that never runs). Other
template fields remain unvalidated at load (unchanged). The comment in
`validation.rs` and `guides/workflow-basics.md` saying unknown filters are
only caught at execution time are rewritten.

Cargo: `tera = { version = "2.4", features = ["preserve_order"] }` (F10),
`tera-contrib` with only the features § 3.6 needs.

### 3.2 `TemplateError`: value-free by construction

Every Tera error is converted at the `stroem-common` boundary into
`template::TemplateError`. No raw `tera::Error`, and no part of its text,
leaves the crate on any server path.

#### 3.2.1 The message

`TemplateError`'s message is assembled by us from three parts, each from a
closed or value-free source:

1. **Category** — a fixed sentence chosen from the error kind:
   `template syntax error` (`SyntaxError`); `template rendering failed`
   (`RenderingError`); for `Msg`: `template uses an unknown filter` /
   `test` / `function`, `{% extends %} is not supported`,
   `{% block %} is not supported`, else `template could not be compiled`;
   for the argument kinds: `a filter received a value of the wrong type`,
   `a filter call is missing a required argument`,
   `a number is out of range`; every other kind, current or future (the enum
   is `non_exhaustive`): `template rendering failed`. `vals` failures (§ 3.7
   side channel — not message text) override the category:
   `vals failed (exit status N)`, `vals timed out`,
   `vals could not be started`, `vals returned invalid output`.
2. **Position** — `(line L, column C)` from the `ReportError` span where
   there is one.
3. **Detail, only from closed sets.** Tera's message text is never copied.
   Because a filter or function error can FORGE any message shape
   (`throw(message=…)` returns its argument as the error text), matching a
   shape proves nothing about where its parts came from, so a match may only
   SELECT fixed text or a member of a closed set — it never captures free
   text:
   - `Variable … is not defined` / `… exists but its value is undefined` /
     `Field … is not defined` → `undefined variable or field`. The name is NOT
     echoed (round 4: a forged message could carry any value there); the
     position locates it.
   - `Invalid type for the value, expected \`T\` but got \`U\`` → the two
     type names, each emitted only if it is a member of Tera 2's type-name set
     (`string`, `i64`, `f64`, `bool`, `array`, `map`, `none`, …, listed in
     code); otherwise the generic argument sentence.
   - Tera's placeholder-free constant messages (`Cannot divide by 0`,
     `Slicing step cannot be 0`, …) → verbatim, matched by EQUALITY against a
     list in code (a forged copy yields the same constant — harmless).
   - For `Msg` unknown-reference errors: the name only when it is in
     `TERA1_BUILTIN_NAMES` — then ``filter `map` is not available in
     Tera 2; see the upgrade guide``.
   - The failing filter's NAME: a Tera 2 filter span starts at the filter
     name, so the span's leading identifier is taken, and accepted only when
     it is followed by the end of the span or `(` and is in the engine's
     registered-filter list (`REGISTERED_FILTERS`, a closed set in code):
     `filter \`int\` failed`. Best effort; may name the wrong filter in an
     odd span, can never emit anything outside the list.
   Anything not matched contributes nothing. A Tera wording change therefore
   degrades a message to its category — never to a leak.

`Display` is the one-line assembly; `Debug` is hand-written (category,
position, flags) like `RedactionMemo`. `{:#}` of an `anyhow` chain holding a
`TemplateError` is value-free as long as the rest of the chain is (§ 3.3).

#### 3.2.2 Drift detection

A unit test per allow-listed shape renders a template that triggers it and
asserts the ENRICHED message (so a Tera wording change fails a test rather
than silently degrading).

A corpus test uses two DISTINCT canaries: `CONTEXT_CANARY` as a context
value and `SOURCE_CANARY` as a string literal in the template source, and
two groups of cases:

- **Value-bearing rendering cases** — one per quoting path in F9
  (`round(method=…)`, `get(key=…)`, `int`/`float` after `upper`, numeric
  operations, `throw`, our `date`, `vals`). Each first asserts the expected
  raw or transformed `CONTEXT_CANARY` IS in Tera's `ReportError::message()`
  (the message before report formatting, so the source line cannot satisfy
  it — the fixture is proven real), then that the `TemplateError`'s
  `Display`, `{:#}` (wrapped in `anyhow` with our contexts) and `Debug`
  contain neither canary nor any transformed form. A forged case renders
  `{{ throw(message=secret.X) }}` where `secret.X` is
  ``Variable `CONTEXT_CANARY` is not defined`` and asserts the same.
- **Cases with no `ReportError` or no context evaluation** — a syntax error
  next to `SOURCE_CANARY`, every `Msg` form (unknown filter / test /
  function, `extends`, blocks, an unmatched report), a `Context` conversion
  failure. Each asserts the exact fixed fallback text and that neither canary
  appears in `Display`, `{:#}` or `Debug`.

#### 3.2.3 Full detail locally

`TemplateError::raw_detail()` returns Tera's original text plus, for a
`vals` failure, its stderr. Only `stroem-cli` calls it, at two named sites:
`local/validate.rs` (the validation error printer), the `stroem run`
printers and `main`'s top-level error print (`stroem.rs`) — each walks the `anyhow` chain, and for a `TemplateError`
prints `raw_detail()` under the value-free line. The operator holds every
secret anyway. A grep-based test fails the build if any other crate calls
it.

### 3.3 No template text in error contexts

Our own error chains embed template source today:
`render_json_strings` (`template.rs:387`, used for action manifests and args)
and `render_value_deep` (`template.rs:778`) attach the whole template string.
Both switch to naming the location instead (the JSON path of the failing
string, e.g. `manifest.spec.containers[0].image`). The § 3.2.2 corpus also
runs through every public render function in `template.rs` (not just the
core render), so an added context that interpolates template text fails it.
A grep-based test fails the build if a `.context()` / `.with_context()` in
`stroem-common/src/template.rs` interpolates a template-source variable (the
names are fixed: `s`, `template`, `src`).

`validation.rs`' template checks (today `invalid when expression '{expr}':
{e}`, `validation.rs:299,327`) name the task, step and field only, followed
by the `TemplateError`; the CLI shows the raw detail (§ 3.2.3).

#### 3.3.2 No rendered values in post-render errors

A value produced by rendering can be a transformed secret, so an error about
it — after rendering succeeded — names the INPUT FIELD and the value's type,
never the value or any part of it.

- **Connection resolution: a rendered reference never travels past the
  resolver.** `resolve_connection_ref` and `found_config`
  (`template.rs:245-355`) today put the reference (`conn_ref`) and its
  parsed `ws` / `item` parts into their own errors; the caller
  `resolve_connection_inputs_scoped` (and its provenance / role variants)
  adds ``references connection '{conn_name}'`` (`template.rs:653`), prints
  `conn_name` in the type-mismatch error (`template.rs:667`), and, after a
  successful lookup, passes `"{resolved.workspace}.{resolved.name}"` to
  `validation::check_connection_values` (`template.rs:684`), whose
  missing-field / empty-value errors and unknown-field warnings print it,
  as does the `tracing::warn!` of those warnings. New rule: inside the
  resolver every message is built from a `ConnectionLabel` — the FIELD
  (``input field 'db'``) — and never from the reference or the resolved
  workspace/name:
  - the resolver returns a value-free typed `ConnectionRefError`
    (`NotFound`, `UnknownWorkspace`, `WorkspaceUnavailable`, `NotShared`,
    `TypeMismatch { expected, found }` with type refs from config, …) and the
    caller REPLACES it with ``Input field 'db': <kind sentence>`` — not a
    `.context()` on top of a value-bearing error;
  - `check_connection_values` takes the label as its first argument instead
    of a connection name; the workspace-load caller (connections from YAML,
    whose names are config keys) passes ``connection 'name'``, the resolver
    passes the field label;
  - the type-mismatch error names the field and the two type refs only.
  Tests, each checking the full `{:#}` chain AND captured log output: a
  connection name rendered from `{{ secret.X | upper }}` that (a) does not
  resolve, per `ConnectionRefError` kind; (b) resolves to a foreign-typed
  connection with a missing required field, an empty value and an unknown
  field (error + warnings); (c) resolves to a connection of the wrong type.
- **Creation-time pre-checks.** Both `precheck_literal_connection_inputs`
  (`job_creator.rs:1312`) and `precheck_task_step_literals`
  (`job_creator.rs:1359`) decide "literal vs template" with
  `contains("{{")`, so a `{% if … %}` value is pre-checked as if literal (and
  can be wrongly rejected at creation). Both switch to one helper,
  `template::looks_templated(s)` = contains `{{`, `{%` or `{#`, and skip such
  values as they skip `{{ … }}` today. Their 400s name the field and the
  kind, never the connection name. Tests include a block-tag and a comment
  template for each. The other `contains("{{")` sites are deliberately
  unchanged, because there a wider test would RENDER strings that are
  literal today (a password containing `{%` would start failing to load):
  secret values (`models/workflow.rs:1160`), image (`rendering.rs:287`), the
  simple-path passthrough and deep render (`template.rs:550`, `:776`);
  `git_ref.rs:55` already checks `{{` and `{%`.
- **Review rule with a grep:** no `format!` / `bail!` / `anyhow!` / `context`
  in `template.rs`, `validation.rs` (`check_connection_values`),
  `rendering.rs`, `cascade.rs`, `dispatch.rs`, `job_creator.rs` interpolates
  a variable holding a rendered value or a part of one (`rendered`,
  `conn_name`, `conn_ref`, `ws`, `item`, `value`, `resolved.*`) — type names,
  labels and field names only.

#### 3.3.1 No rendered content in errors about a rendered value

A rendered value can carry a TRANSFORMED secret (`{{ secret.X | upper }}`,
`| replace`, `| json_encode`, string slicing). An error ABOUT a rendered
result describes its shape only: `cascade.rs::render_for_each_template`
reports "for_each must render a JSON array; the rendered text (N bytes) is
not valid JSON: {serde_json error category, line, column}" or "…rendered a
JSON {object|string|number|bool|null}, not an array" — never the rendered
text or value (today it embeds both). Same rule for the CLI's `for_each`
(`stroem-cli/src/local/run.rs:657`). Canary: `for_each:
"{{ secret.X | upper }}"` — the persisted step error must not contain the
upper-cased secret. A grep over `crates/` for `format!`/`bail!`/`context`
sites interpolating a variable named `rendered` is part of review.

### 3.4 Every render path

| Path | Error goes to | After this change |
|---|---|---|
| Load-time secrets / connections (`models/workflow.rs::render_secrets_with`, `render_connections_with`) | `WorkspaceInfo.error` (API, MCP) via `entry.rs:173` `{e:#}`; startup / watcher logs (`workspace/mod.rs:313`, `watcher.rs:204`); pin load results (`pins.rs:1529` → `PinUnavailable` / `PinLoadFailed`) | value-free (§ 3.4.1); unchanged plumbing |
| Claim: step input, action body, image, agent prompts (`web/worker_api/rendering.rs`, `jobs.rs`) | job log, `job_step.error_message`, 422 body | value-free; post-render connection errors name the field (§ 3.3.2); `fail_claimed_step` scrub + origin withholding kept |
| Cascade: `when`, `for_each` (`cascade.rs`) | job log, step error | value-free; `for_each` shape-only (§ 3.3.1); scrub kept |
| Dispatch: task-step input, approval message (`settlement/dispatch.rs`) | job log, step error | value-free; `fail_task_step` scrub + withholding kept |
| Hooks: hook input (`settlement/hooks.rs::fire_single_hook`) | `tracing::error!` and the SOURCE job's `_server` log | value-free; **NEW scrub** with the source job's redaction values (today unscrubbed) |
| Event source: env (`event_source.rs`) | `tracing::warn!` | value-free; **NEW scrub** with the workspace's secret values (today unscrubbed) |
| CLI `stroem run` / `validate` | the operator's terminal | full Tera detail (§ 3.2.3) |

Any new path that renders a template and reports the error must appear in
this table (CLAUDE.md § Secrets in logs gains this rule).

#### 3.4.1 Why the load-time and pin paths need no new plumbing

Their published text is an `anyhow` chain of our own contexts (`Failed to
render secret 'K'`, `Failed to render connection 'C' field 'F'` — config
names) around a `TemplateError`. Both parts are value-free, so `{e:#}` in
`entry.rs:173`, the startup / watcher log lines and the error text copied into
`PinUnavailable` (`pins.rs:703`) are value-free without changes there. That
holds for a secret template's own literals too (`{{ 'literal' | int }}` →
`filter \`int\` failed (line 1, column 4)`). `vals`' stderr never reaches a
server-side message or log: the category carries the failure kind and exit
status only, and the stderr is kept for `raw_detail()` (CLI). Test: a connection template `{{ secret.X | upper | int }}` with
a non-numeric secret — raw Tera error contains the upper-cased secret
(asserted), `WorkspaceInfo.error` / the `PinUnavailable` text / the
startup log line do not.

### 3.5 Security tests stay meaningful

- The § 3.2.2 corpus is the canary: it proves each fixture leaks through
  Tera's RAW text before proving it does not leak through ours, so the next
  Tera upgrade cannot silently empty it.
- **Re-fixture** every scrub/withhold regression test whose fixture relied on
  Tera 1 quoting (`secret.X | round`, `json_encode | round`), in
  `stroem-common`, `cascade.rs`, `integration_test.rs` (claim and
  cross-workspace), `git_refs_claim_test.rs` (owner/caller/own-ref): each
  uses a shared fixture whose raw Tera error demonstrably carries the secret
  (`1 | round(method=secret.X)`, and `secret.X | upper | int` for a
  transformed value), asserted in a `stroem-common` unit test over the same
  constant. The end-to-end assertion (no secret in the persisted error / 422
  body / job log) now holds by § 3.2 first and by the scrub second.
- The span-union scrub tests (PREFIX/crossing occurrences) move to synthetic
  input strings: they test `redact_secrets_in_str` itself, which no longer
  sees Tera text.
- New tests for § 3.3 (no template text in any chain), § 3.3.1, § 3.4.1 and
  the two new guards (hook input, event-source env).
- Origin-based withholding (`ProvenanceError`, `OwnerSideRender`,
  `withheld_owner_error`, `fail_task_step`) is unchanged.
- **Accepted, documented**: `{{ obj }}` renders a whole object, so
  `"dsn={{ input.db }}"` writes a connection's properties — password
  included — into the step input where Tera 1 wrote `[object]`. That is
  rendered OUTPUT, not an error; it is masked on read when the property is
  `secret: true` (unchanged), and `guides/secrets.md` gets a warning.

### 3.6 Compatibility layer (Tera 1 behaviour kept)

Registered on the shared engine; each with a unit test pinning the Tera 1
result, including the edge cases Tera 1's own tests cover.

- **C1 `default` also replaces `null`.** Overrides the builtin (F3): `null`
  or undefined → `value`; `boolean=true` keeps Tera 2's truthiness mode.
  Without it every `{{ step.output.x | default(…) }}` over a skipped, failed
  or suspended step (whose `output` is `null` in our context) changes meaning.
- **C2 `state` / `global_state` are `null` when there is no snapshot**
  (`render_context::build`, the single owner). Keeps
  `{{ state.x | default(value=0) }}` and `{% if state.x %}` working on a
  task's first run (F4) — the user's own jobs-playground relies on it.
  `{{ not state }}` stays true; only `{{ state is defined }}` flips (no usage
  found).
- **C3 Tera 1 filters restored by porting Tera 1's implementations** (MIT,
  attribution in the module), not by aliasing Tera 2's lookalikes, so output
  is byte-identical: `as_str` (Tera 1 rendering — objects `[object]`, arrays
  `[a, b]`), `trim_start_matches` / `trim_end_matches` (including the
  `\\n`/`\\t` pattern unescaping), `linebreaksbr` (`\r\n` and `\n` only),
  `map(attribute=)`, `filter(attribute=, value=)`, `concat(with=)`,
  `slice(start=, end=)`, `date(format=, timezone=)` and the
  `now(timestamp=, utc=)` function (chrono / chrono-tz, already
  dependencies). `json_encode` from tera-contrib (`json` feature), with a
  parity test against Tera 1 output (key order, floats, `pretty=true`).
  Not restored (user decision Q2, listed in the upgrade guide): `urlencode`,
  `urlencode_strict`, `slugify`, `filesizeformat`, `striptags`; also
  `addslashes`, `get_env` (removed deliberately upstream), test names and
  positional test arguments (parser-level). Their names are in
  `TERA1_BUILTIN_NAMES` (§ 3.2), so using one gives the "not available in
  Tera 2" message.
- **C4 `when:` falsiness, a Strøm convention over the rendered text.**
  `evaluate_condition` treats as false: empty, `false`, `null`, `none` (today),
  plus any text that parses as a number equal to zero (`0`, `0.0`, `-0.0`),
  `[]` and `{}`. This makes `and` / `or` results (F5) and float output (F6)
  evaluate the way the expression read. Changes one Tera 1 case:
  `when: "{{ step.output.items }}"` over an empty array was truthy (it rendered
  `[]`) and is now false (user decision Q1: accepted). Truth-table test.

### 3.7 `vals` failure detection

`is_vals_failure` (used by the PinStore to classify a pin's load failure as
secret-class) cannot read Tera's error kind any more (F8). The per-render
`vals` closure records a structured failure in a side channel —
`ValsFailure { kind: Exited(code) | TimedOut | SpawnFailed | BadOutput,
stderr }` — when it fails; `TemplateError` carries it (the kind feeds the
§ 3.2.1 category, the stderr only `raw_detail()`); `is_vals_failure`
downcasts to `TemplateError`. Classification is
unchanged, including today's (inconsistent) treatment of a `vals` deadline as
secret-class; that inconsistency goes to TODO.md, not into this change.

### 3.8 Smaller items

- **Reserved names**: a step named after a Tera 2 keyword (`none`, `null`,
  `self`, `loop`, `break`, `continue`, `true`, `false`, `and`, `or`, `not`,
  `is`, `in`, `if`, `else`) is no longer addressable. Logged as a collision
  where `FRAMEWORK_KEYS` collisions are logged; listed in the upgrade guide.
  Not rejected (it would break loading a workspace that never references it).
- **`for_each` hint**: the `[object]` detection in `cascade.rs` can no longer
  fire; replaced by a generic "render the array with `| json_encode()`" hint
  when the rendered text is not a JSON array.

### 3.9 What still breaks (upgrade guide)

`docs/src/content/docs/operations/upgrade-tera-2.md`, linked from the
templating guide:

1. Only the last path segment may be undefined: `a.b.c | default(…)` with `b`
   missing, `{% if a.b %}` / `not a.b` / `a.b is defined` with `a` undefined
   are errors (use `a?.b`).
2. `and` / `or` return an operand; an undefined operand that is rendered is
   an error (`{{ input.flag and … }}` with `flag` absent). `when:` evaluates
   the result with § 3.6 C4's rule.
3. Comparing against a missing field is `false`, not an error, so a `when:`
   that used to fail its step may now skip it. `~` and `json_encode` on a
   missing field yield `""` / `null`.
4. Output format: objects render `{"k": v}`, arrays quote strings, floats keep
   `.0` (`/`, `round`).
5. String literals process backslash escapes; an unknown escape (`'\d'`,
   `'C:\path'`) is a syntax error.
6. Tests take keyword arguments (`starting_with(pat="x")`), `divisibleby` →
   `divisible_by(divisor=)`, `object` → `map`; macros and `.0` indexing are
   gone.
7. `int` / `float` error on unparsable input (Tera 1 returned 0);
   `round(method="common")` is invalid; `truncate` needs `length`;
   `first`/`last`/`nth` on an empty array give `none`.
8. Unknown filters/tests/functions fail when the template is parsed (also in
   untaken branches); `stroem validate` reports them for `when`, `for_each`
   and agent prompts.
9. Error messages are value-free: a category, the position and at most a
   type name or the failing filter's name; never Tera's own text, a value, a
   variable name or the template line. `stroem run` and `stroem validate`
   show Tera's full report locally.
10. Reserved step names (§ 3.8).
11. Filters not restored by C3: `urlencode`, `urlencode_strict`, `slugify`,
    `filesizeformat`, `striptags`, `addslashes`, `get_env`.
12. `when:` treats a rendered `[]`, `{}` and numeric zero (`0.0`) as false
    (C4).
13. A workspace that fails to load because a secret or connection does not
    render reports which one plus the value-free message (§ 3.4.1); to see
    Tera's or `vals`' full output, run `stroem validate` / `stroem run`
    locally.

### 3.10 Rollout

Templates render on the server (claim, cascade, dispatch, hooks, event
sources) and in the CLI, never on workers. All server replicas must run the
same Tera: a job can be claimed on one replica and cascaded on another. The
release is a breaking release; the upgrade guide is linked from the release
notes.

## 4. Testing

- `stroem-common`: the § 3.2.2 drift and corpus tests; C1–C4 each against
  its Tera 1 result, including Tera 1's own edge-case tests for the ported
  filters; truth table for `evaluate_condition`; `json_encode` parity;
  `check_template_syntax` accepts `vals` / `json_encode` / every C3 name and
  rejects an unknown filter, a block and `extends`; `is_vals_failure` true
  for a `vals` failure and false for any other render error; each
  `ValsFailure` kind maps to its category and never puts stderr in
  `Display`; the § 3.2.3, § 3.3 and § 3.3.2 grep tests; the § 3.3.2 resolver
  tests (a)–(c) (full `{:#}` chain and captured logs) and pre-check tests
  (block-tag and comment templates skipped by both pre-checks; 400 without
  the name).
- Server: the § 3.4.1 load-time test, § 3.3.1 `for_each` canary, the hook and
  event-source guard tests, and the re-fixtured security tests (§ 3.5).
- `cargo test --workspace`, then `tests/e2e.sh` (state `| default`,
  multi-select `json_encode`, cross-workspace connections) — CI if local disk
  does not allow.
- `stroem validate` over `workspace/`, `tests/` fixtures and the user's
  jobs-playground.

## 5. Docs

Templating guide (Tera 2 links, C1/C2 notes, reserved names), conditionals
(C4 truth table, error section), task state (`| bool` does not exist in either
version — fix), loops (`json_encode`), secrets (§ 3.5 accepted item, value-free
errors),
action types (the pre-existing invalid f-string example), workflow basics
(validation strictness), CLAUDE.md § Secrets in logs (value-free template
errors replace "Tera quotes the offending value"; the § 3.4 table rule; the
§ 3.3.2 post-render rule) and § Tera Templating, TODO.md, the upgrade guide,
`llms.txt`.

## 6. Decisions

- Q1 (2026-10-06, user): C4 accepted — empty array/map and numeric zero are
  false in `when:`.
- Q2 (2026-10-06, user): `urlencode`, `urlencode_strict`, `slugify`,
  `filesizeformat`, `striptags` are dropped, not restored.
- Q3 (2026-10-06, user): template errors are value-free by construction
  (§ 3.2), rather than porting Tera 1's scrub-only model and documenting the
  transformed-value residue.
