use super::*;
use crate::duration::HumanDuration;
use crate::models::workflow::{
    AgentToolRef, BackoffStrategy, McpServerDef, RetryConfig, TaskDef, TriggerDef,
};
use std::collections::HashMap;

fn retry_config(max_attempts: u32) -> RetryConfig {
    RetryConfig {
        max_attempts,
        delay: HumanDuration(30),
        backoff: BackoffStrategy::Fixed,
        jitter: false,
    }
}

/// `max_attempts` counts total executions. `1` (no retry) is the minimum
/// allowed; `0` is rejected since it would mean "never run".
#[test]
fn test_validate_retry_config_max_attempts_boundaries() {
    assert!(validate_retry_config(&retry_config(1), "Test").is_ok());
    assert!(validate_retry_config(&retry_config(10), "Test").is_ok());

    let err = validate_retry_config(&retry_config(0), "Test").unwrap_err();
    assert!(err.to_string().contains("out of range"), "got: {err}");

    let err = validate_retry_config(&retry_config(11), "Test").unwrap_err();
    assert!(err.to_string().contains("out of range"), "got: {err}");
}

#[test]
fn test_check_connection_values_required_unknown_empty() {
    use crate::models::workflow::{ConnectionPropertyDef, ConnectionTypeDef};
    let type_def = ConnectionTypeDef {
        properties: HashMap::from([(
            "host".to_string(),
            ConnectionPropertyDef {
                property_type: "string".into(),
                required: true,
                default: None,
                secret: false,
            },
        )]),
    };
    // missing required
    let err =
        check_connection_values("Connection 'c'", &HashMap::new(), "t", &type_def).unwrap_err();
    assert!(err.to_string().contains("missing required field 'host'"));
    // unknown field → warning, not error
    let vals = HashMap::from([
        ("host".to_string(), serde_json::json!("h")),
        ("extra".to_string(), serde_json::json!(1)),
    ]);
    let warnings = check_connection_values("Connection 'c'", &vals, "t", &type_def).unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("'extra'"));
    // empty string → error
    let vals = HashMap::from([("host".to_string(), serde_json::json!(""))]);
    let err = check_connection_values("Connection 'c'", &vals, "t", &type_def).unwrap_err();
    assert!(err.to_string().contains("empty value"));
}

#[test]
fn test_validate_valid_workflow() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo Hello"
  build:
    type: docker
    image: node:20

tasks:
  test:
    flow:
      step1:
        action: greet
      step2:
        action: build
        depends_on: [step1]

triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * * *"
    task: test
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_action_shell_missing_cmd_and_script() {
    let yaml = r#"
actions:
  bad:
    type: script
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("missing 'script' or 'source'"));
}

#[test]
fn test_validate_action_shell_both_cmd_and_script() {
    let yaml = r#"
actions:
  bad:
    type: script
    cmd: "echo test"
    script: "test.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("not valid on type: script"));
}

#[test]
fn test_validate_action_script_with_script_field() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo Hello"

tasks:
  test:
    flow:
      step1:
        action: greet
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    // No deprecation warning when using 'script' field
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_validate_action_script_with_source_field() {
    let yaml = r#"
actions:
  deploy:
    type: script
    source: "deploy.sh"

tasks:
  test:
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_validate_action_script_cmd_rejected() {
    let yaml = r#"
actions:
  greet:
    type: script
    cmd: "echo Hello"

tasks:
  test:
    flow:
      step1:
        action: greet
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("not valid on type: script"));
}

#[test]
fn test_validate_action_script_both_script_and_source() {
    let yaml = r#"
actions:
  bad:
    type: script
    script: "echo hello"
    source: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("both inline script and source file"));
}

#[test]
fn test_validate_docker_rejects_script_field() {
    let yaml = r#"
actions:
  bad:
    type: docker
    image: node:20
    script: "echo hello"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("has 'script' field"));
}

#[test]
fn test_validate_docker_rejects_source_field() {
    let yaml = r#"
actions:
  bad:
    type: docker
    image: node:20
    source: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("has 'source' field"));
}

#[test]
fn test_validate_pod_rejects_source_field() {
    let yaml = r#"
actions:
  bad:
    type: pod
    image: node:20
    source: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("has 'source' field"));
}

#[test]
fn test_validate_task_rejects_source_field() {
    let yaml = r#"
actions:
  bad:
    type: task
    task: other-task
    source: "deploy.sh"

tasks:
  other-task:
    flow:
      s1:
        action: bad
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("has 'source' field"));
}

#[test]
fn test_validate_action_docker_missing_image() {
    let yaml = r#"
actions:
  bad:
    type: docker
    command: ["echo", "test"]
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("missing 'image'"));
}

#[test]
fn test_validate_action_pod_missing_image() {
    let yaml = r#"
actions:
  bad:
    type: pod
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("missing 'image'"));
}

#[test]
fn test_validate_action_invalid_type() {
    let yaml = r#"
actions:
  bad:
    type: invalid
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid type"));
}

#[test]
fn test_validate_task_missing_action() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo test"

tasks:
  test:
    flow:
      step1:
        action: nonexistent
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("non-existent action"));
}

#[test]
fn test_validate_task_missing_dependency() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo test"

tasks:
  test:
    flow:
      step1:
        action: greet
        depends_on: [nonexistent]
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("non-existent step"));
}

#[test]
fn test_validate_task_cycle() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo test"

tasks:
  test:
    flow:
      step1:
        action: greet
        depends_on: [step2]
      step2:
        action: greet
        depends_on: [step1]
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Cycle detected"));
}

#[test]
fn test_legacy_continue_when_skipped_produces_a_named_migration_error() {
    let yaml = r#"
actions:
  noop:
    type: script
    script: "echo noop"
tasks:
  t:
    flow:
      a: { action: noop }
      b: { action: noop, depends_on: [a], continue_when_skipped: true }
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("continue_when_skipped") && msg.contains("0.18"),
        "expected a named migration error, got: {msg}"
    );
}

#[test]
fn test_duplicate_sibling_dependency_is_rejected() {
    let yaml = r#"
actions:
  noop:
    type: script
    script: "echo noop"
tasks:
  t:
    flow:
      a: { action: noop }
      m: { action: noop, depends_on: [a, a] }
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("duplicate") && msg.contains('a'), "{msg}");
}

#[test]
fn test_duplicate_outcome_in_accept_list_is_rejected() {
    // Spec §7: `Outcomes` is "validated non-empty, no duplicates" —
    // a duplicate entry is harmless at runtime (AcceptSet::contains
    // doesn't care) but is almost certainly a typo, same reasoning as
    // the sibling-duplicate check above.
    let yaml = r#"
actions:
  noop:
    type: script
    script: "echo noop"
tasks:
  t:
    flow:
      a: { action: noop }
      m: { action: noop, depends_on: [{ step: a, accept: [completed, failed, completed] }] }
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("duplicate") && msg.contains("accept"), "{msg}");
}

#[test]
fn test_cross_branch_duplicate_reference_is_accepted() {
    let yaml = r#"
actions:
  noop:
    type: script
    script: "echo noop"
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
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected no errors, got: {result:?}");
}

#[test]
fn test_validate_trigger_missing_task() {
    let yaml = r#"
triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * * *"
    task: nonexistent
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("non-existent task"));
}

#[test]
fn test_validate_trigger_invalid_cron() {
    let mut config = WorkspaceConfig::default();
    config.tasks.insert(
        "test".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );
    config.triggers.insert(
        "bad-cron".to_string(),
        TriggerDef::Scheduler {
            git_ref: None,
            cron: "not a cron".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: None,
            force_refresh: false,
        },
    );

    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid cron expression"));
}

#[test]
fn test_validate_trigger_valid_cron_6field() {
    let mut config = WorkspaceConfig::default();
    config.tasks.insert(
        "test".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );
    config.triggers.insert(
        "every-10s".to_string(),
        TriggerDef::Scheduler {
            git_ref: None,
            cron: "*/10 * * * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: None,
            force_refresh: false,
        },
    );

    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_trigger_valid_timezone() {
    let mut config = WorkspaceConfig::default();
    config.tasks.insert(
        "test".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );
    config.triggers.insert(
        "nightly".to_string(),
        TriggerDef::Scheduler {
            git_ref: None,
            cron: "0 2 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("Europe/Copenhagen".to_string()),
            force_refresh: false,
        },
    );

    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_trigger_utc_timezone() {
    let mut config = WorkspaceConfig::default();
    config.tasks.insert(
        "test".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );
    config.triggers.insert(
        "nightly".to_string(),
        TriggerDef::Scheduler {
            git_ref: None,
            cron: "0 2 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("UTC".to_string()),
            force_refresh: false,
        },
    );

    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_trigger_invalid_timezone() {
    let mut config = WorkspaceConfig::default();
    config.tasks.insert(
        "test".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );
    config.triggers.insert(
        "nightly".to_string(),
        TriggerDef::Scheduler {
            git_ref: None,
            cron: "0 2 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("Not/A/Timezone".to_string()),
            force_refresh: false,
        },
    );

    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid timezone"));
}

#[test]
fn test_validate_trigger_empty_string_timezone() {
    // Empty string is Some("") which should fail timezone parsing
    let mut config = WorkspaceConfig::default();
    config.tasks.insert(
        "test".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );
    config.triggers.insert(
        "nightly".to_string(),
        TriggerDef::Scheduler {
            git_ref: None,
            cron: "0 2 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("".to_string()),
            force_refresh: false,
        },
    );

    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid timezone"));
}

#[test]
fn test_validate_trigger_scheduler_missing_cron() {
    // With the tagged enum, `cron` is a required field on the Scheduler variant.
    // Missing cron now causes a serde parse error rather than a validation error.
    let yaml = r#"
triggers:
  bad:
    type: scheduler
    task: test
"#;
    let result: Result<crate::models::workflow::WorkspaceConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cron"),
        "Error should mention missing cron field: {err}"
    );
}

#[test]
fn test_validate_library_action_warning() {
    let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: common.slack-notify
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    let warnings = result.unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("library action"));
    assert!(warnings[0].contains("common.slack-notify"));
}

#[test]
fn test_validate_shell_with_script() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: actions/deploy.sh
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_shell_with_image_rejected() {
    let yaml = r#"
actions:
  build:
    type: script
    image: node:20
    script: "npm run build"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("has 'image' field"));
}

#[test]
fn test_validate_complex_workflow() {
    let yaml = r#"
actions:
  checkout:
    type: script
    script: "git clone repo"
  build:
    type: script
    runner: docker
    script: "npm run build"
  deploy:
    type: docker
    image: kubectl:latest
    command: ["apply", "-f", "deployment.yaml"]

tasks:
  ci-pipeline:
    flow:
      checkout:
        action: checkout
      build:
        action: build
        depends_on: [checkout]
      deploy:
        action: deploy
        depends_on: [build]

triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_secrets_valid() {
    let yaml = r#"
secrets:
  db_password: "ref+awsssm:///prod/db/password"
  api_key_2: "ref+vault://secret/data/api#key"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_secrets_nested_valid() {
    let yaml = r#"
secrets:
  db:
    password: "ref+sops://secrets.enc.yaml#/db/password"
    host: "ref+sops://secrets.enc.yaml#/db/host"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_secrets_invalid_name() {
    let yaml = r#"
secrets:
  "db-password": "ref+awsssm:///prod/db/password"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid characters"));
}

#[test]
fn test_validate_secrets_nested_invalid_name() {
    let yaml = r#"
secrets:
  db:
    "my-password": "ref+sops://x"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid characters"));
}

#[test]
fn test_validate_secrets_empty_value() {
    let yaml = r#"
secrets:
  db_password: ""
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("empty value"));
}

#[test]
fn test_validate_secrets_nested_empty_value() {
    let yaml = r#"
secrets:
  db:
    password: ""
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("empty value"));
}

#[test]
fn test_validate_secret_reference_warning() {
    let yaml = r#"
secrets:
  db_password: "ref+awsssm:///prod/db/password"

actions:
  backup:
    type: script
    script: "pg_dump"
    env:
      DB_PASSWORD: "{{ secret.db_password }}"
      API_KEY: "{{ secret.missing_key }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    let warnings = result.unwrap();
    // Should warn about missing_key but not db_password
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("missing_key"));
    assert!(warnings[0].contains("not defined in secrets"));
}

