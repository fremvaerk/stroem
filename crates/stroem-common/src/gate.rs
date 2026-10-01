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
///
/// NOTE: this old signature/body is superseded by Step 8 — left as-is here
/// deliberately (it no longer compiles against the new `DepOutcome` shape
/// above, nor against `DependsOnEntry`; that is expected until Step 8 lands,
/// per the brief's own Step 4 note).
pub fn gate(
    depends_on: &[String],
    flow: &std::collections::HashMap<String, crate::models::workflow::FlowStep>,
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
pub fn verdict(outcome: DepOutcome, dep: Option<&crate::models::workflow::FlowStep>) -> Verdict {
    let cof = dep.is_some_and(|d| d.continue_on_failure);
    let cws = dep.is_some_and(|d| d.legacy_continue_when_skipped.unwrap_or(false));
    match outcome {
        DepOutcome::Pending => Verdict::Pending,
        DepOutcome::Completed => Verdict::Pass,
        DepOutcome::Skipped => {
            if cws {
                Verdict::Pass
            } else {
                Verdict::BlockSkip
            }
        }
        DepOutcome::Failed | DepOutcome::Cancelled | DepOutcome::Omitted => {
            if cof {
                Verdict::Pass
            } else {
                Verdict::BlockFail
            }
        }
    }
}

/// Every flow step `s` with `caught(s)` (spec §2.4):
/// `s.continue_on_failure || (dependents non-empty && all dependents caught)`.
/// Structural: computed from the flow definition only.
pub fn caught_steps(
    flow: &std::collections::HashMap<String, crate::models::workflow::FlowStep>,
) -> std::collections::HashSet<String> {
    let mut dependents: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for (name, fs) in flow {
        for d in &fs.depends_on {
            dependents
                .entry(d.as_str())
                .or_default()
                .push(name.as_str());
        }
    }
    let mut memo: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    let mut visiting: std::collections::HashSet<&str> = std::collections::HashSet::new();
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
    caught: &std::collections::HashSet<String>,
    step_name: &str,
    loop_source: Option<&str>,
) -> bool {
    caught.contains(flow_step_name(step_name, loop_source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depends_on::{AcceptSet, AllEntry, AnyEntry, DependsOnEntry, Outcome, StepEntry};

    #[test]
    fn from_row_classifies_live_statuses_as_pending() {
        for live in [
            "pending",
            "ready",
            "claimed",
            "running",
            "suspended",
            "bogus",
        ] {
            assert_eq!(
                DepOutcome::from_row(live, None),
                DepOutcome::Pending,
                "{live}"
            );
        }
    }

    #[test]
    fn from_row_classifies_terminal_statuses() {
        assert_eq!(
            DepOutcome::from_row("completed", None),
            DepOutcome::Completed
        );
        assert_eq!(DepOutcome::from_row("failed", None), DepOutcome::Failed);
        assert_eq!(
            DepOutcome::from_row("cancelled", None),
            DepOutcome::Cancelled
        );
    }

    #[test]
    fn from_row_splits_skip_reasons_into_skipped_vs_omitted() {
        // Own-choice reasons -> Skipped.
        assert_eq!(
            DepOutcome::from_row("skipped", Some("condition")),
            DepOutcome::Skipped
        );
        assert_eq!(
            DepOutcome::from_row("skipped", Some("empty")),
            DepOutcome::Skipped
        );
        // Propagated-block reasons (choice- or failure-origin alike) -> Omitted.
        assert_eq!(
            DepOutcome::from_row("skipped", Some("cascade")),
            DepOutcome::Omitted
        );
        assert_eq!(
            DepOutcome::from_row("skipped", Some("unreachable")),
            DepOutcome::Omitted
        );
        // Unknown/NULL reason -> Omitted (conservative, matches the old
        // Skipped(None) "reads as failure-class" rule).
        assert_eq!(
            DepOutcome::from_row("skipped", Some("brand-new")),
            DepOutcome::Omitted
        );
        assert_eq!(DepOutcome::from_row("skipped", None), DepOutcome::Omitted);
    }

    #[test]
    fn dep_outcome_to_schema_outcome_maps_the_four_terminal_cases() {
        assert_eq!(
            DepOutcome::Completed.as_schema_outcome(),
            Some(Outcome::Completed)
        );
        assert_eq!(
            DepOutcome::Failed.as_schema_outcome(),
            Some(Outcome::Failed)
        );
        assert_eq!(
            DepOutcome::Cancelled.as_schema_outcome(),
            Some(Outcome::Cancelled)
        );
        assert_eq!(
            DepOutcome::Skipped.as_schema_outcome(),
            Some(Outcome::Skipped)
        );
        assert_eq!(
            DepOutcome::Omitted.as_schema_outcome(),
            Some(Outcome::Omitted)
        );
        assert_eq!(DepOutcome::Pending.as_schema_outcome(), None);
    }
}
