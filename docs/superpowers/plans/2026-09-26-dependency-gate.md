# Dependency Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make only a dependency's own `continue_on_failure` / `continue_when_skipped` decide whether its dependents run (strict AND), and fail a job only for a failure that no `continue_on_failure` catches downstream — identically on the server and in `stroem run`.

**Architecture:** One pure module `stroem_common::gate` (per-dependency `verdict`, combined `gate`, structural `caught_steps`) is the single owner of the rule. The server cascade (`cascade.rs` P1/P2/P3), settlement (`settle.rs::decide`), hooks, restart preview and the CLI local runner all call it. No schema change.

**Tech Stack:** Rust (tokio, sqlx runtime queries, testcontainers), React 19 + Vitest (bun), Astro Starlight docs, bash e2e.

**Spec:** `docs/superpowers/specs/2026-09-26-dependency-gate-design.md` (revision 4, Codex sign-off). Read §2 (rules), §5 (phase split), §9 (edge cases) before any task.

## Global Constraints

- Commits: conventional style (`feat(cascade): …`), **no AI co-author / `Co-Authored-By` trailer** (user's global rule).
- Field names stay `continue_on_failure` / `continue_when_skipped` (spec §2.6). No aliases, no renames.
- Phase order P0 → context A → P1 → P2 → context B → P3 is unchanged (spec §5). P1 emits `Skip(Unreachable)` **only when every dependency is skipped**; P2 emits the remaining `Skip(Unreachable)`.
- Job status: only `failed` rows can fail a job; skipped rows never decide job status (spec §2.4).
- sqlx runtime queries only; `anyhow::Result` + `.context(..)`; unit tests in-module; no new crates.
- Before each commit: `cargo fmt --all` and the task's tests green. Before the final task: `cargo fmt --check --all`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, `cd ui && bun run lint && bunx tsc --noEmit && bun run test`.
- Host disk runs near 100%: if cargo dies with "No space left", run `cargo clean -p stroem-server` and retry; never prune Docker volumes without asking.

## Review Focus

1. **Flow reloaded mid-job (flag removed/added, step removed)** — `caught_steps` reads the CURRENT flow; a failed row whose step vanished is uncaught → job `failed`. Pinned in Task 3 (`decide` test `failed_row_missing_from_flow_fails`).
2. **Rows written before migration 046 (NULL `skip_reason`) in jobs still running at upgrade** — the gate treats them as failure-class (dependents skip `unreachable`), settlement ignores them. Pinned in Task 1 (`verdict` table) and Task 3 (`skipped_leaf_with_null_or_unknown_reason_completes`).
3. **Deep flows (long chains)** — `caught_steps` is recursive; a 2 000-step chain must not overflow the stack. Pinned in Task 1 (`caught_steps_handles_a_long_chain`).
4. **Hook consumers reading `failed_steps[].continue_on_failure` for loop instances** — now the placeholder's flag (was always `false` for `p[i]`). Pinned in Task 3 (hooks test).
5. **`stroem run` on a flow where a failure is caught downstream** — exits 0 and the dependents of the catching step run. Pinned in Task 6 (subprocess test `caught_failure_exits_zero`).

---

### Task 1: `stroem_common::gate` and `SkipReason` in stroem-common

**Files:**
- Modify: `crates/stroem-common/src/models/job.rs` (add `SkipReason` after `StepStatus`, ~line 140)
- Create: `crates/stroem-common/src/gate.rs`
- Modify: `crates/stroem-common/src/lib.rs` (add `pub mod gate;`)
- Modify: `crates/stroem-server/src/cascade.rs:36-60` (delete the enum, re-export)

**Interfaces:**
- Produces:
  - `stroem_common::models::job::SkipReason { Condition, Empty, Cascade, Unreachable }` with `fn as_str(self) -> &'static str` and `impl FromStr` (`Err = anyhow::Error`).
  - `stroem_common::gate::DepOutcome { Pending, Completed, Failed, Cancelled, Skipped(Option<SkipReason>) }` (`Copy, Eq`), `DepOutcome::from_row(status: &str, skip_reason: Option<&str>) -> DepOutcome`.
  - `stroem_common::gate::Verdict { Pass, BlockSkip, BlockFail, Pending }`, `fn verdict(outcome: DepOutcome, dep: Option<&FlowStep>) -> Verdict`.
  - `stroem_common::gate::Gate { Open, Wait, Skip(SkipReason) }`, `fn gate(depends_on: &[String], flow: &HashMap<String, FlowStep>, outcome_of: impl Fn(&str) -> DepOutcome) -> Gate`.
  - `fn caught_steps(flow: &HashMap<String, FlowStep>) -> HashSet<String>`.
  - `fn flow_step_name<'a>(step_name: &'a str, loop_source: Option<&'a str>) -> &'a str`.
  - `fn failure_caught(caught: &HashSet<String>, step_name: &str, loop_source: Option<&str>) -> bool`.
  - `stroem_server::cascade::SkipReason` keeps working (re-export).

- [ ] **Step 1: Move `SkipReason` into `models/job.rs`**

Add after the `StepStatus` `FromStr` impl:

```rust
/// Why a step was skipped (spec 2026-09-09 §2.2). Persisted verbatim as
/// `job_step.skip_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// The step's own `when` rendered falsy.
    Condition,
    /// The step's `for_each` produced zero items.
    Empty,
    /// A dependency was skipped by choice and does not carry
    /// `continue_when_skipped` (spec 2026-09-26 §2.3).
    Cascade,
    /// A dependency failed, was cancelled, or was itself skipped
    /// `unreachable`, and does not carry `continue_on_failure`.
    Unreachable,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Condition => "condition",
            SkipReason::Empty => "empty",
            SkipReason::Cascade => "cascade",
            SkipReason::Unreachable => "unreachable",
        }
    }
}

