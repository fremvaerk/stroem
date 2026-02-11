# stroem-common

Shared types, models, DAG walker, templating, and validation for Strøm v2.

## Features

### Models (`src/models/`)

- **workflow.rs**: YAML workflow definitions
  - `WorkflowConfig`: Top-level workflow file (actions, tasks, triggers)
  - `ActionDef`: Reusable execution units (shell, docker, pod types)
  - `TaskDef`: Multi-step workflows with DAG
  - `TriggerDef`: Automated task execution (scheduler, webhook)
  - `WorkspaceConfig`: Merged configuration from multiple YAML files

- **job.rs**: Runtime job models
  - `Job`: Task execution instance
  - `JobStep`: Individual step execution
  - `JobStatus`, `StepStatus`, `SourceType`: Enums for state tracking

- **auth.rs**: Auth models (stub for Phase 2)

### DAG Walker (`src/dag.rs`)

- `ready_steps()`: Returns steps whose dependencies are all completed
- `validate_dag()`: Checks for cycles and returns topological order
- Uses Kahn's algorithm for cycle detection and topological sort

### Template Renderer (`src/template.rs`)

- `render_template()`: Renders a Tera template string against JSON context
- `render_input_map()`: Renders all string values in an input map
- Non-string values pass through unchanged
- Supports nested context access and Tera filters/conditionals

### Validation (`src/validation.rs`)

- `validate_workflow_config()`: Validates entire workflow
  - Action type validation (shell needs cmd/script, docker/pod need image)
  - Flow step references to existing actions
  - Dependency references within flow
  - DAG cycle detection
  - Trigger references to existing tasks
  - Scheduler triggers have cron expressions

## Usage

```rust
use stroem_common::models::workflow::WorkflowConfig;
use stroem_common::validation;
use stroem_common::dag;

// Parse workflow YAML
let config: WorkflowConfig = serde_yml::from_str(yaml)?;

// Validate
let warnings = validation::validate_workflow_config(&config)?;

// Get topological order
let task = config.tasks.get("my-task").unwrap();
let order = dag::validate_dag(&task.flow)?;

// Find ready steps
let ready = dag::ready_steps(&task.flow, &completed);
```

## Testing

```bash
# Run all tests (57 tests: 53 unit + 4 integration)
cargo test -p stroem-common

# Run with lint checks
cargo clippy -p stroem-common -- -D warnings

# Run example
cargo run -p stroem-common --example parse_workflow
```

## Dependencies

- `serde` + `serde_json` + `serde_yml`: Serialization and YAML parsing
- `tera`: Template rendering
- `anyhow`: Error handling
- `chrono`: Date/time types
- `uuid`: Job IDs
- `tracing`: Logging
