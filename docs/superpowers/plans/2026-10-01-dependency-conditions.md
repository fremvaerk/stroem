# Dependency Conditions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Strøm's binary `continue_on_failure`/`continue_when_skipped`/`optional` dependency model with typed per-edge outcome acceptance (`{step, accept}` plus `all`/`any` grouping), so a dependent can name exactly which of a dependency's terminal outcomes satisfy it — closing the "tolerate failure but block a choice-skip" gap no binary marker could express.

**Architecture:** A new recursive `DependsOnEntry` schema type (in a new `stroem-common::depends_on` module) replaces `FlowStep.depends_on: Vec<String>`. `gate.rs`'s verdict logic is replaced by a tree evaluator with a uniform "wait for everything referenced, then decide once" barrier. `cascade.rs`'s P1 (promote) and P2 (skip-unreachable) phases merge into one phase with its own internal relay loop (rebuilding its template context every inner iteration) so multi-hop chains still resolve within one outer cascade pass — P0 (rollup) and P3 (placeholders) are structurally unchanged. `continue_on_failure` keeps its current self-scoped meaning (protects job status only); the old graph-walking `caught_steps()`/`failure_caught()` job-status mechanism is deleted and replaced by a direct per-row flag lookup everywhere it was used (settlement, hooks, restart preview).

**Tech Stack:** Rust (stroem-common, stroem-server, stroem-db, stroem-cli), sqlx/Postgres (no schema migration), TypeScript (UI types only — no UI behavior change in this plan).

**Spec:** `docs/superpowers/specs/2026-10-01-dependency-conditions-design.md` (revision 5, Codex sign-off) — the plan argues from this spec; executors should read both. Where this plan cites a line number in existing code, it was verified by reading the file directly while writing this plan (not assumed).

## Global Constraints