impl std::str::FromStr for SkipReason {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "condition" => Ok(Self::Condition),
            "empty" => Ok(Self::Empty),
            "cascade" => Ok(Self::Cascade),
            "unreachable" => Ok(Self::Unreachable),
            other => anyhow::bail!("unknown skip reason '{other}'"),
        }
    }
}
```

In `crates/stroem-server/src/cascade.rs` delete lines 36-60 (the enum + its `impl`) and add below the `use` block:

```rust
pub use stroem_common::models::job::SkipReason;
```

Run: `cargo build -p stroem-server` — Expected: builds (all existing `SkipReason::…` / `.as_str()` uses resolve through the re-export).

- [ ] **Step 2: Write the failing gate tests**

Create `crates/stroem-common/src/gate.rs` containing only the test module first (so it fails to compile = red):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::job::SkipReason;
    use crate::models::workflow::FlowStep;
    use std::collections::HashMap;

    fn fs(deps: &[&str], cof: bool, cws: bool) -> FlowStep {
        FlowStep {
            action: "noop".into(),
            name: None,
            description: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            input: HashMap::new(),
            continue_on_failure: cof,
            continue_when_skipped: cws,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
    }
    fn flow(steps: Vec<(&str, FlowStep)>) -> HashMap<String, FlowStep> {
        steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }
    fn deps(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn from_row_maps_statuses_and_reasons() {
        use DepOutcome::*;
        for live in ["pending", "ready", "claimed", "running", "suspended", "bogus"] {
            assert_eq!(DepOutcome::from_row(live, None), Pending, "{live}");
        }
        assert_eq!(DepOutcome::from_row("completed", None), Completed);
        assert_eq!(DepOutcome::from_row("failed", None), Failed);
        assert_eq!(DepOutcome::from_row("cancelled", None), Cancelled);
        assert_eq!(
            DepOutcome::from_row("skipped", Some("condition")),
            Skipped(Some(SkipReason::Condition))
        );
        assert_eq!(DepOutcome::from_row("skipped", None), Skipped(None));
        assert_eq!(DepOutcome::from_row("skipped", Some("new-reason")), Skipped(None));
    }

    #[test]
    fn verdict_table() {
        use DepOutcome::*;
        use Verdict::*;
        let none = fs(&[], false, false);
        let cof = fs(&[], true, false);
        let cws = fs(&[], false, true);
        let choice = [
            Skipped(Some(SkipReason::Condition)),
            Skipped(Some(SkipReason::Empty)),
            Skipped(Some(SkipReason::Cascade)),
        ];
        let failure = [
            Failed,
            Cancelled,
            Skipped(Some(SkipReason::Unreachable)),
            Skipped(None),
        ];
        assert_eq!(verdict(Pending, Some(&cof)), Verdict::Pending);
        assert_eq!(verdict(Completed, Some(&none)), Pass);
        for o in choice {
            assert_eq!(verdict(o, Some(&none)), BlockSkip, "{o:?}");
            assert_eq!(verdict(o, Some(&cws)), Pass, "{o:?}");
            assert_eq!(verdict(o, Some(&cof)), BlockSkip, "cof does not pass a skip: {o:?}");
        }
        for o in failure {
            assert_eq!(verdict(o, Some(&none)), BlockFail, "{o:?}");
            assert_eq!(verdict(o, Some(&cof)), Pass, "{o:?}");
            assert_eq!(verdict(o, Some(&cws)), BlockFail, "cws does not pass a failure: {o:?}");
        }
        // A dependency missing from the flow has no flags.
        assert_eq!(verdict(Failed, None), BlockFail);
        assert_eq!(verdict(Skipped(Some(SkipReason::Condition)), None), BlockSkip);
    }

    #[test]
    fn gate_combines_with_failure_first_then_wait_then_skip() {
        use DepOutcome::*;
        let f = flow(vec![
            ("x", fs(&[], false, false)),
            ("y", fs(&[], false, false)),
            ("s", fs(&["x", "y"], false, false)),
        ]);
        let run = |x: DepOutcome, y: DepOutcome| {
            gate(&deps(&["x", "y"]), &f, |d| if d == "x" { x } else { y })
        };
        // failure dominates a pending sibling
        assert_eq!(run(Failed, Pending), Gate::Skip(SkipReason::Unreachable));
        // a choice skip waits for a pending sibling …
        assert_eq!(run(Skipped(Some(SkipReason::Condition)), Pending), Gate::Wait);
        // … and becomes unreachable when that sibling fails
        assert_eq!(
            run(Skipped(Some(SkipReason::Condition)), Failed),
            Gate::Skip(SkipReason::Unreachable)
        );
        // no automatic convergence
        assert_eq!(
            run(Completed, Skipped(Some(SkipReason::Condition))),
            Gate::Skip(SkipReason::Cascade)
        );
        assert_eq!(run(Completed, Completed), Gate::Open);
        assert_eq!(gate(&[], &f, |_| Pending), Gate::Open, "no deps → open");
    }

    #[test]
    fn gate_reads_the_dependency_flags_only() {
        use DepOutcome::*;
        let f = flow(vec![
            ("x", fs(&[], true, false)), // x catches failures
            ("y", fs(&[], false, true)), // y lets skips through
            ("s", fs(&["x", "y"], false, false)),
        ]);
        let g = gate(&deps(&["x", "y"]), &f, |d| {
            if d == "x" { Failed } else { Skipped(Some(SkipReason::Condition)) }
        });
        assert_eq!(g, Gate::Open);
        // a missing row is pending
        assert_eq!(gate(&deps(&["ghost"]), &f, |_| Pending), Gate::Wait);
    }

    #[test]
    fn caught_steps_is_structural() {
        // a → b(cof) → c ; a → d ; e(cof) ; f → g → h(cof)
        let f = flow(vec![
            ("a", fs(&[], false, false)),
            ("b", fs(&["a"], true, false)),
            ("c", fs(&["b"], false, false)),
            ("d", fs(&["a"], false, false)),
            ("e", fs(&[], true, false)),
            ("f", fs(&[], false, false)),
            ("g", fs(&["f"], false, false)),
            ("h", fs(&["g"], true, false)),
        ]);
        let c = caught_steps(&f);
        assert!(!c.contains("a"), "escapes through d");
        assert!(c.contains("b") && c.contains("e") && c.contains("h"));
        assert!(!c.contains("c") && !c.contains("d"), "leaves without the flag");
        assert!(c.contains("f") && c.contains("g"), "caught mid-way at h");
        assert!(!c.contains("ghost"));
    }

    #[test]
    fn caught_steps_diamond_needs_every_path() {
        // a → l(cof), a → r ; l, r → j
        let f = flow(vec![
            ("a", fs(&[], false, false)),
            ("l", fs(&["a"], true, false)),
            ("r", fs(&["a"], false, false)),
            ("j", fs(&["l", "r"], false, false)),
        ]);
        let c = caught_steps(&f);
        assert!(!c.contains("a"), "path a→r→j has no flag");
        assert!(c.contains("l"));
    }

    #[test]
    fn caught_steps_handles_a_long_chain() {
        let mut steps: Vec<(String, FlowStep)> = vec![("s0".into(), fs(&[], false, false))];
        for i in 1..2000 {
            let prev = format!("s{}", i - 1);
            steps.push((format!("s{i}"), fs(&[prev.as_str()], i == 1999, false)));
        }
        let f: HashMap<String, FlowStep> = steps.into_iter().collect();
        assert!(caught_steps(&f).contains("s0"));
    }

    #[test]
    fn flow_step_name_and_failure_caught_normalize_instances() {
        assert_eq!(flow_step_name("p[3]", Some("p")), "p");
        assert_eq!(flow_step_name("p[3]", None), "p");
        assert_eq!(flow_step_name("plain", None), "plain");
        let caught: HashSet<String> = ["p".to_string()].into();
        assert!(failure_caught(&caught, "p[0]", Some("p")));
        assert!(!failure_caught(&caught, "q", None));
    }
}
```

Add `pub mod gate;` to `crates/stroem-common/src/lib.rs` (alphabetical, after `format`).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p stroem-common gate::`
Expected: compile errors (`verdict`, `gate`, `DepOutcome` … not found).

- [ ] **Step 4: Implement the module (above the test module)**

```rust
//! Dependency gate: whether a flow step may run, decided only from each
//! dependency's final state and that dependency's OWN flags, and whether a
//! failure is caught by `continue_on_failure` downstream.
//! Spec: docs/superpowers/specs/2026-09-26-dependency-gate-design.md.

use std::collections::{HashMap, HashSet};

use crate::models::job::{SkipReason, StepStatus};
use crate::models::workflow::FlowStep;

/// A dependency's state as the gate sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepOutcome {
    Pending,
    Completed,
    Failed,
    Cancelled,
    /// `None` = NULL (pre-046 rows) or an unrecognised reason.
    Skipped(Option<SkipReason>),
}

impl DepOutcome {
    /// Non-terminal and unknown statuses are `Pending`; an unknown skip
    /// reason reads as `Skipped(None)`, which blocks as a failure.
    pub fn from_row(status: &str, skip_reason: Option<&str>) -> Self {
        match status.parse::<StepStatus>() {
            Ok(StepStatus::Completed) => Self::Completed,
            Ok(StepStatus::Failed) => Self::Failed,
            Ok(StepStatus::Cancelled) => Self::Cancelled,
            Ok(StepStatus::Skipped) => {
                Self::Skipped(skip_reason.and_then(|r| r.parse::<SkipReason>().ok()))
            }
            _ => Self::Pending,
        }
    }
}

/// What one dependency means for its dependent (spec §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    BlockSkip,
    BlockFail,
    Pending,
}

/// `dep` is the DEPENDENCY's flow definition; `None` (missing from the flow)
/// reads as no flags.
pub fn verdict(outcome: DepOutcome, dep: Option<&FlowStep>) -> Verdict {
    let cof = dep.is_some_and(|d| d.continue_on_failure);
    let cws = dep.is_some_and(|d| d.continue_when_skipped);
    match outcome {
        DepOutcome::Pending => Verdict::Pending,
        DepOutcome::Completed => Verdict::Pass,
        DepOutcome::Skipped(Some(
            SkipReason::Condition | SkipReason::Empty | SkipReason::Cascade,
        )) => {
            if cws {
                Verdict::Pass
            } else {
                Verdict::BlockSkip
            }
        }
        DepOutcome::Failed
        | DepOutcome::Cancelled
        | DepOutcome::Skipped(Some(SkipReason::Unreachable))
        | DepOutcome::Skipped(None) => {
            if cof {
                Verdict::Pass
            } else {
                Verdict::BlockFail
            }
        }
    }
}

/// The combined decision for one step (spec §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Every dependency passes: evaluate the step's own `when`.
    Open,
    /// Some dependency is still running and nothing has blocked as a failure.
    Wait,
    /// Skip with this reason (`Cascade` or `Unreachable` only).
    Skip(SkipReason),
}

/// Strict AND. Takes only the dependent's `depends_on` — never its own
/// `FlowStep` — so the dependent's flags cannot be read.
pub fn gate(
    depends_on: &[String],
    flow: &HashMap<String, FlowStep>,
    outcome_of: impl Fn(&str) -> DepOutcome,
) -> Gate {
    let mut pending = false;
    let mut block_skip = false;
    for d in depends_on {
        match verdict(outcome_of(d), flow.get(d)) {
            Verdict::BlockFail => return Gate::Skip(SkipReason::Unreachable),
            Verdict::Pending => pending = true,
            Verdict::BlockSkip => block_skip = true,
            Verdict::Pass => {}
        }
    }
    if pending {
        Gate::Wait
    } else if block_skip {
        Gate::Skip(SkipReason::Cascade)
    } else {
        Gate::Open
    }
}

/// Every flow step `s` with `caught(s)` (spec §2.4):
/// `s.continue_on_failure || (dependents non-empty && all dependents caught)`.
/// Structural: computed from the flow definition only.
pub fn caught_steps(flow: &HashMap<String, FlowStep>) -> HashSet<String> {
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, fs) in flow {
        for d in &fs.depends_on {
            dependents.entry(d.as_str()).or_default().push(name.as_str());
        }
    }
    // Iterative post-order walk (flows can be long chains).
    let mut memo: HashMap<&str, bool> = HashMap::new();
    for start in flow.keys() {
        let mut stack: Vec<(&str, bool)> = vec![(start.as_str(), false)];
        while let Some((s, expanded)) = stack.pop() {
            if memo.contains_key(s) {
                continue;
            }
            let fs = &flow[s];
            let ds = dependents.get(s).map(Vec::as_slice).unwrap_or(&[]);
            if fs.continue_on_failure {
                memo.insert(s, true);
                continue;
            }
            if !expanded {
                stack.push((s, true));
                for d in ds {
                    if !memo.contains_key(d) {
                        stack.push((d, false));
                    }
                }
                continue;
            }
            // A dependent still unresolved here means a cycle: never caught.
            let v = !ds.is_empty() && ds.iter().all(|d| memo.get(d).copied().unwrap_or(false));
            memo.insert(s, v);
        }
    }
    memo.into_iter()
        .filter(|&(_, v)| v)
        .map(|(k, _)| k.to_string())
        .collect()
}

/// The flow step a `job_step` row belongs to: a loop instance is judged by
/// its placeholder.
pub fn flow_step_name<'a>(step_name: &'a str, loop_source: Option<&'a str>) -> &'a str {
    if let Some(src) = loop_source {
        return src;
    }
    match step_name.find('[') {
        Some(i) => &step_name[..i],
        None => step_name,
    }
}

