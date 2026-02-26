# Phase 6 — AI Agent Actions & MCP Integration

## Context

Strøm orchestrates shell, Docker, Kubernetes, and sub-task actions. Phase 6 extends this to AI agents — letting workflows call LLMs as first-class steps, with full observability, tool access, and structured output. This makes Strøm a natural fit for agentic workflows where deterministic orchestration wraps non-deterministic AI reasoning.

## Phase 6 features (in implementation order)

### 6a. `type: agent` — Single-turn LLM calls (prompt → structured output)
### 6b. Agent tool access — Multi-turn agent loops with Strøm tasks as tools
### 6c. MCP server mode — Expose Strøm tasks as MCP tools for external agents

---

## 6a. Single-Turn Agent Actions — Detailed Design

The simplest useful integration: call an LLM with a rendered prompt, get structured output back. Follows the `type: task` server-side dispatch pattern exactly.

### YAML syntax

```yaml
actions:
  classify-ticket:
    type: agent
    provider: anthropic          # references providers config
    model: claude-sonnet-4-20250514       # override provider default
    system_prompt: |
      You are a support ticket classifier. Categorize tickets into:
      bug, feature_request, question, or other.
    output_schema:               # enforced structured output
      type: object
      properties:
        category:
          type: string
          enum: [bug, feature_request, question, other]
        confidence:
          type: number
        summary:
          type: string

  generate-response:
    type: agent
    provider: anthropic
    input:
      ticket: {}
      category: {}

tasks:
  handle-ticket:
    flow:
      classify:
        action: classify-ticket
        input:
          prompt: "Classify this ticket: {{ input.ticket_text }}"

      route-bug:
        action: create-jira-issue
        depends_on: [classify]
        when: "{{ classify.output.category == 'bug' }}"
        input:
          summary: "{{ classify.output.summary }}"

      respond:
        action: generate-response
        depends_on: [classify]
        input:
          ticket: "{{ input.ticket_text }}"
          category: "{{ classify.output.category }}"
```

### Provider configuration

In `server-config.yaml`:

```yaml
agents:
  providers:
    anthropic:
      provider_type: anthropic
      api_key: "${ANTHROPIC_API_KEY}"    # or use STROEM__AGENTS__PROVIDERS__ANTHROPIC__API_KEY
      model: claude-sonnet-4-20250514             # default model for this provider
      max_tokens: 4096

    openai:
      provider_type: openai
      api_key: "${OPENAI_API_KEY}"
      model: gpt-4o
      max_tokens: 4096
      api_endpoint: https://api.openai.com/v1   # optional, for proxies/compatible APIs

    local-llama:
      provider_type: openai              # OpenAI-compatible API
      api_endpoint: http://localhost:11434/v1
      model: llama3.2
```

Provider types:
- **`anthropic`** — Anthropic Messages API (native support)
- **`openai`** — OpenAI Chat Completions API (also covers Azure OpenAI, Ollama, vLLM, any compatible endpoint)

Two provider types cover ~95% of use cases since most local/alternative LLM servers expose an OpenAI-compatible API.

### ActionDef changes

```rust
pub struct ActionDef {
    // ... existing fields ...

    /// Agent provider reference (for type: agent)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Model override (for type: agent, overrides provider default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// System prompt (for type: agent)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// Structured output JSON schema (for type: agent)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}
```

