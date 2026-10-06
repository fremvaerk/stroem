---
title: Upgrading to Tera 2
description: The template engine moves from Tera 1 to Tera 2 — what keeps working, what breaks, and how to fix each case in your workflow YAML
---

This release moves Strøm's template engine from Tera 1 to
[Tera 2](https://keats.github.io/tera/). Every `{{ … }}` and `{% … %}` in a
workspace — step input, action bodies, `when`, `for_each`, agent prompts,
approval messages, hook input, secrets, connections, event-source `env` — is
now rendered by Tera 2. It is a **breaking release**: most workflows keep
working, but a handful of constructs change meaning or become errors. Read
this page before upgrading, then run `stroem validate` over every workspace.

Full design: `docs/superpowers/specs/2026-10-06-tera-2-upgrade-design.md`.

## Rollout

Templates render on the **server** (claim, cascade, dispatch, hooks, event
sources) and in the **CLI** (`stroem run`, `stroem validate`) — never on
workers. A job can be claimed on one server replica and cascaded on another,
so **upgrade all server replicas together** (a rolling deploy with mixed
versions can evaluate the same template two different ways). The Helm chart's
default is a rolling update, so for the upgrade either scale the server to
1 replica first, or switch the strategy to `Recreate`; otherwise accept that
during the rolling window a job may render on either version. Workers do not
need to be upgraded in lockstep for templating.

## Kept from Tera 1

Strøm adds a compatibility layer so the most common Tera 1 behaviour keeps
working unchanged. You do not need to edit anything for these.

- **C1 — `default` also replaces `null`.** `{{ step.output.x | default(value='n/a') }}`
  still yields `n/a` when the step was skipped, failed or suspended (its
  `output` is `null`). Tera 2's own `default` only replaced *undefined*; Strøm
  overrides it. `default(value=…, boolean=true)` keeps Tera 2's truthiness
  mode (also replaces `""`, `0`, `[]`, `false`).
- **C2 — `state` / `global_state` are `null` before the first snapshot.**
  `{{ state.cursor | default(value=0) }}` and `{% if state.cursor %}` still
  work on a task's first run, and `{{ not state }}` is still true. (Only
  `{{ state is defined }}` changes: it is now true even without a snapshot.)
  Only one level (`state.x`) is covered: `state.a.b` with no snapshot still
  errors — write `state?.a?.b` or use `default`.
- **C3 — Tera 1 filters and functions restored**, ported from Tera 1's own
  implementation so output is byte-identical: `as_str`, `trim_start_matches`,
  `trim_end_matches`, `linebreaksbr`, `map(attribute=)`,
  `filter(attribute=, value=)`, `concat(with=)`, `slice(start=, end=)`,
  `date(format=, timezone=)` and `now(timestamp=, utc=)`. `json_encode` is
  also available (same output as before; keys of maps that come from the
  context are sorted, map literals written in a template keep insertion
  order). `indent` and `unique` are kept with Tera 1 semantics too:
  `indent(prefix=…)` works as in Tera 1 (and `width=` also works), and
  `unique` is case-insensitive by default, with `case_sensitive=` and
  `attribute=` as in Tera 1.