/// Is a failed row's failure caught (spec §2.4)?
pub fn failure_caught(caught: &HashSet<String>, step_name: &str, loop_source: Option<&str>) -> bool {
    caught.contains(flow_step_name(step_name, loop_source))
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p stroem-common gate:: && cargo build -p stroem-server`
Expected: all gate tests PASS; server builds.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-common/src/gate.rs crates/stroem-common/src/lib.rs crates/stroem-common/src/models/job.rs crates/stroem-server/src/cascade.rs
git commit -m "feat(common): dependency gate module and shared SkipReason"
```

---

### Task 2: Cascade phases through the gate

**Files:**
- Modify: `crates/stroem-server/src/cascade.rs` (predicates :268-348, `phase_promote` :447, `phase_skip_unreachable` :490, `phase_placeholders` :511, `Snapshot` :146, tests)

**Interfaces:**
- Consumes: `stroem_common::gate::{gate, DepOutcome, Gate}`, `SkipReason` (Task 1).
- Produces: no new public API; `run`/`execute`/`apply` signatures unchanged.

- [ ] **Step 1: Update existing unit tests to the new rule (red)**

In the `#[cfg(test)] mod tests` of `cascade.rs`:

- `continue_on_failure_promotes_past_failed_and_cancelled_deps` (~:1244) → rename `failed_or_cancelled_dep_with_its_own_cof_promotes_dependent`; flow becomes `("a", fs_cof(&[])), ("b", fs(&["a"])), ("x", fs_cof(&[])), ("y", fs(&["x"]))`. Assertions unchanged.
- `skip_unreachable_ignores_skipped_dep_and_cof` (~:1455) → rename `dependent_cof_no_longer_tolerates_a_failed_dep`; keep the flow; change the `d` assertion to `assert_eq!(s["d"], "skipped");` and add below it a second run with `("a", fs_cof(&[]))` asserting `s["d"] == "ready"` and `s["c"] == "pending"` (b still running, a passes).
- `placeholder_retirement_branches` (~:1568): the `p_cof` case — add a failed dependency row `row("dep_failed_cof", "failed")` with flow `("dep_failed_cof", fs_cof(&[]))` and make `p_cof` depend on it instead of the unflagged failed dep; its expectation (`running`) stays.
- `cof_alone_no_longer_bypasses_all_deps_skipped` (~:2583): flow becomes `("a", fs_cof(&[])), ("b", fs(&["a"]))`; expectation unchanged (`[s("b","cascade")]`); comment: "continue_on_failure on the skipped dependency does not pass a choice skip".
- `cws_and_cof_with_unreachable_dep_promotes` (~:2684) → rename `cof_on_an_unreachable_dep_passes_its_dependent`; flow `("a", fs_cof(&[])), ("b", fs(&["a"]))`; rows unchanged; expectation unchanged.
- `cws_and_cof_combine_on_a_single_step` (~:2699): rewrite:

```rust
    #[test]
    fn cws_and_cof_combine_on_a_single_step() {
        // x (cof) failed → y passes the gate and reaches its own `when`
        // (false → condition skip); y (cws) → z passes y's skip.
        let mut y = fs_cws(&["x"]);
        y.when = Some("false".to_string());
        let t = task(vec![("x", fs_cof(&[])), ("y", y), ("z", fs(&["y"]))]);
        let rows = vec![
            row("x", "failed"),
            row_when("y", "pending", "false"),
            row("z", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert!(skips(&plan).contains(&s("y", "condition")), "{:?}", plan);
        assert!(names(&plan).contains(&"promote:z".to_string()), "{:?}", names(&plan));
    }
```

- `unreachable_propagates_through_a_chain_in_one_run` (~:2749): first half unchanged. Second half: flow `("a", fs(&[])), ("b", fs_cof(&["a"])), ("c", fs(&["b"]))` — b skipped `unreachable` (its own cof no longer runs it), c promotes (b catches). Assert `skips(&plan) == [s("b","unreachable")]` and `promote:c` present.
- `mixed_completed_and_condition_skipped_deps_still_promote` (~:2902) → rename `mixed_completed_and_condition_skipped_dep_is_cascade_skipped`; expect `skips(&plan) == [s("c","cascade")]`. Add sibling `mixed_completed_and_cws_condition_skipped_dep_promotes` with `("b", fs_cws(&[]))` → `names(&plan) == ["promote:c"]`.
- `cof_dependent_tolerates_an_unreachable_skipped_dep_among_completed_ones` (~:2915) → rename `unreachable_dep_with_its_own_cof_passes_among_completed_ones`; flow `("a", fs(&[])), ("b", fs_cof(&[])), ("c", fs(&["a","b"]))`; expectation unchanged.
- `convergence_without_continue_on_failure_scenario` (~:2377): keep the existing assertions, then extend: set the running branch to `completed`, re-run → d skipped `cascade`; and a variant with both branches `fs_when_cws`-style (flow steps with `continue_when_skipped: true`) → d `promote`.

Add new tests (same module):

```rust
    #[test]
    fn failure_dominates_while_a_sibling_runs() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![row("a", "failed"), row("b", "running"), row("c", "pending")];
        let plan = run_default(&t, &rows);
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn cascade_skip_waits_for_a_running_sibling_then_becomes_unreachable() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let waiting = vec![row_skipped("a", "condition"), row("b", "running"), row("c", "pending")];
        assert!(run_default(&t, &waiting).changes.is_empty());
        let failed = vec![row_skipped("a", "condition"), row("b", "failed"), row("c", "pending")];
        assert_eq!(skips(&run_default(&t, &failed)), [s("c", "unreachable")]);
    }

    #[test]
    fn failed_placeholder_with_its_own_cof_lets_dependents_promote() {
        let t = task(vec![("p", fs_cof(&[])), ("d", fs(&["p"]))]);
        let rows = vec![placeholder("p", "failed", "[1]"), row("d", "pending")];
        assert_eq!(names(&run_default(&t, &rows)), ["promote:d"]);
    }

    #[test]
    fn placeholder_with_completed_and_condition_skipped_deps_is_cascade_skipped() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("m", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "condition"),
            placeholder("m", "pending", "[1,2]"),
        ];
        assert_eq!(skips(&run_default(&t, &rows)), [s("m", "cascade")]);
    }

    // ── pass timing (spec §5, Codex rounds 1-3) ──────────────────────
    // An independent placeholder `p` whose `when` sees `b` only once `b` has
    // a row status other than pending (skipped rows enter the context).
    fn observer() -> JobStepRow {
        JobStepRow {
            when_condition: Some(
                "{% if b is defined %}true{% else %}false{% endif %}".to_string(),
            ),
            ..placeholder("p", "pending", "[1]")
        }
    }
    fn timing_task(b: FlowStep) -> TaskDef {
        task(vec![("x", fs(&[])), ("a", fs(&["x"])), ("b", b), ("p", fs(&[]))])
    }

    #[test]
    fn timing_unreachable_chain_keeps_legacy_pass() {
        // x unreachable → a (P1, all deps skipped) → b (P2) → p sees b, expands.
        let t = timing_task(fs(&["a"]));
        let rows = vec![row_skipped("x", "unreachable"), row("a", "pending"), row("b", "pending"), observer()];
        let n = names(&run_default(&t, &rows));
        assert!(n.contains(&"expand:p:1".to_string()), "{n:?}");
    }

    #[test]
    fn timing_failed_root_keeps_legacy_pass() {
        // x failed → a (P2) → b still pending at P3 → p condition-skipped.
        let t = timing_task(fs(&["a"]));
        let rows = vec![row("x", "failed"), row("a", "pending"), row("b", "pending"), observer()];
        assert!(skips(&run_default(&t, &rows)).contains(&s("p", "condition")));
    }

    #[test]
    fn timing_accepted_change_dependent_cof_no_longer_delays() {
        // 0.16 left b (own cof) pending in P2; now b is skipped in P2 → p expands.
        let t = timing_task(fs_cof(&["a"]));
        let rows = vec![row_skipped("x", "unreachable"), row("a", "pending"), row("b", "pending"), observer()];
        let n = names(&run_default(&t, &rows));
        assert!(n.contains(&"expand:p:1".to_string()), "{n:?}");
    }

    #[test]
    fn timing_accepted_change_unknown_reason_is_failure_class() {
        let t = timing_task(fs(&["a"]));
        let rows = vec![row_skipped("x", "brand-new"), row("a", "pending"), row("b", "pending"), observer()];
        let plan = run_default(&t, &rows);
        assert!(skips(&plan).contains(&s("a", "unreachable")));
        assert!(names(&plan).contains(&"expand:p:1".to_string()));
    }

    #[test]
    fn timing_accepted_change_flagged_placeholder_retires_immediately() {
        let t = task(vec![("x", fs(&[])), ("y", fs(&[])), ("l", fs_cof(&["x", "y"]))]);
        let rows = vec![row_skipped("x", "unreachable"), row("y", "running"), placeholder("l", "pending", "[1]")];
        assert_eq!(skips(&run_default(&t, &rows)), [s("l", "unreachable")]);
    }
```

(`b` in `timing_task` depends on `a`; `fs(&["a"])` is passed in. `names` renders an `Expand` as `expand:{placeholder}:{n}`.)

- [ ] **Step 2: Run to verify failures**

Run: `cargo test -p stroem-server --lib cascade::`
Expected: the flipped/new tests FAIL (e.g. `dependent_cof_no_longer_tolerates_a_failed_dep` sees `ready`).

- [ ] **Step 3: Implement**

1. Imports: `use stroem_common::gate::{gate, DepOutcome, Gate};`
2. In `impl Snapshot` add:

```rust
    fn outcome(&self, name: &str) -> DepOutcome {
        match self.index.get(name) {
            Some(&i) => DepOutcome::from_row(&self.rows[i].status, self.rows[i].skip_reason.as_deref()),
            None => DepOutcome::Pending,
        }
    }
```

   and mark `fn skip_reason` with `#[cfg(test)]` (only tests use it now).
3. Replace the predicates block (`dep_tainted` … `any_dep_carries_failure`, :268-348) with:

```rust
// ── dependency gate (spec 2026-09-26 §2, §5) ─────────────────────────

fn gate_for(snap: &Snapshot, task: &TaskDef, fs: &FlowStep) -> Gate {
    gate(&fs.depends_on, &task.flow, |d| snap.outcome(d))
}

/// Every dependency is skipped. P1 keeps 0.16's R1 domain: it applies an
/// `unreachable` skip only in this case; P2 applies the rest (spec §5).
fn all_deps_skipped(snap: &Snapshot, fs: &FlowStep) -> bool {
    !fs.depends_on.is_empty()
        && fs
            .depends_on
            .iter()
            .all(|d| snap.status(d) == Some(SKIPPED))
}
```

4. `phase_promote` loop body becomes:

```rust
        let Some(fs) = task.flow.get(&r.step_name) else {
            continue;
        };
        match gate_for(snap, task, fs) {
            Gate::Wait => continue,
            Gate::Skip(SkipReason::Unreachable) if !all_deps_skipped(snap, fs) => continue,
            Gate::Skip(reason) => {
                out.push(Change::Skip {
                    step: r.step_name.clone(),
                    reason,
                });
                continue;
            }
            Gate::Open => {}
        }
        match (&r.when_condition, ctx) {
            // … unchanged …
        }
```

5. `phase_skip_unreachable` loop body becomes:

```rust
        let Some(fs) = task.flow.get(&r.step_name) else {
            continue;
        };
        if gate_for(snap, task, fs) == Gate::Skip(SkipReason::Unreachable) {
            out.push(Change::Skip {
                step: r.step_name.clone(),
                reason: SkipReason::Unreachable,
            });
        }
```

6. In `phase_placeholders`, after the R0 adopt block, replace the `if !deps_satisfied …` and `if all_deps_skipped … all_skipped_decision …` blocks with:

```rust
        match gate_for(snap, task, fs) {
            Gate::Wait => continue,
            Gate::Skip(reason) => {
                out.push(Change::Skip {
                    step: r.step_name.clone(),
                    reason,
                });
                continue;
            }
            Gate::Open => {}
        }
```

   The `when` evaluation and expansion that follow are unchanged.
7. Update the module doc of `Change::Skip`'s reasons if it references the old rule; remove now-unused imports.

- [ ] **Step 4: Run tests**

Run: `cargo test -p stroem-server --lib cascade:: && cargo clippy -p stroem-server -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/cascade.rs
git commit -m "feat(cascade): decide promotion and skips through the dependency gate"
```

---

### Task 3: Settlement, hooks and restart preview use `caught_steps`

**Files:**
- Modify: `crates/stroem-server/src/settlement/settle.rs:33-67` (+ tests)
- Modify: `crates/stroem-server/src/settlement/hooks.rs:45-58` (`FailedStepInfo`), `:473-490` (`build_hook_context`) (+ every other `FailedStepInfo {` literal: `grep -rn "FailedStepInfo {" crates`)
- Modify: `crates/stroem-server/src/restart.rs:88-120` (+ tests)

**Interfaces:**
- Consumes: `caught_steps`, `failure_caught`, `flow_step_name` (Task 1).
- Produces: `FailedStepInfo.tolerated: bool` (serialized; hook template `hook.failed_steps[].tolerated`).

- [ ] **Step 1: Failing tests in `settle.rs`**

Add to `settle.rs` tests (the module's `row()` builds a `JobStepRow` via `test_default`; add a variant with a skip reason):

```rust
    fn skipped(name: &str, reason: Option<&str>) -> JobStepRow {
        let mut r = row(name, "skipped", None);
        r.skip_reason = reason.map(str::to_string);
        r
    }

    #[test]
    fn failure_caught_downstream_completes() {
        // a (no flag) failed → b (cof) skipped unreachable → c completed
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&["a"], true)),
            ("c", flow_step(&["b"], false)),
        ]);
        let steps = vec![
            row("a", "failed", None),
            skipped("b", Some("unreachable")),
            row("c", "completed", None),
        ];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Completed);
    }

    #[test]
    fn failure_escaping_through_one_branch_fails() {
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&["a"], true)),
            ("d", flow_step(&["a"], false)),
        ]);
        let steps = vec![
            row("a", "failed", None),
            skipped("b", Some("unreachable")),
            skipped("d", Some("unreachable")),
        ];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn cancelled_step_with_unreachable_dependents_cancels() {
        let t = task(vec![("a", flow_step(&[], false)), ("b", flow_step(&["a"], false))]);
        let steps = vec![row("a", "cancelled", None), skipped("b", Some("unreachable"))];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Cancelled);
    }

    #[test]
    fn skipped_leaf_with_null_or_unknown_reason_completes() {
        let t = task(vec![("a", flow_step(&[], false)), ("b", flow_step(&[], false))]);
        for reason in [None, Some("brand-new")] {
            let steps = vec![row("a", "completed", None), skipped("b", reason)];
            assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Completed, "{reason:?}");
        }
    }

    #[test]
    fn failed_row_missing_from_flow_fails() {
        let t = task(vec![("a", flow_step(&[], true))]);
        let steps = vec![row("a", "completed", None), row("gone", "failed", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn mixed_cancelled_and_failed_precedence() {
        // failure caught → cancelled; failure uncaught → failed
        let caught = task(vec![
            ("f", flow_step(&[], true)),
            ("c", flow_step(&[], false)),
        ]);
        let uncaught = task(vec![
            ("f", flow_step(&[], false)),
            ("c", flow_step(&[], false)),
        ]);
        let steps = vec![row("f", "failed", None), row("c", "cancelled", None)];
        assert_eq!(decide(&caught, &steps).unwrap().status, JobStatus::Cancelled);
        assert_eq!(decide(&uncaught, &steps).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn structural_policy_ignores_an_already_completed_dependent() {
        // Codex round 1: a restart can carry `a` failed + `b` completed while
        // the current flow gives neither a flag. `caught` reads the flow, so
        // the carried failure still fails the job (as in 0.16).
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&["a"], false)),
            ("z", flow_step(&[], false)),
        ]);
        let steps = vec![
            row("a", "failed", None),
            row("b", "completed", None),
            row("z", "completed", None),
        ];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn cancelled_instance_under_completed_placeholder_cancels() {
        let t = task(vec![("p", flow_step(&[], false))]);
        let steps = vec![row("p", "completed", None), row("p[0]", "cancelled", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Cancelled);
    }
```

Run: `cargo test -p stroem-server --lib settlement::settle` — Expected: `failure_caught_downstream_completes` FAILS (today: `a` untolerated → Failed).

- [ ] **Step 2: Implement `decide`**

Replace `flow_name`, `tolerated` and `untolerated_failure` (settle.rs:33-51) with:

```rust
    // Spec 2026-09-26 §2.4: only `failed` rows can fail the job, and only when
    // no `continue_on_failure` catches the failure downstream. Loop instances
    // are judged by their placeholder; skipped rows never decide the status.
    let caught = stroem_common::gate::caught_steps(&task.flow);
    let untolerated_failure = steps.iter().any(|s| {
        s.status == StepStatus::Failed.as_ref()
            && !stroem_common::gate::failure_caught(&caught, &s.step_name, s.loop_source.as_deref())
    });
```

Run the settle tests — Expected: PASS (all old ones too).

- [ ] **Step 3: Hooks — failing test, then `tolerated`**

Add to `FailedStepInfo`:

```rust
    /// `true` when this failure is caught by `continue_on_failure` on this
    /// step or on every path below it (spec 2026-09-26 §2.4); a loop instance
    /// is judged by its placeholder.
    pub tolerated: bool,
```

In `build_hook_context` replace the `.map(|s| { … })` body:

```rust
    let caught = stroem_common::gate::caught_steps(&task.flow);
    let failed_steps: Vec<FailedStepInfo> = steps
        .iter()
        .filter(|s| s.status == StepStatus::Failed.as_ref())
        .map(|s| {
            let flow_name = stroem_common::gate::flow_step_name(&s.step_name, s.loop_source.as_deref());
            FailedStepInfo {
                step_name: s.step_name.clone(),
                action_name: s.action_name.clone(),
                error_message: s.error_message.clone(),
                continue_on_failure: task
                    .flow
                    .get(flow_name)
                    .map(|fs| fs.continue_on_failure)
                    .unwrap_or(false),
                tolerated: stroem_common::gate::failure_caught(
                    &caught,
                    &s.step_name,
                    s.loop_source.as_deref(),
                ),
                carried_over: s.carried_over,
            }
        })
        .collect();
```

Add `tolerated: false` to every other `FailedStepInfo { … }` literal (tests). `build_hook_context` needs a pool, so its `tolerated` value is pinned end-to-end in Task 5 (`test_gate_caught_failure_completes_fires_on_success_no_retry` reads it back from the hook step's input). Run: `cargo build -p stroem-server --tests`.

- [ ] **Step 4: Restart preview — failing test, then implement**

Add to `restart.rs` tests:

```rust
    #[test]
    fn carried_failure_caught_downstream_is_tolerated() {
        // a failed (no flag) → b (cof) skipped; restart the independent z.
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], true)),
            ("z".into(), fs(&[], false)),
        ]);
        let mut b = row("b", "skipped", None);
        b.skip_reason = Some("unreachable".into());
        let src = [row("a", "failed", None), b, row("z", "failed", None)];
        let p = compute_restart_set(&flow, &src, "z").unwrap();
        assert_eq!(p.carried_failed_tolerated, vec!["a"]);
        assert!(p.carried_failed.is_empty(), "carried skips never appear");
    }
```

Implement: before the `for name in names` loop add `let caught = stroem_common::gate::caught_steps(flow);` and replace `if flow[name].continue_on_failure {` with `if caught.contains(name.as_str()) {`.

Run: `cargo test -p stroem-server --lib restart:: settlement::` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/settlement crates/stroem-server/src/restart.rs
git commit -m "feat(settlement): a failure fails the job only if nothing catches it downstream"
```

---

### Task 4: DB-backed cascade tests follow the new rule

**Files:**
- Modify: `crates/stroem-server/tests/orchestrator_test.rs`
- Modify: `crates/stroem-server/tests/integration_test.rs` (:10521, :25679, :31336 regions)

**Interfaces:** consumes only production code from Tasks 1-3.

- [ ] **Step 1: Apply the flips** (helpers: `flow_step`, `flow_step_cof`, `flow_step_cws`, `flow_step_when`, `flow_step_when_cws` at orchestrator_test.rs:135-186)

| Test | Change |
|---|---|
| `test_continue_on_failure_promotes_dependent` (:449) | `a` → `flow_step_cof(vec![])`, `b` → `flow_step(vec!["a"])`; rewrite the doc comment (:445) to "continue_on_failure on A lets its dependents run when A fails". Assertions unchanged. |
| `test_skipped_dep_treated_as_satisfied_with_truthy_when` (:906) | Give the skipped dependency `flow_step_when_cws(..)` so the dependent still runs; add a second job without cws asserting the dependent is skipped with `skip_reason = 'cascade'`. |
| `test_convergence_without_continue_on_failure` (:1165) | At :1253 the merge now ends `skipped` (reason `cascade`); add `test_convergence_with_cws_on_branches` identical but both branch steps `flow_step_when_cws` → merge `ready`. |
| `test_mixed_skipped_and_failed_dep_runs_with_cof` (:1326) | Rename `test_mixed_skipped_and_failed_dep_runs_when_both_deps_pass`; failed dep gets `flow_step_cof`, skipped dep `flow_step_when_cws` → dependent `ready`; add a variant without cws on the skipped dep → `skipped`/`cascade`. |
| `test_single_completed_plus_single_skipped_convergence` (:1387) | Skipped dep gets `flow_step_when_cws` to keep `ready`; add the no-cws variant → `cascade`. |
| `test_three_dep_fan_in_one_completed_two_skipped_converges` (:1618) | cws on both skipped deps → `ready`; cws on only one → `cascade`. |
| `test_all_deps_skipped_with_cof_alone_is_cascade_skipped` (:1501) | **Unchanged.** Add `test_all_deps_skipped_with_cof_on_the_skipped_dep_is_still_cascade_skipped` (flag on the dependency, same expectation). |
| `test_failed_dep_with_continue_on_failure_does_not_skip_for_each_placeholder` (:1877) | Move the flag to the failed dependency `a`; existing `assert_ne!`s hold. |
| `test_cancelled_dep_blocks_without_cof` (:1457) | Unchanged; add `test_cancelled_dep_with_its_own_cof_lets_dependent_run` → `ready`. |
| `test_continue_when_skipped_does_not_run_after_upstream_failure` (:2299) | Unchanged; add variant: the unreachable dependency carries `continue_on_failure` → dependent `ready`. |

For `skip_reason` assertions use:

```rust
async fn skip_reason(pool: &PgPool, job_id: Uuid, step: &str) -> Option<String> {
    sqlx::query_scalar("SELECT skip_reason FROM job_step WHERE job_id = $1 AND step_name = $2")
        .bind(job_id)
        .bind(step)
        .fetch_one(pool)
        .await
        .unwrap()
}
```

In `integration_test.rs`:
- `test_continue_on_failure_promotes_after_fail` (:10521): move `continue_on_failure: true` from the dependent (:10553) to `step1` (:10535); the final job status assertion (:10664-10666) becomes `"completed"`; update comments.
- `test_step_retry_with_continue_on_failure` (:25679): flag from `step-b` (:25797) to `step-a` (:25774); fix comments :25676, :25893.
- `test_task_dispatch_failure_path_evaluates_when_against_task_state` (:31336): flag from `after` (:31350) to `spawn` (:31347); fix comment :31343.

- [ ] **Step 2: Run**

Run: `cargo test -p stroem-server --test orchestrator_test && cargo test -p stroem-server --test integration_test continue_on_failure step_retry task_dispatch_failure_path`
Expected: PASS (Docker required).

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/orchestrator_test.rs crates/stroem-server/tests/integration_test.rs
git commit -m "test: move continue_on_failure to the failing step; strict-AND merges"
```

---

### Task 5: Full-settlement tests (hooks, task retry, cancellation)

**Files:**
- Modify: `crates/stroem-server/tests/integration_test.rs` (append a `// ─── Dependency gate: full settlement ───` section; reuse `setup_state_with_workspace` :22600, `create_job_for_task` :71, `register_test_worker` :2471)

**Interfaces:** consumes `AppState::settlement().advance(job_id)` (settlement/mod.rs:200), `JobStepRepo::{mark_running, mark_failed, mark_completed, mark_cancelled}`.

- [ ] **Step 1: Write the tests**

```rust
// ─── Dependency gate: full settlement (spec 2026-09-26 §12.3) ───────────

fn gate_workspace(extra_flow: &str, max_attempts: u32) -> WorkspaceConfig {
    let yaml = format!(
        r#"
actions:
  ok: {{ type: script, script: "true" }}
  hook-ok: {{ type: script, script: "true" }}
  hook-err: {{ type: script, script: "true" }}
  hook-cancel: {{ type: script, script: "true" }}
tasks:
  pipe:
    retry: {{ max_attempts: {max_attempts} }}
    on_success: [{{ action: hook-ok, input: {{ tolerated: "{{{{ hook.failed_steps | map(attribute='tolerated') | join(sep=',') }}}}" }} }}]
    on_error: [{{ action: hook-err }}]
    on_cancel: [{{ action: hook-cancel }}]
    flow:
      pred: {{ action: ok }}
      imp: {{ action: ok, depends_on: [pred], continue_on_failure: true }}
      merge: {{ action: ok, depends_on: [imp] }}
{extra_flow}
"#
    );
    serde_yaml::from_str(&yaml).expect("gate workspace yaml")
}

async fn hook_actions(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT s.action_name FROM job j JOIN job_step s ON s.job_id = j.job_id \
         WHERE j.source_type = 'hook' ORDER BY s.action_name",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn fail_step(state: &AppState, pool: &PgPool, job_id: Uuid, step: &str, worker: Uuid) -> Result<()> {
    JobStepRepo::mark_running(pool, job_id, step, worker).await?;
    JobRepo::mark_running_if_pending(pool, job_id, worker).await?;
    JobStepRepo::mark_failed(pool, job_id, step, "boom").await?;
    state.settlement().advance(job_id).await
}

async fn complete_step(state: &AppState, pool: &PgPool, job_id: Uuid, step: &str, worker: Uuid) -> Result<()> {
    JobStepRepo::mark_running(pool, job_id, step, worker).await?;
    JobStepRepo::mark_completed(pool, job_id, step, None).await?;
    state.settlement().advance(job_id).await
}

#[tokio::test]
async fn test_gate_caught_failure_completes_fires_on_success_no_retry() -> Result<()> {
    // Replay of prod job 9691df79 (spec §3.1).
    let ws = gate_workspace("", 2);
    let (state, pool, _tmp, _c) = setup_state_with_workspace(ws.clone()).await?;
    let job_id = create_job_for_task(&pool, &ws, "default", "pipe", json!({}), "api",
        None, None, None, None, JobDefaults::default()).await?;
    let w = register_test_worker(&pool).await;

    fail_step(&state, &pool, job_id, "pred", w).await?;
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let imp = steps.iter().find(|s| s.step_name == "imp").unwrap();
    assert_eq!((imp.status.as_str(), imp.skip_reason.as_deref()), ("skipped", Some("unreachable")));
    complete_step(&state, &pool, job_id, "merge", w).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    assert!(job.retry_job_id.is_none(), "a caught failure is not retried");
    assert_eq!(hook_actions(&pool).await, ["hook-ok"]);
    // The hook payload is the hook step's literal input (CLAUDE.md § Hooks).
    let tolerated: Option<String> = sqlx::query_scalar(
        "SELECT s.input->>'tolerated' FROM job j JOIN job_step s ON s.job_id = j.job_id \
         WHERE j.source_type = 'hook'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(tolerated.as_deref(), Some("true"), "failed_steps[0].tolerated");
    let recorded: bool = sqlx::query_scalar("SELECT metrics_recorded_at IS NOT NULL FROM job WHERE job_id = $1")
        .bind(job_id).fetch_one(&pool).await?;
    assert!(recorded, "terminal claim ran (completion metric counted once, status=completed)");
    Ok(())
}

#[tokio::test]
async fn test_gate_escaping_failure_with_retry_budget_retries_without_hook() -> Result<()> {
    let ws = gate_workspace("      side: { action: ok, depends_on: [pred] }", 2);
    let (state, pool, _tmp, _c) = setup_state_with_workspace(ws.clone()).await?;
    let job_id = create_job_for_task(&pool, &ws, "default", "pipe", json!({}), "api",
        None, None, None, None, JobDefaults::default()).await?;
    let w = register_test_worker(&pool).await;

    fail_step(&state, &pool, job_id, "pred", w).await?;
    complete_step(&state, &pool, job_id, "merge", w).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed", "escaped through `side`");
    assert!(job.retry_job_id.is_some(), "retry job created");
    assert!(hook_actions(&pool).await.is_empty(), "no hook while a retry is planned");
    Ok(())
}

#[tokio::test]
async fn test_gate_escaping_failure_without_retry_budget_fires_on_error() -> Result<()> {
    let ws = gate_workspace("      side: { action: ok, depends_on: [pred] }", 1);
    let (state, pool, _tmp, _c) = setup_state_with_workspace(ws.clone()).await?;
    let job_id = create_job_for_task(&pool, &ws, "default", "pipe", json!({}), "api",
        None, None, None, None, JobDefaults::default()).await?;
    let w = register_test_worker(&pool).await;

    fail_step(&state, &pool, job_id, "pred", w).await?;
    complete_step(&state, &pool, job_id, "merge", w).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.retry_job_id.is_none());
    assert_eq!(hook_actions(&pool).await, ["hook-err"]);
    Ok(())
}

#[tokio::test]
async fn test_gate_cancelled_step_keeps_job_cancelled() -> Result<()> {
    // Codex round-1 high finding: a cancelled step (e.g. a cancelled child
    // job under a `type: task` step) skips its dependents as `unreachable`,
    // but the job ends `cancelled` — on_cancel, no retry.
    let ws = gate_workspace("", 2);
    let (state, pool, _tmp, _c) = setup_state_with_workspace(ws.clone()).await?;
    let job_id = create_job_for_task(&pool, &ws, "default", "pipe", json!({}), "api",
        None, None, None, None, JobDefaults::default()).await?;

    JobStepRepo::mark_cancelled(&pool, job_id, "pred").await?;
    state.settlement().advance(job_id).await?;
    // `imp` (cof) passes the cancellation to `merge`; finish it.
    let w = register_test_worker(&pool).await;
    complete_step(&state, &pool, job_id, "merge", w).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "cancelled");
    assert!(job.retry_job_id.is_none());
    assert_eq!(hook_actions(&pool).await, ["hook-cancel"]);
    Ok(())
}
```

Also add (same helpers, one test each):
- `test_gate_approval_reject_caught_downstream_completes` — flow `gate: { type: approval, message: "ok?" }` → `after: { action: ok, depends_on: [gate], continue_on_failure: true }` → `last: { action: ok, depends_on: [after] }`; reject via `JobStepRepo::fail_or_retry` is internal — instead drive `POST /api/jobs/{id}/steps/gate/approve` with `{"approved": false, "rejection_reason": "no"}` through a router built by `setup_with_workspace` (:22540); then complete `last` via `/worker/jobs/{id}/steps/last/complete` `{"exit_code":0}` → job `completed`.
- `test_gate_step_retry_exhausted_caught_downstream_completes` — `pred` with `retry: { max_attempts: 2 }`; fail it twice through `/worker/jobs/{id}/steps/pred/complete` `{"exit_code":1}` (the first call resets it to `ready`), then complete `merge` → `completed`.
- `test_gate_failed_sequential_loop_caught_downstream_completes` — `loop: { action: ok, for_each: [1,2], sequential: true }` → `c: { action: ok, depends_on: [loop], continue_on_failure: true }` → `d: { action: ok, depends_on: [c] }`; fail `loop[0]` via `fail_step` → `loop[1]` skipped, placeholder rolls up `failed`, `c` skipped `unreachable`, complete `d` → `completed`.

If the approval endpoint's body shape differs, read `web/api/jobs.rs` (approve handler) and match it.

- [ ] **Step 2: Run**

Run: `cargo test -p stroem-server --test integration_test test_gate_`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/integration_test.rs
git commit -m "test(settlement): hooks, task retry and cancellation under the dependency gate"
```

---

### Task 6: `stroem run` uses the gate

**Files:**
- Modify: `crates/stroem-cli/src/local/run.rs` (`cmd_run` :75-86, `RunSummary` :88, `run_dag` :130-355, `cascade_skip` :569-610, `build_render_context` :430-442, tests)
- Create: `crates/stroem-cli/tests/run_exit_code.rs`

**Interfaces:**
- Consumes: `gate`, `caught_steps`, `DepOutcome`, `Gate`, `SkipReason` (Task 1).
- Produces: `RunSummary { completed, skipped, failed, outcome: RunOutcome }`, `enum RunOutcome { Completed, Failed }`; `build_render_context(input, outputs, errors, secrets)`.

- [ ] **Step 1: Failing tests (in-module)**

Delete `test_cascade_skip_simple` (:947), `test_cascade_skip_partial` (:970), `test_cascade_skip_respects_continue_when_skipped` (:992), `test_cascade_skip_diamond` (:1308). Update the three `build_render_context` tests to pass `&HashMap::new()` as the new `errors` argument. Add:

```rust
    async fn run_yaml(yaml: &str, task: &str) -> RunSummary {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.yaml"), yaml).unwrap();
        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        run_dag(&config.tasks[task], &config, &json!({}), dir.path(), &CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_run_independent_branch_continues_after_failure() {
        let s = run_yaml(r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail }
      b: { action: ok, depends_on: [a] }
      z: { action: ok }
"#, "t").await;
        assert_eq!((s.completed, s.skipped, s.failed), (1, 1, 1));
        assert_eq!(s.outcome, RunOutcome::Failed);
    }

    #[tokio::test]
    async fn test_run_failure_caught_downstream_completes() {
        let s = run_yaml(r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail }
      b: { action: ok, depends_on: [a], continue_on_failure: true }
      c: { action: ok, depends_on: [b] }
"#, "t").await;
        assert_eq!((s.completed, s.skipped, s.failed), (1, 1, 1));
        assert_eq!(s.outcome, RunOutcome::Completed);
    }

    #[tokio::test]
    async fn test_run_merge_needs_cws_on_skipped_branch() {
        let yaml = |cws: bool| format!(r#"
actions:
  ok: {{ type: script, script: echo ok }}
tasks:
  t:
    flow:
      x: {{ action: ok }}
      y: {{ action: ok, when: "false", continue_when_skipped: {cws} }}
      m: {{ action: ok, depends_on: [x, y] }}
"#);
        let without = run_yaml(&yaml(false), "t").await;
        assert_eq!((without.completed, without.skipped), (1, 2), "m cascade-skipped");
        let with = run_yaml(&yaml(true), "t").await;
        assert_eq!((with.completed, with.skipped), (2, 1), "m runs");
    }

    #[tokio::test]
    async fn test_run_when_error_fails_the_step_not_the_run() {
        let s = run_yaml(r#"
actions:
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      bad: { action: ok, when: "{{ nope.nope }}" }
      z: { action: ok }
"#, "t").await;
        assert_eq!((s.completed, s.failed), (1, 1));
    }

    #[tokio::test]
    async fn test_run_failed_loop_output_is_masked_but_own_flag_loop_is_not() {
        // L (no flag) fails → B (cof) unreachable → C reads L.output: null.
        // K (own cof) completes with [null, null] and D reads its length.
        // Step input reaches the action script as `{{ input.x }}`
        // (execute_step inserts the prepared action input as `input`).
        let s = run_yaml(r#"
actions:
  ok: { type: script, script: "true" }
  item: { type: script, script: "test '{{ each.item }}' = good" }
  check:
    type: script
    input:
      c: { type: string }
      want: { type: string }
    script: "test '{{ input.c }}' = '{{ input.want }}'"
tasks:
  t:
    flow:
      L: { action: item, for_each: ["bad", "good"] }
      B: { action: ok, depends_on: [L], continue_on_failure: true }
      C: { action: check, depends_on: [B], input: { c: "{{ L.output | json_encode() }}", want: "null" } }
      K: { action: item, for_each: ["bad", "good"], continue_on_failure: true }
      D: { action: check, depends_on: [K], input: { c: "{{ K.output | length }}", want: "2" } }
"#, "t").await;
        // L's failure is caught at B; C and D fail (uncaught) if their check fails.
        assert_eq!(s.outcome, RunOutcome::Completed, "C and D must both pass their checks");
    }

Create `crates/stroem-cli/tests/run_exit_code.rs`:

```rust
use std::process::Command;

fn run(yaml: &str) -> std::process::ExitStatus {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), yaml).unwrap();
    Command::new(env!("CARGO_BIN_EXE_stroem"))
        .args(["--path", dir.path().to_str().unwrap(), "run", "t"])
        .status()
        .unwrap()
}

#[test]
fn caught_failure_exits_zero() {
    assert!(run(r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail }
      b: { action: ok, depends_on: [a], continue_on_failure: true }
      c: { action: ok, depends_on: [b] }
"#).success());
}

#[test]
fn uncaught_failure_exits_one() {
    let s = run(r#"
actions:
  fail: { type: script, script: exit 1 }
tasks:
  t:
    flow:
      a: { action: fail }
"#);
    assert_eq!(s.code(), Some(1));
}

#[cfg(unix)]
#[test]
fn ctrl_c_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), r#"
actions:
  slow: { type: script, script: sleep 30 }
tasks:
  t:
    flow:
      a: { action: slow }
"#).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stroem"))
        .args(["--path", dir.path().to_str().unwrap(), "run", "t"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    assert!(!child.wait().unwrap().success());
}
```

Ensure `tempfile` is under `[dev-dependencies]` in `crates/stroem-cli/Cargo.toml` (it is listed at line 32; move/duplicate into `[dev-dependencies]` if it sits under `[dependencies]` only — it is usable either way).

Run: `cargo test -p stroem-cli` — Expected: new tests FAIL/compile errors.

- [ ] **Step 2: Implement**

Imports: `use stroem_common::gate::{caught_steps, gate, DepOutcome, Gate}; use stroem_common::models::job::SkipReason;`; drop `dag` from the imports if no longer used.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcome {
    Completed,
    Failed,
}

struct RunSummary {
    completed: usize,
    skipped: usize,
    failed: usize,
    outcome: RunOutcome,
}

fn skip_label(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::Condition => "condition false",
        SkipReason::Empty => "empty for_each",
        SkipReason::Cascade => "a dependency was skipped",
        SkipReason::Unreachable => "an upstream step failed",
    }
}

/// A failed step: its output is masked for templates and `error` exposed,
/// as on the server (render_context.rs:282-289).
fn record_failure(
    outcomes: &mut HashMap<String, DepOutcome>,
    outputs: &mut HashMap<String, Option<serde_json::Value>>,
    errors: &mut HashMap<String, String>,
    step: &str,
    msg: String,
) {
    outcomes.insert(step.to_string(), DepOutcome::Failed);
    outputs.insert(step.to_string(), None);
    errors.insert(step.to_string(), msg);
}
```

`run_dag` body (replace :130-355):

```rust
    let runner = ShellRunner::new();
    let mut outcomes: HashMap<String, DepOutcome> = HashMap::new();
    let mut outputs: HashMap<String, Option<serde_json::Value>> = HashMap::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut failed_count: usize = 0;

    loop {
        if cancel_token.is_cancelled() {
            bail!("Cancelled by user");
        }
        // Steps the gate can decide now (spec 2026-09-26 §2.3). A skip that
        // is decided while a sibling is still pending is final.
        let mut decidable: Vec<(String, Gate)> = task
            .flow
            .iter()
            .filter(|(name, _)| !outcomes.contains_key(*name))
            .filter_map(|(name, fs)| {
                let g = gate(&fs.depends_on, &task.flow, |d| {
                    outcomes.get(d).copied().unwrap_or(DepOutcome::Pending)
                });
                (g != Gate::Wait).then(|| (name.clone(), g))
            })
            .collect();
        if decidable.is_empty() {
            break;
        }
        decidable.sort_by(|a, b| a.0.cmp(&b.0));

        for (step_name, g) in decidable {
            if cancel_token.is_cancelled() {
                bail!("Cancelled by user");
            }
            if let Gate::Skip(reason) = g {
                eprintln!("--- Step: {} [SKIPPED] ({}) ---", step_name, skip_label(reason));
                outcomes.insert(step_name.clone(), DepOutcome::Skipped(Some(reason)));
                outputs.insert(step_name, None);
                continue;
            }
            let step = &task.flow[&step_name];
            let action = &config.actions[&step.action];
            let ctx = build_render_context(input, &outputs, &errors, &config.secrets);

            if let Some(ref when_expr) = step.when {
                match evaluate_condition(when_expr, &ctx) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!("--- Step: {} [SKIPPED] ({}) ---", step_name, skip_label(SkipReason::Condition));
                        outcomes.insert(step_name.clone(), DepOutcome::Skipped(Some(SkipReason::Condition)));
                        outputs.insert(step_name, None);
                        continue;
                    }
                    Err(e) => {
                        let msg = format!("when condition error: {:#}", e);
                        eprintln!("Step '{}' failed: {}", step_name, msg);
                        failed_count += 1;
                        record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, msg);
                        continue;
                    }
                }
            }

            if let Some(ref for_each_expr) = step.for_each {
                let items = match evaluate_for_each(for_each_expr, &ctx) {
                    Ok(items) => items,
                    Err(e) => {
                        let msg = format!("for_each expression error: {:#}", e);
                        eprintln!("Step '{}' failed: {}", step_name, msg);
                        failed_count += 1;
                        record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, msg);
                        continue;
                    }
                };
                if items.is_empty() {
                    eprintln!("--- Step: {} [SKIPPED] ({}) ---", step_name, skip_label(SkipReason::Empty));
                    outcomes.insert(step_name.clone(), DepOutcome::Skipped(Some(SkipReason::Empty)));
                    outputs.insert(step_name, None);
                    continue;
                }
                // … iteration loop exactly as today (:219-280), keeping
                //    `any_failed`, `failed_count += 1` per failed iteration,
                //    and `break` on failure unless `step.continue_on_failure` …
                if any_failed && !step.continue_on_failure {
                    record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name,
                        "for_each loop failed".to_string());
                } else {
                    outcomes.insert(step_name.clone(), DepOutcome::Completed);
                    outputs.insert(step_name, Some(json!(iter_outputs)));
                }
                continue;
            }

            eprintln!("--- Step: {} (action: {}) ---", step_name, step.action);
            match execute_step(&step_name, step, action, config, &ctx, &runner, workspace_path, cancel_token).await {
                Ok(result) if result.success() => {
                    outcomes.insert(step_name.clone(), DepOutcome::Completed);
                    outputs.insert(step_name, result.output);
                }
                Ok(result) => {
                    eprintln!("Step '{}' failed (exit code {})", step_name, result.exit_code);
                    let mut msg = format!("Exit code: {}", result.exit_code);
                    if !result.stderr.is_empty() {
                        eprintln!("stderr: {}", result.stderr);
                        msg.push_str(&format!("\nStderr: {}", result.stderr));
                    }
                    failed_count += 1;
                    record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, msg);
                }
                Err(e) => {
                    eprintln!("Step '{}' error: {:#}", step_name, e);
                    failed_count += 1;
                    record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, format!("{:#}", e));
                }
            }
        }
    }

    // Counts are diagnostic and keep 0.16's arithmetic (spec §6).
    let skipped = outcomes.values().filter(|o| matches!(o, DepOutcome::Skipped(_))).count();
    let completed = outcomes.len().saturating_sub(skipped).saturating_sub(failed_count);
    let caught = caught_steps(&task.flow);
    let uncaught = outcomes
        .iter()
        .any(|(name, o)| *o == DepOutcome::Failed && !caught.contains(name));
    Ok(RunSummary {
        completed,
        skipped,
        failed: failed_count,
        outcome: if uncaught { RunOutcome::Failed } else { RunOutcome::Completed },
    })