#[test]
fn test_validate_secret_nested_reference_warning() {
    let yaml = r#"
secrets:
  db:
    password: "ref+sops://secrets.enc.yaml#/db/password"

actions:
  backup:
    type: script
    script: "pg_dump"
    env:
      DB_PASSWORD: "{{ secret.db.password }}"
      DB_HOST: "{{ secret.db.host }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    let warnings = result.unwrap();
    // Should warn about db.host but not db.password
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("db.host"));
    assert!(warnings[0].contains("not defined in secrets"));
}

#[test]
fn test_validate_shell_with_runner() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: docker
    script: "npm test"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_shell_with_invalid_runner() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: invalid
    script: "npm test"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid runner"));
}

#[test]
fn test_validate_shell_with_entrypoint_rejected() {
    let yaml = r#"
actions:
  test:
    type: script
    script: "echo hi"
    entrypoint: ["/bin/sh"]
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("entrypoint"));
}

#[test]
fn test_validate_docker_with_runner_rejected() {
    let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    runner: local
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("runner"));
}

#[test]
fn test_validate_docker_with_script_rejected() {
    let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    script: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("script"));
}

#[test]
fn test_validate_docker_no_cmd_ok() {
    // docker action with image only (uses image default entrypoint)
    let yaml = r#"
actions:
  migrate:
    type: docker
    image: company/db-migrations:v3
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_docker_with_entrypoint() {
    let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    entrypoint: ["/bin/sh", "-c"]
    cmd: "echo hello"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_shell_with_tags() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: docker
    tags: ["node-20"]
    script: "npm test"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_compute_required_ability_script_local() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str("type: script\nscript: \"echo test\"\n").unwrap();
    assert_eq!(compute_required_ability(&action), "script");
    assert!(compute_required_tags(&action).is_empty());
}

#[test]
fn test_compute_required_ability_script_docker() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef =
        serde_yaml::from_str("type: script\nrunner: docker\nscript: \"npm test\"\n").unwrap();
    assert_eq!(compute_required_ability(&action), "docker");
    assert!(compute_required_tags(&action).is_empty());
}

#[test]
fn test_compute_required_ability_script_docker_with_tags() {
    use crate::models::workflow::ActionDef;
    // Post-041: the ability token is NOT prepended to tags anymore.
    // Only the user-declared "node-20" is a tag; "docker" is the
    // ability, and lives on the separate axis.
    let action: ActionDef = serde_yaml::from_str(
        "type: script\nrunner: docker\ntags: [\"node-20\"]\nscript: \"npm test\"\n",
    )
    .unwrap();
    assert_eq!(compute_required_ability(&action), "docker");
    assert_eq!(compute_required_tags(&action), vec!["node-20"]);
}

#[test]
fn test_compute_required_ability_docker() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str("type: docker\nimage: alpine:latest\n").unwrap();
    assert_eq!(compute_required_ability(&action), "docker");
    assert!(compute_required_tags(&action).is_empty());
}

#[test]
fn test_compute_required_ability_pod() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str("type: pod\nimage: alpine:latest\n").unwrap();
    assert_eq!(compute_required_ability(&action), "kubernetes");
    assert!(compute_required_tags(&action).is_empty());
}

#[test]
fn test_compute_required_ability_pod_with_tags() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef =
        serde_yaml::from_str("type: pod\nimage: alpine:latest\ntags: [\"gpu\"]\n").unwrap();
    assert_eq!(compute_required_ability(&action), "kubernetes");
    assert_eq!(compute_required_tags(&action), vec!["gpu"]);
}

#[test]
fn test_compute_required_ability_script_pod() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef =
        serde_yaml::from_str("type: script\nrunner: pod\nscript: \"npm test\"\n").unwrap();
    assert_eq!(compute_required_ability(&action), "kubernetes");
}

#[test]
fn test_compute_required_tags_dedup() {
    use crate::models::workflow::ActionDef;
    // Duplicate tags in the action should be deduped (order preserved).
    let action: ActionDef = serde_yaml::from_str(
        "type: docker\nimage: alpine:latest\ntags: [\"gpu\", \"gpu\", \"batch-runner\"]\n",
    )
    .unwrap();
    assert_eq!(compute_required_tags(&action), vec!["gpu", "batch-runner"]);
}

#[test]
fn test_validate_pod_with_runner_rejected() {
    let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    runner: local
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("runner"));
}

#[test]
fn test_validate_pod_with_script_rejected() {
    let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    script: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("script"));
}

#[test]
fn test_validate_shell_runner_pod() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: pod
    script: "npm test"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_shell_runner_local() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: local
    script: "echo test"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_derive_runner_shell_default() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: script
script: "echo test"
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "local");
}

#[test]
fn test_derive_runner_shell_docker() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: script
runner: docker
script: "npm test"
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "docker");
}

#[test]
fn test_derive_runner_shell_pod() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: script
runner: pod
script: "npm test"
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "pod");
}

#[test]
fn test_derive_runner_docker() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: docker
image: alpine:latest
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "none");
}

#[test]
fn test_derive_runner_pod() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: pod
image: alpine:latest
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "none");
}

#[test]
fn test_validate_hook_valid_action() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "make deploy"
  notify:
    type: script
    script: "curl $WEBHOOK"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: notify
        input:
          message: "Deploy succeeded"
    on_error:
      - action: notify
        input:
          message: "Deploy FAILED"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_hook_missing_action() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "make deploy"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: nonexistent
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("on_success[0]"));
    assert!(err.contains("nonexistent"));
}

#[test]
fn test_validate_hook_library_action() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "make deploy"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: common.slack-notify
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_task_action_valid() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
  run-cleanup:
    type: task
    task: cleanup

tasks:
  cleanup:
    flow:
      step1:
        action: greet
  deploy:
    flow:
      build:
        action: greet
      cleanup:
        action: run-cleanup
        depends_on: [build]
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_task_action_missing_task_field() {
    let yaml = r#"
actions:
  bad:
    type: task
tasks:
  t:
    flow:
      s:
        action: bad
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("missing 'task'"));
}

#[test]
fn test_validate_task_action_with_cmd_rejected() {
    let yaml = r#"
actions:
  bad:
    type: task
    task: some-task
    cmd: "echo nope"
tasks:
  some-task:
    flow:
      s:
        action: bad
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'cmd'"));
}

#[test]
fn test_validate_task_action_with_image_rejected() {
    let yaml = r#"
actions:
  bad:
    type: task
    task: some-task
    image: alpine:latest
tasks:
  some-task:
    flow:
      s:
        action: bad
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'image'"));
}

#[test]
fn test_validate_task_action_nonexistent_task() {
    let yaml = r#"
actions:
  run-missing:
    type: task
    task: nonexistent
tasks:
  deploy:
    flow:
      s:
        action: run-missing
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("non-existent task"));
}

#[test]
fn test_validate_task_action_self_reference() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
  run-self:
    type: task
    task: loopy
tasks:
  loopy:
    flow:
      step1:
        action: greet
      step2:
        action: run-self
        depends_on: [step1]
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("self-reference"));
}

#[test]
fn test_validate_task_action_self_reference_in_hook() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
  run-self:
    type: task
    task: loopy
tasks:
  loopy:
    flow:
      step1:
        action: greet
    on_error:
      - action: run-self
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("self-reference"));
}

#[test]
fn test_validate_task_action_self_reference_in_on_cancel_hook() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
  run-self:
    type: task
    task: loopy
tasks:
  loopy:
    flow:
      step1:
        action: greet
    on_cancel:
      - action: run-self
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("self-reference"));
}

#[test]
fn test_compute_required_tags_task() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: task
task: other-task
"#,
    )
    .unwrap();
    assert_eq!(compute_required_tags(&action), Vec::<String>::new());
}

#[test]
fn test_derive_runner_task() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: task
task: other-task
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "none");
}

// --- manifest validation tests ---

#[test]
fn test_manifest_rejected_on_shell_local() {
    let yaml = r#"
actions:
  test:
    type: script
    script: "echo hi"
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("manifest"));
}

#[test]
fn test_manifest_rejected_on_shell_docker() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: docker
    script: "echo hi"
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("manifest"));
}

#[test]
fn test_manifest_rejected_on_docker() {
    let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("manifest"));
}

#[test]
fn test_manifest_rejected_on_task() {
    let yaml = r#"
actions:
  test:
    type: task
    task: other
    manifest:
      spec:
        serviceAccountName: my-sa
tasks:
  other:
    flow:
      s:
        action: test
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("manifest"));
}

#[test]
fn test_manifest_accepted_on_pod_type() {
    let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_manifest_accepted_on_shell_runner_pod() {
    let yaml = r#"
actions:
  test:
    type: script
    runner: pod
    script: "echo hi"
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_manifest_must_be_object() {
    let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    manifest: "not an object"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not an object"));
}

// --- webhook trigger validation tests ---

#[test]
fn test_webhook_trigger_requires_name() {
    // Webhook without `name` → serde parse error
    let yaml = r#"
triggers:
  on-push:
    type: webhook
    task: ci-pipeline
"#;
    let result: Result<WorkspaceConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("name"),
        "Error should mention missing name field: {err}"
    );
}

#[test]
fn test_webhook_trigger_name_url_safe() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: "invalid name with spaces!"
    task: ci
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid webhook name"));
}

#[test]
fn test_webhook_trigger_valid() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci
    secret: "whsec_abc123"
    input:
      environment: staging
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_webhook_trigger_no_secret_ok() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    let trigger = config.triggers.get("on-push").unwrap();
    match trigger {
        TriggerDef::Webhook { secret, .. } => assert!(secret.is_none()),
        _ => panic!("Expected Webhook variant"),
    }
}

#[test]
fn test_webhook_trigger_accessors() {
    let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    input:
      env: staging
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let trigger = config.triggers.get("on-push").unwrap();
    assert_eq!(trigger.trigger_type_str(), "webhook");
    assert_eq!(trigger.task(), "ci-pipeline");
    assert!(trigger.enabled());
    assert_eq!(trigger.input().get("env").unwrap(), "staging");
}

#[test]
fn test_webhook_trigger_disabled() {
    let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    enabled: false
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let trigger = config.triggers.get("on-push").unwrap();
    assert!(!trigger.enabled());
}

#[test]
fn test_webhook_trigger_sync_mode() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: sync
    timeout_secs: 60
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    let trigger = config.triggers.get("on-push").unwrap();
    match trigger {
        TriggerDef::Webhook {
            mode, timeout_secs, ..
        } => {
            assert_eq!(mode.as_deref(), Some("sync"));
            assert_eq!(*timeout_secs, Some(60));
        }
        _ => panic!("Expected Webhook variant"),
    }
}

#[test]
fn test_webhook_trigger_async_mode() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: async
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_webhook_trigger_invalid_mode() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: blocking
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("invalid mode"));
    assert!(err.contains("blocking"));
}

#[test]
fn test_webhook_trigger_timeout_zero() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: sync
    timeout_secs: 0
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("timeout_secs"));
}

