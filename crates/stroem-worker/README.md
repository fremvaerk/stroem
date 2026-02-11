# stroem-worker

Worker process for the Strøm workflow orchestration platform. The worker polls the Strøm server for jobs, executes workflow steps, and streams logs back to the server.

## Features

- Polls the server for available workflow steps
- Executes shell-based actions using `stroem-runner`
- Manages concurrent step execution with configurable limits
- Streams logs in real-time to the server
- Sends periodic heartbeats to maintain worker status
- Graceful error handling and reporting

## Configuration

Create a `worker-config.yaml` file (or set the path via `STROEM_CONFIG` environment variable):

```yaml
server_url: "http://localhost:8080"
worker_token: "your-worker-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_dir: "/tmp/stroem-workspace"
capabilities:
  - shell
```

### Configuration Options

- `server_url`: URL of the Strøm server to connect to
- `worker_token`: Authentication token (shared secret) for worker API access
- `worker_name`: Unique name for this worker instance
- `max_concurrent`: Maximum number of steps to execute concurrently
- `poll_interval_secs`: How often to poll for new jobs when no work is available
- `workspace_dir`: Directory where step execution will take place
- `capabilities`: List of action types this worker can execute (currently `["shell"]`)

## Running

```bash
# Using default config path (worker-config.yaml)
cargo run -p stroem-worker

# Using custom config path
STROEM_CONFIG=/path/to/config.yaml cargo run -p stroem-worker
```

## Architecture

The worker consists of several modules:

- **config**: Configuration parsing from YAML
- **client**: HTTP client for server communication (register, heartbeat, claim, report)
- **executor**: Step execution using `stroem-runner` with log streaming
- **poller**: Main event loop that coordinates registration, polling, and execution

## API Endpoints Used

The worker communicates with the following server endpoints:

- `POST /worker/register` - Register worker and receive worker ID
- `POST /worker/heartbeat` - Send periodic heartbeat
- `POST /worker/jobs/claim` - Claim an available step to execute
- `POST /worker/jobs/{id}/steps/{step}/start` - Report step execution started
- `POST /worker/jobs/{id}/steps/{step}/complete` - Report step completion with results
- `POST /worker/jobs/{id}/logs` - Push log lines to server

All requests include an `Authorization: Bearer {token}` header.

## Testing

```bash
# Run unit tests
cargo test -p stroem-worker

# Run with output
cargo test -p stroem-worker -- --nocapture
```