```

(The `// … iteration loop exactly as today` comment is the ONLY elided region: copy lines :219-280 verbatim, minus the old `completed.insert`/`outputs.insert` after the loop.)

`build_render_context`:

```rust
fn build_render_context(
    input: &serde_json::Value,
    outputs: &HashMap<String, Option<serde_json::Value>>,
    errors: &HashMap<String, String>,
    secrets: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    let mut ctx = serde_json::Map::new();
    ctx.insert("input".to_string(), input.clone());
    for (step_name, output) in outputs {
        let sanitized = step_name.replace('-', "_");
        let mut entry = serde_json::Map::new();
        entry.insert("output".into(), output.clone().unwrap_or(json!(null)));
        if let Some(err) = errors.get(step_name) {
            entry.insert("error".into(), json!(err));
        }
        ctx.insert(sanitized, serde_json::Value::Object(entry));
    }
    // … rest unchanged (secrets) …
```

`cmd_run` tail:

```rust
    eprintln!(
        "\n{} completed, {} skipped, {} failed",
        summary.completed, summary.skipped, summary.failed
    );
    if summary.outcome == RunOutcome::Completed && summary.failed > 0 {
        eprintln!("Every failure was caught by continue_on_failure.");
    }
    Ok(summary.outcome == RunOutcome::Completed)
```

