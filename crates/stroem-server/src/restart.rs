//! Restart From Step and Restart Failed Steps — pure planning (spec
//! 2026-09-07 §4.1–4.2, spec 2026-11-02 §4). No I/O.

use std::collections::{BTreeSet, HashMap};
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::FlowStep;
use stroem_db::{JobStepRow, Seed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartError {
    UnknownStep(String),
    LoopInstance {
        base: String,
    },
    /// `RestartFrom::AllFailed` found no failed or cancelled step in the
    /// current flow.
    NoFailedSteps,
}

impl std::fmt::Display for RestartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownStep(s) => write!(f, "Step '{}' is not in the current flow", s),
            Self::LoopInstance { base } => {
                write!(f, "Restart from the loop step '{}', not an instance", base)
            }
            Self::NoFailedSteps => write!(f, "No failed or cancelled steps to restart"),
        }
    }
}
impl std::error::Error for RestartError {}

/// Where a restart starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartFrom {
    /// Restart From Step: one chosen flow step.
    Step(String),
    /// Restart Failed Steps: every failed or cancelled top-level step.
    AllFailed,
}

#[derive(Debug, Clone)]
pub struct RestartPlan {
    pub from: RestartFrom,
    /// The steps the restart starts at (spec 2026-11-02 §4.1), sorted: `[x]`
    /// for a step restart. Steps added to the flow since the source ran are
    /// roots too but are not listed here (they are in `restart_steps`).
    pub start_steps: Vec<String>,
    pub restart_steps: Vec<String>,
    pub carried: Vec<Seed>,
    pub carried_failed: Vec<String>,
    pub carried_failed_tolerated: Vec<String>,
}

pub const CARRIED_CANCELLED_MSG: &str = "carried over from cancelled source job";

/// True for the four statuses a source row is carried with as-is.
fn is_terminal(status: &str) -> bool {
    status == StepStatus::Completed.as_ref()
        || status == StepStatus::Failed.as_ref()
        || status == StepStatus::Skipped.as_ref()
        || status == StepStatus::Cancelled.as_ref()
}

/// The status a source row would be carried with: its own when terminal,
/// `cancelled` otherwise (spec 2026-09-07 §4.2).
fn carried_status(status: &str) -> &str {
    if is_terminal(status) {
        status
    } else {
        StepStatus::Cancelled.as_ref()
    }
}

/// Restart From Step: [`plan_restart`] with [`RestartFrom::Step`].
pub fn compute_restart_set(
    flow: &HashMap<String, FlowStep>,
    source_steps: &[JobStepRow],
    from_step: &str,
) -> Result<RestartPlan, RestartError> {
    plan_restart(
        flow,
        source_steps,
        &RestartFrom::Step(from_step.to_string()),
    )
}