### ServerConfig changes

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    // ... existing fields ...
    #[serde(default)]
    pub agents: Option<AgentsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub providers: HashMap<String, AgentProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProviderConfig {
    pub provider_type: String,              // "anthropic" or "openai"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    pub model: String,                      // default model
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}
```

Env var overrides work automatically: `STROEM__AGENTS__PROVIDERS__ANTHROPIC__API_KEY`.

### Server-side dispatch

Mirrors `handle_task_steps()`:

```rust
pub async fn handle_agent_steps(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    agents_config: &AgentsConfig,
) -> Result<()> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let job = JobRepo::get(pool, job_id).await?;

    for step in &steps {
        if step.status != "ready" || step.action_type != "agent" {
            continue;
        }

        let action_spec: ActionDef = serde_json::from_value(step.action_spec.clone()?)?;
        let provider_name = action_spec.provider.as_ref().context("missing provider")?;
        let provider = agents_config.providers.get(provider_name)
            .context(format!("unknown agent provider: {}", provider_name))?;

        // Build render context (same as task steps)
        let ctx = build_step_render_context(&job, &steps, workspace_config);

        // Render the user prompt from step input
        let rendered_input = render_input_map(&flow_step.input, &ctx)?;
        let user_prompt = rendered_input["prompt"].as_str()
            .context("agent step requires 'prompt' in input")?;

        // Mark running (server-side, no worker)
        JobStepRepo::mark_running_server(pool, job_id, &step.step_name).await?;

        // Call LLM provider
        let model = action_spec.model.as_deref()
            .unwrap_or(&provider.model);

        let result = call_llm(
            provider,
            model,
            action_spec.system_prompt.as_deref(),
            user_prompt,
            action_spec.output_schema.as_ref(),
        ).await;

        match result {
            Ok(output) => {
                // Store structured output (or raw text as { "text": "..." })
                JobStepRepo::complete_step(pool, job_id, &step.step_name, output).await?;
            }
            Err(e) => {
                JobStepRepo::fail_step(pool, job_id, &step.step_name, &e.to_string()).await?;
            }
        }
    }

    Ok(())
}
```

### LLM provider abstraction

```rust
/// Minimal provider trait — just enough for single-turn calls
#[async_trait]
trait LlmProvider: Send + Sync {
    async fn chat(
        &self,
        model: &str,
        system: Option<&str>,
        user_message: &str,
        output_schema: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value>;
}
```

Two implementations:
- `AnthropicProvider` — calls `POST https://api.anthropic.com/v1/messages`
- `OpenAiProvider` — calls `POST {endpoint}/chat/completions`

Both use `reqwest` (already a dependency). No SDK crates needed — the APIs are simple enough for direct HTTP calls, avoiding version churn.

### Structured output

When `output_schema` is set:
- **Anthropic**: Use tool_use with a single tool matching the schema (the standard pattern for structured output)
- **OpenAI**: Use `response_format: { type: "json_schema", json_schema: { ... } }`
- Parse the response and validate against schema before storing as step output
- If no schema: store `{ "text": "raw response" }` as output

### Validation rules

```rust
"agent" => {
    if action.provider.is_none() {
        bail!("Action '{}' is type 'agent' but missing 'provider' field", name);
    }
    // Forbidden fields (same as task)
    if action.cmd.is_some() { bail!("..."); }
    if action.script.is_some() { bail!("..."); }
    if action.image.is_some() { bail!("..."); }
    if action.runner.is_some() { bail!("..."); }
    if action.manifest.is_some() { bail!("..."); }
    if action.task.is_some() { bail!("..."); }
    // Validate output_schema is valid JSON Schema (basic check)
    if let Some(ref schema) = action.output_schema {
        if !schema.is_object() {
            bail!("output_schema must be a JSON object");
        }
    }
}
```

Worker claim filter: `action_type NOT IN ('task', 'agent')`.

Tags/runner: `compute_required_tags("agent")` → empty vec. `derive_runner("agent")` → `"none"`.

### DB changes

Minimal — `action_type` already supports arbitrary strings via the `action_spec` JSONB pattern. The claim query just needs the `!= 'agent'` filter. No migration needed beyond updating the claim SQL.

### Logging

Each agent call logs to the job's log stream:
- Request: model, token count, prompt (truncated)
- Response: output, token usage, latency
- Errors: provider errors, schema validation failures

Uses `AppState::append_server_log()` (existing pattern from hooks/recovery).

### Tests

- **Unit**: `AnthropicProvider` and `OpenAiProvider` with mocked HTTP (using `wiremock`)
- **Unit**: Output schema validation, structured output parsing
- **Unit**: Validation rules for `type: agent`
- **Integration**: Agent step dispatch — mock provider, verify step output stored correctly
- **Integration**: Agent step failure — provider error → step failed with message
- **Integration**: Agent step in conditional branch — `when` + `type: agent` combo

---

## 6b. Multi-Turn Agent Loops — Design Outline

Extend `type: agent` to support multi-turn conversations where the agent can call tools.

### YAML syntax

```yaml
actions:
  investigate-incident:
    type: agent
    provider: anthropic
    model: claude-sonnet-4-20250514
    system_prompt: |
      You are an SRE investigating a production incident.
      Use the available tools to diagnose the issue.
    tools:
      - task: query-metrics       # Strøm task as a tool
      - task: check-logs
      - task: get-pod-status
    max_turns: 20                 # safety limit
    output_schema:
      type: object
      properties:
        root_cause: { type: string }
        severity: { type: string, enum: [p1, p2, p3, p4] }
        remediation: { type: string }

tasks:
  incident-response:
    flow:
      investigate:
        action: investigate-incident
        input:
          prompt: "Investigate alert: {{ input.alert_message }}"

      remediate:
        action: auto-remediate
        depends_on: [investigate]
        when: "{{ investigate.output.severity == 'p1' }}"
        input:
          steps: "{{ investigate.output.remediation }}"
```

### Architecture sketch

- `tools: Vec<ToolRef>` on `ActionDef` — references to Strøm tasks the agent can call
- Each tool maps to a task: the task's input schema becomes the tool's parameters, the task's output becomes the tool result
- Server runs the agent loop:
  1. Send prompt + tool definitions to LLM
  2. If LLM returns tool_use → create child job for referenced task → wait for completion → feed result back
  3. Repeat until LLM returns final response (no tool_use) or `max_turns` hit
- Each turn logged as a server event (visible in log stream)
- Uses Phase 5d's `suspended` status during tool execution (agent step suspends while child job runs)

### Key challenges

- **Async tool execution**: Child jobs may take minutes. Agent step must suspend and resume.
- **Context accumulation**: Each turn adds to the conversation. Need to manage context window.
- **Cost control**: Token limits, turn limits, cost tracking per step.
- **Timeout**: Overall step timeout for the entire agent loop.
- **Concurrent tool calls**: LLMs may request multiple tools at once — run child jobs in parallel.

### Tool definition generation

```rust
fn task_to_tool_def(task: &TaskDef) -> ToolDef {
    ToolDef {
        name: task.name.clone(),
        description: task.description.clone(),
        input_schema: task.input_schema_to_json_schema(),
    }
}
```

### Conversation state

Store the full conversation (messages + tool results) in a new `agent_state` JSONB column on `job_step`, or in log storage. On resume after tool completion, reconstruct the conversation and continue.

---

## 6c. MCP Server Mode — Design Outline

Expose Strøm's tasks as MCP tools so external agent frameworks can orchestrate Strøm workflows.

### Architecture

Strøm runs an MCP server (stdio or SSE transport) that exposes:

**Tools:**
- `run_task(workspace, task, input)` — trigger a task, return job ID
- `get_job_status(job_id)` — check job status
- `get_job_output(job_id)` — get completed job output
- `run_task_sync(workspace, task, input)` — trigger and wait for completion (polls internally)

**Resources:**
- `stroem://workspaces` — list workspaces
- `stroem://workspaces/{ws}/tasks` — list tasks in workspace
- `stroem://workspaces/{ws}/tasks/{name}` — task definition + input schema

### YAML syntax (server config)

```yaml
mcp:
  enabled: true
  transport: sse              # or stdio
  listen: "0.0.0.0:8081"     # for SSE transport
  auth_token: "${MCP_AUTH_TOKEN}"
```

### Use cases

- **Claude Code / Cursor**: Developer runs Strøm tasks from their IDE agent
- **LangChain / CrewAI**: External agent frameworks use Strøm for reliable, observable task execution
- **Chained platforms**: Another orchestrator triggers Strøm workflows via MCP

### Implementation sketch

- New crate: `stroem-mcp` (or module in `stroem-server`)
- Uses the `rmcp` crate (Rust MCP SDK) or direct JSON-RPC implementation
- Shares `AppState` with the HTTP server (same DB pool, workspace manager)
- SSE transport runs as a separate Axum route (`/mcp/sse`)
- Stdio transport for CLI integration (`stroem mcp serve`)

### Key challenges

- **Long-running tasks**: `run_task_sync` needs server-sent progress updates
- **Authentication**: Token-based, separate from user auth
- **Rate limiting**: Prevent agent loops from overwhelming the system
- **Schema translation**: Convert Strøm task input definitions to JSON Schema for MCP tool parameters

---

## Dependency on other phases

| Phase 6 feature | Depends on |
|---|---|
| 6a. Single-turn agent | None (can implement now) |
| 6b. Multi-turn with tools | Phase 5d (suspend/resume for tool calls) |
| 6c. MCP server mode | None (can implement independently) |

## Implementation order recommendation

1. **6a** first — small surface area, high value, proves the pattern
2. **6c** in parallel — independent, enables external agent use immediately
3. **6b** after Phase 5d — requires suspend/resume infrastructure

## Crate dependencies (6a only)

- `reqwest` — already in workspace (HTTP calls to LLM APIs)
- `wiremock` — dev dependency for testing (mock LLM responses)
- No LLM SDK crates — direct HTTP keeps dependencies minimal and avoids version churn

## Verification

```bash
cargo test --workspace
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cd ui && bun run lint && bunx tsc --noEmit
```

## Documentation

- Update `CLAUDE.md` with agent action type patterns
- Add `docs/src/content/docs/guides/agent-actions.md` user guide
- Update `docs/internal/stroem-v2-plan.md` with Phase 6 status
- Add provider configuration reference to docs