#[test]
fn test_webhook_trigger_timeout_too_large() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: sync
    timeout_secs: 500
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("timeout_secs"));
}

#[test]
fn test_webhook_trigger_no_mode_defaults_none() {
    let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let trigger = config.triggers.get("on-push").unwrap();
    match trigger {
        TriggerDef::Webhook {
            mode, timeout_secs, ..
        } => {
            assert!(mode.is_none());
            assert!(timeout_secs.is_none());
        }
        _ => panic!("Expected Webhook variant"),
    }
}

// --- workspace-level hook validation tests ---

#[test]
fn test_validate_workspace_hook_valid_action() {
    let yaml = r#"
actions:
  notify:
    type: script
    script: "curl $WEBHOOK"

on_success:
  - action: notify
    input:
      message: "All good"
on_error:
  - action: notify
    input:
      message: "Something failed"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_workspace_hook_missing_action() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"

on_success:
  - action: nonexistent
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Workspace on_success[0]"));
    assert!(err.contains("nonexistent"));
}

#[test]
fn test_validate_workspace_hook_on_error_missing_action() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"

on_error:
  - action: missing-action
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Workspace on_error[0]"));
    assert!(err.contains("missing-action"));
}

#[test]
fn test_validate_workspace_hook_library_action() {
    let yaml = r#"
on_success:
  - action: common.slack-notify
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_task_on_cancel_hook_missing_action() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"

tasks:
  deploy:
    flow:
      step1:
        action: greet
    on_cancel:
      - action: nonexistent
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("on_cancel[0]"));
    assert!(err.contains("nonexistent"));
}

#[test]
fn test_validate_workspace_on_cancel_hook_missing_action() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"

on_cancel:
  - action: missing-action
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Workspace on_cancel[0]"));
    assert!(err.contains("missing-action"));
}

// --- connection validation tests ---

#[test]
fn test_connection_type_named_bool_is_rejected() {
    // A connection type whose name matches the `bool → boolean` alias
    // would silently lose its connection inputs: the input field's
    // `type: bool` is canonicalized to `boolean` and resolved as a
    // primitive, never reaching the connection registry. Reject at
    // validation time so authors get a clear error instead of
    // misbehavior at runtime.
    let yaml = r#"
connection_types:
  bool:
    host:
      type: string
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "connection type named `bool` must be rejected"
    );
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("reserved") && err_msg.contains("bool"),
        "error should mention the reservation reason: {err_msg}"
    );
}

#[test]
fn test_connection_type_named_primitive_is_rejected() {
    // Same protection for every other primitive name and the array/object
    // output-schema primitives. Spot-check a few — exhaustive coverage
    // would test 10 names and add no value.
    for reserved in ["boolean", "string", "integer", "datetime", "object"] {
        let yaml = format!("connection_types:\n  {reserved}:\n    host:\n      type: string\n");
        let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "connection type named `{reserved}` must be rejected"
        );
    }
}

#[test]
fn test_validate_connections_valid() {
    let yaml = r#"
connection_types:
  postgres:
    host:
      type: string
      required: true
    port:
      type: integer
      default: 5432

connections:
  prod_db:
    type: postgres
    host: "db.example.com"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_connections_invalid_property_type() {
    let yaml = r#"
connection_types:
  custom:
    data:
      type: array
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("invalid type"));
    assert!(err.contains("array"));
}

#[test]
fn test_validate_connections_missing_type() {
    let yaml = r#"
connections:
  prod_db:
    type: nonexistent
    host: "db.example.com"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("non-existent connection type"));
    assert!(err.contains("nonexistent"));
}

#[test]
fn test_validate_connections_missing_required_field() {
    let yaml = r#"
connection_types:
  postgres:
    host:
      type: string
      required: true
    port:
      type: integer
      default: 5432

connections:
  prod_db:
    type: postgres
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("missing required field"));
    assert!(err.contains("host"));
}

#[test]
fn test_validate_connections_unknown_field_warning() {
    let yaml = r#"
connection_types:
  postgres:
    host:
      type: string
      required: true

connections:
  prod_db:
    type: postgres
    host: "db.example.com"
    extra_field: "unknown"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
    let warnings = result.unwrap();
    assert!(warnings.iter().any(|w| w.contains("extra_field")));
}

#[test]
fn test_validate_connections_empty_value() {
    let yaml = r#"
connections:
  api:
    url: ""
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("empty value"));
}

#[test]
fn test_validate_connections_untyped_passthrough() {
    let yaml = r#"
connections:
  custom_api:
    url: "https://example.com"
    token: "abc123"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_connection_input_valid() {
    let yaml = r#"
connection_types:
  postgres:
    host:
      type: string
      required: true

actions:
  run-migration:
    type: script
    script: "migrate"

tasks:
  deploy:
    input:
      db:
        type: postgres
      env:
        type: string
    flow:
      migrate:
        action: run-migration
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_connection_input_unknown_type() {
    let yaml = r#"
actions:
  run-migration:
    type: script
    script: "migrate"

tasks:
  deploy:
    input:
      db:
        type: nonexistent_type
    flow:
      migrate:
        action: run-migration
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("nonexistent_type"));
    assert!(err.contains("not a primitive or connection type"));
}

#[test]
fn test_validate_connection_input_primitives_ok() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: "echo hello"

tasks:
  test:
    input:
      name:
        type: string
      count:
        type: integer
      ratio:
        type: number
      enabled:
        type: boolean
      start_date:
        type: date
      scheduled_at:
        type: datetime
    flow:
      step1:
        action: greet
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_text_input_type_ok() {
    let yaml = r#"
actions:
  run-query:
    type: script
    script: "echo hello"
    input:
      query:
        type: text

tasks:
  test:
    input:
      sql:
        type: text
    flow:
      step1:
        action: run-query
        input:
          query: "{{ input.sql }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok());
}

#[test]
fn test_validate_date_and_datetime_input_types_ok() {
    let yaml = r#"
actions:
  generate:
    type: script
    script: "echo report"
tasks:
  report:
    input:
      start_date:
        type: date
        default: "2026-01-01"
      scheduled_at:
        type: datetime
    flow:
      step1:
        action: generate
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "date/datetime should be valid primitive types: {:?}",
        result
    );
    let warnings = result.unwrap();
    assert_eq!(
        warnings.len(),
        0,
        "Expected no warnings, got: {:?}",
        warnings
    );
}

#[test]
fn test_secret_input_with_default_is_valid() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
    input:
      api_key:
        type: string
        secret: true
        default: "{{ secret.DEPLOY_KEY }}"

tasks:
  release:
    input:
      token:
        type: string
        secret: true
        default: "{{ secret.TOKEN }}"
    flow:
      step1:
        action: deploy
        input:
          api_key: "{{ input.token }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
}

#[test]
fn test_secret_input_without_default_is_valid() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
    input:
      password:
        type: string
        secret: true
        required: true

tasks:
  release:
    input:
      password:
        type: string
        secret: true
        required: true
    flow:
      step1:
        action: deploy
        input:
          password: "{{ input.password }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
}

#[test]
fn test_input_field_options_valid() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production]
        default: staging
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert_eq!(
        warnings.len(),
        0,
        "Expected no warnings, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_options_valid_action_level() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
    input:
      env:
        type: string
        options: [staging, production]
        default: staging
tasks:
  test:
    flow:
      step1:
        action: deploy
        input:
          env: "{{ input.env }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert_eq!(
        warnings.len(),
        0,
        "Expected no warnings for action-level options, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_empty_options_warning_action_level() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
    input:
      env:
        type: string
        options: []
tasks:
  test:
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Action") && w.contains("options list is empty")),
        "Expected action-level empty options warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_empty_options_warning() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: []
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("options list is empty")),
        "Expected empty options warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_allow_custom_without_options_warning() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        allow_custom: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("allow_custom has no effect")),
        "Expected allow_custom warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_default_not_in_options_warning() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production]
        default: development
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("not in options list")),
        "Expected default-not-in-options warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_default_not_in_options_ok_with_allow_custom() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production]
        default: development
        allow_custom: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert_eq!(
        warnings.len(),
        0,
        "Expected no warnings with allow_custom, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_ok() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        options: [dev, staging, prod]
        multiple: true
        default: [dev, staging]
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.is_empty(),
        "Expected no warnings for valid multi-select, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_requires_options() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        multiple: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("multiple requires options")),
        "Expected multiple-requires-options warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_default_must_be_array() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        options: [dev, staging, prod]
        multiple: true
        default: dev
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("default is not an array")),
        "Expected default-not-array warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_default_items_in_options() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        options: [dev, staging, prod]
        multiple: true
        default: [dev, sandbox]
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("'sandbox' is not in options list")),
        "Expected default-item-not-in-options warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_default_allow_custom_ok() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        options: [dev, staging, prod]
        multiple: true
        allow_custom: true
        default: [dev, sandbox]
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.is_empty(),
        "Expected no warnings with allow_custom, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_default_unique() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        options: [dev, staging, prod]
        multiple: true
        default: [dev, dev]
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("duplicate default item")),
        "Expected duplicate-default warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_secret_rejected() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      envs:
        type: string
        options: [dev, staging, prod]
        multiple: true
        secret: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let err = validate_workflow_config(&config).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("secret and multiple cannot be combined"),
        "Expected secret+multiple error, got: {msg}"
    );
}

#[test]
fn test_input_field_multiple_at_action_level_warning() {
    // Verify check_input_field_options runs against action-level inputs too,
    // not just task-level. The warning context should name the action.
    let yaml = r#"
actions:
  notify:
    type: script
    script: "echo hi"
    input:
      tags:
        type: string
        multiple: true
        # No options — should warn.
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Action 'notify'") && w.contains("multiple requires options")),
        "Expected action-level multiple-requires-options warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_at_action_level_secret_rejected() {
    // secret+multiple on an action-level input must also bail!.
    let yaml = r#"
actions:
  notify:
    type: script
    script: "echo hi"
    input:
      keys:
        type: string
        options: [a, b]
        multiple: true
        secret: true
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let err = validate_workflow_config(&config).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("Action 'notify'") && msg.contains("secret and multiple cannot be combined"),
        "Expected action-level secret+multiple error, got: {msg}"
    );
}

#[test]
fn test_input_field_multiple_on_integer_rejected() {
    // multiple is only supported on string/text fields in v1.
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      nums:
        type: integer
        options: [1, 2, 3]
        multiple: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let err = validate_workflow_config(&config).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("multiple is only supported on string/text fields"),
        "Expected multiple-on-integer error, got: {msg}"
    );
}

#[test]
fn test_input_field_multiple_non_string_default_item_warning() {
    // type: string + multiple: true + default contains a non-string item (e.g., a number).
    // Should warn — items must be strings to match string options.
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      tags:
        type: string
        options: [a, b]
        multiple: true
        default: [a, 42]
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("default array items must be strings")),
        "Expected non-string default item warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_on_text_ok() {
    // `text` is also acceptable for multiple.
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      notes:
        type: text
        options: ["one note", "two note"]
        multiple: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.is_empty(),
        "Expected no warnings, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_multiple_on_connection_type_rejected() {
    let yaml = r#"
connection_types:
  postgres:
    host:
      type: string
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      databases:
        type: postgres
        multiple: true
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let err = validate_workflow_config(&config).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("multiple is not supported on connection-type fields"),
        "Expected multiple-on-connection error, got: {msg}"
    );
}

#[test]
fn test_input_field_duplicate_options_warning() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production, staging]
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("duplicate option")),
        "Expected duplicate option warning, got: {:?}",
        warnings
    );
}

#[test]
fn test_input_field_empty_string_option_warning() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options:
          - ""
          - staging
          - production
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("contains empty string")),
        "Expected empty string option warning, got: {:?}",
        warnings
    );
}

