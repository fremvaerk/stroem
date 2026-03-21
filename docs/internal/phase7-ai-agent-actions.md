# Phase 7 — AI Agent Actions & MCP Integration

## Context

Strom orchestrates shell, Docker, Kubernetes, and sub-task actions. Phase 7 adds `type: agent` — letting workflows call LLMs as first-class steps with structured output, multi-turn tool loops, and MCP client integration. This makes Strom a natural fit for agentic workflows where deterministic orchestration wraps non-deterministic AI reasoning.

**Key design decisions:**
- **Merged single/multi-turn**: One `type: agent` with optional `tools` field. No tools = single-turn.
- **Dedicated `prompt` field**: Tera template on ActionDef, not via `input.prompt`.
- **Per-action LLM params**: `temperature`, `max_tokens` overridable per action.
- **Token tracking**: Output `_meta` with model, tokens, latency.
- **Built-in retry**: Exponential backoff for transient LLM errors.
- **rig-core framework**: Use rig-core (v0.33+) for provider abstraction, tool calling, and token tracking.
- **MCP client tools**: Agent steps can call external MCP servers (configured in workspace YAML).
- **Prompt-only context**: Agent sees only what's in the rendered prompt/system_prompt.
- **Multi-turn via DB state**: Conversation persisted in `agent_state` JSONB.
- **Server-side dispatch**: Like `type: task`, workers never claim agent steps.
- **Built-in `ask_user` tool**: LLM can request human input, reuses Phase 5d approval gate infrastructure (`suspended` status, approve API, `on_suspended` hooks).

## YAML Syntax

```yaml
# Provider config (server-config.yaml)
agents:
  providers:
    anthropic:
      type: anthropic
      api_key: "${ANTHROPIC_API_KEY}"
      model: claude-sonnet-4-20250514
      max_tokens: 4096
      max_retries: 3
    openai:
      type: openai
      api_key: "${OPENAI_API_KEY}"
      model: gpt-4o
      api_endpoint: https://api.openai.com/v1  # also works for Ollama, vLLM, etc.

# Workflow YAML
actions:
  # Single-turn (no tools)
  classify-ticket:
    type: agent
    provider: anthropic
    model: claude-sonnet-4-20250514
    temperature: 0.1
    max_tokens: 256
    system_prompt: |
      You are a support ticket classifier.
    prompt: |
      Classify this ticket: {{ input.ticket_text }}
    output:
      type: object
      properties:
        category:
          type: string
          enum: [bug, feature_request, question, other]
        confidence:
          type: number

  # Multi-turn with task tools
  investigate-incident:
    type: agent
    provider: anthropic
    system_prompt: |
      You are an SRE investigating a production incident.
    prompt: |
      Investigate alert: {{ input.alert_message }}
    tools:
      - task: query-metrics
      - task: check-logs
      - task: get-pod-status
    max_turns: 20
    output:
      type: object
      properties:
        root_cause: { type: string }
        severity: { type: string, enum: [p1, p2, p3, p4] }

  # Multi-turn with MCP client tools
  research-topic:
    type: agent
    provider: anthropic
    prompt: "Research: {{ input.topic }}"
    tools:
      - task: search-db
      - mcp: brave-search

# MCP servers (workspace YAML)
mcp_servers:
  brave-search:
    transport: sse
    url: http://localhost:3001/sse

tasks:
  handle-ticket:
    flow:
      classify:
        action: classify-ticket
        input:
          ticket_text: "{{ input.text }}"
      route-bug:
        action: create-jira-issue
        depends_on: [classify]
        when: "{{ classify.output.category == 'bug' }}"
```

## Output Shape

```json
{
  "category": "bug",
  "confidence": 0.95,
  "_meta": {
    "model": "claude-sonnet-4-20250514",
    "provider": "anthropic",
    "input_tokens": 342,
    "output_tokens": 87,
    "latency_ms": 1250,
    "turns": 1
  }
}
```

Without `output`: `{ "text": "...", "_meta": { ... } }`.
Downstream: `{{ classify.output.category }}` works as usual.

