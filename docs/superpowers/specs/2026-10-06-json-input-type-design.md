# `json` input type

Status: revision 1, proposed (2026-10-06)

A new task/action input field type, `type: json`, that holds any JSON value
and keeps it structured through templates. Facts below are verified at
`b310bb44` (v0.18.0).

## Revision history

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
  (`tera-2.4.0/src/value/mod.rs:476-497`) writes strings inside arrays and
  maps with Rust `{:?}` (control characters come out as `\u{1b}`, invalid
  JSON) and writes `None`/`Undefined` as nothing (`{"a": }`). The upgrade
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
- **F7 — claim renders before it knows the action.** `render_step_input`
  (`web/worker_api/rendering.rs:86`) renders the flow step's `input:`; the
  action is looked up afterwards in `prepare_step_action_input`
  (`rendering.rs:96-140`, via `PrepareContext`).
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

## 3. Decisions

| # | Decision | Alternatives rejected |
|---|---|---|
| D1 | One type, `json`, any JSON value (object, array, string, number, boolean, null). | `object` + `array` strict types; all three. |
| D2 | **Single-expression native evaluation, for `json` fields only.** A string inside a `json` field's value that is exactly one `{{ expr }}` takes the expression's value. | (A) decode rendered text, author writes `\| json_encode()` — less clear YAML; (C) native values for every field — breaks every `"{{ input.count }}"`. |
| D3 | A template in a `json` field that is not a single expression is an **error**. | Becomes a string (silent, `{{ input.cfg.host }}` renders `""` downstream); decoded as JSON (a second rule). |
| D4 | `secret: true`, `options`, `allow_custom`, `multiple` are rejected on `json`. | A masked JSON editor (later, TODO). |
| D5 | `json` is rejected in approval action `input` (approver forms). | Shared `JsonField` component now (later, TODO). |
| D6 | For a `type: task` step, the caller's `input:` (bucket C) follows the **task T's** schema; action defaults (bucket D) follow the wrapping action's schema. | Action schema for both. |
| D7 | Redaction masks **numbers** whose JSON text contains a secret value; `MaskAll` masks numbers. | Leave numbers unmasked (D2 makes `{{ secret.port \| int }}` a number). |
| D8 | UI keeps the editor **text** as state; empty text = omit; text unchanged from the prefilled default = omit. | Parsed value + dirty flag. |
| D9 | Agent tool schema for `json`: a property with **no `type`** keyword. | `"object"` (blocks arrays). |

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
3. If step 1 fails, render the ORIGINAL string `s` and return its error —
   the `TemplateError` then carries positions in the author's own text. If
   the original unexpectedly renders, return the fixed error of § 8 row 3.
   If step 2 fails (it should not: `json_encode` emits JSON), return that
   same fixed error.

