use crate::models::workflow::FlowStep;
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet, VecDeque};

/// Returns step names whose dependencies are all in `completed` and that are not themselves completed
pub fn ready_steps(flow: &HashMap<String, FlowStep>, completed: &HashSet<String>) -> Vec<String> {
    flow.iter()
        .filter_map(|(step_name, step)| {
            // Skip if already completed
            if completed.contains(step_name) {
                return None;
            }

            // Check if all dependencies are completed
            let all_deps_met = step.depends_on.iter().all(|dep| completed.contains(dep));

            if all_deps_met {
                Some(step_name.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Validates the DAG and returns a topological ordering of step names
/// Returns an error if there are cycles or invalid dependencies
pub fn validate_dag(flow: &HashMap<String, FlowStep>) -> Result<Vec<String>> {
    // Build adjacency list and in-degree map
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut adj_list: HashMap<String, Vec<String>> = HashMap::new();

    // Initialize all steps
    for step_name in flow.keys() {
        in_degree.insert(step_name.clone(), 0);
        adj_list.insert(step_name.clone(), Vec::new());
    }

    // Build graph
    for (step_name, step) in flow {
        for dep in &step.depends_on {
            // Check if dependency exists
            if !flow.contains_key(dep) {
                bail!(
                    "Step '{}' depends on non-existent step '{}'",
                    step_name,
                    dep
                );
            }

            // Add edge from dep -> step_name
            adj_list.get_mut(dep).unwrap().push(step_name.clone());
            *in_degree.get_mut(step_name).unwrap() += 1;
        }
    }

    // Kahn's algorithm for topological sort
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut result: Vec<String> = Vec::new();

    // Start with nodes that have no dependencies
    for (step_name, &degree) in &in_degree {
        if degree == 0 {
            queue.push_back(step_name.clone());
        }
    }

    while let Some(step_name) = queue.pop_front() {
        result.push(step_name.clone());

        // Process all dependents
        if let Some(dependents) = adj_list.get(&step_name) {
            for dependent in dependents {
                let degree = in_degree.get_mut(dependent).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(dependent.clone());
                }
            }
        }
    }

    // If we didn't process all nodes, there's a cycle
    if result.len() != flow.len() {
        bail!("Cycle detected in workflow DAG");
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_step(action: &str, depends_on: Vec<&str>) -> FlowStep {
        FlowStep {
            action: action.to_string(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        }
    }

    #[test]
    fn test_ready_steps_empty_flow() {
        let flow: HashMap<String, FlowStep> = HashMap::new();
        let completed: HashSet<String> = HashSet::new();
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 0);
    }

    #[test]
    fn test_ready_steps_single_step() {
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("action1", vec![]));

        let completed = HashSet::new();
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&"step1".to_string()));
    }

    #[test]
    fn test_ready_steps_linear_chain() {
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("action1", vec![]));
        flow.insert("step2".to_string(), make_step("action2", vec!["step1"]));
        flow.insert("step3".to_string(), make_step("action3", vec!["step2"]));

        // Initially only step1 is ready
        let completed = HashSet::new();
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&"step1".to_string()));

        // After step1 completes, step2 is ready
        let mut completed = HashSet::new();
        completed.insert("step1".to_string());
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&"step2".to_string()));

        // After step2 completes, step3 is ready
        completed.insert("step2".to_string());
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&"step3".to_string()));

        // After all complete, nothing is ready
        completed.insert("step3".to_string());
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 0);
    }

    #[test]
    fn test_ready_steps_parallel() {
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("action1", vec![]));
        flow.insert("step2".to_string(), make_step("action2", vec![]));
        flow.insert(
            "step3".to_string(),
            make_step("action3", vec!["step1", "step2"]),
        );

        // Initially step1 and step2 are ready
        let completed = HashSet::new();
        let mut ready = ready_steps(&flow, &completed);
        ready.sort();
        assert_eq!(ready.len(), 2);
        assert!(ready.contains(&"step1".to_string()));
        assert!(ready.contains(&"step2".to_string()));

        // After step1 completes, step2 is still ready but step3 is not
        let mut completed = HashSet::new();
        completed.insert("step1".to_string());
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&"step2".to_string()));

        // After both complete, step3 is ready
        completed.insert("step2".to_string());
        let ready = ready_steps(&flow, &completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&"step3".to_string()));
    }

    #[test]
    fn test_validate_dag_empty() {
        let flow: HashMap<String, FlowStep> = HashMap::new();
        let result = validate_dag(&flow).unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_validate_dag_single_step() {
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("action1", vec![]));

        let result = validate_dag(&flow).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "step1");
    }

    #[test]
    fn test_validate_dag_linear_chain() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("action1", vec![]));
        flow.insert("b".to_string(), make_step("action2", vec!["a"]));
        flow.insert("c".to_string(), make_step("action3", vec!["b"]));

        let result = validate_dag(&flow).unwrap();
        assert_eq!(result.len(), 3);
        // Should be in topological order: a, b, c
        let a_pos = result.iter().position(|s| s == "a").unwrap();
        let b_pos = result.iter().position(|s| s == "b").unwrap();
        let c_pos = result.iter().position(|s| s == "c").unwrap();
        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);
    }

    #[test]
    fn test_validate_dag_parallel() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("action1", vec![]));
        flow.insert("b".to_string(), make_step("action2", vec![]));
        flow.insert("c".to_string(), make_step("action3", vec!["a", "b"]));

        let result = validate_dag(&flow).unwrap();
        assert_eq!(result.len(), 3);
        // a and b should come before c
        let a_pos = result.iter().position(|s| s == "a").unwrap();
        let b_pos = result.iter().position(|s| s == "b").unwrap();
        let c_pos = result.iter().position(|s| s == "c").unwrap();
        assert!(a_pos < c_pos);
        assert!(b_pos < c_pos);
    }

    #[test]
    fn test_validate_dag_cycle_simple() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("action1", vec!["b"]));
        flow.insert("b".to_string(), make_step("action2", vec!["a"]));

        let result = validate_dag(&flow);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cycle detected"));
    }

    #[test]
    fn test_validate_dag_cycle_complex() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("action1", vec![]));
        flow.insert("b".to_string(), make_step("action2", vec!["a"]));
        flow.insert("c".to_string(), make_step("action3", vec!["b"]));
        flow.insert("d".to_string(), make_step("action4", vec!["c", "a"]));
        // Add cycle: a -> b -> c -> a
        flow.insert("e".to_string(), make_step("action5", vec!["c"]));
        flow.get_mut("a").unwrap().depends_on.push("e".to_string());

        let result = validate_dag(&flow);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cycle detected"));
    }

    #[test]
    fn test_validate_dag_self_reference() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("action1", vec!["a"]));

        let result = validate_dag(&flow);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cycle detected"));
    }

    #[test]
    fn test_validate_dag_missing_dependency() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("action1", vec![]));
        flow.insert("b".to_string(), make_step("action2", vec!["nonexistent"]));

        let result = validate_dag(&flow);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("non-existent step"));
    }

    #[test]
    fn test_validate_dag_complex_valid() {
        let mut flow = HashMap::new();
        flow.insert("checkout".to_string(), make_step("git-clone", vec![]));
        flow.insert(
            "install".to_string(),
            make_step("npm-install", vec!["checkout"]),
        );
        flow.insert("test".to_string(), make_step("npm-test", vec!["install"]));
        flow.insert("build".to_string(), make_step("npm-build", vec!["install"]));
        flow.insert(
            "deploy".to_string(),
            make_step("kubectl-apply", vec!["test", "build"]),
        );

        let result = validate_dag(&flow).unwrap();
        assert_eq!(result.len(), 5);

        // Verify ordering constraints
        let checkout_pos = result.iter().position(|s| s == "checkout").unwrap();
        let install_pos = result.iter().position(|s| s == "install").unwrap();
        let test_pos = result.iter().position(|s| s == "test").unwrap();
        let build_pos = result.iter().position(|s| s == "build").unwrap();
        let deploy_pos = result.iter().position(|s| s == "deploy").unwrap();

        assert!(checkout_pos < install_pos);
        assert!(install_pos < test_pos);
        assert!(install_pos < build_pos);
        assert!(test_pos < deploy_pos);
        assert!(build_pos < deploy_pos);
    }
}