// --- validate_workflow_config_with_libraries tests ---

#[test]
fn test_validate_with_libraries_rejects_missing_library_action() {
    // Task step references a library action that is NOT in config.actions.
    // In server mode (libraries_resolved = true) this must be an error.
    let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: common.slack-notify
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(result.is_err(), "Expected Err for missing library action");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent action"),
        "Error should mention 'non-existent action', got: {err}"
    );
    assert!(
        err.contains("common.slack-notify"),
        "Error should mention the action name, got: {err}"
    );
}

#[test]
fn test_validate_with_libraries_accepts_present_library_action() {
    // Library action is defined in config.actions AND referenced by a task step.
    // Validation must succeed with no errors.
    let yaml = r#"
actions:
  common.slack-notify:
    type: script
    script: "echo notify"
tasks:
  test:
    flow:
      step1:
        action: common.slack-notify
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(
        result.is_ok(),
        "Expected Ok for present library action, got: {:?}",
        result.unwrap_err()
    );
    let warnings = result.unwrap();
    assert!(
        warnings.is_empty(),
        "Expected no warnings, got: {:?}",
        warnings
    );
}

#[test]
fn test_validate_with_libraries_rejects_missing_library_task_ref() {
    // A type:task action references a library task that is NOT in config.tasks.
    // In server mode this must be an error.
    let yaml = r#"
actions:
  deploy:
    type: task
    task: common.deploy-service
tasks:
  caller:
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(result.is_err(), "Expected Err for missing library task ref");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent task"),
        "Error should mention 'non-existent task', got: {err}"
    );
    assert!(
        err.contains("common.deploy-service"),
        "Error should mention the task name, got: {err}"
    );
}

#[test]
fn test_validate_with_libraries_accepts_present_library_task_ref() {
    // A type:task action references a library task that IS in config.tasks.
    // The child task itself uses a local action.
    let yaml = r#"
actions:
  deploy:
    type: task
    task: common.deploy-service
  run-step:
    type: script
    script: "echo running"
tasks:
  common.deploy-service:
    flow:
      build:
        action: run-step
  caller:
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(
        result.is_ok(),
        "Expected Ok when library task ref is present, got: {:?}",
        result.unwrap_err()
    );
}

// --- cross-workspace action reference tests ---

struct FakeResolver {
    actions: Vec<(&'static str, &'static str)>,
    tasks: Vec<(&'static str, &'static str)>,
}
impl CrossWorkspaceResolver for FakeResolver {
    fn has_action(&self, ws: &str, a: &str) -> bool {
        self.actions.iter().any(|(w, x)| *w == ws && *x == a)
    }
    fn has_task(&self, ws: &str, t: &str) -> bool {
        self.tasks.iter().any(|(w, x)| *w == ws && *x == t)
    }
}

#[test]
fn test_validate_with_resolver_accepts_cross_workspace_action() {
    // Flow step references "B.remote" — not a local/library action, but the
    // resolver confirms workspace "B" has an action named "remote".
    let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: B.remote
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let resolver = FakeResolver {
        actions: vec![("B", "remote")],
        tasks: vec![],
    };
    let result = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver);
    assert!(
        result.is_ok(),
        "Expected Ok for valid cross-workspace action, got: {:?}",
        result.err()
    );
}

#[test]
fn test_validate_with_resolver_rejects_unknown_cross_workspace_action() {
    // Resolver reports workspace "B" does NOT have an action named "remote".
    let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: B.remote
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let resolver = FakeResolver {
        actions: vec![],
        tasks: vec![],
    };
    let result = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver);
    assert!(
        result.is_err(),
        "Expected Err when resolver reports no match"
    );
    let err = result.unwrap_err().to_string();
    assert_eq!(
        err, "action 'B.remote': workspace 'B' has no action 'remote'",
        "Error message should match the precise cross-workspace format, got: {err}"
    );
}

const DOTTED_TASK_YAML: &str = r#"
actions:
  run-remote:
    type: task
    task: B.deploy
tasks:
  caller:
    flow:
      go:
        action: run-remote
"#;

#[test]
fn dotted_task_ref_offline_is_a_warning_not_an_error() {
    let config: WorkspaceConfig = serde_yaml::from_str(DOTTED_TASK_YAML).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("cannot validate cross-workspace task reference offline")),
        "{warnings:?}"
    );
}

#[test]
fn dotted_task_ref_server_accepts_when_resolver_has_task() {
    let config: WorkspaceConfig = serde_yaml::from_str(DOTTED_TASK_YAML).unwrap();
    let resolver = FakeResolver {
        actions: vec![],
        tasks: vec![("B", "deploy")],
    };
    assert!(validate_workflow_config_with_cross_workspace_resolver(&config, &resolver).is_ok());
}

#[test]
fn dotted_task_ref_server_rejects_when_resolver_lacks_task() {
    let config: WorkspaceConfig = serde_yaml::from_str(DOTTED_TASK_YAML).unwrap();
    let resolver = FakeResolver {
        actions: vec![],
        tasks: vec![],
    };
    let err = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver)
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        "action 'run-remote': workspace 'B' has no task 'deploy'"
    );
}

#[test]
fn dotted_task_ref_local_flattened_key_wins_over_split() {
    let yaml = r#"
actions:
  run-remote:
    type: task
    task: common.deploy
  noop:
    type: script
    script: "echo hi"
tasks:
  common.deploy:
    flow:
      a:
        action: noop
  caller:
    flow:
      go:
        action: run-remote
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    // No resolver at all: the flattened key is found locally, never split.
    assert!(validate_workflow_config_with_libraries(&config).is_ok());
}

#[test]
fn dotted_task_ref_with_empty_side_is_an_error_in_both_modes() {
    for bad in [".deploy", "B."] {
        let yaml = format!(
            "actions:\n  run-remote:\n    type: task\n    task: \"{bad}\"\ntasks:\n  caller:\n    flow:\n      go:\n        action: run-remote\n"
        );
        let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = validate_workflow_config(&config).unwrap_err().to_string();
        assert!(err.contains("malformed task"), "{bad}: {err}");
        let resolver = FakeResolver {
            actions: vec![],
            tasks: vec![],
        };
        let err = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver)
            .unwrap_err()
            .to_string();
        assert!(err.contains("malformed task"), "{bad}: {err}");
    }
}

const HOOK_WITH_DOTTED_TASK_YAML: &str = r#"
actions:
  notify:
    type: task
    task: B.notify
  greet:
    type: script
    script: "echo hi"
tasks:
  deploy:
    flow:
      a:
        action: greet
    on_error:
      - action: notify
"#;

#[test]
fn hook_with_qualified_task_is_rejected_on_server() {
    let config: WorkspaceConfig = serde_yaml::from_str(HOOK_WITH_DOTTED_TASK_YAML).unwrap();
    let resolver = FakeResolver {
        actions: vec![],
        tasks: vec![("B", "notify")],
    };
    let err = validate_workflow_config_with_cross_workspace_resolver(&config, &resolver)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("hook actions cannot call tasks across workspaces"),
        "{err}"
    );
}

#[test]
fn hook_with_qualified_task_is_a_warning_offline() {
    let config: WorkspaceConfig = serde_yaml::from_str(HOOK_WITH_DOTTED_TASK_YAML).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("cannot validate offline")),
        "{warnings:?}"
    );
}

#[test]
fn hook_with_unresolved_library_task_offline_stays_a_warning() {
    // `common.notify` might be a library task the CLI cannot see. Must not fail.
    let yaml = HOOK_WITH_DOTTED_TASK_YAML.replace("B.notify", "common.notify");
    let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
    assert!(validate_workflow_config(&config).is_ok());
}

#[test]
fn missing_bare_task_ref_is_still_an_error_offline() {
    let yaml = DOTTED_TASK_YAML.replace("B.deploy", "deploy");
    let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
    let err = validate_workflow_config(&config).unwrap_err().to_string();
    assert!(
        err.contains("references non-existent task 'deploy'"),
        "{err}"
    );
}

#[test]
fn test_validate_with_libraries_still_rejects_dotted_action_without_resolver() {
    // Sanity check: validate_workflow_config_with_libraries (no resolver) must
    // keep treating an unresolved dotted action as a plain missing-action error,
    // not silently accept it as a cross-workspace reference.
    let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: B.remote
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(
        result.is_err(),
        "Expected Err without a cross-workspace resolver"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent action"),
        "Error should mention 'non-existent action', got: {err}"
    );
}

#[test]
fn test_validate_with_libraries_rejects_missing_hook_action() {
    // A task on_error hook references a library action that is NOT in config.actions.
    let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
tasks:
  test:
    flow:
      step1:
        action: build
    on_error:
      - action: common.alert
        input:
          message: "build failed"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(result.is_err(), "Expected Err for missing hook action");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent action"),
        "Error should mention 'non-existent action', got: {err}"
    );
    assert!(
        err.contains("common.alert"),
        "Error should mention the action name, got: {err}"
    );
}

#[test]
fn test_validate_with_libraries_accepts_present_hook_action() {
    // Library alert action is in config.actions; task on_error hook references it.
    let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
  common.alert:
    type: script
    script: "echo alert"
tasks:
  test:
    flow:
      step1:
        action: build
    on_error:
      - action: common.alert
        input:
          message: "build failed"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(
        result.is_ok(),
        "Expected Ok when hook action is present, got: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn test_validate_with_libraries_rejects_missing_workspace_hook_action() {
    // Workspace-level on_error hook references a library action not in config.actions.
    let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
tasks:
  test:
    flow:
      step1:
        action: build
on_error:
  - action: common.notify
    input:
      message: "workspace job failed"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(
        result.is_err(),
        "Expected Err for missing workspace-level hook action"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent action"),
        "Error should mention 'non-existent action', got: {err}"
    );
    assert!(
        err.contains("common.notify"),
        "Error should mention the action name, got: {err}"
    );
}

#[test]
fn test_validate_with_libraries_mixed_local_and_library_actions() {
    // Both a local action and a fully-resolved library action are defined.
    // A task uses both in its flow steps. Validation must succeed with no warnings.
    let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
  common.deploy:
    type: script
    script: "echo deploy"
tasks:
  release:
    flow:
      step-build:
        action: build
      step-deploy:
        action: common.deploy
        depends_on:
          - step-build
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config_with_libraries(&config);
    assert!(
        result.is_ok(),
        "Expected Ok for mixed local and library actions, got: {:?}",
        result.unwrap_err()
    );
    let warnings = result.unwrap();
    assert!(
        warnings.is_empty(),
        "Expected no warnings, got: {:?}",
        warnings
    );
}

#[test]
fn test_validate_step_timeout_within_limit() {
    let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      build:
        action: build
        timeout: 10m
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Expected Ok, got: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn test_validate_step_timeout_exceeds_limit() {
    let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      build:
        action: build
        timeout: 86401
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("exceeds maximum of 86400s"));
}

#[test]
fn test_validate_task_timeout_within_limit() {
    let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    timeout: 30m
    flow:
      build:
        action: build
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Expected Ok, got: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn test_validate_task_timeout_exceeds_limit() {
    let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    timeout: 604801
    flow:
      build:
        action: build
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("exceeds maximum of 604800s"));
}

#[test]
fn test_validate_concurrency_policy_deserializes() {
    let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      build:
        action: build
triggers:
  hourly:
    type: scheduler
    cron: "0 * * * *"
    task: deploy
    concurrency: skip
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Expected Ok, got: {:?}",
        result.unwrap_err()
    );
    let trigger = config.triggers.get("hourly").unwrap();
    assert_eq!(
        trigger.concurrency(),
        crate::models::workflow::ConcurrencyPolicy::Skip
    );
}