Delete `cascade_skip`.

- [ ] **Step 3: Run**

Run: `cargo test -p stroem-cli && cargo clippy -p stroem-cli -- -D warnings`
Expected: PASS (existing :1047-:1606 assertions unchanged).

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-cli
git commit -m "feat(cli): stroem run decides steps with the shared dependency gate"
```

---

### Task 7: Validation warning for merges

**Files:**
- Modify: `crates/stroem-common/src/validation.rs` (~:298, + tests near :6401)

- [ ] **Step 1: Failing tests**

```rust
    #[test]
    fn test_merge_after_conditional_dep_without_cws_warns() {
        let yaml = r#"
actions:
  a: { type: script, script: "true" }
tasks:
  t:
    flow:
      x: { action: a }
      y: { action: a, when: "{{ input.go }}" }
      m: { action: a, depends_on: [x, y] }
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("'m' will be skipped whenever 'y' is skipped")),
            "{warnings:?}"
        );
    }

    #[test]
    fn test_merge_after_conditional_dep_with_cws_or_single_dep_does_not_warn() {
        for yaml in [
            r#"
actions:
  a: { type: script, script: "true" }
tasks:
  t:
    flow:
      x: { action: a }
      y: { action: a, when: "{{ input.go }}", continue_when_skipped: true }
      m: { action: a, depends_on: [x, y] }
"#,
            r#"
actions:
  a: { type: script, script: "true" }
tasks:
  t:
    flow:
      y: { action: a, when: "{{ input.go }}" }
      m: { action: a, depends_on: [y] }
"#,
        ] {
            let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
            let warnings = validate_workflow_config(&config).unwrap();
            assert!(warnings.iter().all(|w| !w.contains("will be skipped whenever")), "{warnings:?}");
        }
    }
