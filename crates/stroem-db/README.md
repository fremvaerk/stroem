# stroem-db

Database layer for Strøm v2 workflow orchestration platform.

## Overview

This crate provides PostgreSQL integration for Strøm using `sqlx` with runtime queries. It includes:

- Database migrations (DDL for worker, job, and job_step tables)
- Repository pattern for data access
- Connection pool management
- Concurrency-safe job claiming with `SELECT FOR UPDATE SKIP LOCKED`

## Architecture

### Migrations

Located in `migrations/001_initial.sql`. Includes:

- `worker` table - Worker registration and heartbeat tracking
- `job` table - Job queue with workspace, task, and status tracking
- `job_step` table - Individual step execution state for distributed mode
- Indexes for efficient querying

Auth tables (user, user_auth_link, refresh_token, api_key) are commented out for Phase 2/3.

### Repositories

#### `WorkerRepo`

Worker registration and heartbeat management:

```rust
WorkerRepo::register(pool, worker_id, name, &capabilities).await?;
WorkerRepo::heartbeat(pool, worker_id).await?;
WorkerRepo::get(pool, worker_id).await?;
```

#### `JobRepo`

Job lifecycle management:

```rust
// Create a job
let job_id = JobRepo::create(
    pool,
    workspace,
    task_name,
    mode,
    input,
    source_type,
    source_id
).await?;

// Query jobs
JobRepo::get(pool, job_id).await?;
JobRepo::list(pool, workspace, limit, offset).await?;

// Update job state
JobRepo::mark_running(pool, job_id, worker_id).await?;
JobRepo::mark_completed(pool, job_id, output).await?;
JobRepo::mark_failed(pool, job_id).await?;
JobRepo::set_log_path(pool, job_id, log_path).await?;
```

#### `JobStepRepo`

Step-level operations for distributed execution:

```rust
// Create steps for a job
JobStepRepo::create_steps(pool, &steps).await?;

// Claim a ready step (concurrency-safe)
let step = JobStepRepo::claim_ready_step(pool, &capabilities, worker_id).await?;

// Update step state
JobStepRepo::mark_running(pool, job_id, step_name, worker_id).await?;
JobStepRepo::mark_completed(pool, job_id, step_name, output).await?;
JobStepRepo::mark_failed(pool, job_id, step_name, error).await?;

// Query steps
JobStepRepo::get_steps_for_job(pool, job_id).await?;
JobStepRepo::get_ready_steps(pool, job_id).await?;

// Orchestration helpers
JobStepRepo::all_steps_terminal(pool, job_id).await?;
JobStepRepo::any_step_failed(pool, job_id).await?;
JobStepRepo::promote_ready_steps(pool, job_id, &flow).await?;
```

The `promote_ready_steps` function implements the orchestrator's dependency resolution logic:
1. Fetches all steps for a job
2. Finds pending steps whose dependencies are all completed
3. Promotes them to `ready` status
4. Returns the names of newly promoted steps

### Concurrency Safety

The `claim_ready_step` function uses PostgreSQL's `FOR UPDATE SKIP LOCKED` to ensure multiple workers can safely claim steps concurrently without conflicts:

```sql
UPDATE job_step SET status = 'running', worker_id = $2, started_at = NOW()
WHERE (job_id, step_name) = (
    SELECT job_id, step_name FROM job_step
    WHERE status = 'ready' AND action_type = ANY($1)
    ORDER BY job_id, step_name
    FOR UPDATE SKIP LOCKED
    LIMIT 1
)
RETURNING *;
```

## Usage

### Setup

```rust
use stroem_db::{create_pool, run_migrations};

let pool = create_pool("postgres://user:pass@localhost/db").await?;
run_migrations(&pool).await?;
```

### Creating and Executing a Job

```rust
use stroem_db::{JobRepo, JobStepRepo, NewJobStep};
use uuid::Uuid;

// Create a job
let job_id = JobRepo::create(
    &pool,
    "default",
    "my-task",
    "distributed",
    Some(serde_json::json!({"env": "prod"})),
    "user",
    None,
).await?;

// Create steps
let steps = vec![
    NewJobStep {
        job_id,
        step_name: "checkout".to_string(),
        action_name: "git-clone".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: Some(serde_json::json!({"cmd": "git clone ..."})),
        input: None,
        status: "ready".to_string(),
    },
    NewJobStep {
        job_id,
        step_name: "build".to_string(),
        action_name: "npm-build".to_string(),
        action_type: "shell".to_string(),
        action_image: Some("node:20".to_string()),
        action_spec: Some(serde_json::json!({"cmd": "npm ci && npm run build"})),
        input: None,
        status: "pending".to_string(),
    },
];

JobStepRepo::create_steps(&pool, &steps).await?;

// Worker claims a step
let worker_id = Uuid::new_v4();
let step = JobStepRepo::claim_ready_step(
    &pool,
    &["shell".to_string()],
    worker_id
).await?;

// Execute step, then mark completed
JobStepRepo::mark_completed(&pool, job_id, &step.step_name, None).await?;

// Promote dependent steps
JobStepRepo::promote_ready_steps(&pool, job_id, &flow).await?;
```

## Testing

Integration tests use `testcontainers` to spin up a real PostgreSQL instance:

```bash
# Run tests (requires Docker)
cargo test -p stroem-db

# Run specific test
cargo test -p stroem-db test_claim_concurrency

# Run with logs
RUST_LOG=debug cargo test -p stroem-db
```

### Test Coverage

- `test_create_and_get_job` - Basic job creation and retrieval
- `test_list_jobs` - Pagination and filtering
- `test_create_steps_and_claim` - Step creation and claiming
- `test_claim_concurrency` - Concurrent claim safety (10 workers, 10 steps, no double-claims)
- `test_step_lifecycle` - Complete step state transition
- `test_promote_ready_steps` - DAG dependency resolution
- `test_worker_register_and_heartbeat` - Worker management
- `test_all_steps_terminal` - Job completion detection
- `test_any_step_failed` - Failure detection

## Design Decisions

### Runtime Queries vs Compile-Time

We use `sqlx::query()` and `sqlx::query_as()` (runtime) instead of `sqlx::query!()` (compile-time macros) because:

1. No database required at compile time
2. Simpler CI/CD pipeline
3. Easier local development
4. The project already uses strong typing via `FromRow` derives

### String Status Fields

Job and step status are stored as TEXT with CHECK constraints rather than enums. This provides:

- Forward compatibility (easy to add new statuses)
- Simpler queries (no need to cast)
- Clear database schema (constraint shows valid values)

The application layer uses Rust enums for type safety.

### Batch Insert for Steps

`create_steps` uses a single multi-value INSERT rather than individual inserts for performance when creating jobs with many steps (e.g., large CI/CD pipelines).

## Dependencies

- `sqlx` - Async PostgreSQL driver with migrations
- `anyhow` - Error handling
- `uuid` - Job and worker IDs
- `serde_json` - JSONB field serialization
- `chrono` - Timestamp handling
- `stroem-common` - Shared workflow types

## Future Work (Phase 2+)

- User authentication tables and repository
- API key management
- Audit logging
- Job metrics and analytics
- Advanced querying (full-text search, complex filters)