#[test]
fn test_validate_concurrency_policy_cancel_previous() {
    let yaml = r#"
actions:
  etl:
    type: script
    script: "python etl.py"
tasks:
  pipeline:
    flow:
      run:
        action: etl
triggers:
  hourly:
    type: scheduler
    cron: "0 * * * *"
    task: pipeline
    concurrency: cancel_previous
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let trigger = config.triggers.get("hourly").unwrap();
    assert_eq!(
        trigger.concurrency(),
        crate::models::workflow::ConcurrencyPolicy::CancelPrevious
    );
}

#[test]
fn test_validate_concurrency_default_is_allow() {
    let yaml = r#"
actions:
  build:
    type: script
    script: "make"
tasks:
  ci:
    flow:
      build:
        action: build
triggers:
  every_min:
    type: scheduler
    cron: "* * * * *"
    task: ci
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let trigger = config.triggers.get("every_min").unwrap();
    assert_eq!(
        trigger.concurrency(),
        crate::models::workflow::ConcurrencyPolicy::Allow
    );
}

// --- script action language / interpreter / dependency validation tests ---

#[test]
fn test_validate_script_action_invalid_language() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "ruby script.rb"
    language: ruby
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid language"));
}

#[test]
fn test_validate_script_action_valid_languages() {
    for lang in &["python", "javascript", "typescript", "go", "js", "ts"] {
        let yaml = format!(
            r#"
actions:
  run:
    type: script
    script: "echo hello"
    language: {lang}
"#
        );
        let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "language '{lang}' should be valid, got: {:?}",
            result.unwrap_err()
        );
    }
}

#[test]
fn test_validate_script_action_deps_without_language_warns() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "python script.py"
    dependencies:
      - requests
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Should not error, got: {:?}", result);
    let warnings = result.unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("dependencies") && w.contains("language")),
        "Expected warning about dependencies without language, got: {:?}",
        warnings
    );
}

#[test]
fn test_validate_script_action_interpreter_without_language_warns() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    interpreter: "python3.12"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Should not error, got: {:?}", result);
    let warnings = result.unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("interpreter") && w.contains("language")),
        "Expected warning about interpreter without language, got: {:?}",
        warnings
    );
}

#[test]
fn test_validate_script_action_full_valid() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "python script.py"
    language: python
    dependencies:
      - requests
    interpreter: "python3.12"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
    let warnings = result.unwrap();
    assert!(
        warnings.is_empty(),
        "Expected no warnings, got: {:?}",
        warnings
    );
}

#[test]
fn test_validate_script_action_interpreter_shell_metachar() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    interpreter: "python3; rm -rf /"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid 'interpreter' value"));
}

#[test]
fn test_validate_script_action_interpreter_empty() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    interpreter: ""
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("empty 'interpreter'"));
}

#[test]
fn test_validate_script_action_deps_shell_metachar() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    dependencies:
      - "requests; evil"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("shell metacharacters"));
}

#[test]
fn test_validate_script_action_deps_empty_entry() {
    let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    dependencies:
      - ""
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("empty dependency entry"));
}

#[test]
fn test_validate_docker_action_rejects_language() {
    let yaml = r#"
actions:
  run:
    type: docker
    image: python:3.12
    language: python
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'language' field"));
}

#[test]
fn test_validate_docker_action_rejects_dependencies() {
    let yaml = r#"
actions:
  run:
    type: docker
    image: python:3.12
    dependencies:
      - requests
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("'dependencies' field"));
}

#[test]
fn test_validate_docker_action_rejects_interpreter() {
    let yaml = r#"
actions:
  run:
    type: docker
    image: python:3.12
    interpreter: "python3.12"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("'interpreter' field"));
}

#[test]
fn test_validate_pod_action_rejects_language() {
    let yaml = r#"
actions:
  run:
    type: pod
    image: python:3.12
    language: python
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'language' field"));
}

#[test]
fn test_validate_task_action_rejects_language() {
    let yaml = r#"
actions:
  run-child:
    type: task
    task: other
    language: python
tasks:
  other:
    flow:
      s:
        action: run-child
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'language' field"));
}

#[test]
fn test_validate_action_script_cmd_and_source_rejected() {
    // cmd is not valid on type: script at all
    let yaml = r#"
actions:
  my-action:
    type: script
    cmd: "echo hello"
    source: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "cmd on type: script should be rejected");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not valid on type: script"),
        "Error should mention 'not valid on type: script'"
    );
}

#[test]
fn test_validate_action_script_all_three_fields_rejected() {
    // cmd is not valid on type: script — error fires before checking script/source
    let yaml = r#"
actions:
  my-action:
    type: script
    cmd: "echo hello"
    script: "echo world"
    source: "deploy.sh"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "cmd on type: script should be rejected");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not valid on type: script"),
        "Error should mention 'not valid on type: script'"
    );
}

#[test]
fn test_validate_action_script_empty_script_field() {
    let yaml = r#"
actions:
  my-action:
    type: script
    script: ""
tasks:
  test-task:
    flow:
      step1:
        action: my-action
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Empty 'script' field should be rejected");
    assert!(
        result.unwrap_err().to_string().contains("empty 'script'"),
        "Error should mention empty 'script'"
    );
}

#[test]
fn test_validate_action_script_empty_source_field() {
    let yaml = r#"
actions:
  my-action:
    type: script
    source: ""
tasks:
  test-task:
    flow:
      step1:
        action: my-action
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Empty 'source' field should be rejected");
    assert!(
        result.unwrap_err().to_string().contains("empty 'source'"),
        "Error should mention empty 'source'"
    );
}

#[test]
fn test_validate_action_script_empty_cmd_field() {
    // Any cmd on type: script is rejected — empty or not
    let yaml = r#"
actions:
  my-action:
    type: script
    cmd: ""
tasks:
  test-task:
    flow:
      step1:
        action: my-action
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "'cmd' on type: script should be rejected");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not valid on type: script"),
        "Error should mention 'not valid on type: script'"
    );
}

#[test]
fn test_validate_task_action_with_script_rejected() {
    // type:task with a script field — should fail since script is not allowed on task actions
    let yaml = r#"
actions:
  run-child:
    type: task
    task: some-task
    script: "echo hi"
tasks:
  some-task:
    flow:
      s:
        action: run-child
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "type:task with 'script' field should be rejected"
    );
    assert!(
        result.unwrap_err().to_string().contains("'script'"),
        "Error should mention 'script' field"
    );
}

#[test]
fn test_validate_when_valid_template() {
    let yaml = r#"
            actions:
              check:
                type: script
                script: "echo ok"
            tasks:
              test-task:
                flow:
                  step1:
                    action: check
                    when: "{{ input.deploy }}"
        "#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Valid when expression should pass: {:?}",
        result
    );
}

#[test]
fn test_validate_when_invalid_template_syntax() {
    let yaml = r#"
            actions:
              check:
                type: script
                script: "echo ok"
            tasks:
              test-task:
                flow:
                  step1:
                    action: check
                    when: "{% if %}"
        "#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Invalid when syntax should fail");
    assert!(result.unwrap_err().to_string().contains("when expression"));
}

#[test]
fn test_validate_when_with_tera_logic() {
    let yaml = r#"
            actions:
              check:
                type: script
                script: "echo ok"
            tasks:
              test-task:
                flow:
                  step1:
                    action: check
                    when: "{% if input.env == \"production\" %}true{% endif %}"
        "#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Tera logic in when should pass: {:?}",
        result
    );
}

// --- for_each validation tests ---

#[test]
fn test_for_each_string_expression_valid() {
    let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      fetch:
        action: process
      loop-step:
        action: process
        depends_on: [fetch]
        for_each: "{{ fetch.output.items }}"
        input:
          item: "{{ each.item }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Valid for_each string should pass: {:?}",
        result
    );
}

#[test]
fn test_for_each_literal_array_valid() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: echo deploy
tasks:
  main:
    flow:
      deploy:
        action: deploy
        for_each: ["us-east-1", "eu-west-1", "ap-south-1"]
        input:
          region: "{{ each.item }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Valid for_each array should pass: {:?}",
        result
    );
}

#[test]
fn test_for_each_invalid_type_rejected() {
    let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        for_each: 42
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "for_each: 42 should be rejected");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("'for_each' must be a string"),
        "Error should mention 'for_each' must be a string"
    );
}

/// Codex review (R19): an invalid literal `for_each` is never echoed —
/// the message names the task, step, field and JSON type only.
#[test]
fn test_for_each_invalid_literal_is_not_echoed() {
    let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        for_each: {"token": "literal-secret"}
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let err = validate_workflow_config(&config).unwrap_err();
    let text = format!("{err:#} {err:?}");
    assert!(!text.contains("literal-secret"), "{text}");
    assert!(!text.contains("token"), "{text}");
    assert_eq!(
        format!("{err:#}"),
        "Task 'main' step 'step' field 'for_each' must be a string (Tera template) or array, got a JSON object"
    );
}

#[test]
fn test_for_each_invalid_tera_syntax() {
    let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        for_each: "{% if %}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "Invalid Tera syntax in for_each should be rejected"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid for_each expression"),
        "Error should mention 'invalid for_each expression'"
    );
}

#[test]
fn test_step_name_with_brackets_rejected() {
    let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      "step[0]":
        action: process
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Step name with '[' should be rejected");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("must not contain '['"),
        "Error should mention that '[' is not allowed"
    );
}

#[test]
fn test_sequential_without_for_each_warns() {
    let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        sequential: true
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("sequential") && w.contains("no for_each")),
        "Expected warning about sequential without for_each, got: {:?}",
        warnings
    );
}

#[test]
fn test_for_each_with_sequential() {
    let yaml = r#"
actions:
  migrate:
    type: script
    script: echo migrate
tasks:
  main:
    flow:
      migrate:
        action: migrate
        for_each: ["schema1", "schema2"]
        sequential: true
        input:
          schema: "{{ each.item }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "for_each with sequential should be valid: {:?}",
        result
    );
    assert!(result.unwrap().is_empty(), "Expected no warnings");
}

// --- agent action validation tests ---

#[test]
fn test_validate_agent_action_valid() {
    let yaml = r#"
actions:
  summarise:
    type: agent
    provider: openai
    prompt: "Summarise {{ input.text }}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Valid agent action should pass: {:?}",
        result
    );
}

#[test]
fn test_validate_agent_action_with_all_optional_fields() {
    let yaml = r#"
actions:
  analyse:
    type: agent
    provider: anthropic
    model: claude-opus-4-5
    system_prompt: "You are a helpful assistant."
    prompt: "Analyse {{ input.data }}"
    temperature: 0.7
    max_tokens: 2048
    max_turns: 5
    output:
      result:
        type: string
    tools:
      - task: run-tool
      - mcp: my-mcp-server
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Agent action with all optional fields should pass: {:?}",
        result
    );
}

#[test]
fn test_validate_agent_action_missing_provider() {
    let yaml = r#"
actions:
  bad:
    type: agent
    prompt: "Do something"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Missing provider should fail");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("missing 'provider'"),
        "Error should mention missing 'provider'"
    );
}

#[test]
fn test_validate_agent_action_missing_prompt() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Missing prompt should fail");
    assert!(
        result.unwrap_err().to_string().contains("missing 'prompt'"),
        "Error should mention missing 'prompt'"
    );
}

#[test]
fn test_validate_agent_action_rejects_script_field() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    script: "echo hi"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Agent with 'script' should fail");
    assert!(result.unwrap_err().to_string().contains("'script' field"));
}

