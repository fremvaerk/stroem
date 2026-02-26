use std::collections::HashSet;
use stroem_common::dag;
use stroem_common::models::workflow::WorkflowConfig;
use stroem_common::validation;

fn main() {
    let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }

  shout:
    type: shell
    cmd: "echo {{ input.message }} | tr '[:lower:]' '[:upper:]'"
    input:
      message: { type: string, required: true }

tasks:
  hello-world:
    mode: distributed
    input:
      name:
        type: string
        default: "World"
    flow:
      say-hello:
        action: greet
        input:
          name: "{{ input.name }}"
      shout-it:
        action: shout
        depends_on: [say-hello]
        input:
          message: "{{ say-hello.output.greeting }}"

triggers:
  every-minute:
    type: scheduler
    cron: "0 * * * * *"
    task: hello-world
    input:
      name: "Cron"
    enabled: false
"#;

    // Parse the workflow
    println!("Parsing workflow YAML...");
    let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();

    // Validate the workflow
    println!("Validating workflow...");
    let warnings = validation::validate_workflow_config(&config).unwrap();

    println!("\nWorkflow validation successful!");
    if !warnings.is_empty() {
        println!("Warnings: {:?}", warnings);
    }

    // Display parsed content
    println!("\nActions: {}", config.actions.len());
    for (name, action) in &config.actions {
        println!("  - {} (type: {})", name, action.action_type);
    }

    println!("\nTasks: {}", config.tasks.len());
    for (name, task) in &config.tasks {
        println!(
            "  - {} (mode: {}, steps: {})",
            name,
            task.mode,
            task.flow.len()
        );

        // Validate DAG and show topological order
        let topo_order = dag::validate_dag(&task.flow).unwrap();
        println!("    Execution order: {:?}", topo_order);

        // Show ready steps at start
        let completed = HashSet::new();
        let ready = dag::ready_steps(&task.flow, &completed);
        println!("    Initially ready: {:?}", ready);
    }

    println!("\nTriggers: {}", config.triggers.len());
    for (name, trigger) in &config.triggers {
        println!(
            "  - {} (type: {}, task: {}, enabled: {})",
            name,
            trigger.trigger_type_str(),
            trigger.task(),
            trigger.enabled()
        );
        if let stroem_common::models::workflow::TriggerDef::Scheduler { cron, .. } = trigger {
            println!("    Schedule: {}", cron);
        }
    }
}
