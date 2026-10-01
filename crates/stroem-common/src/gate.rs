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

    fn outcome_map<'a>(pairs: &'a [(&'a str, DepOutcome)]) -> impl Fn(&str) -> DepOutcome + 'a {
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
        assert_eq!(
            gate(&deps, outcome_map(&[("a", DepOutcome::Completed)])),
            Gate::Open
        );
        assert_eq!(
            gate(&deps, outcome_map(&[("a", DepOutcome::Failed)])),
            Gate::Omitted
        );
        assert_eq!(gate(&deps, outcome_map(&[])), Gate::Wait);
    }

    #[test]
    fn step_entry_with_explicit_accept_tolerates_failure_but_not_skip() {
        let deps = vec![step_entry("a", &[Outcome::Completed, Outcome::Failed])];
        assert_eq!(
            gate(&deps, outcome_map(&[("a", DepOutcome::Failed)])),
            Gate::Open
        );
        assert_eq!(
            gate(&deps, outcome_map(&[("a", DepOutcome::Skipped)])),
            Gate::Omitted
        );
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
            DependsOnEntry::Any(AnyEntry {
                any: vec![name_entry("mirror-a"), name_entry("mirror-b")],
            }),
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

    #[test]
    fn flow_step_name_normalizes_loop_instances() {
        assert_eq!(flow_step_name("p[3]", Some("p")), "p");
        assert_eq!(flow_step_name("p[3]", None), "p");
        assert_eq!(flow_step_name("plain", None), "plain");
    }
}