#[test]
fn test_validate_agent_action_rejects_image_field() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    image: "my-image:latest"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Agent with 'image' should fail");
    assert!(result.unwrap_err().to_string().contains("'image' field"));
}

#[test]
fn test_validate_agent_action_rejects_task_field() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    task: some-task
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Agent with 'task' field should fail");
    assert!(result.unwrap_err().to_string().contains("'task' field"));
}

#[test]
fn test_validate_agent_action_temperature_out_of_range() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    temperature: 2.5
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Temperature > 2.0 should fail");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("temperature must be between"),
        "Error should mention temperature range"
    );
}

#[test]
fn test_validate_agent_action_temperature_zero_valid() {
    let yaml = r#"
actions:
  deterministic:
    type: agent
    provider: openai
    prompt: "Classify {{ input.text }}"
    temperature: 0.0
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Temperature 0.0 should be valid: {:?}",
        result
    );
}

#[test]
fn test_validate_agent_action_temperature_max_valid() {
    let yaml = r#"
actions:
  creative:
    type: agent
    provider: openai
    prompt: "Write a poem about {{ input.topic }}"
    temperature: 2.0
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_ok(),
        "Temperature 2.0 should be valid: {:?}",
        result
    );
}

#[test]
fn test_validate_agent_action_output_invalid_field_type() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    output:
      result:
        type: invalid_type
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err(), "Invalid output field type should fail");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid type 'invalid_type'"),
        "Error should mention invalid type"
    );
}

#[test]
fn test_validate_agent_action_output_rejects_text_type() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    output:
      result:
        type: text
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "type: text must be rejected for output fields"
    );
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("invalid type 'text'"), "got: {}", msg);
}

#[test]
fn test_validate_script_action_output_invalid_type_rejected() {
    let yaml = r#"
actions:
  export:
    type: script
    script: "echo done"
    output:
      result:
        type: invalid_type
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "script action with invalid output type should now fail"
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid type 'invalid_type'"));
}

#[test]
fn test_validate_agent_action_invalid_prompt_template() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "{% if %}"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "Invalid prompt template syntax should fail"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid prompt template"),
        "Error should mention invalid prompt template"
    );
}

#[test]
fn test_validate_agent_action_invalid_system_prompt_template() {
    let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    system_prompt: "{% if %}"
    prompt: "Do something"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(
        result.is_err(),
        "Invalid system_prompt template syntax should fail"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid system_prompt template"),
        "Error should mention invalid system_prompt template"
    );
}

#[test]
fn test_compute_required_ability_agent() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef =
        serde_yaml::from_str("type: agent\nprovider: openai\nprompt: \"Do something\"\n").unwrap();
    // Agent steps require a worker with the "agent" capability.
    assert_eq!(compute_required_ability(&action), "agent");
    assert!(compute_required_tags(&action).is_empty());
}

#[test]
fn test_compute_required_ability_agent_with_extra_tags() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        "type: agent\nprovider: openai\nprompt: \"Do something\"\ntags: [gpu, custom]\n",
    )
    .unwrap();
    // Ability is separate now; tags list is just the user tags.
    assert_eq!(compute_required_ability(&action), "agent");
    assert_eq!(
        compute_required_tags(&action),
        vec!["gpu".to_string(), "custom".to_string()]
    );
}

#[test]
fn test_derive_runner_agent() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: agent
provider: openai
prompt: "Do something"
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "none");
}

// ---- approval action tests ----

#[test]
fn test_validate_approval_action_valid() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Please review the deployment before proceeding."

tasks:
  deploy:
    flow:
      gate:
        action: gate
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected ok but got: {:?}", result);
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_validate_approval_action_missing_message() {
    let yaml = r#"
actions:
  gate:
    type: approval
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("missing 'message'"),
        "expected missing message error"
    );
}

#[test]
fn test_validate_approval_action_rejects_cmd() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    cmd: "echo bad"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'cmd' field"));
}

#[test]
fn test_validate_approval_action_rejects_script() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    script: "echo bad"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'script' field"));
}

#[test]
fn test_validate_approval_action_rejects_image() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    image: "ubuntu:22.04"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'image' field"));
}

#[test]
fn test_validate_approval_action_rejects_runner() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    runner: docker
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'runner' field"));
}

#[test]
fn test_validate_approval_action_rejects_task_field() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    task: other-task
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'task' field"));
}

#[test]
fn test_validate_approval_action_rejects_provider() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    provider: openai
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("'provider' field"));
}

#[test]
fn test_compute_required_tags_approval() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: approval
message: "Please review"
"#,
    )
    .unwrap();
    assert_eq!(compute_required_tags(&action), Vec::<String>::new());
}

#[test]
fn test_derive_runner_approval() {
    use crate::models::workflow::ActionDef;
    let action: ActionDef = serde_yaml::from_str(
        r#"
type: approval
message: "Please review"
"#,
    )
    .unwrap();
    assert_eq!(derive_runner(&action), "none");
}

#[test]
fn test_validate_approval_inline_in_flow() {
    let yaml = r#"
tasks:
  deploy:
    flow:
      gate:
        type: approval
        message: "Approve the deployment to production?"
      run:
        depends_on: [gate]
        type: script
        script: "echo deploying"
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected ok but got: {:?}", result);
}

#[test]
fn test_validate_on_suspended_hook_valid() {
    let yaml = r#"
actions:
  notify:
    type: script
    script: "echo notifying"
  gate:
    type: approval
    message: "Approve?"

tasks:
  deploy:
    on_suspended:
      - action: notify
    flow:
      gate:
        action: gate
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected ok but got: {:?}", result);
}

#[test]
fn test_validate_on_suspended_hook_missing_action() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"

tasks:
  deploy:
    on_suspended:
      - action: nonexistent_action
    flow:
      gate:
        action: gate
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("nonexistent_action"),
        "expected missing action error"
    );
}

#[test]
fn test_validate_workspace_on_suspended_hook_valid() {
    let yaml = r#"
actions:
  notify:
    type: script
    script: "echo notifying"
  gate:
    type: approval
    message: "Approve?"

on_suspended:
  - action: notify

tasks:
  deploy:
    flow:
      gate:
        action: gate
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected ok but got: {:?}", result);
}

#[test]
fn test_validate_workspace_on_suspended_hook_missing_action() {
    let yaml = r#"
on_suspended:
  - action: does_not_exist
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("does_not_exist"),
        "expected missing action error"
    );
}

// ─── Script args validation tests ───────────────────────────────────────

#[test]
fn test_script_action_with_args_valid() {
    let yaml = r#"
actions:
  deploy:
    type: script
    source: scripts/deploy.sh
    args:
      - "--env"
      - "production"
tasks:
  run:
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected ok but got: {:?}", result);
}

#[test]
fn test_script_action_with_args_and_inline_script_valid() {
    let yaml = r#"
actions:
  greet:
    type: script
    script: 'echo "Hello $1"'
    args:
      - "world"
tasks:
  run:
    flow:
      step1:
        action: greet
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "expected ok but got: {:?}", result);
}

#[test]
fn test_script_action_with_empty_arg_rejected() {
    let yaml = r#"
actions:
  deploy:
    type: script
    source: scripts/deploy.sh
    args:
      - "--env"
      - ""
tasks:
  run:
    flow:
      step1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("empty args entry"),
        "expected empty args error"
    );
}

#[test]
fn test_docker_action_with_args_rejected() {
    let yaml = r#"
actions:
  build:
    type: docker
    image: node:20
    args:
      - "--verbose"
tasks:
  run:
    flow:
      step1:
        action: build
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("args"), "expected args error, got: {}", err);
    assert!(
        err.contains("cmd/command/entrypoint"),
        "expected hint, got: {}",
        err
    );
}

#[test]
fn test_pod_action_with_args_rejected() {
    let yaml = r#"
actions:
  train:
    type: pod
    image: pytorch/pytorch:2.1.0
    args:
      - "--epochs"
      - "10"
tasks:
  run:
    flow:
      step1:
        action: train
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("args"), "expected args error, got: {}", err);
}

#[test]
fn test_task_action_with_args_rejected() {
    let yaml = r#"
actions:
  run-sub:
    type: task
    task: sub-task
    args:
      - "nope"
tasks:
  sub-task:
    flow:
      step1:
        action: run-sub
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("args"));
}

#[test]
fn test_agent_action_with_args_rejected() {
    let yaml = r#"
actions:
  analyze:
    type: agent
    provider: anthropic
    prompt: "Analyze data"
    args:
      - "nope"
tasks:
  run:
    flow:
      step1:
        action: analyze
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("args"), "expected args error, got: {}", err);
    assert!(
        err.contains("agent"),
        "expected agent type in error, got: {}",
        err
    );
}

#[test]
fn test_approval_action_with_args_rejected() {
    let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve deployment?"
    args:
      - "nope"
tasks:
  run:
    flow:
      step1:
        action: gate
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("args"), "expected args error, got: {}", err);
    assert!(
        err.contains("approval"),
        "expected approval type in error, got: {}",
        err
    );
}

// ─── Helpers for multi-turn agent and MCP validation tests ───────────────

/// Create a minimal valid agent ActionDef and apply a mutation closure.
fn make_agent_action_with(
    f: impl FnOnce(&mut crate::models::workflow::ActionDef),
) -> crate::models::workflow::ActionDef {
    let mut action: crate::models::workflow::ActionDef = serde_yaml::from_str(
        r#"
type: agent
provider: anthropic
prompt: "Do something useful"
"#,
    )
    .unwrap();
    f(&mut action);
    action
}

/// Wrap a single action into a minimal WorkspaceConfig.
fn make_config_with_action(
    name: &str,
    action: crate::models::workflow::ActionDef,
) -> WorkspaceConfig {
    let mut config = WorkspaceConfig::default();
    config.actions.insert(name.to_string(), action);
    config
}

// ─── Multi-turn agent tool validation ────────────────────────────────