```

(Use the same entry function the neighbouring tests at :6401 use — `validate_workflow_config` or its variant; copy their call.)

Run: `cargo test -p stroem-common validation::tests::test_merge_after` — Expected: FAIL.

- [ ] **Step 2: Implement** (right after the existing `continue_when_skipped` warning)

```rust
            // Strict AND (spec 2026-09-26 §2.3): a merge is skipped whenever a
            // dependency that can be skipped by choice is skipped, unless that
            // dependency carries continue_when_skipped.
            if step.depends_on.len() >= 2 {
                for dep in &step.depends_on {
                    if let Some(d) = task.flow.get(dep) {
                        if (d.when.is_some() || d.for_each.is_some()) && !d.continue_when_skipped {
                            warnings.push(format!(
                                "Task '{}' step '{}' will be skipped whenever '{}' is skipped (add continue_when_skipped: true to '{}' to let '{}' run)",
                                task_name, step_name, dep, dep, step_name
                            ));
                        }
                    }
                }
            }
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p stroem-common validation` — Expected: PASS.

```bash
git add crates/stroem-common/src/validation.rs
git commit -m "feat(validation): warn when a merge depends on a skippable step without continue_when_skipped"
```

---

### Task 8: UI wording

**Files:**
- Modify: `ui/src/lib/skip-reason.ts:30-32`, `ui/src/lib/__tests__/skip-reason.test.ts`
- Modify: `ui/src/pages/task-detail.tsx:434`, `ui/src/pages/job-detail.tsx:324-336`

- [ ] **Step 1: Test first** — in `skip-reason.test.ts` change the cascade expectation to `toContain("a dependency was skipped")` and add `expect(skipExplanation("unreachable")).toContain("did not let its dependents run");`. Run `cd ui && bun run test skip-reason` → FAIL.

- [ ] **Step 2: Implement**

```ts
    case "cascade":
      return "Skipped: a dependency was skipped and did not let its dependents run (continue_when_skipped).";
    case "unreachable":
      return "Skipped: an upstream step failed or was cancelled and did not let its dependents run (continue_on_failure), or the row has no recorded reason (jobs from before 0.16.2).";
