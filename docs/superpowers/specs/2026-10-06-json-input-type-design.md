# `json` input type

Status: revision 4, proposed (2026-10-06)

A new task/action input field type, `type: json`, that holds any JSON value
and keeps it structured through templates. Facts below are verified at
`b310bb44` (v0.18.0).

## Revision history

**Revision 4 (2026-10-06, Codex spec review round 3, verdict "no").** All
five findings verified. (1) `replay_fields` could copy a stored value from a
job of ANOTHER task: the unpinned execute path checks the source's workspace
and the caller's View on it, not its task (`web/api/tasks.rs:484-497`),
while the pinned path does (`:617`). Every re-run now requires the source to
be a run of the same task, on both paths — which also closes the same
pre-existing hole for the secret / connection sentinel (D12, § 7, § 12).
(2) The `replay_fields` 400s had no reachable validation point: the
no-source case is checked in the handler, the rest raise a typed
`ReplayFieldsError` that `classify_execute_error` maps to 400 (§ 7).
(3) Replaying a field the source lacks into a REQUIRED field with no default
→ 400 (§ 7). (4) A persisted `action_spec.input` that does not deserialise
now FAILS the claim with a value-free error instead of skipping preparation,
as `type: task` dispatch already does (`settlement/dispatch.rs:400-410`)
(§ 6). (5) The per-config scrub set never held connection properties
(`workspace_set.rs:103-109`); § 9 now claims numeric WORKSPACE secrets there
and records the connection-property omission as pre-existing (§ 13). Low:
float text — integers match exactly; floats are claimed only for the
formats a test pins (§ 9).

**Revision 3 (2026-10-06, Codex spec review round 2, verdict "no").** All
seven findings verified against the code. (A) A filter that converts a
string secret to a number (`"0042" | int` → `42`) escapes exact-text
matching: § 9 no longer promises it — it is the existing filter-transformed
class (the same expression renders an unmasked `"42"` into a string field
today), now stated in D7 and the secrets guide. (B) Claim classified `json`
by the persisted schema but prepared input with the live one: claim's whole
input preparation — `json` classification, defaults and connection
resolution — now reads the persisted `action_spec.input` (D10), and the live
action lookup leaves `prepare_step_action_input`; this also closes revision
2's persisted-body-vs-live-defaults drift. (3) A removed task / flow step
passes the stored input through unrendered: verified pre-existing for every
type, and NOT fixable by rendering persisted data — claim overwrites
`job_step.input` with the RENDERED input (F13), so a re-claim would render a
render's output (R26). Scoped out with that analysis (§ 6, § 13). (4) Re-run
initial-mode precedence stated: the source's value (replay or value) beats
the default. (5) Replay no longer overloads the value: an explicit
`replay_fields` list on the execute request (D12), so `"••••••"` inside a
`json` value is plain data, warned about but never blocked in the form. (6)
The position formula was one column off; restated in 1-based terms with the
end of `inner` bounded (§ 4.2). (7) The non-ASCII position test puts the
character inside `inner`.

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
  (`:766`); called from `job_creator.rs:381` for `CreationMode::Rerun`
  (`job_creator.rs:44`, built from `ExecuteTaskRequest.source_job_id`,
  `web/api/tasks.rs:102-107`).
- **F13 — `job_step.input` is raw before the first claim, rendered after.**
  Creation stores the flow step's raw `input:` map, templates included
  (`job_creator.rs:586`); every successful claim overwrites it with the
  RENDERED input (`web/worker_api/jobs.rs:1176`, `JobStepRepo::update_input`).
  A hook job's step input is the already-rendered hook payload from the start.
  When claim cannot find the job's task or the step's flow step in the config
  (`rendering.rs:64-77`) it passes `step.input` through unrendered — right
  for a hook step and for a re-claim (retry, agent resume), wrong for a FIRST
  claim, whose `step.input` still holds raw templates. Pre-existing, for every
  field type.

## 3. Decisions