/// start_steps = `[x]`, or every flow step whose source row would be carried
/// `failed` or `cancelled`; roots = start_steps ∪ {current steps with no
/// source row}; restart_set = roots ∪ transitive_dependents(flow, roots);
/// everything else is carried.
pub fn plan_restart(
    flow: &HashMap<String, FlowStep>,
    source_steps: &[JobStepRow],
    from: &RestartFrom,
) -> Result<RestartPlan, RestartError> {
    // Placeholder/plain rows only — instances (loop_source set) are never in the flow.
    let source_by_name: HashMap<&str, &JobStepRow> = source_steps
        .iter()
        .filter(|s| s.loop_source.is_none())
        .map(|s| (s.step_name.as_str(), s))
        .collect();

    let start_steps: Vec<String> = match from {
        RestartFrom::Step(from_step) => {
            if let Some(i) = from_step.find('[') {
                return Err(RestartError::LoopInstance {
                    base: from_step[..i].to_string(),
                });
            }
            if !flow.contains_key(from_step.as_str()) {
                return Err(RestartError::UnknownStep(from_step.clone()));
            }
            vec![from_step.clone()]
        }
        RestartFrom::AllFailed => {
            let mut v: Vec<String> = flow
                .keys()
                .filter(|name| {
                    source_by_name.get(name.as_str()).is_some_and(|src| {
                        let st = carried_status(&src.status);
                        st == StepStatus::Failed.as_ref() || st == StepStatus::Cancelled.as_ref()
                    })
                })
                .cloned()
                .collect();
            if v.is_empty() {
                return Err(RestartError::NoFailedSteps);
            }
            v.sort();
            v
        }
    };

    let mut restart: BTreeSet<String> = start_steps.iter().cloned().collect();
    for name in flow.keys() {
        if !source_by_name.contains_key(name.as_str()) {
            restart.insert(name.clone());
        }
    }
    // Transitive closure over depends_on (fixed point; flow is a DAG, bounded by |flow|).
    loop {
        let before = restart.len();
        for (name, fs) in flow {
            if restart.contains(name) {
                continue;
            }
            let mut dep_names = Vec::new();
            stroem_common::depends_on::collect_names(&fs.depends_on, &mut dep_names);
            if dep_names.iter().any(|d| restart.contains(*d)) {
                restart.insert(name.clone());
            }
        }
        if restart.len() == before {
            break;
        }
    }

    let mut carried = Vec::new();
    let mut carried_failed = Vec::new();
    let mut carried_failed_tolerated = Vec::new();
    let mut names: Vec<&String> = flow.keys().filter(|n| !restart.contains(*n)).collect();
    names.sort();
    for name in names {
        let Some(src) = source_by_name.get(name.as_str()) else {
            continue;
        };
        let seed = if is_terminal(&src.status) {
            Seed {
                step_name: name.clone(),
                status: src.status.clone(),
                output: src.output.clone(),
                error_message: src.error_message.clone(),
                skip_reason: src.skip_reason.clone(),
            }
        } else {
            Seed {
                step_name: name.clone(),
                status: StepStatus::Cancelled.as_ref().to_string(),
                output: None,
                error_message: Some(CARRIED_CANCELLED_MSG.to_string()),
                skip_reason: None,
            }
        };
        if seed.status == StepStatus::Failed.as_ref() {
            // Spec 2026-10-01 §6: only this step's OWN continue_on_failure
            // (in the CURRENT flow) excuses a carried failure — no more
            // structural "caught somewhere downstream" walk. `name` here is
            // always a placeholder (loop instances are excluded from
            // `source_by_name` above), so a direct lookup is correct.
            if flow
                .get(name.as_str())
                .is_some_and(|fs| fs.continue_on_failure)
            {
                carried_failed_tolerated.push(name.clone());
            } else {
                carried_failed.push(name.clone());
            }
        }
        carried.push(seed);
    }

    // Spec 2026-11-02 §4.3: I1 (no carried failure) and I2 (no carried
    // cancellation) in AllFailed mode; I3 (start steps rerun) in both.
    debug_assert!(
        *from != RestartFrom::AllFailed
            || carried
                .iter()
                .all(|s| carried_status(&s.status) != StepStatus::Failed.as_ref()
                    && s.status != StepStatus::Cancelled.as_ref()),
        "all-failed restart carried a failed or cancelled step: {:?}",
        carried
    );
    debug_assert!(start_steps.iter().all(|s| restart.contains(s)));

    Ok(RestartPlan {
        from: from.clone(),
        start_steps,
        restart_steps: restart.into_iter().collect(),
        carried,
        carried_failed,
        carried_failed_tolerated,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::collections::HashMap;
    use stroem_common::models::workflow::FlowStep;
    use stroem_db::{JobStepRow, Seed};

    use super::compute_restart_set;
    use super::RestartError;

    fn fs(deps: &[&str], cof: bool) -> FlowStep {
        FlowStep {
            git_ref: None,
            action: "noop".into(),
            name: None,
            description: None,
            depends_on: deps
                .iter()
                .map(|s| stroem_common::depends_on::DependsOnEntry::Name(s.to_string()))
                .collect(),
            input: Default::default(),
            continue_on_failure: cof,
            legacy_continue_when_skipped: None,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
    }
    fn row(name: &str, status: &str, output: Option<serde_json::Value>) -> JobStepRow {
        JobStepRow {
            step_name: name.into(),
            status: status.into(),
            output,
            error_message: None,
            ..Default::default()
        }
    }
    fn names(v: &[Seed]) -> Vec<&str> {
        v.iter().map(|s| s.step_name.as_str()).collect()
    }

    #[test]
    fn linear_middle_reruns_downstream_carries_upstream() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], false)),
            ("c".into(), fs(&["b"], false)),
        ]);
        let src = [
            row("a", "completed", Some(json!(1))),
            row("b", "failed", None),
            row("c", "skipped", None),
        ];
        let p = compute_restart_set(&flow, &src, "b").unwrap();
        assert_eq!(p.restart_steps, vec!["b", "c"]);
        assert_eq!(names(&p.carried), vec!["a"]);
        assert_eq!(p.carried[0].status, "completed");
        assert_eq!(p.carried[0].output, Some(json!(1)));
    }

    #[test]
    fn diamond_other_branch_carried_as_failed_and_flagged() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], false)),
            ("c".into(), fs(&["a"], false)),
            ("d".into(), fs(&["b", "c"], false)),
        ]);
        let src = [
            row("a", "completed", None),
            row("b", "failed", None),
            row("c", "failed", None),
            row("d", "skipped", None),
        ];
        let p = compute_restart_set(&flow, &src, "b").unwrap();
        assert_eq!(p.restart_steps, vec!["b", "d"]);
        assert_eq!(names(&p.carried), vec!["a", "c"]);
        assert_eq!(p.carried_failed, vec!["c"]);
        assert!(p.carried_failed_tolerated.is_empty());
    }

    #[test]
    fn carried_failed_tolerated_by_current_flow_is_split_out() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], true)),
            ("c".into(), fs(&["a"], false)),
        ]);
        let src = [
            row("a", "completed", None),
            row("b", "failed", None),
            row("c", "failed", None),
        ];
        let p = compute_restart_set(&flow, &src, "c").unwrap();
        assert_eq!(p.carried_failed_tolerated, vec!["b"]);
        assert!(p.carried_failed.is_empty());
    }

    #[test]
    fn root_restart_carries_nothing() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], false)),
        ]);
        let src = [row("a", "completed", None), row("b", "completed", None)];
        let p = compute_restart_set(&flow, &src, "a").unwrap();
        assert_eq!(p.restart_steps, vec!["a", "b"]);
        assert!(p.carried.is_empty());
    }

    #[test]
    fn step_added_upstream_pulls_existing_dependent_into_restart_set() {
        // Source ran a → c. Current flow inserted b between them: a → b → c.
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], false)),
            ("c".into(), fs(&["b"], false)),
            ("z".into(), fs(&[], false)),
        ]);
        let src = [
            row("a", "completed", None),
            row("c", "completed", None),
            row("z", "failed", None),
        ];
        let p = compute_restart_set(&flow, &src, "z").unwrap();
        assert_eq!(
            p.restart_steps,
            vec!["b", "c", "z"],
            "b is new → root; c depends on b → rerun"
        );
        assert_eq!(names(&p.carried), vec!["a"]);
    }

    #[test]
    fn step_removed_from_flow_is_dropped() {
        let flow = HashMap::from([("a".into(), fs(&[], false))]);
        let src = [row("a", "completed", None), row("gone", "failed", None)];
        let p = compute_restart_set(&flow, &src, "a").unwrap();
        assert!(p.carried.is_empty() && p.carried_failed.is_empty());
    }

    #[test]
    fn non_terminal_source_rows_carry_as_cancelled() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&[], false)),
            ("c".into(), fs(&[], false)),
            ("d".into(), fs(&[], false)),
            ("e".into(), fs(&[], false)),
            ("x".into(), fs(&[], false)),
        ]);
        let src = [
            row("a", "pending", None),
            row("b", "ready", None),
            row("c", "claimed", None),
            row("d", "running", None),
            row("e", "suspended", None),
            row("x", "failed", None),
        ];
        let p = compute_restart_set(&flow, &src, "x").unwrap();
        // Exact membership, so an empty `carried` can never satisfy the loop
        // below vacuously and a status dropped from the non-terminal set shows
        // up as a missing name rather than as silence.
        assert_eq!(names(&p.carried), vec!["a", "b", "c", "d", "e"]);
        for s in &p.carried {
            assert_eq!(s.status, "cancelled", "{}", s.step_name);
            assert_eq!(s.output, None, "{}", s.step_name);
            assert_eq!(
                s.error_message.as_deref(),
                Some("carried over from cancelled source job")
            );
        }
    }

    #[test]
    fn for_each_placeholder_carried_with_aggregated_output_instances_ignored() {
        let mut lp = fs(&[], false);
        lp.for_each = Some(json!("{{ x }}"));
        let flow = HashMap::from([
            ("loop".into(), lp),
            ("after".into(), fs(&["loop"], false)),
            ("x".into(), fs(&[], false)),
        ]);
        let mut inst = row("loop[0]", "completed", Some(json!(1)));
        inst.loop_source = Some("loop".into());
        let src = [
            row("loop", "completed", Some(json!([1, 2]))),
            inst,
            row("after", "completed", None),
            row("x", "failed", None),
        ];
        let p = compute_restart_set(&flow, &src, "x").unwrap();
        assert_eq!(names(&p.carried), vec!["after", "loop"]);
        assert_eq!(
            p.carried
                .iter()
                .find(|s| s.step_name == "loop")
                .unwrap()
                .output,
            Some(json!([1, 2]))
        );
    }

    #[test]
    fn carried_failure_is_tolerated_only_by_its_own_flag() {
        // a failed (no flag) → b (cof) skipped; restart the independent z.
        // Under the OLD caught_steps() rule, a's failure was "caught"
        // because b catches it structurally downstream even though a itself
        // has no flag. Under the new rule (spec 2026-10-01 §6), only a's own
        // flag counts — a has none, so it's an untolerated carried failure.
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], true)),
            ("z".into(), fs(&[], false)),
        ]);
        let mut b = row("b", "skipped", None);
        b.skip_reason = Some("unreachable".into());
        let src = [row("a", "failed", None), b, row("z", "failed", None)];
        let p = compute_restart_set(&flow, &src, "z").unwrap();
        assert_eq!(
            p.carried_failed,
            vec!["a"],
            "a has no flag of its own — must be untolerated, regardless of b's"
        );
        assert!(p.carried_failed_tolerated.is_empty());
    }

    #[test]
    fn loop_instance_and_unknown_step_are_rejected() {
        let flow = HashMap::from([("loop".into(), fs(&[], false))]);
        assert!(matches!(
            compute_restart_set(&flow, &[], "loop[2]"),
            Err(RestartError::LoopInstance { base }) if base == "loop"
        ));
        assert!(matches!(
            compute_restart_set(&flow, &[], "nope"),
            Err(RestartError::UnknownStep(s)) if s == "nope"
        ));
    }

    // ---- Restart Failed Steps (spec 2026-11-02) ----

    use super::{plan_restart, RestartFrom};

    fn all_failed(
        flow: &HashMap<String, FlowStep>,
        src: &[JobStepRow],
    ) -> Result<super::RestartPlan, RestartError> {
        plan_restart(flow, src, &RestartFrom::AllFailed)
    }

    #[test]
    fn all_failed_reruns_every_failed_branch_with_its_dependents() {
        // a → b(failed) → c ; a → d(failed) → e ; f independent completed
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], false)),
            ("c".into(), fs(&["b"], false)),
            ("d".into(), fs(&["a"], false)),
            ("e".into(), fs(&["d"], false)),
            ("f".into(), fs(&[], false)),
        ]);
        let mut c = row("c", "skipped", None);
        c.skip_reason = Some("unreachable".into());
        let mut e = row("e", "skipped", None);
        e.skip_reason = Some("unreachable".into());
        let src = [
            row("a", "completed", Some(json!(1))),
            row("b", "failed", None),
            c,
            row("d", "failed", None),
            e,
            row("f", "completed", None),
        ];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.from, RestartFrom::AllFailed);
        assert_eq!(p.start_steps, vec!["b", "d"]);
        assert_eq!(p.restart_steps, vec!["b", "c", "d", "e"]);
        assert_eq!(names(&p.carried), vec!["a", "f"]);
        assert!(p.carried_failed.is_empty() && p.carried_failed_tolerated.is_empty());
    }

    #[test]
    fn all_failed_includes_continue_on_failure_failures() {
        // The prod dd4c3a6a shape: build → ai_maintain (cof, failed, leaf),
        // build → ml_master (no cof, failed) → x, y (unreachable),
        // build → ml_beta (cof, failed, leaf).
        let flow = HashMap::from([
            ("build".into(), fs(&[], false)),
            ("ai_maintain".into(), fs(&["build"], true)),
            ("ml_master".into(), fs(&["build"], false)),
            ("ml_beta".into(), fs(&["build"], true)),
            ("x".into(), fs(&["ml_master"], false)),
            ("y".into(), fs(&["x"], false)),
        ]);
        let mut x = row("x", "skipped", None);
        x.skip_reason = Some("unreachable".into());
        let mut y = row("y", "skipped", None);
        y.skip_reason = Some("unreachable".into());
        let src = [
            row("build", "completed", None),
            row("ai_maintain", "failed", None),
            row("ml_master", "failed", None),
            row("ml_beta", "failed", None),
            x,
            y,
        ];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.start_steps, vec!["ai_maintain", "ml_beta", "ml_master"]);
        assert_eq!(
            p.restart_steps,
            vec!["ai_maintain", "ml_beta", "ml_master", "x", "y"]
        );
        assert_eq!(names(&p.carried), vec!["build"]);
        // I1: nothing failed is carried, tolerated or not.
        assert!(p.carried_failed.is_empty());
        assert!(p.carried_failed_tolerated.is_empty());
    }

    #[test]
    fn all_failed_failed_loop_reruns_as_a_whole() {
        let mut lp = fs(&[], false);
        lp.for_each = Some(json!("{{ x }}"));
        let flow = HashMap::from([("loop".into(), lp), ("after".into(), fs(&["loop"], false))]);
        let mut i0 = row("loop[0]", "completed", Some(json!(1)));
        i0.loop_source = Some("loop".into());
        let mut i1 = row("loop[1]", "failed", None);
        i1.loop_source = Some("loop".into());
        let mut after = row("after", "skipped", None);
        after.skip_reason = Some("unreachable".into());
        let src = [row("loop", "failed", Some(json!([1, null]))), i0, i1, after];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(
            p.start_steps,
            vec!["loop"],
            "instances are never start steps"
        );
        assert_eq!(p.restart_steps, vec!["after", "loop"]);
        assert!(p.carried.is_empty());
    }

    #[test]
    fn all_failed_cancelled_only_loop_rolled_up_completed_is_carried() {
        // Known quirk (CLAUDE.md § For-Each): only cancelled instances roll
        // the placeholder up `completed`, so it is not a start step.
        let mut lp = fs(&[], false);
        lp.for_each = Some(json!("{{ x }}"));
        let flow = HashMap::from([("loop".into(), lp), ("z".into(), fs(&[], false))]);
        let mut i0 = row("loop[0]", "cancelled", None);
        i0.loop_source = Some("loop".into());
        let src = [
            row("loop", "completed", Some(json!([null]))),
            i0,
            row("z", "failed", None),
        ];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.start_steps, vec!["z"]);
        assert_eq!(names(&p.carried), vec!["loop"]);
    }

    #[test]
    fn all_failed_cancelled_and_non_terminal_rows_are_start_steps() {
        let flow = HashMap::from([
            ("c".into(), fs(&[], false)),
            ("p".into(), fs(&[], false)),
            ("r".into(), fs(&[], false)),
            ("cl".into(), fs(&[], false)),
            ("ru".into(), fs(&[], false)),
            ("su".into(), fs(&[], false)),
            ("ok".into(), fs(&[], false)),
            ("sk".into(), fs(&[], false)),
        ]);
        let mut sk = row("sk", "skipped", None);
        sk.skip_reason = Some("condition".into());
        let src = [
            row("c", "cancelled", None),
            row("p", "pending", None),
            row("r", "ready", None),
            row("cl", "claimed", None),
            row("ru", "running", None),
            row("su", "suspended", None),
            row("ok", "completed", None),
            sk,
        ];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.start_steps, vec!["c", "cl", "p", "r", "ru", "su"]);
        assert_eq!(names(&p.carried), vec!["ok", "sk"]);
        // I2: no carried row is cancelled.
        assert!(p.carried.iter().all(|s| s.status != "cancelled"));
    }

    #[test]
    fn all_failed_independent_failed_and_suspended_both_rerun() {
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&[], false))]);
        let src = [row("a", "failed", None), row("b", "suspended", None)];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.start_steps, vec!["a", "b"]);
        assert!(p.carried.is_empty());
    }

    #[test]
    fn all_failed_with_nothing_failed_is_an_error_even_with_an_added_step() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("new".into(), fs(&["a"], false)),
        ]);
        let src = [row("a", "completed", None)];
        assert_eq!(
            all_failed(&flow, &src).unwrap_err(),
            RestartError::NoFailedSteps
        );
    }

    #[test]
    fn all_failed_ignores_a_failed_step_no_longer_in_the_flow() {
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&[], false))]);
        let src = [
            row("a", "completed", None),
            row("gone", "failed", None),
            row("b", "failed", None),
        ];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.start_steps, vec!["b"]);
        assert_eq!(p.restart_steps, vec!["b"]);
        // Alone, a removed failed step is no start step at all.
        let src = [row("a", "completed", None), row("gone", "failed", None)];
        assert_eq!(
            all_failed(&flow, &src).unwrap_err(),
            RestartError::NoFailedSteps
        );
    }

    #[test]
    fn all_failed_added_step_is_a_root_but_not_a_start_step() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&[], false)),
            ("new".into(), fs(&[], false)),
        ]);
        let src = [row("a", "failed", None), row("b", "completed", None)];
        let p = all_failed(&flow, &src).unwrap();
        assert_eq!(p.start_steps, vec!["a"]);
        assert_eq!(p.restart_steps, vec!["a", "new"]);
    }

    #[test]
    fn step_mode_reports_its_step_as_the_only_start_step() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)),
            ("b".into(), fs(&["a"], false)),
        ]);
        let src = [row("a", "completed", None), row("b", "failed", None)];
        let p = compute_restart_set(&flow, &src, "b").unwrap();
        assert_eq!(p.from, RestartFrom::Step("b".into()));
        assert_eq!(p.start_steps, vec!["b"]);
    }
}
