use std::collections::HashSet;
use stroem_common::dag;
use stroem_common::models::workflow::WorkflowConfig;
use stroem_common::template;
use stroem_common::validation;

#[test]
fn test_full_workflow_ci_pipeline() {
    // This is the CI pipeline example from the docs
    let yaml = r#"
actions:
  checkout:
    type: shell
    cmd: "git clone --depth=1 {{ input.repo }} /workspace/src"
    input:
      repo: { type: string, required: true }

  install:
    type: shell
    image: node:20-alpine
    cmd: "cd /workspace/src && npm ci"
    resources:
      cpu: "1"
      memory: "2Gi"

  test:
    type: shell
    image: node:20-alpine
    cmd: "cd /workspace/src && npm test"
    resources:
      cpu: "2"
      memory: "4Gi"

  build-image:
    type: docker
    image: gcr.io/kaniko-project/executor:latest
    command: ["--context=/workspace/src", "--destination=company/app:{{ input.tag }}"]

  deploy:
    type: shell
    image: bitnami/kubectl:1.30
    cmd: "kubectl set image deployment/app app=company/app:{{ input.tag }}"
    input:
      tag: { type: string, required: true }

tasks:
  ci-pipeline:
    mode: distributed
    input:
      repo: { type: string }
      tag: { type: string }
    flow:
      checkout:
        action: checkout
        input:
          repo: "{{ input.repo }}"
      install:
        action: install
        depends_on: [checkout]
      test:
        action: test
        depends_on: [install]
      build:
        action: build-image
        depends_on: [test]
        input:
          tag: "{{ input.tag }}"
      deploy:
        action: deploy
        depends_on: [build]
        input:
          tag: "{{ input.tag }}"
"#;

    // Parse the workflow
    let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();

    // Validate the workflow
    let warnings = validation::validate_workflow_config(&config).unwrap();
    assert_eq!(warnings.len(), 0, "Expected no warnings");

    // Check that all actions are present
    assert_eq!(config.actions.len(), 5);
    assert!(config.actions.contains_key("checkout"));
    assert!(config.actions.contains_key("install"));
    assert!(config.actions.contains_key("test"));
    assert!(config.actions.contains_key("build-image"));
    assert!(config.actions.contains_key("deploy"));

    // Check task
    let task = config.tasks.get("ci-pipeline").unwrap();
    assert_eq!(task.mode, "distributed");
    assert_eq!(task.flow.len(), 5);

    // Validate DAG
    let topo_order = dag::validate_dag(&task.flow).unwrap();
    assert_eq!(topo_order.len(), 5);

    // Check that checkout comes first
    let checkout_pos = topo_order.iter().position(|s| s == "checkout").unwrap();
    let install_pos = topo_order.iter().position(|s| s == "install").unwrap();
    let test_pos = topo_order.iter().position(|s| s == "test").unwrap();
    let build_pos = topo_order.iter().position(|s| s == "build").unwrap();
    let deploy_pos = topo_order.iter().position(|s| s == "deploy").unwrap();

    assert!(checkout_pos < install_pos);
    assert!(install_pos < test_pos);
    assert!(test_pos < build_pos);
    assert!(build_pos < deploy_pos);

    // Test ready_steps
    let mut completed = HashSet::new();

    // Initially only checkout is ready
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"checkout".to_string()));

    // After checkout, install is ready
    completed.insert("checkout".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"install".to_string()));

    // After install, test is ready
    completed.insert("install".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"test".to_string()));

    // After test, build is ready
    completed.insert("test".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"build".to_string()));

    // After build, deploy is ready
    completed.insert("build".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"deploy".to_string()));

    // After all complete, nothing is ready
    completed.insert("deploy".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 0);
}

#[test]
fn test_template_rendering_with_workflow() {
    // Test template rendering with workflow-like context
    let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: greet
        input:
          name: "{{ input.user }}"
      step2:
        action: process
        input:
          message: "{{ step1.output.greeting }}"
          count: 42
"#;

    let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
    let task = config.tasks.get("test").unwrap();

    // Simulate rendering step1 input
    let context = serde_json::json!({
        "input": {
            "user": "Alice"
        }
    });

    let step1 = task.flow.get("step1").unwrap();
    let rendered = template::render_input_map(&step1.input, &context).unwrap();
    assert_eq!(rendered["name"], "Alice");

    // Simulate rendering step2 input with step1 output
    let context_with_output = serde_json::json!({
        "input": {
            "user": "Alice"
        },
        "step1": {
            "output": {
                "greeting": "Hello Alice"
            }
        }
    });

    let step2 = task.flow.get("step2").unwrap();
    let rendered = template::render_input_map(&step2.input, &context_with_output).unwrap();
    assert_eq!(rendered["message"], "Hello Alice");
    assert_eq!(rendered["count"], 42);
}