#[test]
fn test_validate_agent_action_max_turns_zero() {
    let action = make_agent_action_with(|a| {
        a.max_turns = Some(0);
        a.tools = vec![AgentToolRef::Task {
            git_ref: None,
            task: "t".into(),
        }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("max_turns must be between 1 and 100"));
}

#[test]
fn test_validate_agent_action_max_turns_101() {
    let action = make_agent_action_with(|a| {
        a.max_turns = Some(101);
        a.tools = vec![AgentToolRef::Task {
            git_ref: None,
            task: "t".into(),
        }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("max_turns must be between 1 and 100"));
}

#[test]
fn test_validate_agent_action_max_turns_valid_boundaries() {
    // max_turns=1 with tools should be valid
    let action = make_agent_action_with(|a| {
        a.max_turns = Some(1);
        a.tools = vec![AgentToolRef::Task {
            git_ref: None,
            task: "t".into(),
        }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_ok());

    // max_turns=100 with tools should be valid
    let action = make_agent_action_with(|a| {
        a.max_turns = Some(100);
        a.tools = vec![AgentToolRef::Task {
            git_ref: None,
            task: "t".into(),
        }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_ok());
}

#[test]
fn test_validate_agent_action_max_turns_no_tools_warns() {
    let action = make_agent_action_with(|a| {
        a.max_turns = Some(5);
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_ok());
    let warnings = result.unwrap();
    assert!(warnings
        .iter()
        .any(|w| w.contains("max_turns") && w.contains("no tools")));
}

#[test]
fn test_validate_agent_action_interactive_no_tools_warns() {
    let action = make_agent_action_with(|a| {
        a.interactive = true;
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_ok());
    let warnings = result.unwrap();
    assert!(warnings
        .iter()
        .any(|w| w.contains("interactive=true") && w.contains("no tools")));
}

#[test]
fn test_validate_agent_action_interactive_no_max_turns_warns() {
    let action = make_agent_action_with(|a| {
        a.interactive = true;
        a.tools = vec![AgentToolRef::Task {
            git_ref: None,
            task: "t".into(),
        }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_ok());
    let warnings = result.unwrap();
    assert!(warnings
        .iter()
        .any(|w| w.contains("interactive=true") && w.contains("no max_turns")));
}

#[test]
fn test_validate_agent_action_empty_task_ref() {
    let action = make_agent_action_with(|a| {
        a.tools = vec![AgentToolRef::Task {
            git_ref: None,
            task: "".into(),
        }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("empty task reference"));
}

#[test]
fn test_validate_agent_action_empty_mcp_ref() {
    let action = make_agent_action_with(|a| {
        a.tools = vec![AgentToolRef::Mcp { mcp: "".into() }];
    });
    let result = validate_workflow_config(&make_config_with_action("a", action));
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("empty MCP server reference"));
}

#[test]
fn test_validate_mcp_servers_empty() {
    let result = validate_mcp_servers(&std::collections::HashMap::new());
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_validate_mcp_servers_stdio_missing_command() {
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "test".to_string(),
        McpServerDef {
            transport: "stdio".to_string(),
            command: None,
            args: None,
            url: None,
            auth_token: None,
            env: Default::default(),
            timeout_secs: None,
        },
    );
    let result = validate_mcp_servers(&servers);
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("missing 'command'"));
}

#[test]
fn test_validate_mcp_servers_stdio_has_url() {
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "test".to_string(),
        McpServerDef {
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: None,
            url: Some("http://localhost".to_string()),
            auth_token: None,
            env: Default::default(),
            timeout_secs: None,
        },
    );
    let result = validate_mcp_servers(&servers);
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("has 'url' field"));
}

#[test]
fn test_validate_mcp_servers_sse_missing_url() {
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "test".to_string(),
        McpServerDef {
            transport: "sse".to_string(),
            command: None,
            args: None,
            url: None,
            auth_token: None,
            env: Default::default(),
            timeout_secs: None,
        },
    );
    let result = validate_mcp_servers(&servers);
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("missing 'url'"));
}

#[test]
fn test_validate_mcp_servers_unknown_transport() {
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "test".to_string(),
        McpServerDef {
            transport: "grpc".to_string(),
            command: None,
            args: None,
            url: None,
            auth_token: None,
            env: Default::default(),
            timeout_secs: None,
        },
    );
    let result = validate_mcp_servers(&servers);
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("unknown transport type"));
}

#[test]
fn test_task_name_collision_hyphen_underscore() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: echo deploy
tasks:
  deploy-app:
    flow:
      s1:
        action: deploy
  deploy_app:
    flow:
      s1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(
        err.contains("normalized name") && err.contains("deploy_app"),
        "Error: {}",
        err
    );
}

#[test]
fn test_task_name_no_collision_when_different() {
    let yaml = r#"
actions:
  deploy:
    type: script
    script: echo deploy
tasks:
  deploy-app:
    flow:
      s1:
        action: deploy
  deploy-service:
    flow:
      s1:
        action: deploy
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected ok, got: {:?}", result.err());
}

#[test]
fn test_mcp_server_name_collision_hyphen_underscore() {
    let yaml = r#"
actions: {}
tasks: {}
mcp_servers:
  my-server:
    transport: stdio
    command: echo
  my_server:
    transport: stdio
    command: echo
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(
        err.contains("normalized name") && err.contains("my_server"),
        "Error: {}",
        err
    );
}

// --- EventSource trigger validation tests ---

fn make_event_source_trigger_config(trigger_extra: &str) -> String {
    format!(
        r#"
actions:
  consumer-action:
    type: script
    script: "while true; do echo test; sleep 1; done"
tasks:
  consumer-task:
    flow:
      run:
        action: consumer-action
  target-task:
    flow:
      handle:
        action: consumer-action
triggers:
  my-source:
    type: event_source
    task: consumer-task
    target_task: target-task
{}
"#,
        trigger_extra
    )
}

#[test]
fn test_event_source_valid_basic() {
    let yaml = make_event_source_trigger_config("");
    let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
}

#[test]
fn test_event_source_consumer_task_nonexistent() {
    let yaml = r#"
actions:
  consumer-action:
    type: script
    script: "echo hi"
tasks:
  target-task:
    flow:
      handle:
        action: consumer-action
triggers:
  my-source:
    type: event_source
    task: ghost-consumer
    target_task: target-task
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent consumer task"),
        "Unexpected error: {}",
        err
    );
}

#[test]
fn test_event_source_target_task_nonexistent() {
    let yaml = r#"
actions:
  consumer-action:
    type: script
    script: "echo hi"
tasks:
  consumer-task:
    flow:
      run:
        action: consumer-action
triggers:
  my-source:
    type: event_source
    task: consumer-task
    target_task: ghost-target
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-existent target task"),
        "Unexpected error: {}",
        err
    );
}

#[test]
fn test_event_source_consumer_equals_target_fails() {
    let yaml = r#"
actions:
  consumer-action:
    type: script
    script: "echo hi"
tasks:
  consumer-task:
    flow:
      run:
        action: consumer-action
triggers:
  my-source:
    type: event_source
    task: consumer-task
    target_task: consumer-task
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cannot be the same"),
        "Unexpected error: {}",
        err
    );
}

#[test]
fn test_event_source_library_consumer_task_skipped() {
    // A consumer task with a dot is a library reference — validation is skipped.
    let yaml = r#"
actions:
  consumer-action:
    type: script
    script: "echo hi"
tasks:
  target-task:
    flow:
      handle:
        action: consumer-action
triggers:
  my-source:
    type: event_source
    task: lib.consumer-task
    target_task: target-task
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
}

#[test]
fn test_event_source_library_target_task_skipped() {
    // A target task with a dot is a library reference — validation is skipped.
    let yaml = r#"
actions:
  consumer-action:
    type: script
    script: "echo hi"
tasks:
  consumer-task:
    flow:
      run:
        action: consumer-action
triggers:
  my-source:
    type: event_source
    task: consumer-task
    target_task: lib.handle-event
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
}

#[test]
fn test_event_source_zero_backoff_secs() {
    let yaml = make_event_source_trigger_config("    backoff_secs: 0");
    let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("backoff_secs"), "Unexpected error: {}", err);
}

#[test]
fn test_event_source_zero_max_in_flight() {
    let yaml = make_event_source_trigger_config("    max_in_flight: 0");
    let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("max_in_flight"), "Unexpected error: {}", err);
}

#[test]
fn test_event_source_nonzero_max_in_flight_valid() {
    let yaml = make_event_source_trigger_config("    max_in_flight: 10");
    let config: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
}

#[test]
fn test_dotted_input_type_is_warning_not_error() {
    let yaml = r#"
tasks:
  t:
    input:
      ch: { type: jobs.clickhouse }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("jobs.clickhouse") && w.contains("cross-workspace")),
        "{warnings:?}"
    );
}

#[test]
fn test_dotted_connection_type_is_warning_not_error() {
    let yaml = r#"
connections:
  ch-eu:
    type: jobs.clickhouse
    shared: true
    host: ch.eu
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("ch-eu") && w.contains("cross-workspace")),
        "{warnings:?}"
    );
}

#[test]
fn test_literal_library_type_key_is_not_warned() {
    let yaml = r#"
connection_types:
  common.pg:
    host: { type: string }
tasks:
  t:
    input:
      db: { type: common.pg }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let warnings = validate_workflow_config(&config).unwrap();
    assert!(
        !warnings.iter().any(|w| w.contains("cross-workspace")),
        "{warnings:?}"
    );
}

#[test]
fn test_bare_unknown_type_is_still_an_error() {
    let yaml = r#"
tasks:
  t:
    input:
      ch: { type: clickhouse }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(validate_workflow_config(&config).is_err());
}

fn warnings_of(yaml: &str) -> Vec<String> {
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    validate_workflow_config(&config).expect("must validate")
}

fn error_of(yaml: &str) -> String {
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    format!("{:#}", validate_workflow_config(&config).unwrap_err())
}

#[test]
fn git_ref_on_non_task_action_is_rejected() {
    let err = error_of(
        r#"
actions:
  s:
    type: script
    script: echo hi
    ref: release/2.3
"#,
    );
    assert!(
        err.contains("`ref` is only valid on type: task actions"),
        "{err}"
    );
}

#[test]
fn templated_step_ref_is_accepted_with_a_warning() {
    let warnings = warnings_of(
        r#"
actions:
  a:
    type: script
    script: echo hi
tasks:
  t:
    flow:
      s:
        action: a
        ref: "{{ input.release }}"
"#,
    );
    assert!(
        warnings.contains(
            &"Task 't' step 's' references action 'a' at a templated ref - validation skipped (resolved when the step starts)"
                .to_string()
        ),
        "{warnings:?}"
    );
}

#[test]
fn invalid_static_step_ref_is_still_rejected() {
    let err = error_of(
        r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: "a..b"
"#,
    );
    assert!(err.contains("invalid `ref`"), "{err}");
}

#[test]
fn templated_ref_syntax_errors_and_vals_are_rejected() {
    for (template, needle) in [
        ("{{ input.release ", "template syntax error"),
        ("{{ input.release | nope }}", "unknown filter"),
        ("{{ input.release | vals }}", "unknown filter"),
    ] {
        let err = error_of(&format!(
            r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: "{template}"
"#
        ));
        assert!(
            err.contains("has an invalid `ref` template"),
            "{template}: {err}"
        );
        assert!(err.contains(needle), "{template}: {err}");
    }
}

#[test]
fn templated_task_action_ref_is_accepted_with_a_warning() {
    let warnings = warnings_of(
        r#"
actions:
  call:
    type: task
    task: other
    ref: "{{ input.branch }}"
tasks:
  t:
    flow:
      s:
        action: call
"#,
    );
    assert!(
        warnings.contains(
            &"Action 'call' references task 'other' at a templated ref - validation skipped (resolved when the step starts)"
                .to_string()
        ),
        "{warnings:?}"
    );
}

#[test]
fn templated_ref_on_a_non_task_action_is_rejected() {
    let err = error_of(
        r#"
actions:
  a:
    type: script
    script: echo hi
    ref: "{{ input.branch }}"
tasks:
  t:
    flow:
      s:
        action: a
"#,
    );
    assert!(
        err.contains("`ref` is only valid on type: task actions"),
        "{err}"
    );
}

#[test]
fn templated_webhook_ref_is_accepted_and_scheduler_ref_is_rejected() {
    let warnings = warnings_of(
        r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: main
triggers:
  on-push:
    type: webhook
    name: push
    task: t
    ref: "{{ input.body.ref }}"
"#,
    );
    assert!(
        warnings.contains(
            &"Trigger 'on-push' targets task 't' at a templated ref - validation skipped (resolved when the webhook fires)"
                .to_string()
        ),
        "{warnings:?}"
    );
    let err = error_of(
        r#"
tasks:
  t:
    flow:
      s:
        action: a
        ref: main
triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * *"
    task: t
    ref: "{{ input.branch }}"
"#,
    );
    assert!(
        err.contains("Trigger 'nightly': a templated `ref` is only supported on webhook triggers"),
        "{err}"
    );
}

#[test]
fn templated_ref_stays_rejected_where_ref_itself_is() {
    let hook = error_of(
        r#"
actions:
  a:
    type: script
    script: echo hi
tasks:
  t:
    flow:
      s:
        action: a
    on_success:
      - action: a
        ref: "{{ input.branch }}"
"#,
    );
    assert!(
        hook.contains("`ref` is not supported on hooks yet"),
        "{hook}"
    );
    let tool = error_of(
        r#"
actions:
  ask:
    type: agent
    provider: anthropic
    model: m
    prompt: hi
    tools:
      - task: t
        ref: "{{ input.branch }}"
tasks:
  t:
    flow:
      s:
        action: ask
"#,
    );
    assert!(
        tool.contains("`ref` is not supported on agent task tools yet"),
        "{tool}"
    );
}