```

`task-detail.tsx:434`: `{step.continue_on_failure && <> &middot; catches failures (dependents run)</>}`

`job-detail.tsx` banner text: `{n} step(s) failed; the failures were caught by continue_on_failure:{" "}` (keep the list of names).

- [ ] **Step 3: Run + commit**

Run: `cd ui && bun run test && bun run lint && bunx tsc --noEmit` — Expected: PASS (`step-detail.test.tsx:159` still finds "upstream step failed").

```bash
git add ui/src
git commit -m "feat(ui): skip and tolerated-failure wording for the dependency gate"
```

---

### Task 9: E2E fixtures

**Files:**
- Modify: `workspace/.workflows/failing.yaml:36-47`, `ui/e2e/jobs.spec.ts:212-241`
- Modify: `tests/e2e-workspace/conditional.yaml`, `tests/e2e.sh` (conditional-report block ~:686-727)

- [ ] **Step 1: `fail-continue`** — move `continue_on_failure: true` from `step-after` to `step-fail`. In `jobs.spec.ts` change the test title to "continue_on_failure on a failing step lets its dependents run" and `expect(apiData.status).toBe("completed")` with comment "step-fail's failure is caught by its own flag".

- [ ] **Step 2: Strict-AND merges** — append to `conditional-report` flow in `tests/e2e-workspace/conditional.yaml`:

```yaml
      # Strict AND (0.17): a merge of a completed step and an unflagged skip
      # is skipped; with the flag on the skipped branch it runs.
      always:
        action: say
      plain-merge:
        action: say
        depends_on: [always, plain-check]
      cws-merge:
        action: say
        depends_on: [always, optional-check]
