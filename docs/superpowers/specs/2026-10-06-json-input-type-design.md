# `json` input type

Status: revision 2, proposed (2026-10-06)

A new task/action input field type, `type: json`, that holds any JSON value
and keeps it structured through templates. Facts below are verified at
`b310bb44` (v0.18.0).

## Revision history

**Revision 2 (2026-10-06, Codex spec review round 1, verdict "no").** Every
finding verified against the code; all held. (1) Numeric secrets never reach
the redaction set — `collect_strings` collects strings only (F11) — so
masking numbers alone matched nothing: the collector now collects numbers as
their JSON text (§ 9, D7). (2) An unchanged re-run could submit job detail's
`••••••` markers inside a `json` value as data: a `json` field whose
prefilled re-run value carries a marker is now replayed WHOLE by the server
from the source's stored `raw_input` (§ 7, D8; `resolve_rerun_sentinels`
extended, F12). (3) Editing one leaf of a templated default sent the other
leaves as template text: a templated default is never placed in the editor
(§ 7 "default" mode, Override starts empty). (4) Boolean secrets stay
unmasked as values — now an explicit, documented policy with its reason
(D7). (5) Claim-time schema source was unstated: claim now classifies `json`
fields by the step's persisted `action_spec.input` (F7, D10), which also
removes revision 1's claim restructure. (6) The error fallback re-rendered the
expression (a second `vals` call): wrapper error positions are now mapped
back to the author's text, with no second render (§ 4.2, D11). (7) The
"unchanged text → omit" rule conflated three intents: replaced by explicit
form modes (§ 7, D8). (8) CLI action defaults row added to § 6. Corrections:
F4 cites Tera's `format_map`; § 5.2 says exactly which lists are replaced;
§ 9's short-secret example was wrong (values of ≤ 3 characters are never
redaction values, F11).

**Revision 1 (2026-10-06).** First version. Decisions D1–D9 (§ 3) were made
with the user in the design conversation.

## 1. Goal

Four uses, all wanted:

1. **UI form payload** — a person pastes or edits a structured value (a
   config, a list of targets) in the Run Task form.
2. **Data between steps** — a flow step or `type: task` step passes
   `{{ prev.output.items }}`, `{{ each.item }}` or `{{ x | length }}` into an
   input and the value stays an object / array / number, not a string.
3. **Declared shape for API / webhook / MCP / agent callers** — a field that
   says "this is structured" instead of being left undeclared.
4. **Inline connection-like config** — an action input whose default is an
   object built from secrets (`{ host: "{{ secret.h }}", port: 5432 }`), the
   shape `merge_action_defaults`'s doc comment already uses as its example
   (`template.rs:812-818`).

Non-goals: validating values of other types (no type checks exist today,
F1, and this change adds none); a JSON Schema for the shape of a `json`
value; `secret: true` on `json` (D4); `json` in approval forms (D5); any
change to how non-`json` fields render.

## 2. Facts

- **F1 — no value/type checks.** `merge_defaults` (`template.rs:567`) only
  fills absent fields; values present pass through, and fields the schema
  does not declare pass through too. An `integer` field accepts a string
  today. A webhook `body` arrives as an object (`web/hooks.rs:646`) and
  works because most tasks do not declare it.
- **F2 — non-primitive = connection.** `PRIMITIVE_TYPES`
  (`template.rs:611-613`: `string text integer number boolean date
  datetime`) is the switch; every type outside it is a connection-type
  reference. Consumers: connection resolution (`template.rs:656`), re-run
  sentinel replay (`template.rs:766`), the cross-workspace "connection must
  cross as a name" rule (`template.rs:1219`), literal pre-checks
  (`job_creator.rs:1308`, `:1355`), the connection dropdown list
  (`web/api/tasks.rs:291`), and the UI mirror
  (`ui/src/components/task/constants.ts:7`). `validation.rs` keeps its own
  three copies (`:761`, `:769` reserved names, `:958`).
- **F3 — templates render to strings.** `render_input_map`
  (`template.rs:484`) renders top-level strings only (non-string values pass
  through unrendered, `:524`). Its one exception: a string that is exactly
  `{{ a.b.c }}` (`extract_simple_variable_path`, `:535` — identifiers and
  dots, no filters) whose path holds an object or array returns that raw
  value (`:499-509`), so connection objects survive `{{ input.db }}`. Every
  other template — filters, arithmetic, scalars — becomes a string.