---

## Implementation Phases

### Phase A: Core Agent Infrastructure (single-turn)

#### A1. ActionDef fields (`crates/stroem-common/src/models/workflow.rs:86-162`)

Add to `ActionDef`:
- `provider: Option<String>` — references provider in server config
- `model: Option<String>` — overrides provider default
- `system_prompt: Option<String>` — Tera template
- `prompt: Option<String>` — Tera template (required for agent)
- `output: Option<serde_json::Value>` — JSON Schema for structured output
- `temperature: Option<f32>` — 0.0-2.0
- `max_tokens: Option<u32>` — overrides provider default
- `tools: Vec<AgentToolRef>` — tool references (Phase B, add to model now)
- `max_turns: Option<u32>` — safety limit (Phase B, add to model now)

New enum (forward-compatible for Phase C):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentToolRef {
    Task { task: String },
    Mcp { mcp: String },
}
```

#### A2. ActionType enum (`crates/stroem-common/src/models/job.rs:~152`)

Add `Agent` variant with Display/FromStr/serde.

#### A3. ServerConfig (`crates/stroem-server/src/config.rs:289-309`)

Add `agents: Option<AgentsConfig>` to `ServerConfig`.

```rust
pub struct AgentsConfig {
    pub providers: HashMap<String, AgentProviderConfig>,
}
pub struct AgentProviderConfig {
    pub type: String,     // "anthropic", "openai", "gemini", "groq", etc. (19 total)
    pub api_key: Option<String>,
    pub api_endpoint: Option<String>,
    pub model: String,
    pub max_tokens: u32,           // default 4096
    pub temperature: Option<f32>,
    pub max_retries: u32,          // default 3
}
```

Env var overrides work automatically: `STROEM__AGENTS__PROVIDERS__ANTHROPIC__API_KEY`.

#### A4. DB migration (`crates/stroem-db/migrations/023_agent_type.sql`)

- Update `action_type` CHECK constraint to include `'agent'`
- Add `agent_state JSONB` column to `job_step` (nullable, for Phase B)
- Update `JobStepRow`, `NewJobStep`, `STEP_COLUMNS`, INSERT SQL

#### A5. Worker claim filter (`crates/stroem-db/src/repos/job_step.rs:222`)

Change `action_type != 'task'` to `action_type NOT IN ('task', 'agent')`.
Same in `get_unmatched_ready_steps` (recovery Phase 4 should skip agent steps).

#### A6. Validation (`crates/stroem-common/src/validation.rs`)

New `validate_agent_action()`:
- Required: `provider`, `prompt`
- Forbidden: `cmd`, `script`, `source`, `image`, `runner`, `manifest`, `task`, `language`, `entrypoint`, `dependencies`, `interpreter`
- `output` must be a JSON object
- `temperature` must be 0.0-2.0
- `prompt` and `system_prompt` must be valid Tera syntax

Update `compute_required_tags("agent")` -> `[]`, `derive_runner("agent")` -> `"none"`.

#### A7. LLM integration via rig-core (NEW: `crates/stroem-server/src/agent/`)

Uses **rig-core** (v0.33+) — a Rust LLM framework with 20+ providers, built-in tool calling, and token tracking. This replaces hand-rolled reqwest HTTP calls.

```
agent/
  mod.rs          -- module root, provider factory, re-exports
  dispatch.rs     -- handle_agent_steps(), structured output helpers
  tools.rs        -- Phase B: StromTaskTool impl of rig's Tool trait
  mcp_client.rs   -- Phase C: MCP client tool integration