```

In `tests/e2e.sh`, after the `CWS_FOLLOW` assertion add:

```bash
CWS_PMERGE=$(echo "$CWS_DETAIL" | jq -r '.steps[] | select(.step_name == "plain-merge") | "\(.status)/\(.skip_reason)"')
CWS_CMERGE=$(echo "$CWS_DETAIL" | jq -r '.steps[] | select(.step_name == "cws-merge") | "\(.status)/\(.skip_reason)"')
[ "$CWS_PMERGE" = "skipped/cascade" ] || { echo "$CWS_DETAIL" | jq .steps; fail "plain-merge expected skipped/cascade, got $CWS_PMERGE"; }
[ "$CWS_CMERGE" = "completed/null" ] || { echo "$CWS_DETAIL" | jq .steps; fail "cws-merge expected completed/null, got $CWS_CMERGE"; }
pass "strict AND: merge with an unflagged skipped branch skipped, flagged branch ran"
```

- [ ] **Step 3: Run** `./tests/e2e.sh` (Docker; ~10 min) — Expected: all PASS. Then Playwright `jobs.spec.ts` via the docker-compose test profile (CLAUDE.md § Frontend).

- [ ] **Step 4: Commit**

```bash
git add workspace/.workflows/failing.yaml ui/e2e/jobs.spec.ts tests/e2e-workspace/conditional.yaml tests/e2e.sh
git commit -m "test(e2e): flag on the failing step; strict-AND merge scenarios"
```

---

### Task 10: Documentation

**Files:** spec §11 list. Core text to reuse everywhere (keep wording consistent):

> **`continue_on_failure`** on a step: if this step fails, is cancelled, or is skipped because something above it failed, the steps that depend on it still run, and the failure does not fail the job. It never makes the step itself run.
> **`continue_when_skipped`** on a step: if this step is skipped by its own `when`, an empty `for_each`, or because a step above it was skipped the same way, the steps that depend on it still run.
> A step runs only when **every** dependency lets it through (completed, or not completed but carrying the matching flag). A failure fails the job unless a `continue_on_failure` catches it — on the failing step or on every path below it.

- [ ] **Step 1: Reference + guides** — rewrite per spec §11: `reference/workflow-yaml.md` rows :406-407 (use the two bold sentences above), :442, :444, :532 and the hook field table :793 (+ `tolerated`); `guides/workflow-basics.md:266-287` (replace "dual semantics" with the core text; cleanup example → `on_error` hook, copy the pattern from `guides/hooks.md:254+`); `guides/conditionals.md` (overview :14-15 — drop "Convergence runs automatically"; convergence :100-129, if/else :241-264, optional step :300-326, example workflow :359-415 — add `continue_when_skipped: true` to each branch step; :155-169 move `continue_on_failure` to `last-step`; skip reasons :177-186 + "Changed in 0.17" note); `guides/retry.md:515-538` (flag → `process`); `guides/templating.md:150-166`; `examples/ci-pipeline.md` (notify → `on_success`/`on_error`); `guides/event-sources.md:582,605` (→ `on_error`); `guides/action-types.md:635,649` ("steps after a rejected approval run only if the approval step has `continue_on_failure`"); `guides/loops.md:169-173` (note: a loop with its own flag completes, so its dependents run); `guides/hooks.md:58` (+ `tolerated`, cleanup pointer); `reference/cli.md` (`stroem run` continues independent branches; exit 0 when every failure is caught); `reference/worker-api.md:159`; `operations/migration-046.md:25-32` (point to the new page).
- [ ] **Step 2: Upgrade page** — create `docs/src/content/docs/operations/upgrade-0-17-dependency-flags.md` (frontmatter `title: Upgrading to 0.17 — dependency flags`) with: the core text; spec §10's table verbatim; the job 9691df79 before/after table from spec §3.1; a checklist ("move `continue_on_failure` from cleanup/notify steps to `on_error` hooks; add `continue_when_skipped` to each branch of a merge; run `stroem validate` — it warns about merges"). Register it in `docs/astro.config.mjs` next to `migration-046` (~:77).
- [ ] **Step 3: Code/project docs** — `crates/stroem-common/src/models/workflow.rs:397-404`: doc comments = the two bold sentences. `CLAUDE.md` § Step Cascade: add "Promotion/skip decisions go through `stroem_common::gate` (`gate`, `verdict`); P1 applies unreachable skips only when all deps are skipped, P2 the rest (spec 2026-09-26 §5)". § Conditional Flow Steps: replace the all-deps-skipped formula paragraph and the mixed-deps paragraph with the core text + "`caught_steps` decides job status (settle.rs); skipped rows never decide it; the CLI uses the same gate and produces `unreachable`". `CONTEXT.md`: add **Dependency gate** ("the rule deciding whether a step may run, from each dependency's state and that dependency's own flags; strict AND") and **Caught failure** ("a failure with `continue_on_failure` on the failing step or on every path below it; it does not fail the job"). `settlement/dispatch.rs:42` comment. `docs/internal/TODO.md`: add under Code Quality "CLI summary counts: successful loop iterations count as one step, failed ones individually (spec 2026-09-26 §6)". Add at the top of `docs/superpowers/specs/2026-09-09-continue-when-skipped-design.md`: "Superseded in part by 2026-09-26-dependency-gate-design.md (§2.3, §2.4, Revision 4)."
- [ ] **Step 4: llms** — update `docs/scripts/generate-llms-txt.ts:55` bullet with the core text; run `cd docs && bun run build` (regenerates llms files, checks links) — Expected: build succeeds.
- [ ] **Step 5: Commit**

```bash
git add docs CLAUDE.md CONTEXT.md crates/stroem-common/src/models/workflow.rs crates/stroem-server/src/settlement/dispatch.rs
git commit -m "docs: dependency gate — flags on the upstream step, caught failures, 0.17 upgrade page"
```

---

### Task 11: Full verification and Codex implementation review

- [ ] **Step 1:** `cargo fmt --check --all && cargo clippy --workspace -- -D warnings && cargo test --workspace` — Expected: all green (Docker for integration tests). Known flaky: `log_storage::tests` under parallel runs — re-run alone before investigating.
- [ ] **Step 2:** `cd ui && bun run lint && bunx tsc --noEmit && bun run test`.
- [ ] **Step 3:** `./tests/e2e.sh`.
- [ ] **Step 4:** Codex review of the branch diff against the spec (continue thread 01a0dcb7 from the main checkout's cwd — Codex threads are per-cwd; from a worktree use `--fresh` and point it at the spec). Fix findings, re-run Steps 1-3.