#[test]
fn test_backup_workflow() {
    // Test the backup workflow from docs
    let yaml = r#"
actions:
  backup:
    type: docker
    image: company/db-backup:v2.1
    command: ["--compress", "--output", "/data/backup.sql"]
    env:
      DB_HOST: "{{ input.db_host }}"
    resources:
      cpu: "500m"
      memory: "512Mi"
    input:
      db_host: { type: string, required: true }

  notify:
    type: shell
    cmd: "echo 'Backup completed for {{ input.db_host }}'"
    input:
      db_host: { type: string, required: true }

tasks:
  nightly-backup:
    mode: distributed
    input:
      db_host:
        type: string
        default: "db.internal"
    flow:
      run-backup:
        action: backup
        input:
          db_host: "{{ input.db_host }}"
      send-notification:
        action: notify
        depends_on: [run-backup]
        input:
          db_host: "{{ input.db_host }}"

triggers:
  nightly:
    type: scheduler
    cron: "0 0 2 * * *"
    task: nightly-backup
    enabled: true
"#;

    let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();

    // Validate
    let warnings = validation::validate_workflow_config(&config).unwrap();
    assert_eq!(warnings.len(), 0);

    // Check actions
    let backup = config.actions.get("backup").unwrap();
    assert_eq!(backup.action_type, "docker");
    assert_eq!(backup.image.as_ref().unwrap(), "company/db-backup:v2.1");
    assert!(backup.resources.is_some());

    let notify = config.actions.get("notify").unwrap();
    assert_eq!(notify.action_type, "shell");

    // Check task
    let task = config.tasks.get("nightly-backup").unwrap();
    assert_eq!(task.mode, "distributed");

    // Check trigger
    let trigger = config.triggers.get("nightly").unwrap();
    assert_eq!(trigger.trigger_type, "scheduler");
    assert_eq!(trigger.cron.as_ref().unwrap(), "0 0 2 * * *");
    assert!(trigger.enabled);

    // Validate DAG
    let topo = dag::validate_dag(&task.flow).unwrap();
    assert_eq!(topo.len(), 2);

    let backup_pos = topo.iter().position(|s| s == "run-backup").unwrap();
    let notify_pos = topo.iter().position(|s| s == "send-notification").unwrap();
    assert!(backup_pos < notify_pos);
}

#[test]
fn test_parallel_workflow() {
    let yaml = r#"
actions:
  test1:
    type: shell
    cmd: "echo test1"
  test2:
    type: shell
    cmd: "echo test2"
  merge:
    type: shell
    cmd: "echo merge"

tasks:
  parallel-test:
    flow:
      test-a:
        action: test1
      test-b:
        action: test2
      combine:
        action: merge
        depends_on: [test-a, test-b]
"#;

    let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
    let warnings = validation::validate_workflow_config(&config).unwrap();
    assert_eq!(warnings.len(), 0);

    let task = config.tasks.get("parallel-test").unwrap();

    // Test that both test-a and test-b can start in parallel
    let completed = HashSet::new();
    let mut ready = dag::ready_steps(&task.flow, &completed);
    ready.sort();
    assert_eq!(ready.len(), 2);
    assert!(ready.contains(&"test-a".to_string()));
    assert!(ready.contains(&"test-b".to_string()));

    // Only after both complete should combine be ready
    let mut completed = HashSet::new();
    completed.insert("test-a".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"test-b".to_string()));

    completed.insert("test-b".to_string());
    let ready = dag::ready_steps(&task.flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"combine".to_string()));
}

#[test]
fn test_dag_fan_out_fan_in() {
    use std::collections::HashMap;
    use stroem_common::models::workflow::FlowStep;

    // Diamond: step1 → step2, step1 → step3, step2+step3 → step4
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "a".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "a".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "a".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
        },
    );
    flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "a".to_string(),
            depends_on: vec!["step2".to_string(), "step3".to_string()],
            input: HashMap::new(),
        },
    );

    // Validate DAG
    let topo = dag::validate_dag(&flow).unwrap();
    assert_eq!(topo.len(), 4);
    let s1 = topo.iter().position(|s| s == "step1").unwrap();
    let s2 = topo.iter().position(|s| s == "step2").unwrap();
    let s3 = topo.iter().position(|s| s == "step3").unwrap();
    let s4 = topo.iter().position(|s| s == "step4").unwrap();
    assert!(s1 < s2);
    assert!(s1 < s3);
    assert!(s2 < s4);
    assert!(s3 < s4);

    // Stage 0: only step1 ready
    let mut completed = HashSet::new();
    let ready = dag::ready_steps(&flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"step1".to_string()));

    // Stage 1: step1 done → step2 and step3 ready (fan-out)
    completed.insert("step1".to_string());
    let mut ready = dag::ready_steps(&flow, &completed);
    ready.sort();
    assert_eq!(ready.len(), 2);
    assert_eq!(ready, vec!["step2", "step3"]);

    // Stage 2: step2 done, step3 not → step4 NOT ready
    completed.insert("step2".to_string());
    let ready = dag::ready_steps(&flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"step3".to_string()));

    // Stage 3: step3 done → step4 ready (fan-in)
    completed.insert("step3".to_string());
    let ready = dag::ready_steps(&flow, &completed);
    assert_eq!(ready.len(), 1);
    assert!(ready.contains(&"step4".to_string()));

    // Stage 4: all done
    completed.insert("step4".to_string());
    let ready = dag::ready_steps(&flow, &completed);
    assert_eq!(ready.len(), 0);
}

#[test]
fn test_template_missing_step_output_errors() {
    use std::collections::HashMap;

    // Template references non-existent step output
    let mut input_map = HashMap::new();
    input_map.insert(
        "msg".to_string(),
        serde_json::json!("{{ nonexistent_step.output.value }}"),
    );

    let context = serde_json::json!({
        "input": {"name": "test"}
    });

    let result = template::render_input_map(&input_map, &context);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("msg"), "Error should reference the key name");
}