```

**Why rig-core over direct HTTP:**
- Unified API across Anthropic, OpenAI, and 20+ other providers
- Built-in tool calling (critical for Phase B multi-turn)
- Token usage tracking on all responses
- Custom endpoint support for OpenAI-compatible APIs (Ollama, vLLM)
- Battle-tested, actively maintained (weekly releases)
- Reduces ~500-800 lines of provider boilerplate

**Provider creation from config:**
```rust
fn create_rig_client(config: &AgentProviderConfig) -> Result<Box<dyn CompletionModel>> {
    match config.type.as_str() {
        "anthropic" => {
            let client = rig::providers::anthropic::ClientBuilder::new(&api_key)
                .base_url(config.api_endpoint.as_deref().unwrap_or("https://api.anthropic.com"))
                .build();
            Ok(Box::new(client.completion_model(&config.model)))
        }
        "openai" => {
            let mut builder = rig::providers::openai::Client::new(&api_key);
            if let Some(ref endpoint) = config.api_endpoint {
                builder = builder.base_url(endpoint);
            }
            Ok(Box::new(builder.completion_model(&config.model)))
        }
        "gemini" => {
            let client = rig::providers::gemini::ClientBuilder::new(&api_key).build();
            Ok(Box::new(client.completion_model(&config.model)))
        }
        "groq" => {
            let client = rig::providers::groq::ClientBuilder::new(&api_key).build();
            Ok(Box::new(client.completion_model(&config.model)))
        }
        // ... support all 19 providers via rig-core
        other => bail!("Unknown provider type: {}", other),
    }
}
```

**Structured output via JSON Schema:**
The YAML `output` is already JSON Schema syntax (`type`, `properties`, `enum`, etc.). At dispatch time, normalize it into a full JSON Schema (add `title`, ensure `type: "object"` at root) and pass it to rig's extractor API. Rig handles the provider-specific mechanics:
- Anthropic: tool_use pattern with forced tool choice
- OpenAI: `response_format` with `json_schema`

For unstructured output (no `output`): use rig's standard `.prompt()` API directly.

**Retry logic**: Wrap rig calls with a shared retry helper. Retry on transient errors (429, 500, 502, 503, 529). Exponential backoff 1s/2s/4s with jitter. Up to `max_retries` per provider config.

#### A8. Dispatch (`crates/stroem-server/src/agent/dispatch.rs`)

`handle_agent_steps()` mirrors `handle_task_steps()`:
1. Find ready agent steps
2. Deserialize `action_spec`, look up provider
3. Build render context via `build_step_render_context()`
4. Render `prompt` and `system_prompt` as Tera templates
5. Mark step running (server-side, no worker)
6. Call LLM via provider
7. On success: store output + `_meta`, mark completed
8. On failure: mark failed with error message
9. Call `orchestrate_after_step()` to trigger downstream

**Integration points** -- call `handle_agent_steps` alongside `handle_task_steps`:
- `job_creator.rs:204` -- after job creation (for initially-ready agent steps)
- `job_recovery.rs` `orchestrate_after_step()` -- after orchestration
- `job_recovery.rs` `propagate_to_parent()` -- after child completion

The `create_job_for_task_inner` needs `&AppState` (or at least `agents_config`) threaded through for initially-ready dispatch.

#### A9. Logging

Use `AppState::append_server_log()`:
```
[agent] Step 'classify': calling anthropic/claude-sonnet-4-20250514 (342 input tokens)
[agent] Step 'classify': completed in 1250ms (87 output tokens)
[agent] Step 'classify': failed -- 429 Too Many Requests after 3 retries
```

#### A10. Tests

**Unit** (stroem-common):
- Validation: valid agent, missing provider, missing prompt, forbidden fields, bad temperature, bad Tera
- `compute_required_tags("agent")` -> `[]`
- `derive_runner("agent")` -> `"none"`
- YAML parsing: agent action, inline agent step

**Unit** (stroem-server, using `wiremock`):
- rig-core Anthropic client: text response, structured output, retry on 429, error propagation
- rig-core OpenAI client: text response, structured output, retry on 500, custom endpoint
- Provider factory: create_rig_client with different provider_type values

**Integration** (testcontainers Postgres + wiremock):
- Agent step dispatch: mock LLM -> output stored correctly
- Agent failure: provider error -> step fails
- Agent with `when` condition: conditional + agent combo
- Agent in DAG: agent output used by downstream step via templating

**Dev dependency**: Add `wiremock = "0.6"` to stroem-server.

#### A11. Documentation

- Update `CLAUDE.md`: agent action type description
- Create `docs/src/content/docs/guides/agent-actions.md`: user guide
- Update `docs/internal/stroem-v2-plan.md`: Phase 7 status
- Update `docs/internal/TODO.md`: Phase 7 items
- Regenerate `docs/public/llms.txt`

---

### Phase B: Multi-Turn Conversations, Task Tools & ask_user

**Depends on Phase 5d (approval gates)** -- reuses `suspended` status, approve API, `on_suspended` hooks.

`max_turns` is independent of `tools` -- an agent can have multiple turns even without tools (e.g., self-refinement). Tools add Strom tasks as callable tools. A built-in `ask_user` tool lets the LLM request human input.

#### B1. Built-in `ask_user` tool

The LLM automatically gets an `ask_user` tool. When called:
1. Agent step enters `suspended` status (Phase 5d infrastructure)
2. The question is stored in step metadata, visible in UI approval card
3. `on_suspended` hooks fire (Slack notification, etc.)
4. User responds via `POST /api/jobs/{id}/steps/{step}/approve` (same as approval gates)
5. Agent step resumes -- user's response fed back as tool result in conversation
6. LLM continues with the new information

```yaml
# The LLM sees this tool automatically (no YAML config needed):
# ask_user(question: str) -> str
#
# Example flow:
# LLM: "I need to investigate. What severity is this incident?"
#   -> ask_user("What severity is this incident?")
#   -> step suspended, user notified
#   -> user responds "P1"
#   -> agent resumes, LLM gets "P1" as tool result
```

#### B2. Agent dispatch loop algorithm

**Initial dispatch** (step becomes ready):
1. Render prompt, generate tool definitions from referenced tasks' input schemas (if any) + built-in `ask_user` tool
2. Call LLM with messages + tool definitions
3. If final answer (no tool calls):
   - If `max_turns > 1` and turn < max_turns: feed response back as conversation history, call LLM again
   - Otherwise: mark step completed, done
4. If tool calls:
   - `ask_user` -> step enters `suspended`, saves conversation to `agent_state`
   - Strom task tools -> create child jobs, save conversation to `agent_state`, step stays `running`
   - Both can appear in same turn: execute task tools first, then suspend for user input

**Resumption** (user responds or child job completes):
1. **User response** (via approve API): load `agent_state`, append user's answer as tool result, call LLM again
2. **Child job completion**: `propagate_to_parent()` detects agent step, records tool result in `agent_state`, checks if all pending tool calls resolved
3. When all resolved: call LLM with updated conversation (next turn)
4. Repeat until final answer or `max_turns` exceeded

**`agent_state` JSONB shape:**
```json
{
  "messages": [...],
  "turn": 3,
  "pending_tool_calls": [
    {"tool_call_id": "tc_1", "task": "query-metrics", "child_job_id": "uuid"},
    {"tool_call_id": "tc_2", "type": "ask_user", "question": "What severity?"}
  ]
}
```

#### B3. Key changes

- **rig Tool trait**: `StromTaskTool` wraps Strom tasks as rig tools. `AskUserTool` is a built-in tool that triggers step suspension.
- `propagate_to_parent()` in `job_recovery.rs`: agent-aware branch -- if parent is agent with `agent_state`, call `handle_agent_tool_result()`
- Approve API handler: detect when approved step is an agent step -> resume agent loop instead of normal completion
- `dispatch.rs`: use rig's agent builder with `.tool()` for each referenced task + `ask_user`
- Concurrent tool calls: multiple child jobs in parallel, all tracked in `pending_tool_calls`
- `update_agent_state()` repo method

#### B4. Safety limits

- `max_turns` default 10, max 100
- Step timeout (`FlowStep.timeout`) applies to entire agent loop (including suspended time)
- Token budget tracked in `_meta` (cumulative across turns)

---

### Phase C: MCP Client Tools

#### C1. Workspace YAML

```yaml
mcp_servers:
  brave-search:
    transport: sse
    url: http://localhost:3001/sse
  local-db:
    transport: stdio
    command: npx
    args: ["-y", "@modelcontextprotocol/server-sqlite"]