- **F4 — Tera 2's own output is not JSON.** `Value::format`
  (`tera-2.4.0/src/value/mod.rs:476-497`) and `format_map` (`:34-58`) write
  strings inside arrays and maps with Rust `{:?}` (control characters come
  out as `\u{1b}`, invalid JSON) and write `None`/`Undefined` as nothing
  (`{"a": }`). The upgrade
  guide's `{{ obj }}` → `{"k": "v", "n": 1}` row
  (`operations/upgrade-tera-2.md:127`) holds for simple values only.
  `json_encode` (registered in `tera_engine.rs:10`) is the stable encoder.
- **F5 — `{{ }}` and `{% set %}` parse the same expression.** Both call
  `parse_expression(0)` (`tera-2.4.0/src/parsing/parser.rs:1529` for `set`,
  `:1816` for a variable block). Tera 2 exposes no AST and no
  evaluate-to-value API (public surface `lib.rs:84-101`; `Tera` has
  `render*` only).
- **F6 — precedent for decoding a render.** `for_each` renders text and
  decodes it with `serde_json` (`cascade.rs:100-124`); its error names the
  byte count, error class, line and column, never the text.
- **F7 — claim has the action's schema on the step row.**
  `render_step_input` (`web/worker_api/rendering.rs:86`) renders the flow
  step's `input:` before `prepare_step_action_input` (`rendering.rs:96-140`)
  looks the action up in the job's workspace config (live, or the pin's for a
  pinned job) for its defaults. But every step row already carries the full
  `ActionDef` as persisted at creation, `action_spec`
  (`job_creator.rs:762`, `serde_json::to_value(action)`), including its
  `input` schema; claim renders the action body from that persisted spec
  (`web/worker_api/jobs.rs:1127`), and `for_each` instance rows copy it from
  the placeholder (`cascade.rs:538`). The persisted spec is the owner's
  action for a cross-workspace step and the pin's for a ref'd one.
- **F8 — user values are never rendered.** Only schema defaults (absent
  fields), flow-step `input:`, hook `input:` and action bodies are rendered.
  The UI prefills a string field's default verbatim
  (`ui/src/pages/task-detail.tsx:131-132`) and submits it
  (`ui/src/lib/execute-input.ts:20-27`), so a non-secret default
  `"{{ secret.X }}"` submitted unchanged reaches the job as template text.
- **F9 — redaction masks strings only.** `map_strings`
  (`redaction.rs:555-570`) applies the mask to `Value::String` and skips
  numbers, booleans and nulls, for both `redact_value_tree` (`:545`) and
  `mask_value_tree` (`:550`).
- **F10 — the names are free.** `object` and `array` are already reserved
  connection-type names (`validation.rs:769-772`); `json` is not.
- **F11 — the redaction set holds strings only.** Every set is built by
  `workspace_set::collect_strings` (`workspace_set.rs:268-274`), which
  collects `Value::String` and skips numbers, booleans and nulls: the live set
  (`collect_redaction_values`, `:228`), each pinned commit's set (the same
  function, `redaction.rs:395`) and the per-config scrub set
  (`collect_config_secret_values`, `:103`, used by the cascade, dispatch,
  hooks and event sources). `collect_redaction_values` drops values of 3
  characters or fewer (`:262`). So a numeric secret (`secrets: { PIN: 1234 }`
  or a numeric `secret: true` connection property) is never masked today,
  even after it renders into a string.
- **F12 — re-run replays whole fields only, and only for some types.** The
  UI's re-run prefill comes from job detail, whose `raw_input` is redacted
  string by string, nested strings included (`web/api/jobs.rs:392`, `:513`;
  `redaction::redact_job_response`, `redaction.rs:576`). `resolve_rerun_sentinels` (`template.rs:754-790`)
  replaces an incoming field whose WHOLE value is `"••••••"` with the
  source's stored `raw_input` value, for secret and connection fields only
  (`:766`); called from `job_creator.rs:381`.

## 3. Decisions

