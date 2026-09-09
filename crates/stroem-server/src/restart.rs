//! Restart From Step — pure planning (spec §4.1–4.2). No I/O.

use std::collections::{BTreeSet, HashMap, HashSet};
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::FlowStep;
use stroem_db::{JobStepRow, Seed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartError {
    UnknownStep(String),
    LoopInstance { base: String },
}

impl std::fmt::Display for RestartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownStep(s) => write!(f, "Step '{}' is not in the current flow", s),
            Self::LoopInstance { base } => {
                write!(f, "Restart from the loop step '{}', not an instance", base)
            }
        }
    }
}
impl std::error::Error for RestartError {}

#[derive(Debug, Clone, Default)]
pub struct RestartPlan {
    pub restart_steps: Vec<String>,
    pub carried: Vec<Seed>,
    pub carried_failed: Vec<String>,
    pub carried_failed_tolerated: Vec<String>,
}

pub const CARRIED_CANCELLED_MSG: &str = "carried over from cancelled source job";

/// roots = {from_step} ∪ {current steps with no source row};
/// restart_set = roots ∪ transitive_dependents(flow, roots); everything else is carried.
pub fn compute_restart_set(
    flow: &HashMap<String, FlowStep>,
    source_steps: &[JobStepRow],
    from_step: &str,
) -> Result<RestartPlan, RestartError> {
    if let Some(i) = from_step.find('[') {
        return Err(RestartError::LoopInstance {
            base: from_step[..i].to_string(),
        });
    }
    if !flow.contains_key(from_step) {
        return Err(RestartError::UnknownStep(from_step.to_string()));
    }

    // Placeholder/plain rows only — instances (loop_source set) are never in the flow.
    let source_by_name: HashMap<&str, &JobStepRow> = source_steps
        .iter()
        .filter(|s| s.loop_source.is_none())
        .map(|s| (s.step_name.as_str(), s))
        .collect();

    let mut restart: BTreeSet<String> = BTreeSet::new();
    restart.insert(from_step.to_string());
    for name in flow.keys() {
        if !source_by_name.contains_key(name.as_str()) {
            restart.insert(name.clone());
        }
    }
    // Transitive closure over depends_on (fixed point; flow is a DAG, bounded by |flow|).
    loop {
        let before = restart.len();
        for (name, fs) in flow {
            if !restart.contains(name) && fs.depends_on.iter().any(|d| restart.contains(d)) {
                restart.insert(name.clone());
            }
        }
        if restart.len() == before {
            break;
        }
    }

    let terminal: HashSet<&str> = [
        StepStatus::Completed.as_ref(),
        StepStatus::Failed.as_ref(),
        StepStatus::Skipped.as_ref(),
        StepStatus::Cancelled.as_ref(),
    ]
    .into_iter()
    .collect();

    let mut carried = Vec::new();
    let mut carried_failed = Vec::new();
    let mut carried_failed_tolerated = Vec::new();
    let mut names: Vec<&String> = flow.keys().filter(|n| !restart.contains(*n)).collect();
    names.sort();
    for name in names {
        let Some(src) = source_by_name.get(name.as_str()) else {
            continue;
        };
        let seed = if terminal.contains(src.status.as_str()) {
            Seed {
                step_name: name.clone(),
                status: src.status.clone(),
                output: src.output.clone(),
                error_message: src.error_message.clone(),
            }
        } else {
            Seed {
                step_name: name.clone(),
                status: StepStatus::Cancelled.as_ref().to_string(),
                output: None,
                error_message: Some(CARRIED_CANCELLED_MSG.to_string()),
            }
        };
        if seed.status == StepStatus::Failed.as_ref() {
            if flow[name].continue_on_failure {
                carried_failed_tolerated.push(name.clone());
            } else {
                carried_failed.push(name.clone());
            }
        }
        carried.push(seed);
    }

    Ok(RestartPlan {
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
            action: "noop".into(),
            name: None,
            description: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            input: Default::default(),
            continue_on_failure: cof,
            continue_when_skipped: false,
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
}
