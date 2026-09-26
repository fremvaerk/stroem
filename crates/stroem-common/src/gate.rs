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
///
/// `visiting` guards against a cycle: server-side validation of `depends_on`
/// is not wired into workspace load (see CLAUDE.md § Cross-Workspace
/// References), so a cyclic flow can reach this function. A node popped for
/// expansion while it is still open earlier on the same walk is a cycle back
/// edge; it is left unresolved for this walk rather than re-pushed, which
/// guarantees the walk always terminates. An unresolved dependent reads as
/// not caught (`unwrap_or(false)` below) — a conservative answer for the
/// cyclic portion of the graph, never a hang.
pub fn caught_steps(flow: &HashMap<String, FlowStep>) -> HashSet<String> {
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, fs) in flow {
        for d in &fs.depends_on {
            dependents
                .entry(d.as_str())
                .or_default()
                .push(name.as_str());
        }
    }
    // Iterative post-order walk (flows can be long chains).
    let mut memo: HashMap<&str, bool> = HashMap::new();
    let mut visiting: HashSet<&str> = HashSet::new();
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
                if visiting.contains(s) {
                    // Cycle back edge: leave unresolved for this walk.
                    continue;
                }
                visiting.insert(s);
                stack.push((s, true));
                for d in ds {
                    if !memo.contains_key(d) && !visiting.contains(d) {
                        stack.push((d, false));
                    }
                }
                continue;
            }
            // A dependent still unresolved here means a cycle: never caught.
            visiting.remove(s);
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
pub fn failure_caught(
    caught: &HashSet<String>,
    step_name: &str,
    loop_source: Option<&str>,
) -> bool {
    caught.contains(flow_step_name(step_name, loop_source))
}

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
        for live in [
            "pending",
            "ready",
            "claimed",
            "running",
            "suspended",
            "bogus",
        ] {
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
        assert_eq!(
            DepOutcome::from_row("skipped", Some("new-reason")),
            Skipped(None)
        );
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
        assert_eq!(verdict(DepOutcome::Pending, Some(&cof)), Verdict::Pending);
        assert_eq!(verdict(Completed, Some(&none)), Pass);
        for o in choice {
            assert_eq!(verdict(o, Some(&none)), BlockSkip, "{o:?}");
            assert_eq!(verdict(o, Some(&cws)), Pass, "{o:?}");
            assert_eq!(
                verdict(o, Some(&cof)),
                BlockSkip,
                "cof does not pass a skip: {o:?}"
            );
        }
        for o in failure {
            assert_eq!(verdict(o, Some(&none)), BlockFail, "{o:?}");
            assert_eq!(verdict(o, Some(&cof)), Pass, "{o:?}");
            assert_eq!(
                verdict(o, Some(&cws)),
                BlockFail,
                "cws does not pass a failure: {o:?}"
            );
        }
        // A dependency missing from the flow has no flags.
        assert_eq!(verdict(Failed, None), BlockFail);
        assert_eq!(
            verdict(Skipped(Some(SkipReason::Condition)), None),
            BlockSkip
        );
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
        assert_eq!(
            run(Skipped(Some(SkipReason::Condition)), Pending),
            Gate::Wait
        );
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
            if d == "x" {
                Failed
            } else {
                Skipped(Some(SkipReason::Condition))
            }
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
        assert!(
            !c.contains("c") && !c.contains("d"),
            "leaves without the flag"
        );
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

    #[test]
    fn caught_steps_terminates_on_a_cycle() {
        // x → y → x, no flags: neither is caught, and the walk terminates.
        let f = flow(vec![
            ("x", fs(&["y"], false, false)),
            ("y", fs(&["x"], false, false)),
        ]);
        let c = caught_steps(&f);
        assert!(!c.contains("x") && !c.contains("y"));

        // x → y → x, y has continue_on_failure: y is caught (its own flag),
        // and so is x — its only dependent, y, is caught.
        let f2 = flow(vec![
            ("x", fs(&["y"], false, false)),
            ("y", fs(&["x"], true, false)),
        ]);
        let c2 = caught_steps(&f2);
        assert!(c2.contains("y"));
        assert!(c2.contains("x"));
    }
}