| # | Decision | Alternatives rejected |
|---|---|---|
| D1 | One type, `json`, any JSON value (object, array, string, number, boolean, null). | `object` + `array` strict types; all three. |
| D2 | **Single-expression native evaluation, for `json` fields only.** A string inside a `json` field's value that is exactly one `{{ expr }}` takes the expression's value. | (A) decode rendered text, author writes `\| json_encode()` — less clear YAML; (C) native values for every field — breaks every `"{{ input.count }}"`. |
| D3 | A template in a `json` field that is not a single expression is an **error**. | Becomes a string (silent, `{{ input.cfg.host }}` renders `""` downstream); decoded as JSON (a second rule). |
| D4 | `secret: true`, `options`, `allow_custom`, `multiple` are rejected on `json`. | A masked JSON editor (later, TODO). |
| D5 | `json` is rejected in approval action `input` (approver forms). | Shared `JsonField` component now (later, TODO). |
| D6 | For a `type: task` step, the caller's `input:` (bucket C) follows the **task T's** schema; action defaults (bucket D) follow the wrapping action's schema. | Action schema for both. |
| D7 | Numeric secret values join the redaction set as their JSON text, and redaction masks **numbers** whose text contains one; `MaskAll` masks numbers. Boolean and null secret values are never redaction values (a one-bit value cannot be hidden by masking it, and masking every boolean would destroy the response) — unchanged from today, now documented. | Leave numbers unmasked (D2 makes `{{ secret.port \| int }}` a number); mask all booleans. |
| D8 | The Run form gives a `json` field one of three explicit modes — **default** (field omitted; the server applies and renders the default), **replay** (re-run: the server replays the source's stored value), **value** (the parsed editor text, always sent). A templated default is never placed in the editor. | Revision 1's "text unchanged from the prefill → omit", which conflated the three intents and could submit masked markers or template text as data. |
| D9 | Agent tool schema for `json`: a property with **no `type`** keyword. | `"object"` (blocks arrays). |
| D10 | At claim, which fields of a step's `input:` are `json` is decided by the step's persisted `action_spec.input` (F7), the schema of the action body that consumes the input. | A live/pinned config lookup before rendering (revision 1's claim restructure): the input could then be typed by a newer definition than the body using it. |
| D11 | A wrapper error's position is mapped back to the author's text; the original string is never rendered a second time. | Re-rendering the original for its error (a second `vals` call, possibly a different outcome). |

## 4. The json rule

Applies to the value of a field whose schema type is `json`, at the sites in
§ 6. Fields of every other type are rendered exactly as today (§ 4.5).

### 4.1 Classification

Every string `s` inside the value (the value itself, object values at any
depth, array elements at any depth — object KEYS are never rendered) is one
of:

- **Literal** — `s` contains none of `{{`, `{%`, `{#`. Used as is. (Tera
  renders delimiter-free text to itself, so this is the same result as a
  render, without one.)
- **Single expression** — after trimming ASCII whitespace (a YAML `|` block
  adds a trailing newline), `s` starts with `{{` and ends with `}}`; after
  removing those delimiters and an optional `-` whitespace-control marker on
  each side, the inner text contains none of `{{`, `}}`, `{%`, `%}`, `{#`,
  `#}`. Evaluated natively (§ 4.2).
- **Mixed** — anything else (`"id {{ x }}"`, `"{{ a }}{{ b }}"`, any
  `{% %}` tag). An error (§ 8).

Known false negative: a `}}` / `{{` inside a string literal of the
expression (`{{ x | default(value="}}") }}`) classifies as Mixed. Pinned by
a test and documented; the workaround is a variable.

A `json` value that is not a string (number, boolean, null) is literal. An
object or array is walked.

### 4.2 Evaluating a single expression

With `inner` from § 4.1:

1. Render `{% set __stroem_v = <inner> %}{{ __stroem_v | json_encode() }}`
   through `render_template` (the shared `tera_engine` compile path, so every
   registered filter — `vals`, compat ports — is available). F5 guarantees
   `set` accepts exactly the expressions `{{ }}` accepts.
2. Decode the output with `serde_json::from_str`. That is the field's value.
3. If step 1 fails, its `TemplateError` is returned with its position
   MAPPED to the author's string `s` (D11) — the expression is never
   evaluated a second time. The wrapper is `PREFIX + inner + SUFFIX` with a
   one-line `PREFIX` (`{% set __stroem_v = `); `inner` is copied verbatim,
   so with `(L0, C0)` = the position of `inner`'s first character in `s`
   (after the leading whitespace, `{{` and an optional `-`):
   - wrapper line 1, column `c` inside `inner` → `(L0, C0 + c - len(PREFIX))`;
   - wrapper line `l > 1` (a multi-line `inner`) → `(L0 + l - 1, c)`;
   - a position inside `PREFIX` or `SUFFIX`, or none → no position.
   Columns count characters: Tera's lexer advances its column once per
   `char` (`tera-2.4.0/src/parsing/lexer.rs:270-277`) and `TemplateError`
   reports `start_col + 1` (`template_error.rs:80-81`). A new crate-private
   `TemplateError::with_position(line, column)` does the replacement; the
   category is unchanged, so the message stays value-free. `raw_detail()`
   (stroem-cli only) still holds Tera's text for the WRAPPER source; the CLI
   printer labels it as the evaluated form of the expression.
   If step 2 fails (it should not: `json_encode` emits JSON), return the
   fixed error of § 8 row 3.

Every path renders exactly once. The result is never rendered again (R26,
`merge_action_defaults` doc comment, holds).

### 4.3 Values

- A missing variable is `null` (Tera 2 renders a missing field as `""` and
  `json_encode` of it as `null`, `upgrade-tera-2.md:115`).
- `{{ x | json_encode() }}` yields the JSON **text**, a string — exactly what
  the expression means. `stroem validate` warns about it (§ 5.4) because
  `for_each` taught authors to add that filter.
- Numbers keep their kind: an integer context value stays an integer, a Tera
  float (`10 / 2` → `5.0`) becomes a JSON float.

### 4.4 One function

`stroem_common::template`:

```rust
/// Render a step/hook input map against `schema`: `json` fields by the json
/// rule (§ 4), every other field exactly as `render_input_map` does.
pub fn render_input_typed(
    input_map: &HashMap<String, serde_json::Value>,
    schema: Option<&HashMap<String, InputFieldDef>>,
    context: &serde_json::Value,
) -> Result<serde_json::Value>;

/// The json rule for one field value (recursive). `field` names the field in
/// errors.
pub fn render_json_value(
    value: &serde_json::Value,
    field: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value>;
```

`render_input_map` stays (and stays the body of the non-`json` branch), so
callers without a schema are unchanged. `merge_defaults` and
`merge_action_defaults` gain one branch each: a field whose type is `json`
has its default rendered with `render_json_value` instead of the current
string / `render_value_deep` paths (`template.rs:584-600`, `:845-850`).

### 4.5 Non-`json` fields

Byte-for-byte unchanged, including F3's simple-path shortcut. A parity test
renders a fixture map through `render_input_map` and through
`render_input_typed` with a schema that declares no `json` field and asserts
equal output.

## 5. Declaring a json field; validation

### 5.1 Model

No struct change: `InputFieldDef.field_type == "json"`. The doc comment on
`InputFieldDef.field_type` (`models/workflow.rs:88-91`) lists it.

### 5.2 Type lists

- `template::PRIMITIVE_TYPES` gains `"json"` — the one switch that takes the
  field off every connection path in F2.
- New `template::RESERVED_TYPE_NAMES` = `PRIMITIVE_TYPES` + the aliases
  (`bool`) + `array`, `object`. In `validation.rs`, exactly two literal lists
  are replaced: the reserved connection-type names (`:769`) by
  `RESERVED_TYPE_NAMES`, and the input primitives of
  `validate_connection_inputs` (`:958`) by `PRIMITIVE_TYPES`. The list at
  `:761` is a different thing — the property types a CONNECTION TYPE may
  declare — and stays as it is, without `json` (connection-type properties
  are out of scope).
- `ui/src/components/task/constants.ts` `PRIMITIVE_TYPES` gains `"json"`
  (it carries a "Must match" comment).
- Breaking: a connection type named `json` is now rejected (upgrade note,
  § 12).

### 5.3 Field options

`check_input_field_options` (`validation.rs:868`) gains hard errors for a
`json` field with `secret`, `options`, `allow_custom` or `multiple`
(`multiple` is already rejected by the string/text rule at `:879`; the
message names `json` explicitly). `validate_approval_action`
(`validation.rs:1812`) rejects a `json` field in an approval action's
`input`.

### 5.4 `stroem validate` template checks

- **Error:** a Mixed string (§ 4.1) in a `json` field's `default`, or in a
  flow step's `input:` for a field the step's action (local, resolvable)
  declares `json`. For a `type: task` step, the task's schema (D6).
- **Warning:** a single expression in those places whose last filter is
  `json_encode`.
- Library (dotted) and cross-workspace actions are skipped, as validation
  already does for them.
- Server workspace loads still do not run validation (pre-existing gap); the
  runtime error of § 8 is the safety net.

## 6. Render sites

Every site where a templated value lands in a field whose schema is known:

| Site | Today | Change | Schema that decides `json` |
|---|---|---|---|
| Claim: action step `input:` (`rendering.rs:86`, from `jobs.rs:1063`; also agent steps and loop instances) | `render_input_map` | `render_input_typed` | the step's persisted `action_spec.input` (D10) |
| Claim: action defaults (`prepare_action_input_roles` → `merge_action_defaults`, `template.rs:1072`) | `merge_action_defaults` | json branch (§ 4.4) | the action `prepare_step_action_input` already reads defaults from (unchanged source) |
| Dispatch: `type: task` bucket C (`settlement/dispatch.rs:359`) | `render_input_map` | `render_input_typed` | task T `input` (resolved earlier in `handle_task_steps_pass`) |
| Dispatch: `type: task` bucket D (`dispatch.rs:432`) | `merge_action_defaults` | json branch | wrapping action `input` (`action_spec.input`) |
| Job creation: task defaults (`job_creator.rs:409`) — API, MCP, triggers, hooks, child jobs, agent tools, re-run, restart | `merge_defaults` | json branch | task `input` |
| Hooks: `type: task` hook input (`settlement/hooks.rs:657`) | `render_input_map` | `render_input_typed` | hooked task's `input` |
| Hooks: plain action hook input (`hooks.rs:657`) | `render_input_map` | `render_input_typed` | hook action's `input`, looked up in the hook's workspace config at that point |
| CLI `stroem run`: task defaults (`stroem-cli/src/local/run.rs:60`) | `merge_defaults` | json branch | task `input` |
| CLI `stroem run`: step input (`run.rs:509`) | `render_input_map` | `render_input_typed` | action `input` (in scope at `:517`) |
| CLI `stroem run`: action defaults (`run.rs:512` → `prepare_action_input` → `merge_action_defaults`, `template.rs:1072`) | `merge_action_defaults` | json branch (shared function) | action `input` |

Unchanged: user-supplied values (F8), trigger `input:` (literal:
`scheduler.rs:348`, `web/hooks.rs:646`, `event_source.rs:478`), agent tool
and MCP arguments (literal), approval step input (`dispatch.rs:720`, no
input schema applies), `when` and `for_each`.

**Claim schema source (D10, F7).** `render_step_input` reads the input
schema from `prep.step.action_spec` (its `input` key, deserialised as
`HashMap<String, InputFieldDef>`); no config lookup, no reordering of claim.
A step row without `action_spec`, or whose `input` key is absent or does not
deserialise, renders with `schema = None` — today's behaviour. Consequences,
each pinned by a test (§ 10):

- the action is deleted, renamed or retyped between job creation and claim
  → the step's `input:` keeps the types it was created with, matching the
  persisted body that consumes it;
- `for_each` instances, agent steps, library (dotted) actions,
  cross-workspace owners and pinned refs need no special case: each step row
  carries its own resolved `action_spec`.

Action DEFAULTS keep their existing source: `prepare_step_action_input`'s
lookup in the job's config (live, or the pin's), whose early returns
(`rendering.rs:102`, `:114`, `:135`, `:137`) are unchanged. The json branch
classifies a default by the schema it is read from, so a default and its type
always come from one definition. For an UNPINNED job whose action changes
between creation and claim, the step's own `input:` (persisted schema) and an
absent field's default (live schema) can be typed by different versions —
the same pre-existing drift as today's persisted body vs live defaults
(§ 13), not introduced here.

## 7. UI, CLI, MCP, agent tools

**Run form** (`input-field-row.tsx`, `task-detail.tsx`, `execute-input.ts`):

A `json` field's form state is `{ mode, text }` (D8). The three modes are the
three things a user can mean; the field's wire value follows from the mode
alone, never from comparing text to a prefill.

| Mode | Shown as | Sent | Entered when |
|---|---|---|---|
| **default** | the default, read-only, labelled "evaluated when the job runs"; buttons *Override* | field omitted — the server applies and renders the default | initial mode for a field whose default contains a template (any string with `{{`, `{%` or `{#`) |
| **replay** | "the previous run's value (contains masked secrets)", read-only; buttons *Override* | the string `"••••••"`; the server substitutes the source job's STORED `raw_input` value for the whole field (`resolve_rerun_sentinels`, extended to `json`, F12) | initial mode on re-run when the source's `raw_input` has the field and any string inside the prefilled (redacted) value contains `••••••` |
| **value** | a monospace `Textarea` (8 rows) with live validity, `Invalid JSON: line L, column C` (the user's own text); buttons *Use default* / *Use previous value* where applicable | the parsed text, always — even when it equals a default | every other case; *Override* enters it with an EMPTY editor |

- Initial `value` text: the re-run source's value when it has no masked
  string; else an untemplated default; else empty — as
  `JSON.stringify(v, null, 2)`.
- Submitting in `value` mode (`buildExecuteInput`): invalid JSON blocks the
  run with the error inline; empty text omits the field (then the server
  applies the default, or the run is blocked as "required" when there is
  none — `""` and `null` are typed explicitly); a parsed value with
  `••••••` in any string blocks the run ("contains masked values — reset to
  *Use previous value* or replace them"); a string containing `{{` shows a
  non-blocking note that it is sent as text, not evaluated.
- Server side, `resolve_rerun_sentinels` treats a `json` field like a secret
  field: an incoming whole-field `"••••••"` with a `source_job_id` is replaced
  by the source's stored value, or the field is removed when the source had
  none (the default then applies). `restart` replays `raw_input` server-side
  and never sees the UI, so it is unaffected.
- Docs call out the difference from string fields (clearing a string field
  with a default sends `""`; an empty `json` editor sends nothing).

**CLI**: `stroem run --input` and `stroem-api trigger --input` already take
JSON. `stroem tasks` / `inspect` print the type string.

**MCP**: `get_task` returns `InputFieldDef` as is (`mcp/tools.rs:468`), so
`type: "json"` is visible; `execute_task` input is literal.

**Agent task tools** (`stroem-agent/src/tools.rs:117` `map_field_type`): a
`json` field becomes a property with no `type` keyword and the description
suffix "Any JSON value." (D9). Risk: a provider that rejects typeless
properties. One provider's wire is pinned with `test_support::capture_one_request`;
if a provider rejects it, that provider maps `json` to `"object"`.

## 8. Errors

All value-free (CLAUDE.md § Secrets in logs): no value, no template text, no
object key.

| Failure | Message |
|---|---|
| Mixed string | ``Input field 'cfg': a json field takes a literal value or a single {{ expression }}``, plus ``at `[1]` `` for a string nested in an array (array indices only, as `render_location` does) |
| Expression fails | the wrapper's `TemplateError`, position mapped to the author's string (§ 4.2 step 3) |
| The wrapper's output does not decode (not expected: `json_encode` emits JSON) | ``Input field 'cfg': the expression's value could not be converted to JSON`` |

Field names are schema keys (author config), already named by today's
messages (`Input field '{}'`, `template.rs:680`).

Reporting is unchanged per site: claim failures go through
`fail_claimed_step` (scrubbed; an owner-side failure — action defaults of a
step whose owner ≠ `job.workspace` — is withheld as today, because the json
branch runs inside `merge_action_defaults`, which `prepare_action_input_roles`
already tags `ActionDefault`); dispatch failures go through `fail_task_step`
(bucket C visible after scrubbing, bucket D withheld when `O != A`, task
defaults tagged `OwnerSideRender`, `job_creator.rs:410`); hook failures go to
the source job's `_server` log, scrubbed. The json rule adds no reporting
path, so the render-path table (Tera 2 spec § 3.4) gains a note, not a row.

## 9. Secrets and redaction

D2 lets a secret become a NUMBER (`{{ secret.port | int }}`, or a numeric
secret passed through). Masking numbers is only useful if numeric secrets
are in the set, and today they are not (F11). Both halves change:

- **Collection.** `collect_strings` (`workspace_set.rs:268`) also collects
  a `Value::Number` as its JSON text (`n.to_string()`, serde_json's
  canonical form — the same text Tera renders for an integer or a float), and
  is renamed `collect_secret_scalars`. Numeric values whose text has 3
  characters or fewer are not collected, in every set (the existing
  `collect_redaction_values` length rule, `:262`, applied to numbers in the
  per-config scrub set too, so a secret `RETRIES: 3` cannot scrub every `3`
  out of an error message). String values keep each collector's current
  rule. Because every set — live, pinned, per-config scrub — is built by this
  one function (F11), every outlet and every scrub gets numeric secrets with
  no further change.
- **Masking.** `redact_value_tree`: a `Value::Number` whose text contains a
  redaction value (the span rule strings get, `redact_secrets_in_str`) is
  replaced by the string `"••••••"`. `mask_value_tree` (`MaskAll`) masks
  numbers too; its "numbers are kept" comment changes.
- **Booleans and null** are never redaction values (D7): masking a one-bit
  value hides nothing, and masking every boolean would destroy the response.
  This is today's behaviour, now stated in `guides/secrets.md`.
- Pre-existing gaps this closes: a numeric workspace secret rendered into a
  STRING field (`"port={{ secret.PORT }}"`) is masked from now on, and so is a
  numeric `secret: true` connection property reaching step input inside a
  connection object (F3's shortcut).
- Outlets covered with no change, because they redact through these
  functions: job detail (`web/api/jobs.rs:392` → `redaction.rs:576`), webhook sync + status poll,
  MCP `get_job_status`, worker detail. Implementation task: grep every call
  site of `redact_str` / `redact_secrets_in_str` given a JSON value's TEXT
  rather than a tree, and confirm none relies on numbers being skipped.
- A `json` value crossing a workspace boundary is data, not a connection:
  `resolve_provenance_bucket` skips it as a primitive (F2). Owner defaults
  rendered into a `json` field of a cross-workspace action are not persisted
  on a caller-visible parent step when `O != A` (existing rule, CLAUDE.md
  § Cross-Workspace References).
- Accepted cost: a numeric secret masks every occurrence of its digits in
  every string and number of a response — secret `5432` masks `154321` and
  `"port 5432"` — exactly as a string secret `"5432"` does today. In the
  response redaction set, values of 3 characters or fewer are never
  redaction values, string or number (F11); in the per-config scrub set the
  rule applies to numbers only (strings keep today's behaviour there).

## 10. Testing

- **stroem-common unit:** § 4.1 classification table (literal, single,
  mixed, trimming, `{{-`/`-}}`, YAML `|` trailing newline, `{%`/`{#`, the
  pinned false negative); § 4.2 native values (object, array, integer, float,
  bool, null, string, missing → null, `default(value={})`, `length`,
  `json_encode` → string, `each.item`); recursion and `[i]` locations;
  canary secret in the context and canary text in the template never appear
  in any error; § 4.5 parity test; the R26 render-once test extended to a
  `json` default; `merge_defaults` / `merge_action_defaults` json branches.
  § 4.2 step 3: a `vals`-style counting filter registered in a test engine is
  invoked exactly ONCE when the expression fails; error positions point into
  the author's string for a single-line `inner`, a multi-line `inner`, leading
  whitespace / newlines before `{{`, `{{-`, and a non-ASCII prefix; a
  position inside the wrapper's own text yields no position.
- **validation:** `json` accepted; § 5.3 rejections; a connection type named
  `json` rejected; § 5.4 error and warning.
- **redaction / collection:** a numeric workspace secret and a numeric
  `secret: true` connection property are collected (live, pinned and
  per-config sets) as text; numeric values of ≤ 3 characters are not; a
  number containing a secret is masked; a string containing a numeric secret
  is masked; booleans and nulls are not collected or masked; `MaskAll` masks
  numbers.
- **stroem-agent:** `map_field_type("json")` has no `type`; one provider wire.
- **stroem-server integration** (new `mod` lines in `tests/main.rs`): claim —
  an object from a previous step's output, `{{ items | length }}` as a
  number, `{{ each.item }}` in a `for_each` instance, an agent step, a
  library action, a cross-workspace owner action and a ref'd action; D10 —
  the action retyped (string → json and json → string) and deleted between
  creation and claim: the step's `input:` follows the persisted
  `action_spec`; cross-workspace owner-default json error withheld, caller
  json error visible; `type: task` buckets C and D; a task `json` default
  with a secret leaf, masked in job detail; a numeric secret rendered
  natively, masked in job detail, MCP `get_job_status` and the webhook sync
  response; `type: task` and plain action hooks; re-run — an incoming
  whole-field `"••••••"` for a `json` field is replaced by the source's
  stored value, and removed (default applies) when the source had none.
- **CLI:** `stroem run` with a `json` field fed from a previous step, and a
  `json` ACTION default with a template leaf.
- **UI:** vitest for the mode model (initial mode for: templated default,
  untemplated default, no default, re-run with masked value, re-run with
  unmasked value, re-run where the source lacks the field), for
  `buildExecuteInput` per mode (default → omitted, replay → `"••••••"`,
  value → parsed, always sent even when equal to the default; empty value →
  omitted or blocked when required; invalid JSON blocked; a value containing
  `••••••` blocked) and `InputFieldRow` (`json` editor, Override /
  Use default / Use previous value); one Playwright run of a task with a
  `json` input and one re-run of it.
- **E2E** (`tests/e2e.sh`, new section): step A emits an object via
  `OUTPUT:`, step B receives it through a `json` field and echoes
  `{{ input.cfg.key }}`.

## 11. Documentation

- `guides/input-and-output.md`: `json` row in Supported types; a "JSON
  inputs" section (literal / single expression / error, `null` for missing,
  `json_encode` gives text, the Run form's three modes, D4/D5 limits).
- `guides/rerun-and-restart.md`: a `json` value with masked secrets is
  replayed whole.
- `guides/templating.md`: native values in `json` fields.
- `guides/secrets.md`: numeric secrets are masked (in numbers and in
  strings); boolean and null secret values are never masked, and why; values
  of ≤ 3 characters are never masked.
- Upgrade note under `operations/`: a connection type named `json` is
  rejected; numeric secrets are now masked everywhere, and a masked number
  appears as the string `"••••••"`; rollout order (§ 12).
- `docs/public/llms.txt` regenerated.
- `CLAUDE.md`: a Key Patterns bullet (`render_input_typed` is the schema-aware
  renderer; the json rule; claim classifies `json` fields by the persisted
  `action_spec.input`; wrapper errors are position-mapped, never re-rendered)
  and § Secrets in logs (numeric secrets collected and masked; booleans never).
- `CONTEXT.md`: glossary entry **Native value** — the value a `json` field
  takes from a single-expression template.
- `docs/internal/TODO.md`: § 13.

## 12. Rollout and compatibility

- No migration. Templates render on the server and in the CLI, never on
  workers; workers are unaffected.
- Every server replica must run the release before any YAML uses
  `type: json`: an older replica treats `json` as a connection type and
  fails the step at claim (or the job at creation). Same rule as git refs.
- An older `stroem validate` rejects `type: json` ("references unknown
  type"); upgrade the CLI with the server.
- Visible changes without `json`: a connection type named `json` is
  rejected; numeric secrets (workspace secrets and `secret: true` connection
  properties written as YAML numbers) are now masked in job detail / MCP /
  webhook responses and scrubbed from error messages — in strings as well as
  numbers — where today they are shown.
- Proposed version: minor bump (0.19.0); the release decision is separate.

## 13. Follow-ups (TODO.md, not in this change)

- Plain action hooks never get action defaults or connection resolution: the
  hook job's task is `_hook:<action>` (`hooks.rs:722`), which claim cannot
  find (`rendering.rs:102`).
- A non-secret string field with a templated default is submitted verbatim
  by the Run form (F8). The `json` modes of § 7 are the model to extend to
  every type.
- Re-run of a non-secret string field whose value was partially masked in
  job detail (`"token=••••••"`) submits the marker as data (F12 replays
  whole-field sentinels of secret / connection / `json` fields only).
- Claim reads action DEFAULTS from the job's config (live for unpinned jobs)
  but the action body from the persisted `action_spec` (F7); `type: task`
  dispatch already reads persisted defaults only. Unify on the persisted
  spec.
- The Run form converts `number` but not `integer` (`execute-input.ts:27`).
- `json` in approval forms (D5); `secret: true` on `json` (D4).
- If a provider rejects typeless tool properties: per-provider fallback (D9).