The successful path renders once. The result is never rendered again (R26,
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
  (`bool`) + `array`, `object`. `validation.rs`'s three literal copies
  (`:761`, `:769`, `:958`) are replaced by these two consts. `:761` (the
  property types allowed on a CONNECTION TYPE) keeps its own list without
  `json` — connection-type properties are out of scope.
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
| Claim: action step `input:` (`rendering.rs:86`, from `jobs.rs:1063`; also agent steps and loop instances) | `render_input_map` | `render_input_typed`; the action (owner's, for a cross-workspace step) is resolved BEFORE rendering (F7) | action `input` |
| Claim: action defaults (`prepare_action_input_roles` → `merge_action_defaults`, `template.rs:1072`) | `merge_action_defaults` | json branch (§ 4.4) | action `input` |
| Dispatch: `type: task` bucket C (`settlement/dispatch.rs:359`) | `render_input_map` | `render_input_typed` | task T `input` (resolved earlier in `handle_task_steps_pass`) |
| Dispatch: `type: task` bucket D (`dispatch.rs:432`) | `merge_action_defaults` | json branch | wrapping action `input` (`action_spec.input`) |
| Job creation: task defaults (`job_creator.rs:409`) — API, MCP, triggers, hooks, child jobs, agent tools, re-run, restart | `merge_defaults` | json branch | task `input` |
| Hooks: `type: task` hook input (`settlement/hooks.rs:657`) | `render_input_map` | `render_input_typed` | hooked task's `input` |
| Hooks: plain action hook input (`hooks.rs:657`) | `render_input_map` | `render_input_typed` | hook action's `input`, looked up in the hook's workspace config at that point |
| CLI `stroem run`: task defaults (`stroem-cli/src/local/run.rs:60`) | `merge_defaults` | json branch | task `input` |
| CLI `stroem run`: step input (`run.rs:509`) | `render_input_map` | `render_input_typed` | action `input` (in scope at `:517`) |

Unchanged: user-supplied values (F8), trigger `input:` (literal:
`scheduler.rs:348`, `web/hooks.rs:646`, `event_source.rs:478`), agent tool
and MCP arguments (literal), approval step input (`dispatch.rs:720`, no
input schema applies), `when` and `for_each`.

**Claim restructure (F7).** `render_step_input` takes the resolved action's
input schema (or `None` when the step has no resolvable action — today's
early returns in `prepare_step_action_input`, `rendering.rs:102`, `:114`,
`:135`, `:137`). The owner lookup moves into one helper used by both
`render_step_input` and `prepare_step_action_input`, so the two can never
disagree about which action a step runs. No behaviour change for steps
without `json` fields.

## 7. UI, CLI, MCP, agent tools

**Run form** (`input-field-row.tsx`, `task-detail.tsx`, `execute-input.ts`):

- A `json` field renders a monospace `Textarea` (8 rows) with live validity:
  `Invalid JSON: line L, column C` (the user's own text; no value-free rule
  applies client-side). No new dependency.
- State is the editor text. Prefill: the default, or the re-run source's
  `raw_input` value, as `JSON.stringify(v, null, 2)`.
- Submit (`buildExecuteInput`): invalid JSON blocks the run and shows the
  error inline; empty text → field omitted (the server applies the default;
  `""` and `null` are typed explicitly); text identical to the prefilled
  default → field omitted, so the SERVER renders a templated default (D8,
  and the fix F8 shows is needed). Otherwise the parsed value is sent.
- Docs call out the difference from string fields, where clearing a field
  with a default sends `""`.

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
| Expression fails | the `TemplateError` from rendering the original string (§ 4.2 step 3) |
| Wrapper fails but the original renders, or the wrapper's output does not decode | ``Input field 'cfg': the expression's value could not be converted to JSON`` |

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

- `redact_value_tree`: a `Value::Number` whose text (`n.to_string()`)
  contains a secret value — the same span rule strings get,
  `redact_secrets_in_str` — is replaced by the string `"••••••"`. Booleans
  and nulls are untouched.
- `mask_value_tree` (`MaskAll`) masks numbers too; its comment changes.
- This also closes a pre-existing gap: a numeric `secret: true` connection
  property reaching step input inside a connection object (F3's shortcut).
- Every outlet that redacts through these two functions is covered without
  further change (job detail, webhook sync + status poll, MCP
  `get_job_status`, worker detail). Implementation task: grep every call
  site of `redact_str` / `redact_secrets_in_str` that is given a JSON value's
  text rather than a tree, and confirm none relies on numbers being skipped.
- A `json` value crossing a workspace boundary is data, not a connection:
  `resolve_provenance_bucket` skips it as a primitive (F2). Owner defaults
  rendered into a `json` field of a cross-workspace action are not persisted
  on a caller-visible parent step when `O != A` (existing rule, CLAUDE.md
  § Cross-Workspace References).
- Accepted cost: a short numeric secret (`42`) masks `1428` in outputs, as it
  already masks the string `"1428"`.

## 10. Testing

- **stroem-common unit:** § 4.1 classification table (literal, single,
  mixed, trimming, `{{-`/`-}}`, YAML `|` trailing newline, `{%`/`{#`, the
  pinned false negative); § 4.2 native values (object, array, integer, float,
  bool, null, string, missing → null, `default(value={})`, `length`,
  `json_encode` → string, `each.item`); recursion and `[i]` locations;
  canary secret in the context and canary text in the template never appear
  in any error; § 4.5 parity test; the R26 render-once test extended to a
  `json` default; `merge_defaults` / `merge_action_defaults` json branches.
- **validation:** `json` accepted; § 5.3 rejections; a connection type named
  `json` rejected; § 5.4 error and warning.
- **redaction:** number containing a secret masked; boolean not; `MaskAll`
  masks numbers; numeric connection property masked.
- **stroem-agent:** `map_field_type("json")` has no `type`; one provider wire.
- **stroem-server integration** (new `mod` lines in `tests/main.rs`): claim —
  an object from a previous step's output and `{{ items | length }}` as a
  number; the claim restructure (action resolved before render, cross-
  workspace owner action); cross-workspace owner-default json error withheld,
  caller json error visible; `type: task` buckets C and D; a task `json`
  default with a secret leaf, masked in job detail; `type: task` and plain
  action hooks.
- **CLI:** `stroem run` with a `json` field fed from a previous step.
- **UI:** vitest for `buildExecuteInput` (parsed value, empty → omit,
  unchanged default → omit, invalid blocks) and `InputFieldRow` (`json`
  editor); one Playwright run of a task with a `json` input.
- **E2E** (`tests/e2e.sh`, new section): step A emits an object via
  `OUTPUT:`, step B receives it through a `json` field and echoes
  `{{ input.cfg.key }}`.

## 11. Documentation

- `guides/input-and-output.md`: `json` row in Supported types; a "JSON
  inputs" section (literal / single expression / error, `null` for missing,
  `json_encode` gives text, the UI's empty / unchanged rules, D4/D5 limits).
- `guides/templating.md`: native values in `json` fields.
- `guides/secrets.md`: numbers are masked.
- Upgrade note under `operations/`: a connection type named `json` is
  rejected; masked numbers appear as the string `"••••••"`; rollout order
  (§ 12).
- `docs/public/llms.txt` regenerated.
- `CLAUDE.md`: a Key Patterns bullet (`render_input_typed` is the schema-aware
  renderer; the json rule; claim resolves the action before rendering) and
  § Secrets in logs (numbers masked).
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
  rejected; numbers containing a secret are masked in job detail / MCP /
  webhook responses.
- Proposed version: minor bump (0.19.0); the release decision is separate.

## 13. Follow-ups (TODO.md, not in this change)

- Plain action hooks never get action defaults or connection resolution: the
  hook job's task is `_hook:<action>` (`hooks.rs:722`), which claim cannot
  find (`rendering.rs:102`).
- A non-secret string field with a templated default is submitted verbatim
  by the Run form (F8).
- The Run form converts `number` but not `integer` (`execute-input.ts:27`).
- `json` in approval forms (D5); `secret: true` on `json` (D4).
- If a provider rejects typeless tool properties: per-provider fallback (D9).