- **C4 — `when:` falsiness is decided on the rendered text.** Empty,
  `false`, `null`, `none` (case-insensitive) are false, as before — plus
  `[]`, `{}` and any number equal to zero. See [item 12](#12-when-treats-empty-arrays-empty-maps-and-numeric-zero-as-false)
  and the [Conditionals guide](/guides/conditionals/#truthiness-rules).

## What changes

### 1. Only the last path segment may be undefined

In Tera 1, a missing field anywhere in a path made `default`, `if`, `not` and
`is defined` behave as "absent". In Tera 2 only the **last** segment may be
missing. A missing *middle* segment, or a path that starts at an undefined
variable, is an error — even under `default`.

```yaml
# Before: fine in Tera 1, an error in Tera 2 when `build` has no `meta` field
when: "{% if build.output.meta.tag %}true{% endif %}"
input:
  tag: "{{ build.output.meta.tag | default(value='latest') }}"
```

```yaml
# After: optional chaining stops at the first missing segment
when: "{% if build.output?.meta?.tag %}true{% endif %}"
input:
  tag: "{{ build.output?.meta?.tag | default(value='latest') }}"
```

The same applies to `not a.b` and `a.b is defined` when `a` itself is
undefined. Use `a?.b`. (`state` and `global_state` are exempt — see C2.)

### 2. `and` / `or` return an operand

`and` / `or` now behave like Python: they return one of their operands rather
than a boolean. An undefined operand that ends up *rendered* is an error.

```yaml
# Before: renders "true" / "false" in Tera 1; errors in Tera 2 if `flag` is absent
when: "{{ input.flag and input.mode == 'fast' }}"
```

```yaml
# After: guard the optional field
when: "{{ input.flag | default(value=false) and input.mode == 'fast' }}"
```

`when:` evaluates the rendered result with the C4 rule, so `[] and x`,
`0 or 1` and friends evaluate the way the expression reads.

### 3. Comparing against a missing field is `false`, not an error

`{{ a.missing == 'v' }}` (and `<`, `>`, …) used to fail the step; it now
renders `false`. A `when:` that used to **fail** its step because of a typo'd
or missing field may now quietly **skip** it. `~` (concatenation) with a
missing field renders `""`, and `json_encode` of one renders `null`.

```yaml
# Before: step fails with "variable not found" — you notice the typo
when: "{{ check.output.statuss == 'ok' }}"
# After: step is skipped (the comparison is false) — double-check field names
```

### 4. Output format of objects, arrays and floats

| Expression | Tera 1 | Tera 2 |
|---|---|---|
| `{{ obj }}` | `[object]` | `{"k": "v", "n": 1}` |
| `{{ ["a", "b"] }}` | `[a, b]` | `["a", "b"]` |
| `{{ 10 / 2 }}` | `5` | `5.0` |
| `{{ 2.7 \| round }}` | `3` | `3.0` |

Wherever a rendered object/array is consumed as text, check the result. When
you need JSON, ask for it explicitly:

```yaml
# After: explicit and stable
for_each: "{{ fetch.output.items | json_encode() }}"
input:
  payload: "{{ build.output | json_encode() }}"
```

:::caution
`{{ obj }}` now renders the **whole object**. `"dsn={{ input.db }}"` with a
connection-typed input writes every property of the connection — password
included — into the step input, where Tera 1 wrote `[object]`. Properties
marked `secret: true` are still masked when the job is read, but pick the
fields you mean (`{{ input.db.host }}`) instead of rendering the object. See
[Secrets](/guides/secrets/).
:::

### 5. String literals process backslash escapes

Inside a string literal in a template, `\n \t \r \\ \" \' \/` are now
unescaped, and **any other** backslash sequence is a syntax error. Tera 1 kept
backslashes literally.

```yaml
# Before: worked in Tera 1, syntax error in Tera 2
script: |
  echo {{ input.path | replace(from='C:\path', to='/') }}
```

```yaml
# After: double the backslash (Tera sees one escaped backslash)
script: |
  echo {{ input.path | replace(from='C:\\path', to='/') }}
```

YAML and Tera each process escapes, as two separate layers. The snippets use
a block scalar (`script: |`) because it passes backslashes to Tera untouched.
In a YAML *double-quoted* string, YAML would consume the backslashes first
(`\p` is not even a valid YAML escape, and `\\` collapses to one backslash
before Tera sees it), so use a block scalar or a single-quoted YAML string
for templates containing backslashes.

### 6. Tests take keyword arguments; macros and `.0` indexing are gone

```yaml
# Before
when: "{{ input.name is starting_with('prod') }}"
when: "{{ input.n is divisibleby(3) }}"
when: "{{ input.cfg is object }}"
```

```yaml
# After
when: "{{ input.name is starting_with(pat='prod') }}"
when: "{{ input.n is divisible_by(divisor=3) }}"
when: "{{ input.cfg is map }}"
```

`{% macro %}` / `{% import %}` no longer exist, and `list.0` must be written
`list[0]`.

### 7. Stricter numeric and list filters

- `int` and `float` **error** on unparsable input (Tera 1 returned `0`):
  `{{ "abc" | int }}` fails, and the `default=` kwarg is gone — it is no
  longer accepted. Guard with `{% if x is number %}` or validate the input.
  Note `is number` is false for numeric strings such as `"42"`.
- `round(method="common")` is invalid — use `round` without `method`, or
  `"ceil"` / `"floor"`.
- `truncate` requires `length=`.
- `first`, `last` and `nth` on an empty array give `none` instead of Tera 1's
  `""`.

```yaml
# Before
input:
  count: "{{ state.count | int }}"          # silently 0 on garbage
  label: "{{ input.name | truncate }}"
# After
input:
  count: "{{ state.count | default(value=0) | int }}"
  label: "{{ input.name | truncate(length=40) }}"
```

### 8. Unknown filters, tests and functions fail at parse time

Tera 2 checks every filter, test and function when the template is compiled
— including in branches that never execute. A typo that used to hide in an
untaken `{% if %}` now fails the template. `stroem validate` reports unknown
references in `when`, `for_each` and agent `prompt` / `system_prompt` only
(plus secrets and connections, which compile at workspace load); the other
template fields — script, env, input, args, manifest, approval messages and
hook input — are first compiled when a step is claimed.

```yaml
# Fails at parse time even though the branch is never taken
script: "{% if false %}{{ x | urlencode }}{% endif %}echo ok"
```

### 9. Error messages are value-free

Template errors never contain Tera's own text, a rendered value, a variable
name or the template line. They are assembled from a fixed category, the
position, and at most a type name or the failing filter's name:

```text
# Illustrative shapes; exact text may differ slightly
template rendering failed (line 1, column 12)
undefined variable or field (line 1, column 4)
filter `int` failed (line 1, column 14)
a filter received a value of the wrong type (expected f64, got string)
filter `urlencode` is not available in Tera 2; see the upgrade guide
```

This is deliberate: Tera 2 can quote values (including transformed secrets)
in its messages, and those messages would otherwise reach job logs, step
errors and API responses. Anything you match on (alerts, scripts) that
depended on Tera 1's wording needs updating.

For full detail, reproduce the problem locally: **`stroem run` and
`stroem validate` print Tera's complete report** (with the source line and
available fields) under the value-free line. The operator running the CLI
already holds the secrets.

Errors that arise *after* rendering — for example a rendered connection name
that does not resolve — name the **input field** and never the rendered
value: `Input field 'db': no connection with that name exists`.

### 10. Reserved step names

A flow step named like a Tera 2 keyword can no longer be referenced from a
template: `none`, `null`, `self`, `loop`, `break`, `continue`, `true`,
`false`, `and`, `or`, `not`, `is`, `in`, `if`, `else`. The workspace still
loads — a step with such a name that is never referenced keeps working — but
the server logs `[render] step 'none' is a Tera keyword and cannot be
referenced in templates` when it renders. Rename the step.

```yaml
# Before
flow:
  loop:                      # `{{ loop.output.x }}` now fails
    action: poll
# After
flow:
  poller:
    action: poll
```

### 11. Filters that were not restored

These Tera 1 builtins are **not** available in Tera 2 and were deliberately
not restored: `urlencode`, `urlencode_strict`, `slugify`, `filesizeformat`,
`striptags`, `addslashes`, and the `get_env` function. Using one fails with
``filter `urlencode` is not available in Tera 2; see the upgrade guide``.

Also removed in Tera 2 and not restored: the `spaceless` filter, the
`matching(...)` test (`is matching`) and the `get_random()` function.

Do the transformation where it is cheap and explicit — in the script:

```yaml
# Before
script: "curl 'https://example.com/?q={{ input.q | urlencode }}'"
# After
script: |
  q=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$Q")
  curl "https://example.com/?q=$q"
env:
  Q: "{{ input.q }}"
```

Passing the value through `env` also avoids shell-quoting problems.

### 12. `when:` treats empty arrays, empty maps and numeric zero as false

`when:` is false when the rendered text is empty, `false`, `null`, `none`
(case-insensitive), `[]`, `{}` or a number equal to zero (`0`, `0.0`, `-0.0`).
The one behaviour change from Tera 1: a condition that renders an **empty
array** used to be truthy (it rendered `[]`) and is now false.

```yaml
# Before: ran even when there were no items ("[]" was truthy)
when: "{{ scan.output.items }}"
# After: skipped when there are no items — usually what was meant
```

### 13. Workspace load errors name the item, not the value

A workspace whose secret or connection template fails to render does not
load. The error (shown in the UI, API and MCP, and logged) names which secret
or connection and the value-free message. Illustrative shape (the wording
of the surrounding context may differ):

```text
Failed to render secret 'DB_PASSWORD': filter `int` failed (line 1, column 4)
```

To see Tera's or `vals`' full output, run `stroem validate` or `stroem run`
against the workspace locally. `vals` stderr is shown **only** there; the
server logs no longer carry it. Local runs use the operator's own
credentials, so a failure that only happens in the pod (for example a missing
IAM permission) may not reproduce locally.

## Checklist

1. Upgrade all server replicas together (scale to 1 or use `Recreate`).
2. Run `stroem validate` on every workspace and fix the unknown-filter and
   syntax errors (items 5, 6, 8, 11). It compiles only `when`, `for_each` and
   agent prompts (plus secrets and connections at load); the other template
   fields are first compiled at claim time. `stroem run` only runs tasks made
   entirely of local `type: script` steps.
3. Search your YAML for `| default` on nested paths, `{% if a.b %}` over
   optional parents, and `and` / `or` over optional input (items 1, 2).
4. Search for `when:` over arrays or comparisons against fields that might be
   misspelt (items 3, 12).
5. Search for step names from item 10.
6. Replace `{{ obj }}` / `{{ array }}` used as text with `| json_encode()` or
   explicit fields (item 4), and review any template that renders a
   connection object.
7. Update anything that parses template error text (item 9).