#[test]
fn git_ref_step_skips_the_existence_check_with_a_warning() {
    // `import` exists only on the release branch: the step must not fail
    // validation (spec § 4.3: the name need not exist in the caller).
    let warnings = warnings_of(
        r#"
tasks:
  t:
    flow:
      import:
        action: import
        ref: release/2.3
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("action 'import' at ref 'release/2.3' - validation skipped")),
        "{warnings:?}"
    );
}

#[test]
fn git_ref_task_action_skips_task_existence_and_self_reference() {
    // `nightly` calling `nightly` at another ref is not a self-reference.
    let warnings = warnings_of(
        r#"
actions:
  nightly-2-3:
    type: task
    task: nightly
    ref: release/2.3
  run:
    type: script
    script: echo hi
tasks:
  nightly:
    flow:
      a:
        action: run
      older:
        action: nightly-2-3
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("task 'nightly' at ref 'release/2.3' - validation skipped")),
        "{warnings:?}"
    );
    // Server mode (libraries resolved) skips it the same way.
    let config: WorkspaceConfig = serde_yaml::from_str(
        r#"
actions:
  remote:
    type: task
    task: only-on-the-branch
    ref: v1
tasks:
  t:
    flow:
      s:
        action: remote
"#,
    )
    .unwrap();
    assert!(validate_workflow_config_with_libraries(&config).is_ok());
}

#[test]
fn git_ref_on_hooks_is_rejected() {
    let err = error_of(
        r#"
actions:
  notify:
    type: script
    script: echo hi
tasks:
  t:
    on_error:
      - action: notify
        ref: release/2.3
    flow:
      s:
        action: notify
"#,
    );
    assert!(err.contains("`ref` is not supported on hooks yet"), "{err}");
    let err = error_of(
        r#"
actions:
  notify:
    type: script
    script: echo hi
on_success:
  - action: notify
    ref: v1
"#,
    );
    assert!(err.contains("`ref` is not supported on hooks yet"), "{err}");
}

#[test]
fn hook_using_a_ref_task_action_is_rejected() {
    // spec § 4.6 (G8): both hook creation branches read `ActionDef.task`
    // directly, so the action's `ref` would be dropped.
    let err = error_of(
        r#"
actions:
  run-old:
    type: task
    task: cleanup
    ref: release/2.3
  run:
    type: script
    script: echo hi
tasks:
  cleanup:
    flow:
      s:
        action: run
  t:
    on_success:
      - action: run-old
    flow:
      s:
        action: run
"#,
    );
    assert!(err.contains("`ref` is not supported on hooks yet"), "{err}");
}

#[test]
fn git_ref_on_event_source_is_rejected() {
    let err = error_of(
        r#"
actions:
  run:
    type: script
    script: echo hi
tasks:
  consumer:
    flow:
      s:
        action: run
  target:
    flow:
      s:
        action: run
triggers:
  q:
    type: event_source
    task: consumer
    target_task: target
    ref: main
"#,
    );
    assert!(
        err.contains("`ref` is not supported on event_source triggers yet"),
        "{err}"
    );
}

#[test]
fn git_ref_on_agent_task_tool_is_rejected() {
    let err = error_of(
        r#"
actions:
  helper:
    type: agent
    provider: anthropic
    model: m
    prompt: hi
    tools:
      - task: summarize
        ref: release/2.3
"#,
    );
    assert!(
        err.contains("`ref` is not supported on agent task tools yet"),
        "{err}"
    );
}

#[test]
fn trigger_with_ref_or_cross_workspace_task() {
    // With `ref`, the task need not exist here.
    let warnings = warnings_of(
        r#"
triggers:
  cron:
    type: scheduler
    cron: "0 2 * * *"
    task: only-on-the-branch
    ref: release/2.3
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("task 'only-on-the-branch' at ref 'release/2.3'")),
        "{warnings:?}"
    );
    // Cross-workspace trigger task (new): offline it is a warning…
    let yaml = r#"
triggers:
  cron:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly
"#;
    let warnings = warnings_of(yaml);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("outside this workspace")),
        "{warnings:?}"
    );
    // …with a resolver it is checked…
    let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
    let has = FakeResolver {
        actions: vec![],
        tasks: vec![("billing", "nightly")],
    };
    assert!(validate_workflow_config_with_cross_workspace_resolver(&config, &has).is_ok());
    let lacks = FakeResolver {
        actions: vec![],
        tasks: vec![],
    };
    let err = validate_workflow_config_with_cross_workspace_resolver(&config, &lacks)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("workspace 'billing' has no task 'nightly'"),
        "{err}"
    );
    // …and an invalid ref on a trigger is rejected.
    let err = error_of(
        r#"
triggers:
  cron:
    type: scheduler
    cron: "0 2 * * *"
    task: x
    ref: "a b"
"#,
    );
    assert!(err.contains("invalid `ref`"), "{err}");
}

// --- json input type (spec 2026-10-06-json-input-type § 5) ---

#[test]
fn mixed_template_in_a_json_step_input_is_an_error() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n      \
             cfg: { type: json }\n\
             tasks:\n  t:\n    flow:\n      s:\n        action: a\n        input:\n          \
             cfg: \"id {{ x }}\"\n",
    )
    .unwrap();
    let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
    assert!(err.contains("Task 't' step 's' input 'cfg'"), "{err}");
    assert!(err.contains("a json field takes a literal value"), "{err}");
}

#[test]
fn mixed_template_in_a_json_default_is_an_error() {
    let cfg = json_cfg("      cfg: { type: json, default: \"v={{ secret.X }}\" }");
    let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
    assert!(err.contains("Action 'a' input 'cfg' default"), "{err}");
}

#[test]
fn json_encode_tail_in_a_json_field_is_a_warning() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n      \
             cfg: { type: json }\n\
             tasks:\n  t:\n    flow:\n      s:\n        action: a\n        input:\n          \
             cfg: \"{{ x | json_encode() }}\"\n",
    )
    .unwrap();
    let warnings = validate_workflow_config(&cfg).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("input 'cfg'") && w.contains("json_encode")),
        "{warnings:?}"
    );
}

#[test]
fn task_step_input_is_checked_against_the_task_schema() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "actions:\n  call: { type: task, task: child }\n  a: { type: script, script: \"true\" }\n\
         tasks:\n  child:\n    input:\n      cfg: { type: json }\n    flow:\n      s: { action: a }\n  \
         parent:\n    flow:\n      c:\n        action: call\n        input:\n          cfg: \"x {{ y }}\"\n",
    )
    .unwrap();
    let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
    assert!(err.contains("Task 'parent' step 'c' input 'cfg'"), "{err}");
}

#[test]
fn ref_pinned_step_is_not_linted_against_the_local_json_schema() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n      cfg: { type: json }\n\
         tasks:\n  t:\n    flow:\n      s:\n        action: a\n        ref: main\n        input:\n          cfg: \"x {{ y }}\"\n",
    )
    .unwrap();
    validate_workflow_config(&cfg).unwrap();
}

#[test]
fn ref_pinned_task_action_is_not_linted_against_the_local_task_schema() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "actions:\n  call: { type: task, task: child, ref: main }\n  a: { type: script, script: \"true\" }\n\
         tasks:\n  child:\n    input:\n      cfg: { type: json }\n    flow:\n      s: { action: a }\n  \
         parent:\n    flow:\n      c:\n        action: call\n        input:\n          cfg: \"x {{ y }}\"\n",
    )
    .unwrap();
    validate_workflow_config(&cfg).unwrap();
}

#[test]
fn non_adjacent_numeric_secret_kinds_warn_once_each() {
    let cfg: WorkspaceConfig =
        serde_yaml::from_str("secrets:\n  S: { a: 1, b: true, c: 2 }\n").unwrap();
    let warnings = validate_workflow_config(&cfg).unwrap();
    let n = |needle: &str| warnings.iter().filter(|w| w.contains(needle)).count();
    assert_eq!(n("secret 'S' is a number"), 1, "{warnings:?}");
    assert_eq!(n("secret 'S' is a boolean"), 1, "{warnings:?}");
}

#[test]
fn numeric_secrets_are_warned_about_by_name_only() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "secrets:\n  PORT: 5432\n  FLAG: true\n  NESTED: { pin: 1234 }\n  OK: \"5432\"\n  \
         REF: \"ref+awsssm://x\"\n\
         connection_types:\n  pg:\n    host: { type: string }\n    pw: { type: integer, secret: true }\n\
         connections:\n  db: { type: pg, host: h, pw: 991234 }\n",
    )
    .unwrap();
    let warnings = validate_workflow_config(&cfg).unwrap();
    let has = |needle: &str| warnings.iter().any(|w| w.contains(needle));
    assert!(has("secret 'PORT' is a number"), "{warnings:?}");
    assert!(has("secret 'FLAG' is a boolean"), "{warnings:?}");
    assert!(has("secret 'NESTED' is a number"), "{warnings:?}");
    assert!(
        has("connection 'db' secret property 'pw' is a number"),
        "{warnings:?}"
    );
    assert!(!has("'OK'") && !has("'REF'"), "{warnings:?}");
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("5432") || w.contains("991234") || w.contains("1234")),
        "a warning must never print a secret value: {warnings:?}"
    );
}

fn json_cfg(input_yaml: &str) -> WorkspaceConfig {
    serde_yaml::from_str(&format!(
        "actions:\n  a:\n    type: script\n    script: \"true\"\n    input:\n{input_yaml}\n\
             tasks:\n  t:\n    flow:\n      s: {{ action: a }}\n"
    ))
    .unwrap()
}

#[test]
fn json_input_type_is_accepted() {
    let cfg = json_cfg("      cfg: { type: json, default: { a: 1, b: [x, y] } }");
    validate_workflow_config(&cfg).expect("json is a valid input type");
}

#[test]
fn json_input_rejects_secret_options_allow_custom_and_multiple() {
    for (opt, word) in [
        ("secret: true", "secret"),
        ("options: [a, b]", "options"),
        ("allow_custom: true", "allow_custom"),
        ("multiple: true", "multiple"),
    ] {
        let cfg = json_cfg(&format!("      cfg: {{ type: json, {opt} }}"));
        let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
        assert!(err.contains("json") && err.contains(word), "{opt}: {err}");
    }
}

#[test]
fn connection_type_named_json_is_rejected() {
    let cfg: WorkspaceConfig =
        serde_yaml::from_str("connection_types:\n  json:\n    host:\n      type: string\n")
            .unwrap();
    let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
    assert!(err.contains("reserved"), "{err}");
}

#[test]
fn approval_action_rejects_json_input() {
    let cfg: WorkspaceConfig = serde_yaml::from_str(
        "actions:\n  gate:\n    type: approval\n    message: \"ok?\"\n    input:\n      \
             reason: { type: json }\n",
    )
    .unwrap();
    let err = format!("{:#}", validate_workflow_config(&cfg).unwrap_err());
    assert!(err.contains("approval") && err.contains("json"), "{err}");
}

#[test]
fn reserved_type_names_cover_every_primitive() {
    for t in crate::template::PRIMITIVE_TYPES {
        assert!(crate::template::RESERVED_TYPE_NAMES.contains(t), "{t}");
    }
    for t in ["bool", "array", "object"] {
        assert!(crate::template::RESERVED_TYPE_NAMES.contains(&t), "{t}");
    }
}