```

New `McpServerDef` struct, `mcp_servers: HashMap` on `WorkflowConfig`/`WorkspaceConfig`.

#### C2. MCP client integration (`crates/stroem-server/src/agent/mcp_client.rs`)

- **rig has MCP tool support** -- rig-core includes examples of using MCP servers as tools
- Connects to configured MCP servers (lazy, on first use)
- Discovers tools via `tools/list`, wraps them as rig `Tool` impls
- Calls tools via `tools/call`
- Uses `rmcp` crate client features (already a dependency for MCP server) + rig's MCP integration

#### C3. Mixed tool execution

In the agent loop, when LLM returns tool calls:
- **MCP tools**: called synchronously (inline), result fed back immediately
- **Task tools**: create child jobs (async), wait for completion via propagation
- **Mixed**: execute MCP tools first, create task child jobs, wait for tasks, combine all results

---

## Implementation order

**5d -> 7A -> 7B -> 7C**

| Phase | Depends on | Key deliverable |
|-------|-----------|-----------------|
| 5d. Approval gates | None | `suspended` status, approve API, `on_suspended` hooks, UI card |
| 7A. Single-turn agent | None (parallel with 5d) | `type: agent`, rig-core providers, structured output |
| 7B. Multi-turn + tools + ask_user | 5d + 7A | Tool calling loop, `ask_user` via suspended, `agent_state` |
| 7C. MCP client tools | 7B | Workspace MCP servers, mixed sync/async tools |

## Files Summary

### Phase A -- Create
| File | Purpose |
|------|---------|
| `crates/stroem-server/src/agent/mod.rs` | Module root, provider factory |
| `crates/stroem-server/src/agent/dispatch.rs` | handle_agent_steps, structured output |
| `crates/stroem-db/migrations/023_agent_type.sql` | Migration |
| `docs/src/content/docs/guides/agent-actions.md` | User guide |

### Phase A -- Modify
| File | Change |
|------|--------|
| `crates/stroem-common/src/models/workflow.rs` | ActionDef + AgentToolRef |
| `crates/stroem-common/src/models/job.rs` | ActionType::Agent |
| `crates/stroem-common/src/validation.rs` | validate_agent_action + tag/runner |
| `crates/stroem-server/src/config.rs` | AgentsConfig + AgentProviderConfig |
| `crates/stroem-server/src/lib.rs` | `pub mod agent;` |
| `crates/stroem-server/src/job_creator.rs` | Call handle_agent_steps |
| `crates/stroem-server/src/job_recovery.rs` | Call handle_agent_steps |
| `crates/stroem-db/src/repos/job_step.rs` | agent_state, claim filter |
| `crates/stroem-server/Cargo.toml` | rig-core + wiremock dev-dep |
| `Cargo.toml` | rig-core workspace dep |
| `CLAUDE.md` | Agent action docs |

### Phase B -- Create
| File | Purpose |
|------|---------|
| `crates/stroem-server/src/agent/tools.rs` | StromTaskTool + AskUserTool (rig Tool trait impls) |

### Phase C -- Create
| File | Purpose |
|------|---------|
| `crates/stroem-server/src/agent/mcp_client.rs` | MCP client manager |

## Crate dependencies

- `rig-core` -- LLM framework (Anthropic, OpenAI, 20+ providers, tool calling, token tracking)
- `wiremock` -- dev dependency for testing (mock LLM responses)
- `rmcp` client features -- Phase C only (already a dep for MCP server)
- `reqwest` -- already in workspace (used by rig internally)
- `schemars` -- may be needed for rig integration (evaluate during Phase A)

## Verification

```bash
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
cd ui && bun run lint && bunx tsc --noEmit
```

Manual verification:
1. Add agents config to server-config.yaml with Anthropic provider
2. Create a workflow with type: agent action
3. Trigger the task, verify step runs and output is stored
4. Check _server logs for agent call details
5. Verify downstream steps can access `{{ step.output.field }}`