- No DB schema change. `job_step.skip_reason` keeps writing its existing four values (`condition`, `empty`, `cascade`, `unreachable`) — `cascade` and `unreachable` are now both *read* as the `omitted` outcome, but going forward the gate only ever *writes* `unreachable` for a blocked step (never `cascade` again; `cascade` only appears on pre-upgrade rows).
- `continue_on_failure` keeps its exact current name and self-scoped meaning (own `Failed`/own job-status-exemption only, no propagation to dependents) — the user explicitly declined to rename it (spec Review Log, 2026-10-01).
- `continue_when_skipped` is retired as a flag. Any workspace config still setting it must get a named, actionable parse/validation error pointing at the upgrade guide — never silent acceptance, never a fixed prescribed replacement (the right `accept` set is context-dependent per spec §9).
- Every object-shaped `depends_on` entry (`{step, accept}`, `{all: [...]}`, `{any: [...]}`) must reject unknown keys — `#[serde(deny_unknown_fields)]` as a *container* attribute on a named struct per variant, never attempted on an enum variant directly (doesn't compile under serde — spec §7 documents exactly why).
- Every parse error in a `depends_on` entry must propagate as a hard error — no `.unwrap_or_default()` swallowing, for both the inline-step and reference-step flow-step parse paths.
- Duplicate-reference rejection is scoped to direct siblings of the same `all`/`any` group (including the implicit root group) — never the whole tree. The same step name may legitimately recur across different branches with different `accept` sets.
- An absent `depends_on` field or an explicit `depends_on: []` is unchanged from today (no dependencies, immediately satisfied) at the **top level only**. A *nested* `all: []` or `any: []` is always a validation error.
- The render-context fix that lets a tolerated-failed loop rollup's output reach a template is scoped to `for_each` placeholders specifically — never a blanket change to how an *ordinary* failed step's `output` renders (a rejected approval step already stores retained output today; this plan must not newly expose it).
- Two existing pinned cascade timing tests (`timing_failed_root_keeps_legacy_pass`, `timing_accepted_change_flagged_placeholder_retires_immediately`) must be renamed and have their assertions flipped as part of this work, not left in place with a new test added alongside — they are what pins the actual, deliberate behavior change.
- Ships as 0.18.0 (breaking, on top of 0.17.0).

## Review Focus

- A malformed `depends_on` entry (typo'd key, e.g. `{step: a, accpet: [...]}`, or two discriminating keys at once) silently parsing into something other than what the author wrote, instead of a hard error — Task 1.
- A dependent that explicitly accepts a tolerated loop failure (`accept: [completed, failed]`) still getting `null` for that loop's output, because only three of the four places the data has to flow through got fixed — Task 6.
- The merged promote/skip-unreachable phase losing the one-pass, multi-hop propagation the old two-phase split provided for a `failed`-rooted chain, silently regressing `recalc-pipeline`-shaped workflows to a slower, staler timing than they have today — Task 7.
- A duplicate step reference inside one `all`/`any` group silently picking an arbitrary winner instead of erroring, *or* the opposite mistake: a legitimate cross-branch reference (the same step tested under two different `accept` sets in two different `any` branches) wrongly rejected as a duplicate — Task 4.
- A workspace migrating `continue_on_failure`/`continue_when_skipped` to `accept` sets and silently getting different tolerance than before (narrower or wider) because the two single-flag cases have no exact translation — must surface as an explicit, documented choice, not a silent behavior change nobody notices until a job's outcome differs — Task 12 (migration tests) and Task 13 (docs).

---

## Task 1: Schema types — `DependsOnEntry`, `AcceptSet`, `Outcome`

**Files:**
- Create: `crates/stroem-common/src/depends_on.rs`
- Modify: `crates/stroem-common/src/lib.rs:1-40` (add `pub mod depends_on;`)
- Test: inline `#[cfg(test)] mod tests` in `depends_on.rs`

**Interfaces:**
- Produces: `pub enum Outcome { Completed, Failed, Cancelled, Skipped, Omitted }` (serde `rename_all = "snake_case"`), `pub struct TerminalKeyword` (custom Serialize/Deserialize, literal `"terminal"` only), `pub enum AcceptSet { Terminal(TerminalKeyword), Outcomes(Vec<Outcome>) }` with `pub fn contains(&self, outcome: Outcome) -> bool`, `pub struct StepEntry { pub step: String, pub accept: AcceptSet }`, `pub struct AllEntry { pub all: Vec<DependsOnEntry> }`, `pub struct AnyEntry { pub any: Vec<DependsOnEntry> }`, `pub enum DependsOnEntry { Name(String), Step(StepEntry), All(AllEntry), Any(AnyEntry) }` with `pub fn leaf_name(&self) -> Option<&str>`, `pub fn collect_names<'a>(entries: &'a [DependsOnEntry], out: &mut Vec<&'a str>)`, `pub fn validate_tree(entries: &[DependsOnEntry]) -> Result<(), Vec<String>>`.

- [ ] **Step 1: Write the failing tests for `Outcome` and `TerminalKeyword`**

```rust
// crates/stroem-common/src/depends_on.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_round_trips_snake_case() {
        let json = serde_json::to_string(&Outcome::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Outcome::Completed);
        assert_eq!(
            serde_json::to_string(&Outcome::Omitted).unwrap(),
            "\"omitted\""
        );
    }

    #[test]
    fn terminal_keyword_accepts_only_the_exact_string() {
        let ok: TerminalKeyword = serde_json::from_str("\"terminal\"").unwrap();
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"terminal\"");
        let err = serde_json::from_str::<TerminalKeyword>("\"Terminal\"");
        assert!(err.is_err(), "must reject case variants, not fuzzy-match");
        let err2 = serde_json::from_str::<TerminalKeyword>("\"all\"");
        assert!(err2.is_err());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-common depends_on::tests::outcome_round_trips -- --nocapture`
Expected: FAIL — `depends_on` module does not exist yet.

- [ ] **Step 3: Implement `Outcome` and `TerminalKeyword`**

```rust
//! Per-edge dependency outcome acceptance — spec
//! docs/superpowers/specs/2026-10-01-dependency-conditions-design.md.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Every dependency resolves, once terminal, to exactly one of these.
/// Spec §2.1. No `Pending` variant here — this is the schema-facing
/// vocabulary used inside `accept`, never a row's live state (see
/// `gate::DepOutcome` for that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
    Skipped,
    Omitted,
}

impl Outcome {
    pub const ALL: [Outcome; 5] = [
        Outcome::Completed,
        Outcome::Failed,
        Outcome::Cancelled,
        Outcome::Skipped,
        Outcome::Omitted,
    ];
}

/// Deserializes only from the exact string "terminal"; any other value is a
/// parse error. Serializes back to that same literal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalKeyword;

impl Serialize for TerminalKeyword {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("terminal")
    }
}

impl<'de> Deserialize<'de> for TerminalKeyword {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TerminalVisitor;
        impl Visitor<'_> for TerminalVisitor {
            type Value = TerminalKeyword;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "the literal string \"terminal\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<TerminalKeyword, E> {
                if v == "terminal" {
                    Ok(TerminalKeyword)
                } else {
                    Err(E::custom(format!(
                        "expected the literal string \"terminal\", got \"{v}\""
                    )))
                }
            }
        }
        deserializer.deserialize_str(TerminalVisitor)
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p stroem-common depends_on::tests -- --nocapture`
Expected: PASS (2 tests)

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/depends_on.rs
git commit -m "feat(depends-on): add Outcome and TerminalKeyword schema types"
```

- [ ] **Step 6: Write the failing tests for `AcceptSet`**

```rust
    #[test]
    fn accept_set_outcomes_contains_matches_the_list() {
        let a = AcceptSet::Outcomes(vec![Outcome::Completed, Outcome::Failed]);
        assert!(a.contains(Outcome::Completed));
        assert!(a.contains(Outcome::Failed));
        assert!(!a.contains(Outcome::Skipped));
    }

    #[test]
    fn accept_set_terminal_contains_everything() {
        let a = AcceptSet::Terminal(TerminalKeyword);
        for o in Outcome::ALL {
            assert!(a.contains(o), "{o:?} should be accepted by terminal");
        }
    }

    #[test]
    fn accept_set_deserializes_both_shapes() {
        let list: AcceptSet = serde_json::from_str("[\"completed\",\"failed\"]").unwrap();
        assert!(matches!(list, AcceptSet::Outcomes(v) if v == vec![Outcome::Completed, Outcome::Failed]));
        let term: AcceptSet = serde_json::from_str("\"terminal\"").unwrap();
        assert!(matches!(term, AcceptSet::Terminal(_)));
    }
```

- [ ] **Step 7: Run to verify it fails**

Run: `cargo test -p stroem-common depends_on::tests::accept_set -- --nocapture`
Expected: FAIL — `AcceptSet` does not exist yet.

- [ ] **Step 8: Implement `AcceptSet`**

```rust
/// A non-empty set of outcomes that satisfies one dependency edge, or the
/// literal `terminal` sugar for "all five, I don't care which." Spec §2.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcceptSet {
    Terminal(TerminalKeyword),
    Outcomes(Vec<Outcome>),
}

impl AcceptSet {
    pub fn contains(&self, outcome: Outcome) -> bool {
        match self {
            AcceptSet::Terminal(_) => true,
            AcceptSet::Outcomes(v) => v.contains(&outcome),
        }
    }

    /// The default for a bare-string `depends_on` entry and for a `{step:
    /// ...}` entry with no explicit `accept` — matches today's "must
    /// complete" meaning.
    pub fn default_completed_only() -> Self {
        AcceptSet::Outcomes(vec![Outcome::Completed])
    }
}
```

- [ ] **Step 9: Run to verify it passes, then commit**

Run: `cargo test -p stroem-common depends_on::tests -- --nocapture`
Expected: PASS (5 tests)

```bash
git add crates/stroem-common/src/depends_on.rs
git commit -m "feat(depends-on): add AcceptSet"
```

- [ ] **Step 10: Write the failing tests for `DependsOnEntry` — parsing, `deny_unknown_fields`, and the two-key-ambiguity case**

```rust
    #[test]
    fn bare_name_parses_as_a_name_entry() {
        let e: DependsOnEntry = serde_json::from_str("\"build-sessions\"").unwrap();
        assert!(matches!(e, DependsOnEntry::Name(n) if n == "build-sessions"));
    }

    #[test]
    fn step_entry_parses_with_explicit_accept() {
        let e: DependsOnEntry =
            serde_json::from_str("{\"step\":\"a\",\"accept\":[\"completed\",\"failed\"]}").unwrap();
        match e {
            DependsOnEntry::Step(s) => {
                assert_eq!(s.step, "a");
                assert!(s.accept.contains(Outcome::Failed));
            }
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn step_entry_without_accept_defaults_to_completed_only() {
        let e: DependsOnEntry = serde_json::from_str("{\"step\":\"a\"}").unwrap();
        match e {
            DependsOnEntry::Step(s) => assert!(s.accept.contains(Outcome::Completed) && !s.accept.contains(Outcome::Failed)),
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn all_and_any_groups_parse_and_nest() {
        let e: DependsOnEntry =
            serde_json::from_str("{\"any\":[\"mirror-a\",{\"all\":[\"audit\",\"source\"]}]}").unwrap();
        match e {
            DependsOnEntry::Any(a) => assert_eq!(a.any.len(), 2),
            other => panic!("expected Any, got {other:?}"),
        }
    }

    #[test]
    fn typo_d_key_is_a_hard_error_not_silently_dropped() {
        let err = serde_json::from_str::<DependsOnEntry>("{\"step\":\"a\",\"accpet\":[\"completed\"]}");
        assert!(err.is_err(), "typo'd key must fail to parse, not silently default accept");
    }

    #[test]
    fn two_discriminating_keys_at_once_is_a_hard_error() {
        let err = serde_json::from_str::<DependsOnEntry>(
            "{\"step\":\"a\",\"any\":[\"b\"]}",
        );
        assert!(
            err.is_err(),
            "a mapping with both 'step' and 'any' must not silently resolve to whichever variant matches first"
        );
    }
```

- [ ] **Step 11: Run to verify it fails**

Run: `cargo test -p stroem-common depends_on::tests -- --nocapture`
Expected: FAIL — `DependsOnEntry`/`StepEntry`/`AllEntry`/`AnyEntry` do not exist yet.

- [ ] **Step 12: Implement `StepEntry`, `AllEntry`, `AnyEntry`, `DependsOnEntry`**

```rust
fn default_accept() -> AcceptSet {
    AcceptSet::default_completed_only()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEntry {
    pub step: String,
    #[serde(default = "default_accept")]
    pub accept: AcceptSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllEntry {
    pub all: Vec<DependsOnEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnyEntry {
    pub any: Vec<DependsOnEntry>,
}

/// One entry in a `depends_on` list. `deny_unknown_fields` lives on each
/// named struct above, not on this enum's variants directly — serde treats
/// that as a container attribute, and rejects it on a bare enum variant.
/// See spec §7 for why this is the standard pattern, not a workaround.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DependsOnEntry {
    Name(String),
    Step(StepEntry),
    All(AllEntry),
    Any(AnyEntry),
}

impl DependsOnEntry {
    /// For a leaf (`Name`/`Step`) entry, the step it names; `None` for a
    /// group (`All`/`Any`), which doesn't name exactly one step.
    pub fn leaf_name(&self) -> Option<&str> {
        match self {
            DependsOnEntry::Name(n) => Some(n.as_str()),
            DependsOnEntry::Step(s) => Some(s.step.as_str()),
            DependsOnEntry::All(_) | DependsOnEntry::Any(_) => None,
        }
    }

    /// This entry's direct children, if it's a group; `None` for a leaf.
    pub fn children(&self) -> Option<&[DependsOnEntry]> {
        match self {
            DependsOnEntry::All(a) => Some(&a.all),
            DependsOnEntry::Any(a) => Some(&a.any),
            DependsOnEntry::Name(_) | DependsOnEntry::Step(_) => None,
        }
    }
}
```

- [ ] **Step 13: Run to verify it passes**

Run: `cargo test -p stroem-common depends_on::tests -- --nocapture`
Expected: PASS (11 tests)

- [ ] **Step 14: Commit**

```bash
git add crates/stroem-common/src/depends_on.rs
git commit -m "feat(depends-on): add StepEntry/AllEntry/AnyEntry/DependsOnEntry with deny_unknown_fields"
```

- [ ] **Step 15: Write the failing tests for `collect_names` and `validate_tree`**

```rust
    fn step(name: &str) -> DependsOnEntry {
        DependsOnEntry::Name(name.to_string())
    }

    #[test]
    fn collect_names_walks_the_whole_tree_with_duplicates_preserved() {
        let tree = vec![
            step("a"),
            DependsOnEntry::Any(AnyEntry {
                any: vec![
                    DependsOnEntry::All(AllEntry { all: vec![step("a"), step("b")] }),
                    DependsOnEntry::All(AllEntry { all: vec![step("a"), step("c")] }),
                ],
            }),
        ];
        let mut names = Vec::new();
        collect_names(&tree, &mut names);
        names.sort();
        assert_eq!(names, vec!["a", "a", "a", "b", "c"]);
    }

    #[test]
    fn validate_tree_rejects_duplicate_siblings_in_the_same_group() {
        let tree = vec![step("a"), step("a")];
        let err = validate_tree(&tree).unwrap_err();
        assert!(err.iter().any(|e| e.contains("duplicate") && e.contains('a')));
    }

    #[test]
    fn validate_tree_accepts_the_same_name_in_different_any_branches() {
        // (A completed AND B completed) OR (A failed AND C completed) —
        // `A` legitimately appears twice, once per branch, with different
        // implied roles. This must NOT be rejected (spec §8).
        let tree = vec![DependsOnEntry::Any(AnyEntry {
            any: vec![
                DependsOnEntry::All(AllEntry {
                    all: vec![
                        DependsOnEntry::Step(StepEntry { step: "a".into(), accept: AcceptSet::default_completed_only() }),
                        step("b"),
                    ],
                }),
                DependsOnEntry::All(AllEntry {
                    all: vec![
                        DependsOnEntry::Step(StepEntry {
                            step: "a".into(),
                            accept: AcceptSet::Outcomes(vec![Outcome::Failed]),
                        }),
                        step("c"),
                    ],
                }),
            ],
        })];
        assert!(validate_tree(&tree).is_ok());
    }

    #[test]
    fn validate_tree_rejects_nested_empty_groups_but_allows_empty_root() {
        assert!(validate_tree(&[]).is_ok(), "an empty root depends_on is unchanged from today");
        let nested_empty = vec![DependsOnEntry::All(AllEntry { all: vec![] })];
        let err = validate_tree(&nested_empty).unwrap_err();
        assert!(err.iter().any(|e| e.contains("empty")));
    }

    #[test]
    fn validate_tree_rejects_an_empty_accept_outcomes_list() {
        let tree = vec![DependsOnEntry::Step(StepEntry {
            step: "a".into(),
            accept: AcceptSet::Outcomes(vec![]),
        })];
        let err = validate_tree(&tree).unwrap_err();
        assert!(err.iter().any(|e| e.contains("empty") && e.contains('a')));
    }

    #[test]
    fn validate_tree_runs_against_the_authored_shape_not_a_flattened_one() {
        // Implementation caution from spec §8: check siblings before any
        // normalization. A nested `all` inside an `any` is NOT a sibling of
        // the `any`'s other children even though it shares a name with one.
        let tree = vec![DependsOnEntry::Any(AnyEntry {
            any: vec![
                step("a"),
                DependsOnEntry::All(AllEntry { all: vec![step("a")] }),
            ],
        })];
        // "a" appears once as a direct child of the `any`, and once nested
        // one level deeper inside an `all` that is itself a child of the
        // `any` — these are different groups, so this must be accepted.
        assert!(validate_tree(&tree).is_ok());
    }
```

- [ ] **Step 16: Run to verify it fails**

Run: `cargo test -p stroem-common depends_on::tests -- --nocapture`
Expected: FAIL — `collect_names`/`validate_tree` do not exist yet.

- [ ] **Step 17: Implement `collect_names` and `validate_tree`**

```rust
/// All step names referenced anywhere in this tree, duplicates preserved —
/// callers (cycle detection, unknown-name checks) decide what to do with
/// repeats.
pub fn collect_names<'a>(entries: &'a [DependsOnEntry], out: &mut Vec<&'a str>) {
    for e in entries {
        if let Some(name) = e.leaf_name() {
            out.push(name);
        }
        if let Some(children) = e.children() {
            collect_names(children, out);
        }
    }
}

fn check_siblings(children: &[DependsOnEntry], errors: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    for c in children {
        if let Some(name) = c.leaf_name() {
            if !seen.insert(name) {
                errors.push(format!(
                    "duplicate dependency '{name}' in the same depends_on group"
                ));
            }
        }
    }
}

fn check_nested(entry: &DependsOnEntry, errors: &mut Vec<String>) {
    match entry {
        DependsOnEntry::Step(s) => {
            if let AcceptSet::Outcomes(v) = &s.accept {
                if v.is_empty() {
                    errors.push(format!("'{}' has an empty accept list", s.step));
                }
            }
        }
        DependsOnEntry::All(a) => {
            if a.all.is_empty() {
                errors.push("an 'all' group must not be empty".into());
            }
            check_siblings(&a.all, errors);
            for c in &a.all {
                check_nested(c, errors);
            }
        }
        DependsOnEntry::Any(a) => {
            if a.any.is_empty() {
                errors.push("an 'any' group must not be empty".into());
            }
            check_siblings(&a.any, errors);
            for c in &a.any {
                check_nested(c, errors);
            }
        }
        DependsOnEntry::Name(_) => {}
    }
}

/// Structural validation of a `depends_on` tree: duplicate siblings, empty
/// groups, empty accept lists. Does NOT check that referenced step names
/// exist in the flow — that needs the whole flow map and lives in
/// `validation.rs` (Task 4). An empty root (`entries.is_empty()`) is valid:
/// a step with no dependencies at all, unchanged from today.
pub fn validate_tree(entries: &[DependsOnEntry]) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check_siblings(entries, &mut errors);
    for e in entries {
        check_nested(e, &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
```

- [ ] **Step 18: Run to verify it passes**

Run: `cargo test -p stroem-common depends_on::tests -- --nocapture`
Expected: PASS (17 tests total in this module)

- [ ] **Step 19: Register the module and commit**

```rust
// crates/stroem-common/src/lib.rs — add alongside the other `pub mod` lines
pub mod depends_on;
```

```bash
git add crates/stroem-common/src/depends_on.rs crates/stroem-common/src/lib.rs
git commit -m "feat(depends-on): collect_names and validate_tree, wire up the module"
```

---

## Task 2: Wire `DependsOnEntry` into `FlowStep`, retire `continue_when_skipped`

**Files:**
- Modify: `crates/stroem-common/src/models/workflow.rs:385-434` (struct field), `:436-660ish` (the manual `Deserialize` impl — both the `RefStep` reference-step path and the inline-step manual-map path)
- Test: `crates/stroem-common/src/models/workflow.rs` existing `#[cfg(test)] mod tests` (search for `test_continue_when_skipped_*` — these get replaced, not just added to)

**Interfaces:**
- Consumes: `stroem_common::depends_on::{DependsOnEntry, Outcome}` (Task 1).
- Produces: `FlowStep.depends_on: Vec<DependsOnEntry>` (was `Vec<String>`); `FlowStep.legacy_continue_when_skipped: Option<bool>` (detection-only sentinel, never read for behavior — see Task 4 for the validation error that uses it). `continue_when_skipped: bool` is removed from `FlowStep` entirely.

This task touches a hand-rolled `Deserialize` impl with two parse paths (reference steps via a derived `RefStep`, inline steps via manual `serde_yaml::Mapping` field lookups). Read both paths fully before editing — they are not symmetric today.

- [ ] **Step 1: Write the failing test for depends_on's new shape**

```rust
// crates/stroem-common/src/models/workflow.rs, inside the existing tests module
#[test]
fn test_depends_on_accepts_bare_names_and_step_entries() {
    let yaml = r#"
action: a
depends_on:
  - x
  - step: y
    accept: [completed, failed]
"#;
    let step: FlowStep = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(step.depends_on.len(), 2);
    assert!(matches!(&step.depends_on[0], stroem_common_depends_on_name_matches));
}
```

Actually write it using the real types directly (no placeholder macro):

```rust
#[test]
fn test_depends_on_accepts_bare_names_and_step_entries() {
    use crate::depends_on::{DependsOnEntry, Outcome};
    let yaml = r#"
action: a
depends_on:
  - x
  - step: y
    accept: [completed, failed]
"#;
    let step: FlowStep = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(step.depends_on.len(), 2);
    assert!(matches!(&step.depends_on[0], DependsOnEntry::Name(n) if n == "x"));
    match &step.depends_on[1] {
        DependsOnEntry::Step(s) => {
            assert_eq!(s.step, "y");
            assert!(s.accept.contains(Outcome::Failed));
        }
        other => panic!("expected Step entry, got {other:?}"),
    }
}

#[test]
fn test_malformed_depends_on_entry_is_a_hard_error_inline_step() {
    let yaml = r#"
type: script
script: "echo hi"
depends_on:
  - step: y
    accpet: [completed]
"#;
    let err = serde_yaml::from_str::<FlowStep>(yaml);
    assert!(err.is_err(), "a typo'd key in an inline step's depends_on must fail to parse, not silently drop the dependency");
}

#[test]
fn test_malformed_depends_on_entry_is_a_hard_error_reference_step() {
    let yaml = r#"
action: a
depends_on:
  - step: y
    accpet: [completed]
"#;
    let err = serde_yaml::from_str::<FlowStep>(yaml);
    assert!(err.is_err(), "a typo'd key in a reference step's depends_on must fail to parse, not silently drop the dependency");
}

#[test]
fn test_legacy_continue_when_skipped_is_captured_not_silently_ignored() {
    let yaml = r#"
action: a
depends_on: [x]
continue_when_skipped: true
"#;
    let step: FlowStep = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(step.legacy_continue_when_skipped, Some(true));

    let yaml_inline = r#"
type: script
script: "echo hi"
depends_on: [x]
continue_when_skipped: false
"#;
    let step2: FlowStep = serde_yaml::from_str(yaml_inline).unwrap();
    assert_eq!(step2.legacy_continue_when_skipped, Some(false));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-common test_depends_on_accepts_bare_names test_malformed_depends_on test_legacy_continue_when_skipped -- --nocapture`
Expected: FAIL (compile error — `depends_on` field is still `Vec<String>`, `legacy_continue_when_skipped` doesn't exist)

- [ ] **Step 3: Change the `FlowStep` struct field and remove `continue_when_skipped`'s live behavior**

```rust
// crates/stroem-common/src/models/workflow.rs:385-434 — replace the struct
#[derive(Debug, Clone, Serialize)]
pub struct FlowStep {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<crate::depends_on::DependsOnEntry>,
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
    /// If this step fails or is cancelled, the job does not fail because of
    /// it. Self-scoped only: no effect on dependents. See spec
    /// 2026-10-01 (dependency conditions) §6.
    #[serde(default)]
    pub continue_on_failure: bool,
    /// Detection-only: captures a legacy `continue_when_skipped` value so
    /// validation (Task 4) can emit a named migration error. NEVER read for
    /// behavior — the flag itself is retired. `None` when the key was
    /// absent; `Some(_)` (even `Some(false)`) means the workspace still has
    /// it and hasn't migrated.
    #[serde(default, rename = "continue_when_skipped")]
    pub legacy_continue_when_skipped: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<HumanDuration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_each: Option<serde_json::Value>,
    #[serde(default)]
    pub sequential: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
    #[serde(skip)]
    pub inline_action: Option<ActionDef>,
}
```

- [ ] **Step 4: Fix the reference-step (`RefStep`) parse path**

```rust
// Inside the manual Deserialize impl, `has_action` branch — replace RefStep
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefStep {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    depends_on: Vec<crate::depends_on::DependsOnEntry>,
    #[serde(default)]
    input: HashMap<String, serde_json::Value>,
    #[serde(default)]
    continue_on_failure: bool,
    #[serde(default, rename = "continue_when_skipped")]
    legacy_continue_when_skipped: Option<bool>,
    #[serde(default)]
    timeout: Option<HumanDuration>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    for_each: Option<serde_json::Value>,
    #[serde(default)]
    sequential: bool,
    #[serde(default)]
    retry: Option<RetryConfig>,
}
let ref_step: RefStep =
    serde_yaml::from_value(serde_yaml::Value::Mapping(mapping.clone()))
        .map_err(D::Error::custom)?;
Ok(FlowStep {
    action: ref_step.action,
    name: ref_step.name,
    description: ref_step.description,
    depends_on: ref_step.depends_on,
    input: ref_step.input,
    continue_on_failure: ref_step.continue_on_failure,
    legacy_continue_when_skipped: ref_step.legacy_continue_when_skipped,
    timeout: ref_step.timeout,
    when: ref_step.when,
    for_each: ref_step.for_each,
    sequential: ref_step.sequential,
    retry: ref_step.retry,
    inline_action: None,
})
```

Note the added `#[serde(deny_unknown_fields)]` on `RefStep` itself — this is what turns *any* unrecognized key (not just `continue_when_skipped`, which is still explicitly captured above) into a hard parse error for reference steps, matching the inline path's `step_field_keys` allow-list approach below.

- [ ] **Step 5: Fix the inline-step manual parse path — depends_on error propagation and the `continue_when_skipped` sentinel**

```rust
// In the `has_type` branch: add "continue_when_skipped" to step_field_keys (already there, line 512 — no change needed to the allow-list itself).

// Replace the depends_on parsing (was: .unwrap_or_default() swallowing errors):
let depends_on: Vec<crate::depends_on::DependsOnEntry> = match step_map
    .get(serde_yaml::Value::String("depends_on".into()))
{
    Some(v) => serde_yaml::from_value(v.clone()).map_err(|e| {
        D::Error::custom(format!("invalid depends_on: {e}"))
    })?,
    None => Vec::new(),
};

// Replace the continue_when_skipped parsing (was read into a `bool` with
// unwrap_or(false) — now a sentinel, same error-propagation fix applied):
let legacy_continue_when_skipped: Option<bool> = match step_map
    .get(serde_yaml::Value::String("continue_when_skipped".into()))
{
    Some(v) => Some(serde_yaml::from_value(v.clone()).map_err(|e| {
        D::Error::custom(format!("invalid continue_when_skipped: {e}"))
    })?),
    None => None,
};
```

Update the final `Ok(FlowStep { ... })` construction in this branch to use `legacy_continue_when_skipped` instead of `continue_when_skipped`.

- [ ] **Step 6: Run to verify the new tests pass**

Run: `cargo test -p stroem-common -- --nocapture test_depends_on_accepts_bare_names test_malformed_depends_on test_legacy_continue_when_skipped`
Expected: PASS (4 tests)

- [ ] **Step 7: Fix every other compile error this field rename causes**

Run: `cargo build --workspace 2>&1 | grep "error\[" `
Expected: a list of call sites referencing `FlowStep.continue_when_skipped` or `FlowStep.depends_on` as `Vec<String>`. Do not fix them here — this task only establishes the type. Confirm the failures are limited to: `gate.rs`, `validation.rs`, `dag.rs`, `cascade.rs`, `settlement/{settle,hooks}.rs`, `restart.rs`, `stroem-cli/src/local/run.rs`, `web/api/{tasks,jobs}.rs` — these are fixed in Tasks 3-10. If a compile error shows up anywhere *not* in that list, note it — it's a consumer the spec's §7 enumeration didn't anticipate and needs its own fix before this plan is done.

- [ ] **Step 8: Update the old `continue_when_skipped` behavior tests that no longer apply**

The existing tests `test_continue_when_skipped_defaults_false`, `test_continue_when_skipped_true_on_reference_step`, `test_continue_when_skipped_true_on_inline_step` (search for them — they assert `step.continue_when_skipped == true/false`) must be updated to assert on `step.legacy_continue_when_skipped` instead (`None`/`Some(true)`/`Some(false)`), not deleted — they still pin that the key round-trips through parsing correctly, just not as a live behavior flag anymore.

- [ ] **Step 9: Run the full stroem-common test suite for this file**

Run: `cargo test -p stroem-common models::workflow:: -- --nocapture`
Expected: PASS for every test in this file that doesn't depend on downstream crates (gate.rs consumers will still fail to compile until later tasks — that's expected and tracked by Step 7's list).

- [ ] **Step 10: Commit**

```bash
git add crates/stroem-common/src/models/workflow.rs
git commit -m "feat(depends-on): wire DependsOnEntry into FlowStep, retire continue_when_skipped as a live flag"
```

---

## Task 3: Gate tree evaluator (`gate.rs` rewrite)

**Files:**
- Modify: `crates/stroem-common/src/gate.rs` (near-total rewrite — `DepOutcome`, `Verdict`/`verdict`, `Gate`, `gate`, `caught_steps`, `failure_caught` all change or are deleted)
- Test: `crates/stroem-common/src/gate.rs`'s existing `#[cfg(test)] mod tests` (replaced, not appended to)

**Interfaces:**
- Consumes: `stroem_common::depends_on::{DependsOnEntry, Outcome, AcceptSet}` (Task 1).
- Produces: `pub enum DepOutcome { Pending, Completed, Failed, Cancelled, Skipped, Omitted }` with `pub fn from_row(status: &str, skip_reason: Option<&str>) -> Self`, `pub enum Gate { Open, Wait, Omitted }`, `pub fn gate(depends_on: &[DependsOnEntry], outcome_of: impl Fn(&str) -> DepOutcome) -> Gate`, `pub fn flow_step_name<'a>(step_name: &'a str, loop_source: Option<&'a str>) -> &'a str` (kept, unchanged — still needed by Task 5's settlement/hooks/restart fixes).
- Removed entirely: `Verdict`, `verdict()`, `caught_steps()`, `failure_caught()`.

- [ ] **Step 1: Write the failing tests for the new `DepOutcome` classification**

```rust
// crates/stroem-common/src/gate.rs — replace the whole #[cfg(test)] mod tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::depends_on::{AcceptSet, AllEntry, AnyEntry, DependsOnEntry, Outcome, StepEntry};

    #[test]
    fn from_row_classifies_live_statuses_as_pending() {
        for live in ["pending", "ready", "claimed", "running", "suspended", "bogus"] {
            assert_eq!(DepOutcome::from_row(live, None), DepOutcome::Pending, "{live}");
        }
    }

    #[test]
    fn from_row_classifies_terminal_statuses() {
        assert_eq!(DepOutcome::from_row("completed", None), DepOutcome::Completed);
        assert_eq!(DepOutcome::from_row("failed", None), DepOutcome::Failed);
        assert_eq!(DepOutcome::from_row("cancelled", None), DepOutcome::Cancelled);
    }

    #[test]
    fn from_row_splits_skip_reasons_into_skipped_vs_omitted() {
        // Own-choice reasons -> Skipped.
        assert_eq!(DepOutcome::from_row("skipped", Some("condition")), DepOutcome::Skipped);
        assert_eq!(DepOutcome::from_row("skipped", Some("empty")), DepOutcome::Skipped);
        // Propagated-block reasons (choice- or failure-origin alike) -> Omitted.
        assert_eq!(DepOutcome::from_row("skipped", Some("cascade")), DepOutcome::Omitted);
        assert_eq!(DepOutcome::from_row("skipped", Some("unreachable")), DepOutcome::Omitted);
        // Unknown/NULL reason -> Omitted (conservative, matches the old
        // Skipped(None) "reads as failure-class" rule).
        assert_eq!(DepOutcome::from_row("skipped", Some("brand-new")), DepOutcome::Omitted);
        assert_eq!(DepOutcome::from_row("skipped", None), DepOutcome::Omitted);
    }

    #[test]
    fn dep_outcome_to_schema_outcome_maps_the_four_terminal_cases() {
        assert_eq!(DepOutcome::Completed.as_schema_outcome(), Some(Outcome::Completed));
        assert_eq!(DepOutcome::Failed.as_schema_outcome(), Some(Outcome::Failed));
        assert_eq!(DepOutcome::Cancelled.as_schema_outcome(), Some(Outcome::Cancelled));
        assert_eq!(DepOutcome::Skipped.as_schema_outcome(), Some(Outcome::Skipped));
        assert_eq!(DepOutcome::Omitted.as_schema_outcome(), Some(Outcome::Omitted));
        assert_eq!(DepOutcome::Pending.as_schema_outcome(), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-common gate::tests::from_row gate::tests::dep_outcome -- --nocapture`
Expected: FAIL (compile error — old `DepOutcome` shape, no `as_schema_outcome`)

- [ ] **Step 3: Replace `DepOutcome`**

```rust
// crates/stroem-common/src/gate.rs — replace lines 1-75 (imports through old verdict())
//! Dependency gate: whether a flow step may run, decided from a uniform
//! readiness barrier (wait for every referenced dependency to go terminal)
//! and a typed per-edge `accept` tree evaluation. Spec:
//! docs/superpowers/specs/2026-10-01-dependency-conditions-design.md.

use crate::depends_on::{DependsOnEntry, Outcome};
use crate::models::job::{SkipReason, StepStatus};

/// A dependency's live state as the gate sees it. `Pending` has no schema
/// `Outcome` equivalent — nothing is ever "accepted" while still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepOutcome {
    Pending,
    Completed,
    Failed,
    Cancelled,
    /// The step's own choice: its own `when` was false, or an empty
    /// `for_each` (`SkipReason::Condition` / `SkipReason::Empty`).
    Skipped,
    /// Blocked by its own dependency tree — regardless of whether the
    /// ultimate cause several hops back was a choice-skip or a failure
    /// (`SkipReason::Cascade` / `SkipReason::Unreachable`), or an
    /// unrecognised/NULL reason, read conservatively the same way. Spec
    /// §2.1: one unified outcome, by design — see the migration guide
    /// (spec §9) for what this collapses.
    Omitted,
}

impl DepOutcome {
    pub fn from_row(status: &str, skip_reason: Option<&str>) -> Self {
        match status.parse::<StepStatus>() {
            Ok(StepStatus::Completed) => Self::Completed,
            Ok(StepStatus::Failed) => Self::Failed,
            Ok(StepStatus::Cancelled) => Self::Cancelled,
            Ok(StepStatus::Skipped) => {
                match skip_reason.and_then(|r| r.parse::<SkipReason>().ok()) {
                    Some(SkipReason::Condition) | Some(SkipReason::Empty) => Self::Skipped,
                    Some(SkipReason::Cascade) | Some(SkipReason::Unreachable) | None => {
                        Self::Omitted
                    }
                }
            }
            _ => Self::Pending,
        }
    }

    /// `None` while still pending; the matching schema `Outcome` once
    /// terminal, for matching against an `AcceptSet`.
    pub fn as_schema_outcome(self) -> Option<Outcome> {
        match self {
            DepOutcome::Pending => None,
            DepOutcome::Completed => Some(Outcome::Completed),
            DepOutcome::Failed => Some(Outcome::Failed),
            DepOutcome::Cancelled => Some(Outcome::Cancelled),
            DepOutcome::Skipped => Some(Outcome::Skipped),
            DepOutcome::Omitted => Some(Outcome::Omitted),
        }
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p stroem-common gate::tests::from_row gate::tests::dep_outcome -- --nocapture`
Expected: PASS (4 tests). (The rest of the test module and the rest of the file won't compile yet — expected, fixed in the next steps.)

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/gate.rs
git commit -m "feat(gate): replace DepOutcome's skip-reason split with Skipped/Omitted per spec §2.1"
```

- [ ] **Step 6: Write the failing tests for the tree evaluator**

```rust
    fn outcome_map(pairs: &[(&str, DepOutcome)]) -> impl Fn(&str) -> DepOutcome + '_ {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, o)| *o)
                .unwrap_or(DepOutcome::Pending)
        }
    }

    fn step_entry(name: &str, accept: &[Outcome]) -> DependsOnEntry {
        DependsOnEntry::Step(StepEntry {
            step: name.to_string(),
            accept: AcceptSet::Outcomes(accept.to_vec()),
        })
    }
    fn name_entry(name: &str) -> DependsOnEntry {
        DependsOnEntry::Name(name.to_string())
    }

    #[test]
    fn empty_depends_on_is_immediately_open() {
        assert_eq!(gate(&[], outcome_map(&[])), Gate::Open);
    }

    #[test]
    fn bare_name_requires_completed() {
        let deps = vec![name_entry("a")];
        assert_eq!(gate(&deps, outcome_map(&[("a", DepOutcome::Completed)])), Gate::Open);
        assert_eq!(gate(&deps, outcome_map(&[("a", DepOutcome::Failed)])), Gate::Omitted);
        assert_eq!(gate(&deps, outcome_map(&[])), Gate::Wait);
    }

    #[test]
    fn step_entry_with_explicit_accept_tolerates_failure_but_not_skip() {
        let deps = vec![step_entry("a", &[Outcome::Completed, Outcome::Failed])];
        assert_eq!(gate(&deps, outcome_map(&[("a", DepOutcome::Failed)])), Gate::Open);
        assert_eq!(gate(&deps, outcome_map(&[("a", DepOutcome::Skipped)])), Gate::Omitted);
    }

    #[test]
    fn a_hard_block_still_waits_for_a_pending_sibling() {
        // The deliberate change from today's fail-fast BlockFail dominance
        // (spec §2.3) — pins `timing_accepted_change_flagged_placeholder_retires_immediately`'s new behavior at the gate level.
        let deps = vec![name_entry("a"), name_entry("b")];
        let outcome = outcome_map(&[("a", DepOutcome::Failed)]); // b still pending
        assert_eq!(gate(&deps, outcome), Gate::Wait);
    }

    #[test]
    fn an_any_group_satisfied_by_one_child_still_waits_for_a_pending_sibling() {
        // No short-circuit (spec §12 non-goal) — pinned here, not just claimed.
        let deps = vec![DependsOnEntry::Any(AnyEntry {
            any: vec![name_entry("a"), name_entry("b")],
        })];
        let outcome = outcome_map(&[("a", DepOutcome::Completed)]); // b still pending
        assert_eq!(gate(&deps, outcome), Gate::Wait);
        let outcome2 = outcome_map(&[("a", DepOutcome::Completed), ("b", DepOutcome::Failed)]);
        assert_eq!(gate(&deps, outcome2), Gate::Open);
    }

    #[test]
    fn an_all_group_needs_every_child_satisfied() {
        let deps = vec![DependsOnEntry::All(AllEntry {
            all: vec![name_entry("a"), name_entry("b")],
        })];
        let outcome = outcome_map(&[("a", DepOutcome::Completed), ("b", DepOutcome::Completed)]);
        assert_eq!(gate(&deps, outcome), Gate::Open);
        let outcome2 = outcome_map(&[("a", DepOutcome::Completed), ("b", DepOutcome::Failed)]);
        assert_eq!(gate(&deps, outcome2), Gate::Omitted);
    }

    #[test]
    fn nested_any_inside_all_combines_correctly() {
        // audit must finish (terminal), and at least one mirror must complete.
        let deps = vec![
            step_entry("audit", &Outcome::ALL),
            DependsOnEntry::Any(AnyEntry { any: vec![name_entry("mirror-a"), name_entry("mirror-b")] }),
        ];
        let outcome = outcome_map(&[
            ("audit", DepOutcome::Failed),
            ("mirror-a", DepOutcome::Failed),
            ("mirror-b", DepOutcome::Completed),
        ]);
        assert_eq!(gate(&deps, outcome), Gate::Open);
        let outcome2 = outcome_map(&[
            ("audit", DepOutcome::Completed),
            ("mirror-a", DepOutcome::Failed),
            ("mirror-b", DepOutcome::Failed),
        ]);
        assert_eq!(gate(&deps, outcome2), Gate::Omitted);
    }
```

- [ ] **Step 7: Run to verify it fails**

Run: `cargo test -p stroem-common gate::tests -- --nocapture`
Expected: FAIL — `gate()`'s old signature (`&[String], &HashMap<...>, impl Fn`) doesn't match.

- [ ] **Step 8: Implement the tree evaluator, replacing `Verdict`/`verdict`/the old `gate`**

```rust
/// The gate's decision for one step. Spec §2.3: evaluated only once every
/// referenced dependency is terminal — no fail-fast short-circuit on a
/// hard block, no special-casing a group already logically decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Open,
    Wait,
    Omitted,
}

fn all_terminal(entry: &DependsOnEntry, outcome_of: &impl Fn(&str) -> DepOutcome) -> bool {
    if let Some(name) = entry.leaf_name() {
        return !matches!(outcome_of(name), DepOutcome::Pending);
    }
    entry
        .children()
        .expect("leaf_name() and children() are exhaustive over the 4 variants")
        .iter()
        .all(|c| all_terminal(c, outcome_of))
}

fn satisfied(entry: &DependsOnEntry, outcome_of: &impl Fn(&str) -> DepOutcome) -> bool {
    match entry {
        DependsOnEntry::Name(n) => {
            matches!(outcome_of(n), DepOutcome::Completed)
        }
        DependsOnEntry::Step(s) => outcome_of(&s.step)
            .as_schema_outcome()
            .is_some_and(|o| s.accept.contains(o)),
        DependsOnEntry::All(a) => a.all.iter().all(|c| satisfied(c, outcome_of)),
        DependsOnEntry::Any(a) => a.any.iter().any(|c| satisfied(c, outcome_of)),
    }
}

/// Evaluate one step's `depends_on` tree (an implicit `all` group at the
/// top level) against a row-outcome lookup. Spec §2.3: wait for every
/// referenced step to go terminal before deciding anything, then evaluate
/// the whole tree once.
pub fn gate(depends_on: &[DependsOnEntry], outcome_of: impl Fn(&str) -> DepOutcome) -> Gate {
    if !depends_on.iter().all(|e| all_terminal(e, &outcome_of)) {
        return Gate::Wait;
    }
    if depends_on.iter().all(|e| satisfied(e, &outcome_of)) {
        Gate::Open
    } else {
        Gate::Omitted
    }
}

/// The flow step a `job_step` row belongs to: a loop instance is judged by
/// its placeholder. Unchanged from the prior revision — still needed by
/// job-status/hooks/restart (Task 5).
pub fn flow_step_name<'a>(step_name: &'a str, loop_source: Option<&'a str>) -> &'a str {
    if let Some(src) = loop_source {
        return src;
    }
    match step_name.find('[') {
        Some(i) => &step_name[..i],
        None => step_name,
    }
}
```

Delete the old `Verdict` enum, `verdict()` function, `caught_steps()`, and `failure_caught()` entirely — search the file for each and remove them along with their doc comments.

- [ ] **Step 9: Run to verify the new tests pass**

Run: `cargo test -p stroem-common gate::tests -- --nocapture`
Expected: PASS (all tree-evaluator tests; `flow_step_name`'s own existing tests, if any remain in the file, should still pass unchanged)

- [ ] **Step 10: Delete the now-dead old tests**

Search for and remove: `verdict_table`, `gate_reads_the_dependency_flags_only`, `gate_combines_with_failure_first_then_wait_then_skip`, `caught_steps_is_structural`, `caught_steps_diamond_needs_every_path`, `caught_steps_handles_a_long_chain`, `caught_steps_terminates_on_a_cycle` — the mechanisms they tested (`Verdict`, `caught_steps`) no longer exist. Do not leave them commented out.

- [ ] **Step 11: Run the whole module's tests once more, then commit**

Run: `cargo test -p stroem-common gate:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-common/src/gate.rs
git commit -m "feat(gate): replace Verdict-based gating with a uniform tree evaluator, delete caught_steps/failure_caught"
```

---

## Task 4: Validation — unknown names, cycles, sibling duplicates, legacy flag

**Files:**
- Modify: `crates/stroem-common/src/dag.rs:6-60ish` (`ready_steps`, `validate_dag` — both iterate `step.depends_on` as if it were `Vec<String>`)
- Modify: `crates/stroem-common/src/validation.rs:190-345ish` (unknown-dependency check, the `continue_when_skipped`-without-dependents warning, `is_transitively_skippable` at `:1116-1150ish`) — add the new legacy-flag error and the tree-shape validation call
- Test: existing test modules in both files

**Interfaces:**
- Consumes: `depends_on::{collect_names, validate_tree, DependsOnEntry}` (Task 1), `FlowStep.legacy_continue_when_skipped` (Task 2).

- [ ] **Step 1: Write the failing tests for `dag.rs`'s updated iteration**

```rust
// crates/stroem-common/src/dag.rs — in the existing tests module
#[test]
fn validate_dag_walks_grouped_dependencies() {
    use crate::depends_on::{AnyEntry, DependsOnEntry};
    let mut flow = HashMap::new();
    flow.insert(
        "m".to_string(),
        make_step_with_entries(
            "a",
            vec![DependsOnEntry::Any(AnyEntry {
                any: vec![DependsOnEntry::Name("x".into()), DependsOnEntry::Name("y".into())],
            })],
        ),
    );
    flow.insert("x".to_string(), make_step("a", vec![]));
    flow.insert("y".to_string(), make_step("a", vec![]));
    let order = validate_dag(&flow).unwrap();
    assert!(order.iter().position(|s| s == "x").unwrap() < order.iter().position(|s| s == "m").unwrap());
}

#[test]
fn validate_dag_rejects_a_grouped_reference_to_a_nonexistent_step() {
    use crate::depends_on::{AllEntry, DependsOnEntry};
    let mut flow = HashMap::new();
    flow.insert(
        "m".to_string(),
        make_step_with_entries(
            "a",
            vec![DependsOnEntry::All(AllEntry { all: vec![DependsOnEntry::Name("ghost".into())] })],
        ),
    );
    assert!(validate_dag(&flow).is_err());
}
```

(Add a `make_step_with_entries` test helper alongside the existing `make_step` — same shape, but taking `Vec<DependsOnEntry>` directly instead of building from `Vec<&str>`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-common dag:: -- --nocapture`
Expected: FAIL (compile error — the whole file still assumes `Vec<String>`)

- [ ] **Step 3: Fix `ready_steps` and `validate_dag`**

```rust
// crates/stroem-common/src/dag.rs
use crate::depends_on;

pub fn ready_steps(flow: &HashMap<String, FlowStep>, completed: &HashSet<String>) -> Vec<String> {
    flow.iter()
        .filter_map(|(step_name, step)| {
            if completed.contains(step_name) {
                return None;
            }
            let mut names = Vec::new();
            depends_on::collect_names(&step.depends_on, &mut names);
            let all_deps_met = names.iter().all(|dep| completed.contains(*dep));
            if all_deps_met {
                Some(step_name.clone())
            } else {
                None
            }
        })
        .collect()
}

pub fn validate_dag(flow: &HashMap<String, FlowStep>) -> Result<Vec<String>> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adj_list: HashMap<&str, Vec<&str>> = HashMap::new();

    for step_name in flow.keys() {
        in_degree.insert(step_name.as_str(), 0);
        adj_list.insert(step_name.as_str(), Vec::new());
    }

    for (step_name, step) in flow {
        let mut names = Vec::new();
        depends_on::collect_names(&step.depends_on, &mut names);
        for dep in names {
            if !flow.contains_key(dep) {
                bail!("Step '{}' depends on non-existent step '{}'", step_name, dep);
            }
            adj_list
                .get_mut(dep)
                .expect("dep key was inserted during initialization")
                .push(step_name.as_str());
        }
    }
    // ... rest of the function (in-degree counting, topo sort) is unchanged —
    // it already operates on the adj_list/in_degree maps built above, not on
    // FlowStep.depends_on directly.
```

(Read the rest of `validate_dag` below the dependency-building loop before editing — the in-degree/topo-sort body after this point does not reference `depends_on` directly and should need no changes, but confirm this while implementing rather than assuming.)

- [ ] **Step 4: Run to verify it passes, then commit**

Run: `cargo test -p stroem-common dag:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-common/src/dag.rs
git commit -m "fix(dag): walk grouped depends_on entries via collect_names"
```

- [ ] **Step 5: Write the failing tests for the legacy-flag validation error**

```rust
// crates/stroem-common/src/validation.rs
#[test]
fn test_legacy_continue_when_skipped_produces_a_named_migration_error() {
    let yaml = r#"
tasks:
  t:
    flow:
      a: { action: noop }
      b: { action: noop, depends_on: [a], continue_when_skipped: true }
"#;
    let errors = validate_workflow_config_errors_only(yaml); // see Step 6 note
    assert!(
        errors.iter().any(|e| e.contains("continue_when_skipped") && e.contains("0.18")),
        "expected a named migration error, got: {errors:?}"
    );
}

#[test]
fn test_duplicate_sibling_dependency_is_rejected() {
    let yaml = r#"
tasks:
  t:
    flow:
      a: { action: noop }
      m: { action: noop, depends_on: [a, a] }
"#;
    let errors = validate_workflow_config_errors_only(yaml);
    assert!(errors.iter().any(|e| e.contains("duplicate") && e.contains('a')));
}

#[test]
fn test_cross_branch_duplicate_reference_is_accepted() {
    let yaml = r#"
tasks:
  t:
    flow:
      a: { action: noop }
      b: { action: noop }
      c: { action: noop }
      m:
        action: noop
        depends_on:
          - any:
              - all: [{step: a, accept: [completed]}, b]
              - all: [{step: a, accept: [failed]}, c]
"#;
    let errors = validate_workflow_config_errors_only(yaml);
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}
```

(`validate_workflow_config_errors_only` is a hypothetical name — check the existing test module for the actual helper name used by tests like `test_validate_task_cycle` around line 2497, and call that one instead; don't invent a new entry point if one already exists that returns a list of error/warning strings.)

- [ ] **Step 6: Run to verify it fails**

Run: `cargo test -p stroem-common validation::tests::test_legacy_continue_when_skipped validation::tests::test_duplicate_sibling validation::tests::test_cross_branch -- --nocapture`
Expected: FAIL (no such errors produced yet)

- [ ] **Step 7: Add the validation checks**

Locate the per-step validation loop (around line 190-200, where the existing "Validate depends_on references" comment is). Add, for each step:

```rust
// Legacy flag detection — a named, actionable error, not a generic "unknown field."
if step.legacy_continue_when_skipped.is_some() {
    errors.push(format!(
        "Task '{}' step '{}': continue_when_skipped was removed in 0.18.0; \
         see the 0.18 upgrade guide to choose the right `accept` set for \
         this step's dependents — it depends on what else was present on \
         this dependency.",
        task_name, step_name
    ));
}

// Tree-shape validation (duplicate siblings, empty groups, empty accept lists).
if let Err(tree_errors) = stroem_common::depends_on::validate_tree(&step.depends_on) {
    for e in tree_errors {
        errors.push(format!("Task '{}' step '{}': {}", task_name, step_name, e));
    }
}

// Unknown-dependency check — was `for dep in &step.depends_on`, now walks
// the tree.
let mut referenced = Vec::new();
stroem_common::depends_on::collect_names(&step.depends_on, &mut referenced);
for dep in referenced {
    if !flow.contains_key(dep) {
        errors.push(format!(
            "Task '{}' step '{}' depends on non-existent step '{}'",
            task_name, step_name, dep
        ));
    }
}
```

Remove whatever the *old* unknown-dependency loop at this location did (it likely iterated `step.depends_on` directly as strings — replace it with the `collect_names`-based version above, don't leave both).

- [ ] **Step 8: Retire the old "will be skipped whenever its dependency is skipped" warning and its `is_transitively_skippable` helper**

This warning and `is_transitively_skippable` (around line 303-340 and 1116-1150) existed specifically to tell an author "add `continue_when_skipped`" — that flag is gone. Delete:
- The warning block at ~303-340 (`"will be skipped whenever '{}' is skipped"` and the `continue_when_skipped`-without-dependents warning above it).
- The `is_transitively_skippable` function.
- Their tests: `test_continue_when_skipped_without_dependents_warns`, `test_continue_when_skipped_with_depends_on_still_warns_without_dependents`, `test_continue_when_skipped_with_dependent_does_not_warn`, and every `test_transitively_skippable_*` test (search for the full set — there are several scenario tests around line 6560-6710 per the earlier grep).

- [ ] **Step 9: Run to verify the new tests pass and old dead tests are gone**

Run: `cargo test -p stroem-common validation:: -- --nocapture`
Expected: PASS — the 3 new tests pass; none of the deleted tests' names appear in the output.

- [ ] **Step 10: Commit**

```bash
git add crates/stroem-common/src/validation.rs
git commit -m "feat(validation): legacy continue_when_skipped error, tree-shape validation, retire the transitively-skippable warning"
```

---

## Task 5: Job status, hooks, restart — replace `caught_steps`/`failure_caught`

**Files:**
- Modify: `crates/stroem-server/src/settlement/settle.rs:19-55ish`
- Modify: `crates/stroem-server/src/settlement/hooks.rs` (around line 485-510, search for `caught_steps`/`failure_caught`)
- Modify: `crates/stroem-server/src/restart.rs` (around line 88, search for `caught_steps`)
- Test: each file's existing test module (`failure_caught_downstream_completes` in settle.rs, `carried_failure_caught_downstream_is_tolerated` in restart.rs — both get renamed, since "caught downstream" is no longer a thing)

**Interfaces:**
- Consumes: `stroem_common::gate::flow_step_name` (Task 3, unchanged), `FlowStep.continue_on_failure` (unchanged field).

- [ ] **Step 1: Write the failing test pinning the new, simpler job-status rule**

```rust
// crates/stroem-server/src/settlement/settle.rs, in its test module
#[test]
fn only_a_failed_step_s_own_flag_excuses_it_not_a_downstream_catcher() {
    // a(no flag) -> b(cof: true) -> c. Under the OLD caught_steps() rule, a's
    // failure was "caught" because b catches it structurally, even though a
    // itself has no flag. Under the new rule, only a's own flag counts.
    let task = task_with_flow(vec![
        ("a", fs_no_flags(&[])),
        ("b", fs_cof(&["a"])),
        ("c", fs_no_flags(&["b"])),
    ]);
    let steps = vec![
        row("a", "failed"),
        row_skipped("b", "unreachable"),
        row_skipped("c", "unreachable"),
    ];
    let settled = decide(&task, &steps).expect("all rows terminal");
    assert_eq!(settled.status, JobStatus::Failed, "a has no flag of its own — must fail, regardless of b's");
}

#[test]
fn a_step_s_own_continue_on_failure_still_excuses_its_own_failure() {
    let task = task_with_flow(vec![("a", fs_cof(&[]))]);
    let steps = vec![row("a", "failed")];
    let settled = decide(&task, &steps).expect("all rows terminal");
    assert_eq!(settled.status, JobStatus::Completed);
}
```

(Use whatever test helpers — `task_with_flow`, `fs_no_flags`, `fs_cof`, `row`, `row_skipped` — the existing test module in this file already defines; check for them before inventing new ones.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-server settlement::settle::tests::only_a_failed_step settlement::settle::tests::a_step_s_own -- --nocapture`
Expected: FAIL (today's `caught_steps` would actually make the first test pass for the wrong reason — rename/replace it rather than trusting the old green)

- [ ] **Step 3: Replace the job-status check**

```rust
// crates/stroem-server/src/settlement/settle.rs:33-40ish
// Spec 2026-10-01 §6: only a failed row's OWN continue_on_failure excuses
// it — no more structural "caught somewhere downstream" walk. Loop
// instances are judged by their placeholder (flow_step_name).
let untolerated_failure = steps.iter().any(|s| {
    s.status == StepStatus::Failed.as_ref()
        && !task
            .flow
            .get(stroem_common::gate::flow_step_name(&s.step_name, s.loop_source.as_deref()))
            .is_some_and(|fs| fs.continue_on_failure)
});
```

Delete the old `let caught = stroem_common::gate::caught_steps(&task.flow);` line entirely — it's now unused.

- [ ] **Step 4: Run to verify it passes, update the old test name, then commit**

Run: `cargo test -p stroem-server settlement::settle:: -- --nocapture`
Expected: PASS. Rename `failure_caught_downstream_completes` (it tested the old, now-removed behavior) — replace its body with `only_a_failed_step_s_own_flag_excuses_it_not_a_downstream_catcher`'s assertion if it's a duplicate, or delete it if Step 1 already superseded it.

```bash
git add crates/stroem-server/src/settlement/settle.rs
git commit -m "feat(settlement): job status uses the failed row's own continue_on_failure directly"
```

- [ ] **Step 5: Apply the identical fix to `hooks.rs`'s `tolerated` field**

Read `settlement/hooks.rs` around line 485-510 first (the exact surrounding code wasn't re-verified in this plan — confirm the local variable names match before editing). Replace:

```rust
let caught = stroem_common::gate::caught_steps(&task.flow);
// ... later, per failed step `s`:
tolerated: stroem_common::gate::failure_caught(&caught, &s.step_name, s.loop_source.as_deref()),
```

with:

```rust
// per failed step `s`:
tolerated: task
    .flow
    .get(stroem_common::gate::flow_step_name(&s.step_name, s.loop_source.as_deref()))
    .is_some_and(|fs| fs.continue_on_failure),
```

removing the now-unused `caught` variable.

- [ ] **Step 6: Write/update a test pinning `tolerated` as a direct self-flag check**

```rust
#[test]
fn tolerated_reflects_only_the_failed_step_s_own_flag() {
    // a(no flag) -> b(cof). a fails; b never runs (unreachable). a's hook
    // entry must show tolerated: false, even though b would have caught it
    // under the old structural rule.
    let task = task_with_flow(vec![("a", fs_no_flags(&[])), ("b", fs_cof(&["a"]))]);
    let steps = vec![row("a", "failed"), row_skipped("b", "unreachable")];
    let payload = build_hook_payload(&task, &steps); // use whatever this file's existing builder is named
    let a_entry = payload.failed_steps.iter().find(|f| f.step_name == "a").unwrap();
    assert!(!a_entry.tolerated);
}
```

- [ ] **Step 7: Run to verify it passes, then commit**

Run: `cargo test -p stroem-server settlement::hooks:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-server/src/settlement/hooks.rs
git commit -m "feat(hooks): tolerated reflects only the failed step's own flag"
```

- [ ] **Step 8: Apply the identical fix to `restart.rs`'s `carried_failed_tolerated` classification**

Read `restart.rs` around line 88 first. Replace the `caught_steps`/`failure_caught` pair the same way as Steps 3/5 above. Rename the existing test `carried_failure_caught_downstream_is_tolerated` (it tested the old structural behavior) to something reflecting the new rule, e.g. `carried_failure_is_tolerated_only_by_its_own_flag`, and update its assertion to match Step 1's pattern (a step with no flag of its own is `carried_failed`, not `carried_failed_tolerated`, even if something downstream has `continue_on_failure`).

- [ ] **Step 9: Run to verify it passes, then commit**

Run: `cargo test -p stroem-server restart:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-server/src/restart.rs
git commit -m "feat(restart): carried-failure tolerance reflects only the failed step's own flag"
```

---

## Task 6: Loop output — the four-layer fix

**Files:**
- Modify: `crates/stroem-server/src/cascade.rs:40-43` (`RollupOutcome`), `:202-218` (`Snapshot::apply`'s `Rollup` arm), `:345-362` (`phase_rollup`'s R6 output-building), `:798-801` (the `apply()` call site)
- Modify: `crates/stroem-db/src/repos/job_step.rs:464-484` (`fail_placeholder_tx`)
- Modify: `crates/stroem-server/src/render_context.rs:97-121` (`StepView`, its `From<&JobStepRow>`), `:271-293` (the failed-row branch)
- Test: `crates/stroem-server/tests/` wherever `cascade_apply_test.rs` or similar lives, plus inline tests in `render_context.rs` and `job_step.rs`

**Interfaces:**
- Produces: `RollupOutcome::Failed(String, serde_json::Value)` (was `Failed(String)`), `JobStepRepo::fail_placeholder_tx(executor, job_id, name, error, output: &JsonValue) -> Result<u64>`, `StepView.is_placeholder: bool`.

- [ ] **Step 1: Write the failing test for `RollupOutcome::Failed` carrying output**

```rust
// crates/stroem-server/src/cascade.rs, in its test module
#[test]
fn rollup_builds_the_output_array_on_failure_too_not_just_completion() {
    let task = task(vec![("p", fs_cof(&[]))]); // placeholder with its own cof
    let rows = vec![
        placeholder("p", "running", "[2]"),
        instance("p", 0, "completed", Some(json!({"n": 1}))),
        instance("p", 1, "failed", None),
    ];
    let plan_changes = phase_rollup(&Snapshot::new(rows), &task);
    let rollup = plan_changes
        .iter()
        .find_map(|c| match c {
            Change::Rollup { outcome: RollupOutcome::Failed(_, out), .. } => Some(out.clone()),
            _ => None,
        })
        .expect("expected a Failed rollup");
    assert_eq!(rollup, json!([{"n": 1}, null]));
}
```

(`instance(...)` is a hypothetical test helper matching this file's existing fixture style — check the existing `placeholder`/`row` helpers in this test module and match their signature rather than inventing a new shape.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-server cascade::tests::rollup_builds_the_output_array_on_failure -- --nocapture`
Expected: FAIL (compile error — `RollupOutcome::Failed` is still one-field)

- [ ] **Step 3: Change `RollupOutcome` and `phase_rollup`'s R6**

```rust
// cascade.rs:39-43
#[derive(Debug, Clone, PartialEq)]
pub enum RollupOutcome {
    Completed(Value),
    Failed(String, Value),
}
```

```rust
// cascade.rs:335-362 — R6, inside phase_rollup
if instances.iter().all(|i| is_terminal(&i.status)) {
    let any_failed = instances.iter().any(|i| i.status == FAILED);
    let failed_indices: Vec<i32> = instances
        .iter()
        .filter(|i| i.status == FAILED)
        .filter_map(|i| i.loop_index)
        .collect();
    // Build the output array unconditionally now — one element per
    // existing instance, null where it produced none — regardless of
    // whether the rollup ends up Completed or Failed (spec §4).
    let output_array = Value::Array(
        instances
            .iter()
            .map(|i| i.output.clone().unwrap_or(Value::Null))
            .collect(),
    );
    let outcome = if any_failed && !cof {
        RollupOutcome::Failed(
            format!("for_each loop failed: instances {:?} failed", failed_indices),
            output_array,
        )
    } else {
        RollupOutcome::Completed(output_array)
    };
    out.push(Change::Rollup {
        placeholder: ph.step_name.clone(),
        outcome,
    });
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p stroem-server cascade::tests::rollup_builds_the_output_array_on_failure -- --nocapture`
Expected: PASS

- [ ] **Step 5: Write the failing test for `Snapshot::apply` copying output on the `Failed` branch**

```rust
#[test]
fn snapshot_apply_copies_output_on_a_failed_rollup_too() {
    let mut snap = Snapshot::new(vec![placeholder("p", "running", "[1]")]);
    snap.apply(&Change::Rollup {
        placeholder: "p".to_string(),
        outcome: RollupOutcome::Failed("boom".into(), json!([null])),
    });
    assert_eq!(snap.status("p"), Some("failed"));
    assert_eq!(snap.rows[0].output, Some(json!([null])));
}
```

- [ ] **Step 6: Run to verify it fails**

Run: `cargo test -p stroem-server cascade::tests::snapshot_apply_copies_output_on_a_failed_rollup_too -- --nocapture`
Expected: FAIL

- [ ] **Step 7: Fix `Snapshot::apply`'s `Rollup` arm**

```rust
// cascade.rs:202-218
Change::Rollup { placeholder, outcome } => {
    if let Some(r) = self.get_mut(placeholder) {
        match outcome {
            RollupOutcome::Completed(out) => {
                r.status = COMPLETED.to_string();
                r.output = Some(out.clone());
            }
            RollupOutcome::Failed(e, out) => {
                r.status = FAILED.to_string();
                r.error_message = Some(e.clone());
                r.output = Some(out.clone());
            }
        }
    }
}
```

- [ ] **Step 8: Run to verify it passes, then commit**

Run: `cargo test -p stroem-server cascade::tests -- --nocapture`
Expected: PASS (all cascade tests still compiling and passing — fix any other match on `RollupOutcome::Failed(e)` elsewhere in this file to `RollupOutcome::Failed(e, _)` as needed; grep for `RollupOutcome::Failed` to find every site)

```bash
git add crates/stroem-server/src/cascade.rs
git commit -m "feat(cascade): RollupOutcome::Failed carries output, Snapshot::apply assigns it — fixes same-pass visibility"
```

- [ ] **Step 9: Write the failing test for `fail_placeholder_tx` persisting output**

```rust
// crates/stroem-db/src/repos/job_step.rs, in its test module (needs testcontainers Postgres)
#[sqlx::test]
async fn fail_placeholder_tx_persists_the_output_column(pool: PgPool) {
    let job_id = seed_job_with_running_placeholder(&pool, "p").await;
    let mut tx = pool.begin().await.unwrap();
    let output = serde_json::json!([{"n": 1}, null]);
    JobStepRepo::fail_placeholder_tx(&mut *tx, job_id, "p", "boom", &output)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let row = JobStepRepo::get(&pool, job_id, "p").await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.output, Some(output));
}
```

(`seed_job_with_running_placeholder` — check this file's existing integration test helpers for the real seeding function name before inventing one; `JobStepRepo::get` similarly should already exist.)

- [ ] **Step 10: Run to verify it fails**

Run: `cargo test -p stroem-db fail_placeholder_tx_persists_the_output_column -- --nocapture`
Expected: FAIL (compile error — extra argument)

- [ ] **Step 11: Fix `fail_placeholder_tx`**

```rust
// crates/stroem-db/src/repos/job_step.rs:464-484
pub async fn fail_placeholder_tx<'e, E>(
    executor: E,
    job_id: Uuid,
    name: &str,
    error: &str,
    output: &JsonValue,
) -> Result<u64>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let r = sqlx::query(
        "UPDATE job_step SET status = 'failed', error_message = $3, output = $4, completed_at = NOW() \
         WHERE job_id = $1 AND step_name = $2 AND status = 'running'",
    )
    .bind(job_id)
    .bind(name)
    .bind(error)
    .bind(output)
    .execute(executor)
    .await
    .context("fail_placeholder_tx")?;
    Ok(r.rows_affected())
}
```

- [ ] **Step 12: Fix the call site in `cascade.rs:798-801`**

```rust
RollupOutcome::Failed(err, out) => {
    JobStepRepo::fail_placeholder_tx(&mut **tx, job_id, placeholder, err, out).await?
}
```

- [ ] **Step 13: Run to verify both pass, then commit**

Run: `cargo test -p stroem-db fail_placeholder_tx -- --nocapture && cargo test -p stroem-server cascade:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-db/src/repos/job_step.rs crates/stroem-server/src/cascade.rs
git commit -m "feat(job_step): fail_placeholder_tx persists the output column"
```

- [ ] **Step 14: Write the failing tests for `StepView.is_placeholder` and the scoped render fix**

```rust
// crates/stroem-server/src/render_context.rs, in its test module
#[test]
fn step_view_carries_is_placeholder_from_for_each_expr() {
    let mut row = make_job_step_row("p", "failed");
    row.for_each_expr = Some("{{ items }}".to_string());
    let view = StepView::from(&row);
    assert!(view.is_placeholder);

    let mut ordinary = make_job_step_row("a", "failed");
    ordinary.for_each_expr = None;
    assert!(!StepView::from(&ordinary).is_placeholder);
}

#[test]
fn a_failed_loop_placeholder_exposes_its_output() {
    let mut row = make_job_step_row("p", "failed");
    row.for_each_expr = Some("{{ items }}".to_string());
    row.output = Some(json!([{"n": 1}, null]));
    let ctx = render_steps_into_context(&[row]); // use this file's existing context-building test helper
    assert_eq!(ctx["p"]["output"], json!([{"n": 1}, null]));
}

#[test]
fn an_ordinary_failed_step_still_renders_null_even_with_retained_output() {
    // A rejected approval step already stores {"approval_message": ...}
    // before failing — this must stay masked. Pins the scope boundary from
    // spec §4/§12: this is NOT a blanket completed/failed merge.
    let mut row = make_job_step_row("approval", "failed");
    row.for_each_expr = None;
    row.output = Some(json!({"approval_message": "please confirm"}));
    let ctx = render_steps_into_context(&[row]);
    assert_eq!(ctx["approval"]["output"], Value::Null);
}
```

(`make_job_step_row` / `render_steps_into_context` — check this file's existing test helpers; it already has tests building `StepView`/contexts from rows per the earlier `views()`/`build()` functions seen in `condition_context`.)

- [ ] **Step 15: Run to verify it fails**

Run: `cargo test -p stroem-server render_context::tests -- --nocapture`
Expected: FAIL

- [ ] **Step 16: Add `is_placeholder` to `StepView` and scope the render fix**

```rust
// render_context.rs:97-121
pub struct StepView<'a> {
    pub step_name: &'a str,
    pub status: &'a str,
    pub output: Option<&'a Value>,
    pub error_message: Option<&'a str>,
    pub loop_source: Option<&'a str>,
    pub is_placeholder: bool,
}

impl<'a> From<&'a JobStepRow> for StepView<'a> {
    fn from(r: &'a JobStepRow) -> Self {
        StepView {
            step_name: &r.step_name,
            status: &r.status,
            output: r.output.as_ref(),
            error_message: r.error_message.as_deref(),
            loop_source: r.loop_source.as_deref(),
            is_placeholder: r.for_each_expr.is_some(),
        }
    }
}
```

```rust
// render_context.rs:271-293 — the per-step entry-building loop
} else if s.status == failed {
    // Scoped to loop placeholders only (spec §4/§12) — an ordinary failed
    // step's output (e.g. a rejected approval's stored message) must stay
    // masked; only a tolerated loop rollup's output is newly exposed here.
    let output = if s.is_placeholder {
        s.output.cloned().unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    entry.insert("output".into(), output);
    if let Some(err) = s.error_message {
        entry.insert("error".into(), Value::String(err.to_string()));
    }
}
```

- [ ] **Step 17: Run to verify it passes, then commit**

Run: `cargo test -p stroem-server render_context:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-server/src/render_context.rs
git commit -m "feat(render-context): expose a tolerated loop rollup's output, scoped to placeholders only"
```

- [ ] **Step 18: Mirror the whole four-layer fix in the CLI's local runner**

Read `crates/stroem-cli/src/local/run.rs` around lines 117 (`record_failure`) and 333 (its `for_each` rollup builder) in full before editing — this is a second, independent implementation, not a thin wrapper, so it has its own equivalent of every layer above (its own rollup-outcome type or inline equivalent, its own in-memory row update, its own render/output exposure for whatever templating the CLI does). Apply the same four changes: build the output array on failure too, copy it into whatever in-memory structure the CLI tracks, and expose it to CLI-rendered templates only for a loop placeholder, never for an ordinary failed step.

- [ ] **Step 19: Write a CLI-side test mirroring Step 1/5/16's assertions**

```rust
// crates/stroem-cli/src/local/run.rs, in its test module
#[test]
fn cli_for_each_rollup_exposes_output_on_a_tolerated_failure_too() {
    // Mirror of the cascade.rs test above, exercising the CLI's own rollup
    // path end to end (run a local task with a cof-flagged for_each loop
    // where one instance fails, assert the rollup's recorded output array
    // is non-null and that a dependent step's template sees it).
    // Use whatever this file's existing local-run test harness already does
    // for driving a task to completion (search for an existing for_each
    // test in this module and follow its exact setup shape).
}
```

- [ ] **Step 20: Run to verify it passes, then commit**

Run: `cargo test -p stroem-cli local::run:: -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-cli/src/local/run.rs
git commit -m "feat(cli): mirror the four-layer loop-output fix in the local runner"
```

---

## Task 7: Cascade phase merge — P1/P2 with inner relay

**Files:**
- Modify: `crates/stroem-server/src/cascade.rs:257-432` (`gate_for`, `all_deps_skipped`, `phase_promote`, `phase_skip_unreachable`), `:596-649` (`run()`'s phase sequence)
- Modify: `crates/stroem-server/tests/cascade_apply_test.rs` (if the pinned timing tests live there) or the inline test module at the end of `cascade.rs` (confirmed location: `cascade.rs:3080-3160` area)

**Interfaces:**
- Consumes: `stroem_common::gate::{gate, DepOutcome, Gate}` (Task 3).
- Removes: `phase_skip_unreachable`, `all_deps_skipped` (the old all-skipped guard has no equivalent in the uniform barrier). `gate_for`'s signature drops its `task: &TaskDef` parameter (flags are never consulted during gating anymore).
- Produces: `phase_promote` now runs its own internal relay to a fixpoint before returning, and takes an owned context-building closure instead of a single pre-built `Option<&Value>`, since it must rebuild context between its own inner iterations.

This is the task the spec's five rounds of review were hardest on. Read spec §5 in full before starting — it documents exactly why a naive single-pass merge regresses `timing_failed_root_keeps_legacy_pass`'s case, and exactly what the context/batch contract must guarantee.

- [ ] **Step 1: Write the failing tests pinning the two timing flips, renamed**

```rust
// crates/stroem-server/src/cascade.rs test module, replacing the existing
// `timing_failed_root_keeps_legacy_pass` and
// `timing_accepted_change_flagged_placeholder_retires_immediately`
#[test]
fn timing_failed_root_now_resolves_same_pass_as_unreachable_root() {
    // x failed -> a -> b -> p (placeholder). Under the old P1/P2 split, a
    // failed (not skipped) root needed pass 2 to reach b, stranding p's
    // already-committed condition-skip from pass 1. The merged relay
    // decides a, then b, inside pass 1's single merged phase, before p (in
    // P3) ever runs — so p now expands, matching the unreachable-root case
    // exactly. Spec §5.
    let t = timing_task(fs(&["a"]));
    let rows = vec![
        row("x", "failed"),
        row("a", "pending"),
        row("b", "pending"),
        observer(),
    ];
    let n = names(&run_default(&t, &rows));
    assert!(n.contains(&"expand:p:1".to_string()), "{n:?}");
}

#[test]
fn timing_flagged_placeholder_now_waits_for_a_pending_sibling() {
    // l depends on [x (already unreachable), y (still running)]. Today's
    // fail-fast BlockFail dominance retires l immediately, ignoring y. The
    // uniform barrier (spec §2.3) waits for y before deciding anything.
    let t = task(vec![
        ("x", fs(&[])),
        ("y", fs(&[])),
        ("l", fs_cof(&["x", "y"])),
    ]);
    let rows = vec![
        row_skipped("x", "unreachable"),
        row("y", "running"),
        placeholder("l", "pending", "[1]"),
    ];
    let plan = run_default(&t, &rows);
    assert!(skips(&plan).is_empty(), "l must not retire yet — y is still running: {plan:?}");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-server cascade::tests::timing_failed_root_now_resolves cascade::tests::timing_flagged_placeholder_now_waits -- --nocapture`
Expected: FAIL (today's code still produces the old timing)

- [ ] **Step 3: Write the failing test for the context/batch contract**

```rust
#[test]
fn inner_relay_rebuilds_context_for_each_batch_not_once_for_the_whole_pass() {
    // a(when: false); b accepts a's Skipped outcome AND tests in its own
    // `when` whether `a` is defined. If the relay reused one stale context
    // across inner iterations, b (decided in a later inner iteration than
    // a) would incorrectly see `a` as still absent. Spec §5's
    // context/batch contract.
    let t = task(vec![
        ("a", FlowStep { when: Some("false".into()), ..fs(&[]) }),
        (
            "b",
            FlowStep {
                when: Some("{% if a is defined %}true{% else %}false{% endif %}".into()),
                ..fs_with_accept(&[("a", &[Outcome::Completed, Outcome::Skipped])])
            },
        ),
    ]);
    let rows = vec![row("a", "pending"), row("b", "pending")];
    let plan = run_default(&t, &rows);
    assert!(
        names(&plan).contains(&"promote:b".to_string()),
        "b must see a's Skipped outcome and run, not condition-skip on a stale read: {plan:?}"
    );
}
```

(`fs_with_accept` is a new test helper this task needs to add alongside the existing `fs`/`fs_cof` helpers, building a `FlowStep` whose `depends_on` is a list of `{step, accept}` entries from `(name, &[Outcome])` pairs.)

- [ ] **Step 4: Run to verify it fails**

Run: `cargo test -p stroem-server cascade::tests::inner_relay_rebuilds_context -- --nocapture`
Expected: FAIL (compile error — `fs_with_accept` doesn't exist; once added, fails on behavior)

- [ ] **Step 5: Rewrite `gate_for`, delete `all_deps_skipped`**

```rust
// cascade.rs:257-269 — replace both functions
fn gate_for(snap: &Snapshot, fs: &FlowStep) -> Gate {
    gate(&fs.depends_on, |d| snap.outcome(d))
}
```

(`all_deps_skipped` is deleted outright — the uniform barrier has no equivalent special case, per spec §2.3/§5.)

- [ ] **Step 6: Merge `phase_promote` + `phase_skip_unreachable` into one phase with an internal relay**

This phase needs its own context, rebuilt from its own just-applied snapshot, on every inner iteration — it can no longer take a single pre-built `Option<&Value>` the way the old `phase_promote` did, since that context was built once per *outer* pass, not once per *inner* iteration. Change its signature to take a context-building closure instead:

```rust
/// P1 (merged): cascade-skip + promote with `when`, relayed internally
/// until stable. Spec §5: replaces the old P1-then-P2 split, which only
/// ever gave one hop of same-pass relay and gave it asymmetrically
/// (skip-rooted chains got it, failed-rooted chains didn't). Returns every
/// change made across all its inner iterations, and leaves `snap` updated
/// to the final, stable state.
fn phase_promote(
    snap: &mut Snapshot,
    task: &TaskDef,
    mut build_ctx: impl FnMut(&Snapshot) -> Option<Value>,
) -> Vec<Change> {
    let mut all_changes = Vec::new();
    loop {
        let ctx = build_ctx(snap);
        let mut batch = Vec::new();
        for r in snap
            .rows
            .iter()
            .filter(|r| r.status == PENDING && !is_placeholder(r))
        {
            let Some(fs) = task.flow.get(&r.step_name) else {
                continue;
            };
            match gate_for(snap, fs) {
                Gate::Wait => continue,
                Gate::Omitted => {
                    batch.push(Change::Skip {
                        step: r.step_name.clone(),
                        reason: SkipReason::Unreachable,
                    });
                }
                Gate::Open => match (&r.when_condition, ctx.as_ref()) {
                    (None, _) => batch.push(Change::Promote {
                        step: r.step_name.clone(),
                    }),
                    (Some(_), None) => {} // no template context: stays pending (today's behaviour)
                    (Some(w), Some(c)) => {
                        match stroem_common::template::evaluate_condition(w, c) {
                            Ok(true) => batch.push(Change::Promote {
                                step: r.step_name.clone(),
                            }),
                            Ok(false) => batch.push(Change::Skip {
                                step: r.step_name.clone(),
                                reason: SkipReason::Condition,
                            }),
                            Err(e) => batch.push(Change::Fail {
                                step: r.step_name.clone(),
                                error: format!("when condition error: {:#}", e),
                            }),
                        }
                    }
                },
            }
        }
        if batch.is_empty() {
            return all_changes;
        }
        apply_all(snap, &batch);
        all_changes.extend(batch);
        // Loop: the next iteration's build_ctx(snap) call sees everything
        // just applied, and may now be able to decide rows this iteration
        // couldn't. Terminates because `batch.is_empty()` requires strictly
        // fewer pending rows each productive iteration, and the pending set
        // is finite.
    }
}
```

Delete `phase_skip_unreachable` entirely — its job is now inside the loop above (the `Gate::Omitted` arm, applied unconditionally, no `all_deps_skipped` guard).

- [ ] **Step 7: Update `run()`'s phase sequence**

```rust
// cascade.rs:596-649 — inside the outer `loop { ... }`
let p0 = phase_rollup(&snap, task);
apply_all(&mut snap, &p0);
pass.extend(p0);

let build_ctx = |s: &Snapshot| {
    workspace_config.map(|ws| {
        condition_context(job, &s.rows, ws, snapshots).as_value().clone()
    })
};
let p1 = phase_promote(&mut snap, task, build_ctx);
pass.extend(p1);
// (phase_promote already applied its own changes to `snap` internally —
// no separate apply_all(&mut snap, &p1) call here, unlike the other phases.)

let ctx_b = workspace_config.map(|ws| condition_context(job, &snap.rows, ws, snapshots));
let p3 = phase_placeholders(&snap, task, ctx_b.as_ref().map(|c| c.as_value()), job.job_id);
apply_all(&mut snap, &p3);
pass.extend(p3);

if pass.is_empty() {
    break;
}
changes.extend(pass);
```

Remove the deleted `let p2 = phase_skip_unreachable(...)` block entirely.

- [ ] **Step 8: Run to verify all four new/changed tests pass**

Run: `cargo test -p stroem-server cascade::tests::timing_failed_root_now_resolves cascade::tests::timing_flagged_placeholder_now_waits cascade::tests::inner_relay_rebuilds_context -- --nocapture`
Expected: PASS

- [ ] **Step 9: Run the full existing pinned-timing suite to confirm the three unaffected tests are still green**

Run: `cargo test -p stroem-server cascade::tests::timing_ -- --nocapture`
Expected: PASS for `timing_unreachable_chain_keeps_legacy_pass`, `timing_accepted_change_dependent_cof_no_longer_delays`, `timing_accepted_change_unknown_reason_is_failure_class` — no change to their outcomes.

- [ ] **Step 10: Run the entire cascade.rs test module and the whole crate**

Run: `cargo test -p stroem-server cascade:: -- --nocapture && cargo test -p stroem-server`
Expected: PASS. Fix any other test in this file that constructed a `FlowStep` with the old `continue_when_skipped: bool` field or the old flat `depends_on: Vec<&str>`-via-`fs()` shape in a way that no longer compiles — this file has dozens of cascade tests built on the same handful of helpers (`fs`, `fs_cof`, `task`, `row`, `placeholder`), so a signature change to one ripples through many call sites; update the helpers once rather than each call site individually where possible.

- [ ] **Step 11: Commit**

```bash
git add crates/stroem-server/src/cascade.rs
git commit -m "feat(cascade): merge P1/P2 into one phase with an internal relay, fixing the multi-hop timing regression"
```

---

## Task 8: `recalc-pipeline` worked-example integration test

**Files:**
- Create: `crates/stroem-server/tests/dependency_conditions_recalc_pipeline_test.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-7 — this is the first test exercising the full stack together against the spec's own worked example (spec §3).

- [ ] **Step 1: Write the test driving all seven `recalc-pipeline` rules in one flow**

```rust
// crates/stroem-server/tests/dependency_conditions_recalc_pipeline_test.rs
//! Exercises spec §3's recalc-pipeline worked example end to end: all seven
//! target rules, in one flow, through the real cascade (not hand-built
//! Snapshot fixtures) — the first test to combine Tasks 1-7.

use stroem_common::depends_on::{AcceptSet, DependsOnEntry, Outcome, StepEntry};
use stroem_common::models::workflow::{FlowStep, TaskDef};
// ... plus whatever this crate's existing integration-test harness uses to
// build a TaskDef/JobRow/JobStepRow set and drive `cascade::run`.

fn recalc_pipeline_flow() -> TaskDef {
    // Build the flow from spec §3's full diff directly — build-sessions,
    // ai_maintain, ai_sources, ml-prediction-master/beta/stage,
    // ml-impressions-master/beta/stage, dwell-time, merge-ml — with exactly
    // the depends_on/accept/continue_on_failure spec §3 specifies for each.
    // This is the single source of truth for the test below; write it out
    // in full rather than a subset, since partial coverage would defeat the
    // point of an integration test pinning the whole example.
}

#[test]
fn rule_1_build_sessions_fails_blocks_everything_below_it() {
    let task = recalc_pipeline_flow();
    let rows = seed_all_pending(&task, &[("build-sessions", "failed")]);
    let plan = cascade::run(&task, &job(), &rows, Some(&workspace_config()), &snapshots()).unwrap();
    for downstream in ["ml-prediction-master", "ml-prediction-beta", "ml-prediction-stage", "merge-ml", "agg-sessions"] {
        assert!(
            skip_reason_of(&plan, downstream) == Some("unreachable"),
            "{downstream} should be unreachable"
        );
    }
}

#[test]
fn rules_2_and_3_ai_maintain_fails_or_skips_ai_sources_still_runs() {
    let task = recalc_pipeline_flow();
    for ai_maintain_status in [("failed", None), ("skipped", Some("condition"))] {
        let rows = seed_with_build_sessions_ok(&task, ai_maintain_status);
        let plan = cascade::run(&task, &job(), &rows, Some(&workspace_config()), &snapshots()).unwrap();
        assert!(promoted(&plan, "ai_sources"), "ai_sources must run for {ai_maintain_status:?}");
    }
}

#[test]
fn rule_4_ml_prediction_master_fails_fails_the_job() {
    let task = recalc_pipeline_flow();
    let steps = run_to_terminal(&task, &[("ml-prediction-master", "failed")]);
    assert_eq!(settlement::settle::decide(&task, &steps).unwrap().status, JobStatus::Failed);
}

#[test]
fn rules_5_and_6_beta_stage_prediction_fails_survives_job_but_impressions_skip() {
    let task = recalc_pipeline_flow();
    for variant in ["beta", "stage"] {
        let steps = run_to_terminal(&task, &[(&format!("ml-prediction-{variant}"), "failed")]);
        assert!(
            skip_reason_of_terminal(&steps, &format!("ml-impressions-{variant}")) == Some("unreachable")
        );
        assert_ne!(settlement::settle::decide(&task, &steps).unwrap().status, JobStatus::Failed);
    }
}

#[test]
fn rule_7_merge_ml_requires_master_tolerates_beta_and_stage() {
    let task = recalc_pipeline_flow();
    let steps = run_to_terminal(
        &task,
        &[
            ("ml-impressions-master", "completed"),
            ("ml-impressions-beta", "failed"),
            ("ml-impressions-stage", "failed"),
        ],
    );
    assert!(promoted_or_completed(&steps, "merge-ml"));
}
```

(The helper functions `seed_all_pending`, `seed_with_build_sessions_ok`, `run_to_terminal`, `skip_reason_of`, `skip_reason_of_terminal`, `promoted`, `promoted_or_completed`, `job`, `workspace_config`, `snapshots` are new — build them against whatever this crate's *other* integration tests under `tests/` already use for seeding a `TaskDef`/driving `cascade::run`/reading back results; do not invent a parallel harness if one already exists under `crates/stroem-server/tests/`.)

- [ ] **Step 2: Run to verify it fails initially (sanity check the fixture is wired correctly)**

Run: `cargo test -p stroem-server --test dependency_conditions_recalc_pipeline_test -- --nocapture`
Expected: likely PASS already if Tasks 1-7 are correctly implemented — if any of the 5 tests fails, that's a real integration gap between the unit-level work in Tasks 1-7 and the full flow; do not treat this as "the test is wrong," treat it as a signal to go back and check the relevant task.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/dependency_conditions_recalc_pipeline_test.rs
git commit -m "test(cascade): integration test driving all seven recalc-pipeline rules from spec §3"
```

---

## Task 9: API/UI wire-through

**Files:**
- Modify: `crates/stroem-server/src/web/api/tasks.rs` (verify/adjust dependency serialization for the task-detail endpoint)
- Modify: `crates/stroem-server/src/web/api/jobs.rs` (same, for job-detail's step dependency serialization)
- Modify: `ui/src/lib/types.ts` (TypeScript type for a flow step's `depends_on`)

**Interfaces:**
- Consumes: `DependsOnEntry`'s `Serialize` impl (Task 1/2 — already derived, this task is about confirming/adjusting the *shape* consumers see, not adding new serialization logic).

- [ ] **Step 1: Write the failing test for the task-detail API's dependency shape**

```rust
// crates/stroem-server/src/web/api/tasks.rs, in its test module
#[test]
fn task_detail_serializes_grouped_dependencies_as_authored() {
    let task = task_with_flow(vec![(
        "m",
        fs_with_entries(vec![DependsOnEntry::Any(AnyEntry {
            any: vec![DependsOnEntry::Name("a".into()), DependsOnEntry::Name("b".into())],
        })]),
    )]);
    let body = render_task_detail_json(&task); // this endpoint's existing response-building function
    assert_eq!(
        body["flow"]["m"]["depends_on"],
        json!([{"any": ["a", "b"]}])
    );
}
```

- [ ] **Step 2: Run to verify it fails or passes as-is**

Run: `cargo test -p stroem-server web::api::tasks::tests::task_detail_serializes_grouped -- --nocapture`
Expected: Likely PASS already, since `DependsOnEntry` derives `Serialize` and `FlowStep` derives `Serialize` too (confirmed in Task 2, the struct keeps `#[derive(... Serialize)]`) — if it fails, the response builder is doing its own manual re-shaping of `depends_on` somewhere that needs updating to stop assuming `Vec<String>`.

- [ ] **Step 3: If it failed, fix the response-building code; if it passed, skip to Step 4**

(No speculative code here — whether a fix is needed depends on Step 2's actual result, which can't be predicted without running it.)

- [ ] **Step 4: Repeat Steps 1-3 for `jobs.rs`'s job-detail step dependency serialization**

Same pattern: write a test asserting the job-detail endpoint's step-dependency field round-trips a grouped entry correctly, run it, fix only if it fails.

- [ ] **Step 5: Update the UI TypeScript type**

```typescript
// ui/src/lib/types.ts — replace the current `depends_on: string[]` shape
export type Outcome = "completed" | "failed" | "cancelled" | "skipped" | "omitted";

export type DependsOnEntry =
  | string
  | { step: string; accept?: Outcome[] | "terminal" }
  | { all: DependsOnEntry[] }
  | { any: DependsOnEntry[] };

// Update the FlowStep (or equivalent) interface's `depends_on` field type
// from `string[]` to `DependsOnEntry[]`. Grep this file and any consumer
// (DAG visualization components, task-detail views) for `.depends_on` usage
// that assumes a flat string array — e.g. `.map(name => ...)` or
// `.includes(name)` — and widen those call sites to handle the richer
// shape (at minimum, extracting bare names via a small recursive helper
// mirroring `collect_names` for anything that only needs names, such as a
// DAG edge list).
```

- [ ] **Step 6: Run the UI typecheck**

Run: `cd ui && bun run build`
Expected: either PASS, or a list of now-type-erroring call sites — fix each one by widening it to the new type rather than casting past the error.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server/src/web/api/tasks.rs crates/stroem-server/src/web/api/jobs.rs ui/src/lib/types.ts
git commit -m "feat(api,ui): surface grouped depends_on entries through task/job detail and UI types"
```

---

## Task 10: Migration-behaviour tests

**Files:**
- Create: `crates/stroem-server/tests/dependency_conditions_migration_test.rs`

**Interfaces:**
- Consumes: Tasks 3 (gate), 5 (settlement), 7 (cascade) — this task is pure test, no production code changes.

- [ ] **Step 1: Write the structural-catch migration test (fresh job)**

```rust
// crates/stroem-server/tests/dependency_conditions_migration_test.rs
//! Pins the spec §9 "A -> B -> C(flagged) -> D" migration scenario: 0.17.0
//! structurally caught A's failure at C (every path from A reaches a
//! flag), completing the job even though neither A nor B had their own
//! flag. Under this design, only A's own continue_on_failure would excuse
//! it — it has none, so the job must now end Failed. D still runs if its
//! edge to C is marked optional-equivalent (accept including Failed/Omitted).

#[test]
fn structural_catch_chain_now_fails_the_job_fresh() {
    let task = task_with_flow(vec![
        ("a", fs_no_flags(&[])),
        ("b", fs_no_flags(&["a"])),
        (
            "c",
            FlowStep { continue_on_failure: true, ..fs_no_flags(&["b"]) },
        ),
        ("d", fs_with_accept(&[("c", &[Outcome::Completed, Outcome::Failed, Outcome::Omitted])])),
    ]);
    let rows = seed_all_pending(&task, &[("a", "failed")]);
    let plan = cascade::run(&task, &job(), &rows, Some(&workspace_config()), &snapshots()).unwrap();
    let steps = apply_plan_to_rows(&rows, &plan);
    assert!(promoted_or_completed(&steps, "d"), "d must still run — c's own continue_on_failure excuses c");
    assert_eq!(
        settlement::settle::decide(&task, &steps).unwrap().status,
        JobStatus::Failed,
        "a has no flag of its own — the job must fail, matching spec §9's documented, accepted change"
    );
}
```

- [ ] **Step 2: Run to verify it passes (this pins already-implemented behavior from Tasks 3/5/7)**

Run: `cargo test -p stroem-server --test dependency_conditions_migration_test structural_catch_chain_now_fails_the_job_fresh -- --nocapture`
Expected: PASS

- [ ] **Step 3: Write the same scenario seeded as an already-running (pre-upgrade) job**

```rust
#[test]
fn structural_catch_chain_mid_flight_also_now_fails_the_job() {
    // Same flow, but seeded with b, c already terminal (as if this job
    // started running under 0.17.0 before the upgrade) and only d still
    // pending at the moment the new code takes over. Pins that in-flight
    // jobs really do see the new rule on their next cascade (spec §11),
    // not just freshly created ones.
    let task = task_with_flow(vec![
        ("a", fs_no_flags(&[])),
        ("b", fs_no_flags(&["a"])),
        ("c", FlowStep { continue_on_failure: true, ..fs_no_flags(&["b"]) }),
        ("d", fs_with_accept(&[("c", &[Outcome::Completed, Outcome::Failed, Outcome::Omitted])])),
    ]);
    let rows = vec![
        row("a", "failed"),
        row_skipped("b", "unreachable"), // already decided under 0.17.0 before upgrade
        row_skipped("c", "unreachable"),
        row("d", "pending"),
    ];
    let plan = cascade::run(&task, &job(), &rows, Some(&workspace_config()), &snapshots()).unwrap();
    let steps = apply_plan_to_rows(&rows, &plan);
    assert!(promoted_or_completed(&steps, "d"));
    assert_eq!(settlement::settle::decide(&task, &steps).unwrap().status, JobStatus::Failed);
}
```

- [ ] **Step 4: Run to verify it passes, then commit**

Run: `cargo test -p stroem-server --test dependency_conditions_migration_test -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-server/tests/dependency_conditions_migration_test.rs
git commit -m "test(migration): pin the structural-catch-chain behavior change, fresh and mid-flight"
```

- [ ] **Step 5: Write the in-flight loop-timing test (spec §11)**

```rust
#[test]
fn a_loop_already_rolled_up_before_upgrade_keeps_its_historical_status_hiding_a_failure() {
    // Simulates a loop that finished (rolled up to completed, hiding a
    // tolerated failure) before the upgrade took effect — P0 only visits
    // RUNNING placeholders, so an already-terminal one must never be
    // revisited or re-rolled-up.
    let task = task(vec![("p", fs_cof(&[]))]);
    let rows = vec![
        placeholder("p", "completed", "[1]"), // already rolled up, pre-upgrade style
        instance("p", 0, "failed", None),
    ];
    let plan = phase_rollup(&Snapshot::new(rows), &task);
    assert!(plan.is_empty(), "an already-completed placeholder must not be re-rolled-up");
}

#[test]
fn a_loop_still_running_at_upgrade_time_rolls_up_under_the_new_rule() {
    let task = task(vec![("p", fs_cof(&[]))]);
    let rows = vec![
        placeholder("p", "running", "[1]"),
        instance("p", 0, "failed", None),
    ];
    let plan = phase_rollup(&Snapshot::new(rows), &task);
    let rolled_up_as_failed = plan.iter().any(|c| {
        matches!(c, Change::Rollup { outcome: RollupOutcome::Failed(..), .. })
    });
    assert!(rolled_up_as_failed, "a mid-flight loop must roll up under the new (truthful-status) rule");
}
```

- [ ] **Step 6: Run to verify it passes, then commit**

Run: `cargo test -p stroem-server --test dependency_conditions_migration_test -- --nocapture`
Expected: PASS

```bash
git add crates/stroem-server/tests/dependency_conditions_migration_test.rs
git commit -m "test(migration): pin the in-flight loop-rollup timing cutover"
```

---

## Task 11: e2e fan-in fixture for `any`/`all` grouping

**Files:**
- Create: `tests/e2e-workspace/.workflows/dependency-conditions-fanin.yaml` (or add to an existing conditionals e2e fixture file — check `tests/e2e-workspace/` for the convention first)
- Modify: `tests/e2e.sh` (register the new scenario)

**Interfaces:**
- Consumes: the shipped `depends_on` YAML syntax end to end (no new code — this exercises the whole stack through the real binaries).

- [ ] **Step 1: Write the fixture workflow**

```yaml
# tests/e2e-workspace/.workflows/dependency-conditions-fanin.yaml
tasks:
  fanin-demo:
    flow:
      audit:
        action: noop
      mirror-a:
        action: fail-always
      mirror-b:
        action: noop
      ranked:
        action: noop
        depends_on:
          - step: audit
            accept: terminal
          - any: [mirror-a, mirror-b]
```

(Use whichever no-op/always-fail action names this e2e workspace's existing fixtures already define — check `tests/e2e-workspace/` for the convention, e.g. an existing `noop`/`fail-always` action, rather than inventing new action names that don't exist in the shared actions file.)

- [ ] **Step 2: Add the scenario to `tests/e2e.sh`**

Follow the file's existing pattern for triggering a task and asserting its final status — add a case that runs `fanin-demo`, asserts `ranked` runs (since `mirror-b` completes even though `mirror-a` fails and `audit` is irrelevant to the outcome either way), and the job completes.

- [ ] **Step 3: Run the e2e suite**

Run: `./tests/e2e.sh`
Expected: PASS (needs Docker; this is the first fully-end-to-end exercise of the new YAML syntax through the real server + worker)

- [ ] **Step 4: Commit**

```bash
git add tests/e2e-workspace/.workflows/dependency-conditions-fanin.yaml tests/e2e.sh
git commit -m "test(e2e): fan-in fixture exercising any/all grouping end to end"
```

---

## Task 12: Documentation

**Files:**
- Modify: `CLAUDE.md` (§ Conditional Flow Steps)
- Modify: `CONTEXT.md` (glossary: Dependency gate, Caught failure)
- Modify: `docs/src/content/docs/guides/conditionals.md`
- Modify: `docs/src/content/docs/reference/workflow-yaml.md`
- Create: `docs/src/content/docs/operations/upgrade-0-18-dependency-conditions.md`
- Modify: `docs/internal/TODO.md` (the `continue_on_failure`/depends_on parsing pattern found in Task 2 Step 7 for any fields outside this plan's scope, if any were found; the cancelled-only-loop quirk from spec §4/§12 if not already tracked there)

- [ ] **Step 1: Rewrite CLAUDE.md § Conditional Flow Steps**

Replace the current description of `continue_on_failure`/`continue_when_skipped`'s gating role with: `continue_on_failure` is self-scoped only (job status, no propagation); dependency gating is `depends_on`'s per-edge `{step, accept}` / `all` / `any` tree, evaluated once every referenced step is terminal; `continue_when_skipped` is retired. Link to the spec file rather than re-deriving the whole design in CLAUDE.md.

- [ ] **Step 2: Update CONTEXT.md's glossary**

Update the `Dependency gate` and `Caught failure` entries — "caught" no longer means a structural graph-walk; it's a direct per-step flag check. Reference the new outcome vocabulary (`completed`/`failed`/`cancelled`/`skipped`/`omitted`).

- [ ] **Step 3: Rewrite `docs/src/content/docs/guides/conditionals.md`**

Replace the `continue_on_failure`/`continue_when_skipped` flag reference section with the `accept`/`all`/`any` syntax, using spec §2.4's `publish`/`ranked` example as the worked illustration.

- [ ] **Step 4: Update `docs/src/content/docs/reference/workflow-yaml.md`**

`depends_on`'s schema entry gains the full `{step, accept}` / `{all}` / `{any}` shape documentation, replacing the plain string-list description.

- [ ] **Step 5: Write the 0.18 upgrade guide**

Cover, concretely, with examples:
- The exact migration table from spec §9 (the 4-row table, including the two approximate rows and why).
- The corrected worked example from spec §9 (`A(when=false) → B(cws=true) → C`).
- The loop-rollup behavior change (spec §4) and its checklist item.
- The structural-catch audit methodology (walk flow definitions with the old `caught_steps()` logic, not job history) from spec §9.
- The duplicate-`depends_on`-entry deduplication checklist item.
- A link back to the 0.17 upgrade guide, since a workspace migrating straight from 0.16.x needs both.

- [ ] **Step 6: Check `docs/internal/TODO.md` for anything this plan's tasks surfaced but didn't fix**

In particular: anything Task 2 Step 7 found outside the enumerated consumer list; the cancelled-only-loop job-status quirk (spec §4/§12 — confirm whether it's already tracked from the original dependency-gate work, add an entry if not); the broader ordinary-step output-exposure non-goal (spec §12).

- [ ] **Step 7: Commit**

```bash
git add CLAUDE.md CONTEXT.md docs/src/content/docs/guides/conditionals.md docs/src/content/docs/reference/workflow-yaml.md docs/src/content/docs/operations/upgrade-0-18-dependency-conditions.md docs/internal/TODO.md
git commit -m "docs: dependency conditions — CLAUDE.md, CONTEXT.md glossary, guides, and the 0.18 upgrade guide"
```

---

## Final Verification

- [ ] Run the full workspace test suite: `cargo fmt --check --all && cargo clippy --workspace -- -D warnings && cargo test --workspace`
- [ ] Run the UI build/lint: `cd ui && bun run lint && bun run build`
- [ ] Run the e2e suite: `./tests/e2e.sh`
- [ ] Re-read `docs/superpowers/specs/2026-10-01-dependency-conditions-design.md` once more in full and confirm every numbered section (§1-§12, Appendix) has a corresponding task above — if any gap is found, add a task rather than shipping with a silent gap.