| # | Decision | Alternatives rejected |
|---|---|---|
| D1 | One type, `json`, any JSON value (object, array, string, number, boolean, null). | `object` + `array` strict types; all three. |
| D2 | **Single-expression native evaluation, for `json` fields only.** A string inside a `json` field's value that is exactly one `{{ expr }}` takes the expression's value. | (A) decode rendered text, author writes `\| json_encode()` — less clear YAML; (C) native values for every field — breaks every `"{{ input.count }}"`. |
| D3 | A template in a `json` field that is not a single expression is an **error**. | Becomes a string (silent, `{{ input.cfg.host }}` renders `""` downstream); decoded as JSON (a second rule). |
| D4 | `secret: true`, `options`, `allow_custom`, `multiple` are rejected on `json`. | A masked JSON editor (later, TODO). |
| D5 | `json` is rejected in approval action `input` (approver forms). | Shared `JsonField` component now (later, TODO). |
| D6 | For a `type: task` step, the caller's `input:` (bucket C) follows the **task T's** schema; action defaults (bucket D) follow the wrapping action's schema. | Action schema for both. |
| D7 | Numeric secret values join the redaction set as their JSON text, and redaction masks **numbers** whose text contains one; `MaskAll` masks numbers. Boolean and null secret values are never redaction values (a one-bit value cannot be hidden by masking it, and masking every boolean would destroy the response) — unchanged from today, now documented. A FILTER-CONVERTED representation of a secret (`\| int` of `"0042"`, `\| upper`, `\| b64encode`, a slice) is not covered — the existing filter-transformed class (CLAUDE.md § Secrets in logs: no finite scrub matches every encoding), unchanged by `json`. | Leave numeric secrets unmasked; mask all booleans; collect derived forms (`"0042"` → also `42`), which only covers the conversions someone thought of. |
| D8 | The Run form gives a `json` field one of three explicit modes — **default** (field omitted; the server applies and renders the default), **replay** (re-run: the field is named in `replay_fields`, D12, and the server replays the source's stored value), **value** (the parsed editor text, always sent). On a re-run the source's value takes precedence over the default. A templated default is never placed in the editor. | Revision 1's "text unchanged from the prefill → omit", which conflated the three intents and could submit masked markers or template text as data. |
| D9 | Agent tool schema for `json`: a property with **no `type`** keyword. | `"object"` (blocks arrays). |
| D10 | At claim, the step's persisted `action_spec.input` (F7) is THE action input schema for all of input preparation: which fields of the step's `input:` are `json`, which defaults are merged, which fields are connection-typed. The live action is no longer looked up for input preparation. Same rule `type: task` dispatch already follows (`action_spec.input`, never a live lookup). | Live/pinned lookup before rendering (revision 1); persisted for classification but live for defaults and connections (revision 2 — a retyped field could be classified `json` and then resolved as a connection). |
| D11 | A wrapper error's position is mapped back to the author's text; the original string is never rendered a second time. | Re-rendering the original for its error (a second `vals` call, possibly a different outcome). |
| D12 | Re-run replay of a `json` field is requested by NAME: the execute request gains `replay_fields: [..]` (with `source_job_id`). Values are always data — `"••••••"` inside a `json` value means nothing special. A re-run's source must be a run of the SAME task (both execute paths), so replay copies a value only between runs of one task. | Overloading the value with the `"••••••"` sentinel (revision 2): a literal bullet string became inexpressible, and the form had to block innocent strings. |

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
   evaluated a second time. The wrapper is `PREFIX + inner + SUFFIX`;
   `PREFIX` (`{% set __stroem_v = `) is one line of `P` characters and
   `inner` is copied verbatim. All positions below are 1-based
   `(line, column)`, as `TemplateError` reports them.
   - `(L0, C0)` = the position of `inner`'s first character in `s` (after
     the leading whitespace, `{{` and an optional `-`). In the wrapper that
     character is at `(1, P + 1)`.
   - `inner`'s last character is at wrapper `(Le, Ce)`: `(1, P + n)` for a
     one-line `inner` of `n` characters; otherwise `Le = 1 + ` the number of
     newlines in `inner`, `Ce` = the characters after its last newline.
   - A wrapper position `(l, c)` before `(1, P + 1)` or after `(Le, Ce)`
     (lexicographic order — inside `PREFIX` or `SUFFIX`) → no position.
   - `l = 1` → `(L0, C0 + c - (P + 1))`; so `(1, P + 1)` maps to `(L0, C0)`.
   - `l > 1` → `(L0 + l - 1, c)` — the lines after the first are copied
     whole, so their columns do not move.
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
| Claim: action defaults and connection resolution (`prepare_step_action_input` → `prepare_action_input_roles` → `merge_action_defaults`, `template.rs:1072`) | `merge_action_defaults`, schema from the LIVE action (`rendering.rs:127-139`) | json branch (§ 4.4); schema from the persisted spec (D10) | the step's persisted `action_spec.input` |
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

**Claim schema source (D10, F7).** One schema serves all of claim's input
handling: `step_input_schema(&JobStepRow) -> Option<HashMap<String,
InputFieldDef>>` reads `action_spec`'s `input` key. `render_step_input` uses
it to classify `json` fields; `prepare_step_action_input` uses it for
`merge_missing_action_fields`, the defaults merge and both connection passes
(`prepare_action_input_roles`) — in place of today's live action lookup
(`rendering.rs:127-139`, removed). The owner workspace for CONNECTION
VALUES is unchanged (`ctx.action_workspace`, live or pinned): the persisted
spec says which fields are connection-typed and what their defaults are; the
owner's config still says what a connection name resolves to.

- No `action_spec`, no `input` key, `input: null`, or an empty map → the
  rendered input is returned as is — today's `action.input.is_empty()`
  result (`rendering.rs:137`). This is the same absent/null test `type: task`
  dispatch applies (`settlement/dispatch.rs:400`).
- An `input` that is present but does not deserialise as
  `HashMap<String, InputFieldDef>` FAILS the claim — it never falls back to
  "no schema", which would skip defaults and connection resolution and could
  hand an unresolved connection name to the action. The step fails through
  `fail_claimed_step` (§ 8) with the fixed message `the step's persisted
  action definition has an unreadable input schema`: no serde text (which
  can quote the stored value), no value. Same outcome as dispatch's
  (`dispatch.rs:400-410`), minus its serde detail. Reachable only through a
  row written by a different server version or by hand.
- The task / flow-step early returns (`rendering.rs:64-77` in rendering,
  `:102`, `:114` in preparation) are unchanged: when rendering passes the
  stored input through, preparation does too (F13; see "Removed task or flow
  step" below).

Consequences, each pinned by a test (§ 10):

- the action is deleted, renamed or retyped between job creation and claim
  → claim prepares the input exactly as the persisted definition says —
  types, defaults and connection-typed fields — matching the persisted body
  that consumes it. A field retyped `json` → connection after creation stays
  `json` for that step;
- behaviour change, all types: an action's DEFAULTS edited after a job was
  created no longer reach that job's unclaimed steps (today they do for
  unpinned jobs; pinned jobs never saw edits). This is the rule `type: task`
  dispatch already follows (bucket D, `action_spec.input`) and is listed in
  the upgrade note;
- `for_each` instances (`cascade.rs:538` copies the spec), agent steps,
  library (dotted) actions, cross-workspace owners and pinned refs need no
  special case: each step row carries its own resolved `action_spec`.

**Removed task or flow step (pre-existing, not changed here).** When claim
cannot find the job's task or the step's flow step, `step.input` is passed
through unrendered (F13). For a FIRST claim that is raw template text, for
every field type alike; a `json` field inherits exactly that. It cannot be
fixed by rendering `step.input` instead: after the first claim that column
holds the RENDERED input (`jobs.rs:1176`), and a hook step's input is a
rendered payload from the start, so rendering it would render a render's
output (R26). A correct fix needs to know whether `step.input` is still raw —
e.g. a persisted "rendered" marker, or failing the first claim of a step
whose flow step is gone — and belongs to claim, not to this type (§ 13).

## 7. UI, CLI, MCP, agent tools

**Run form** (`input-field-row.tsx`, `task-detail.tsx`, `execute-input.ts`):

A `json` field's form state is `{ mode, text }` (D8). The three modes are the
three things a user can mean; the field's wire value follows from the mode
alone, never from comparing text to a prefill.

| Mode | Shown as | Sent | Entered when |
|---|---|---|---|
| **default** | the default, read-only, labelled "evaluated when the job runs"; button *Override* | field omitted — the server applies and renders the default | see "Initial mode" |
| **replay** | "the previous run's value (contains masked secrets)", read-only; button *Override* | field omitted from `input` and its name listed in `replay_fields` (D12); the server takes the source job's STORED, unredacted `raw_input` value for the whole field | see "Initial mode" |
| **value** | a monospace `Textarea` (8 rows) with live validity, `Invalid JSON: line L, column C` (the user's own text); buttons *Use default* / *Use previous value* where applicable | the parsed text in `input`, always — even when it equals a default | see "Initial mode"; *Override* enters it with an EMPTY editor |

- **Initial mode**, first match wins (the source's value beats the default,
  because the source run's explicit value is stored before defaults are
  merged, `job_creator.rs:404`, and replaying it is what Re-run means):
  1. re-run, the source's `raw_input` has the field, and any string in its
     prefilled (redacted) value contains `••••••` → **replay**;
  2. re-run, the source's `raw_input` has the field → **value**, prefilled
     with that value;
  3. the default contains a template (any string with `{{`, `{%` or `{#`)
     → **default**;
  4. otherwise → **value**, prefilled with the default if there is one,
     else empty.
  Prefills are `JSON.stringify(v, null, 2)`.
- Submitting in `value` mode (`buildExecuteInput`): invalid JSON blocks the
  run with the error inline; empty text omits the field (then the server
  applies the default, or the run is blocked as "required" when there is
  none — `""` and `null` are typed explicitly). Two NON-blocking notes, since
  any JSON string is valid (D1): a string containing `••••••` ("sent as
  text; *Use previous value* replays the masked value instead"), and a
  string containing `{{` ("sent as text, not evaluated").

- Docs call out the difference from string fields (clearing a string field
  with a default sends `""`; an empty `json` editor sends nothing).

**`replay_fields` (D12)** — `ExecuteTaskRequest` (`web/api/tasks.rs:102`)
gains `#[serde(default)] replay_fields: Vec<String>`, carried into
`CreationMode::Rerun { source_job_id, replay_fields }`
(`job_creator.rs:44`). Both execute paths reach that one variant: the
unpinned one through `create_job_for_task_detailed` (`job_creator.rs:105`),
the pinned one directly (`web/api/tasks.rs:647`). Checks, in order:

1. **Handler, before the pinned/unpinned branch** (`web/api/tasks.rs:454`):
   `replay_fields` non-empty and `source_job_id` absent → 400
   `replay_fields requires source_job_id`. (Without a source the request
   becomes `CreationMode::Normal`, which never sees the list.)
2. **Handler, both paths: same task.** The unpinned path gains the check the
   pinned path already has (`web/api/tasks.rs:617`): `source_job.task_name
   != name` → 400 `Source job {id} is a run of task '{t}', not '{name}'`,
   after the existing workspace and ACL checks (`:487-497`). It applies to
   EVERY re-run request, with or without `replay_fields`, so the existing
   secret / connection sentinel can no longer copy a value across tasks
   either (it could: `resolve_rerun_sentinels` matches by field name only).
3. **`create_job_for_task_inner`, `CreationMode::Rerun` branch**
   (`job_creator.rs:360-385`), before `resolve_rerun_sentinels` and
   `merge_defaults`, against the task's input schema (the live task, or the
   pin's on the pinned path) and the source's stored `raw_input`:
   - a name that is not a field of the schema → `ReplayFieldsError::UnknownField`;
   - a name that also appears in `input` → `ReplayFieldsError::AlsoInInput`;
   - otherwise the field takes the source's `raw_input` value; when the
     source has none, the field is left absent, and if it is `required`
     with no `default` → `ReplayFieldsError::MissingRequired`.
   `ReplayFieldsError` is a typed error naming the field (a schema key) and
   never a value; `classify_execute_error` (`web/api/mod.rs:420`) downcasts
   it to 400 in its typed tier, before the phrase tiers, so no message text
   is matched.

`replay_fields` is accepted for every field type: the server has no reason
to refuse it, and it is what the secret / connection sentinel would be if
designed now. The UI uses it for `json` fields only; moving secret and
connection replay onto it is a follow-up (§ 13). An `input` value is always
data: `"••••••"` anywhere in a `json` value — from the form or an API
client — is stored and used as text. `resolve_rerun_sentinels` is NOT
extended to `json` (revision 2 did that; D12 replaces it). `restart` replays
`raw_input` server-side and never sees the form, so it is unaffected. The
redaction closure already follows `source_job_id` (`redaction.rs:308`), so
a replayed value stays masked in every outlet of the new job.

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

D2 lets a secret reach a `json` field as a native NUMBER — a numeric secret
passed through (`{{ secret.PORT }}` with `PORT: 5432`). Masking numbers is
only useful if numeric secrets are in the set, and today they are not (F11).
Both halves change (below).

What is covered, exactly: a redaction value is matched against the TEXT of
each string and (new) each number. So a secret's own value is masked
wherever it appears, as a string or as a number whose text equals or
contains it — including a string secret that `| int` turns into the same
digits (`"5432"` → `5432`). A conversion that changes the text is NOT
covered: `"0042" | int` → `42`, `| upper`, `| b64encode`, `| round`, a
slice. That is the filter-transformed class CLAUDE.md § Secrets in logs
already names (a filter chain can produce encodings no finite scrub
matches), and it is not new: the same `{{ secret.PIN | int }}` renders an
unmasked `"42"` into a STRING field today. `json` adds no new member to the
class — a string field holds the same digits as text. Collecting derived
forms (`"0042"` → also `42`) is rejected (D7): it covers only the
conversions someone listed. `guides/secrets.md` states the rule.

- **Collection.** `collect_strings` (`workspace_set.rs:268`) also collects
  a `Value::Number` as its JSON text (`n.to_string()`), and is renamed
  `collect_secret_scalars`. Numeric values whose text has 3 characters or
  fewer are not collected, in every set (the existing
  `collect_redaction_values` length rule, `:262`, applied to numbers in the
  per-config scrub set too, so a secret `RETRIES: 3` cannot scrub every `3`
  out of an error message). String values keep each collector's current
  rule. What each set gains is bounded by what its collector WALKS, which
  does not change:
  - the response sets — live (`collect_redaction_values`, `:228`) and each
    pin's (`redaction.rs:395`) — walk workspace secrets AND `secret: true`
    connection properties (`:230-259`), so both gain their numeric values;
  - the per-config scrub set (`collect_config_secret_values`, `:103-109`,
    used by the cascade, dispatch, hooks and event sources) walks workspace
    secrets ONLY, so it gains numeric workspace secrets. It has never held
    connection properties; that omission is pre-existing and unchanged
    (§ 13).
- **Number text.** Masking compares texts, so it is exact only where the two
  texts agree. Integers: serde_json and Tera both print plain decimal digits,
  so an integer secret matches as a number AND inside rendered strings.
  Floats: number-to-number matching is exact (collection and masking both use
  `Number::to_string()`), but serde_json formats floats with its own
  shortest-representation writer (`zmij`, `serde_json-1.0.150/src/number.rs:356`)
  while Tera prints `f64` with Rust `Debug` (`tera-2.4.0/src/value/mod.rs:498-503`),
  so a float secret rendered into a STRING is matched only where the two
  agree — claimed only for the values a test pins (`0.5`, `3.14`, `1e-7`,
  `1e21`, `-2.0`); anything else is the filter-transformed class above.
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
  invoked exactly ONCE when the expression fails; exact mapped `(line,
  column)` of the FIRST and of the LAST token of `inner` for: a one-line
  `inner`, a multi-line `inner` (error on its second line), leading spaces
  and newlines before `{{`, `{{-`, and a non-ASCII character inside `inner`
  before the failing token (columns count characters, not bytes); a
  position inside the wrapper's `PREFIX` or `SUFFIX` yields no position.
- **validation:** `json` accepted; § 5.3 rejections; a connection type named
  `json` rejected; § 5.4 error and warning.
- **redaction / collection:** a numeric workspace secret is collected as
  text in the live, pinned and per-config sets; a numeric `secret: true`
  connection property in the live and pinned sets (not the per-config set,
  which walks no connections, § 9); numeric values of ≤ 3 characters are
  not; float secrets `0.5`, `3.14`, `1e-7`, `1e21`, `-2.0` rendered into a
  string field are masked (or the list in § 9 shrinks to the ones that are); a
  number containing a secret is masked; a string containing a numeric secret
  is masked; a string secret `"5432"` rendered with `| int` into a `json`
  field is masked; `"0042" | int` is NOT (pins the documented limit, § 9);
  booleans and nulls are not collected or masked; `MaskAll` masks numbers.
- **stroem-agent:** `map_field_type("json")` has no `type`; one provider wire.
- **stroem-server integration** (new `mod` lines in `tests/main.rs`): claim —
  an object from a previous step's output, `{{ items | length }}` as a
  number, `{{ each.item }}` in a `for_each` instance, an agent step, a
  library action, a cross-workspace owner action and a ref'd action; D10 —
  between creation and claim the action is (a) retyped string → json,
  (b) retyped json → string, (c) retyped json → a connection type,
  (d) given a different default, (e) deleted: in every case the FINAL
  prepared input sent to the worker equals the input prepared from the
  persisted `action_spec` (types, defaults, connection-typed fields); a
  step whose `action_spec.input` is present but not an input schema fails at
  claim with the fixed § 6 message (no serde text, no value), while one with
  no `input` key, `input: null` or `{}` is claimed with no preparation;
  F13 — a removed flow step on a first claim still passes `step.input`
  through unrendered (pins today's behaviour, § 6), and a re-claimed step
  whose flow step is removed passes its rendered input through unchanged
  (no second render); cross-workspace owner-default json error withheld, caller
  json error visible; `type: task` buckets C and D; a task `json` default
  with a secret leaf, masked in job detail; a numeric secret rendered
  natively, masked in job detail, MCP `get_job_status` and the webhook sync
  response; `type: task` and plain action hooks; `replay_fields`, on BOTH
  the unpinned and the pinned execute path — a named `json` field takes the
  source's stored (unredacted) value; is absent (default applies) when the
  source had none; 400 for each of: no `source_job_id` (handler), unknown
  field, field also in `input`, and a required field with no default that
  the source lacks (`ReplayFieldsError`, classified in the typed tier); a
  `json` value containing `"••••••"` sent in `input` on a re-run is stored as
  text; same-task rule — a re-run whose source is a run of ANOTHER task in
  the same workspace is 400 on the unpinned path (with and without
  `replay_fields`, and with a secret-field sentinel), for a caller holding
  View on the source's task and Run on the destination (mixed ACL), and the
  pinned path's existing 400 is unchanged.
- **CLI:** `stroem run` with a `json` field fed from a previous step, and a
  `json` ACTION default with a template leaf.
- **UI:** vitest for the mode model — initial mode for: templated default,
  untemplated default, no default, re-run with masked value, re-run with
  unmasked value, re-run where the source lacks the field, and re-run with
  a masked source value AND a templated default (→ replay: the source wins);
  `buildExecuteInput` per mode (default → omitted; replay → omitted from
  `input` and named in `replay_fields`; value → parsed, always sent even when
  equal to the default; empty value → omitted, or blocked when required;
  invalid JSON blocked; a value containing `••••••` or `{{` sent with a
  non-blocking note); `InputFieldRow` (`json` editor, Override / Use default /
  Use previous value); one Playwright run of a task with a `json` input and
  one re-run of it.
- **E2E** (`tests/e2e.sh`, new section): step A emits an object via
  `OUTPUT:`, step B receives it through a `json` field and echoes
  `{{ input.cfg.key }}`.

## 11. Documentation

- `guides/input-and-output.md`: `json` row in Supported types; a "JSON
  inputs" section (literal / single expression / error, `null` for missing,
  `json_encode` gives text, the Run form's three modes, D4/D5 limits).
- `guides/rerun-and-restart.md`: a `json` value with masked secrets is
  replayed whole (*Use previous value*); the API's `replay_fields`.
- API reference for `POST /api/workspaces/{ws}/tasks/{name}/execute`:
  `replay_fields`, its four 400s, and the same-task rule for every re-run.
- `guides/templating.md`: native values in `json` fields.
- `guides/secrets.md`: numeric secrets are masked (in numbers and in
  strings); boolean and null secret values are never masked, and why; values
  of ≤ 3 characters are never masked; a filter that changes a secret's text
  (`| int` of `"0042"`, `| upper`, `| b64encode`) is not covered, in any
  field type.
- Upgrade note under `operations/`: a connection type named `json` is
  rejected; numeric secrets are now masked everywhere, and a masked number
  appears as the string `"••••••"`; claim prepares a step's input from the
  action definition persisted at job creation (an edited default no longer
  reaches in-flight jobs); rollout order (§ 12).
- `docs/public/llms.txt` regenerated.
- `CLAUDE.md`: a Key Patterns bullet (`render_input_typed` is the schema-aware
  renderer; the json rule; claim prepares input — `json` classification,
  defaults, connection-typed fields — from the persisted `action_spec.input`;
  wrapper errors are position-mapped, never re-rendered; `replay_fields`)
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
  numbers — where today they are shown; claim reads an action's input
  schema and defaults from the definition persisted at job creation, so an
  action edited mid-job no longer changes that job's unclaimed steps (D10).
- A re-run whose source is a run of a DIFFERENT task is now 400 on the
  unpinned execute path too (the pinned path already refused it). The UI's
  Re-run always targets the source's own task; an API client that re-ran
  across tasks must start a normal run instead.
- `replay_fields` is additive: old clients never send it; a server that does
  not know it ignores it (serde default), so during a rolling deploy a re-run
  from a new UI against an old replica runs the field's DEFAULT instead of
  replaying — one more reason for the replica-first rollout above.
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
  whole-field sentinels of secret / connection fields only). Moving secret
  and connection replay onto `replay_fields` (D12) and offering *Use previous
  value* for every type closes it.
- The per-config scrub set (`collect_config_secret_values`) walks workspace
  secrets only, never `secret: true` connection properties, so a connection
  secret in a cascade / dispatch / hook / event-source error is not scrubbed
  there (the response sets do mask it). Pre-existing; § 9.
- The secret / connection re-run sentinel leaves a field absent when the
  source lacks it, even when the field is required with no default; only
  `replay_fields` returns 400 for that (§ 7). Moving the sentinel onto
  `replay_fields` gives it the same rule.
- First claim of a step whose task or flow step was removed passes raw
  template text to the worker, for every field type (F13, § 6). Needs a way
  to know whether `step.input` is still raw (a persisted marker), or to fail
  that first claim.
- The Run form converts `number` but not `integer` (`execute-input.ts:27`).
- `json` in approval forms (D5); `secret: true` on `json` (D4).
- If a provider rejects typeless tool properties: per-provider fallback (D9).
