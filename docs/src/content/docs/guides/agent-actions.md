---
title: Agent Actions
description: Call LLMs as workflow steps with structured output
---

Agent actions let you call LLMs (Anthropic, OpenAI) as first-class workflow steps. Generate text, classify content, extract data, or feed LLM responses directly into downstream steps.

## Quick Start

### 1. Configure a provider

In your `server-config.yaml`:

```yaml
agents:
  providers:
    - id: anthropic-main
      provider_type: anthropic
      api_key: "{{ env.ANTHROPIC_API_KEY }}"
      model: claude-opus-4-1-20250805
      max_tokens: 2048
      temperature: 0.7
      max_retries: 2

    - id: openai-gpt4
      provider_type: openai
      api_key: "{{ env.OPENAI_API_KEY }}"
      model: gpt-4o
      max_tokens: 1024
```

### 2. Define an agent action

In your workflow YAML:

```yaml
actions:
  classify-ticket:
    type: agent
    provider: anthropic-main
    system_prompt: |
      You are a support ticket classifier.
      Classify tickets as bug, feature_request, or question.
    prompt: |
      Classify this ticket:
      Subject: {{ input.subject }}
      Body: {{ input.body }}
    output_schema:
      type: object
      properties:
        category: { type: string, enum: [bug, feature_request, question] }
        confidence: { type: number }
        summary: { type: string }
      required: [category, confidence]
    input:
      subject: { type: string, required: true }
      body: { type: string, required: true }
```

### 3. Use in a flow

```yaml
tasks:
  support-workflow:
    input:
      ticket_subject: { type: string }
      ticket_body: { type: string }
    flow:
      classify:
        action: classify-ticket
        input:
          subject: "{{ input.ticket_subject }}"
          body: "{{ input.ticket_body }}"

      route-bug:
        action: escalate-to-engineering
        depends_on: [classify]
        when: "{{ classify.output.category == 'bug' }}"
        input:
          ticket_id: "{{ input.ticket_id }}"

      route-feature:
        action: add-to-roadmap
        depends_on: [classify]
        when: "{{ classify.output.category == 'feature_request' }}"

      route-question:
        action: send-faq-response
        depends_on: [classify]
        when: "{{ classify.output.category == 'question' }}"
```

## Provider Configuration

Agent actions dispatch to a configured provider. The server loads providers from `server-config.yaml`.

### Provider Types

**Anthropic:**

```yaml
agents:
  providers:
    - id: anthropic-main
      provider_type: anthropic
      api_key: "{{ env.ANTHROPIC_API_KEY }}"
      model: claude-opus-4-1-20250805
      max_tokens: 2048
      temperature: 0.7
      max_retries: 2
```

**OpenAI:**

```yaml
agents:
  providers:
    - id: openai-gpt4
      provider_type: openai
      api_key: "{{ env.OPENAI_API_KEY }}"
      model: gpt-4o
      max_tokens: 1024
      temperature: 0.5
```

**Custom Endpoint (OpenAI-compatible):**

```yaml
agents:
  providers:
    - id: local-llm
      provider_type: openai
      api_key: "{{ env.CUSTOM_API_KEY }}"
      api_endpoint: "http://localhost:8000/v1"
      model: local-model
```

### Configuration Fields

| Field | Required | Description |
|-------|----------|-------------|
| `id` | Yes | Unique provider identifier used in actions |
| `provider_type` | Yes | `anthropic` or `openai` |
| `api_key` | Yes | API key (supports env var templating with `{{ env.VAR_NAME }}`) |
| `model` | Yes | Model identifier (e.g., `claude-opus-4-1-20250805`, `gpt-4o`) |
| `api_endpoint` | No | Custom endpoint URL (OpenAI-compatible only) |
| `max_tokens` | No | Default max completion tokens (can be overridden per action) |
| `temperature` | No | Default sampling temperature (0–2) |
| `max_retries` | No | Number of retries on transient errors (default 2) |

## Action Fields

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `type` | Yes | String | Must be `agent` |
| `provider` | Yes | String | Provider ID from config |
| `prompt` | Yes | String | Tera template for the user message |
| `system_prompt` | No | String | Tera template for system/instruction message |
| `output_schema` | No | JSON Schema | Schema for structured output parsing |
| `model` | No | String | Override provider's default model |
| `max_tokens` | No | Integer | Override provider's max tokens |
| `temperature` | No | Number | Override provider's temperature |
| `input` | No | Object | Input schema (same as other actions) |
| `timeout` | No | Duration | Max time for LLM call (default 5m) |

## Structured Output

When `output_schema` is set, the LLM response is parsed as JSON against the schema. If parsing fails, the step fails with an error.

### With output_schema

```yaml
actions:
  analyze:
    type: agent
    provider: anthropic-main
    prompt: "Analyze this data: {{ input.data }}"
    output_schema:
      type: object
      properties:
        sentiment: { type: string, enum: [positive, negative, neutral] }
        keywords: { type: array, items: { type: string } }
        score: { type: number, minimum: 0, maximum: 1 }
```

**Output:**

```json
{
  "sentiment": "positive",
  "keywords": ["great", "amazing"],
  "score": 0.95,
  "_meta": {
    "provider": "anthropic",
    "model": "claude-opus-4-1-20250805",
    "tokens": {
      "input": 156,
      "output": 42
    },
    "latency_ms": 1234
  }
}
```

### Without output_schema

```yaml
actions:
  summarize:
    type: agent
    provider: anthropic-main
    prompt: "Summarize: {{ input.text }}"
    # No output_schema — response captured as text
```

**Output:**

```json
{
  "text": "This is a summary of the provided text...",
  "_meta": {
    "provider": "anthropic",
    "model": "claude-opus-4-1-20250805",
    "tokens": {
      "input": 128,
      "output": 87
    },
    "latency_ms": 987
  }
}
```

> **Note:** The `_meta` key is reserved. If your `output_schema` defines a `_meta` property, it will be overwritten by the system metadata.

### Using _meta in downstream steps

```yaml
tasks:
  workflow:
    flow:
      analyze:
        action: analyze

      log-usage:
        action: log-api-usage
        depends_on: [analyze]
        input:
          tokens: "{{ analyze.output._meta.tokens.input + analyze.output._meta.tokens.output }}"
          latency_ms: "{{ analyze.output._meta.latency_ms }}"
```

## Prompt Templating

Both `prompt` and `system_prompt` are Tera templates. They're rendered at step execution time with access to:

| Variable | Description |
|----------|-------------|
| `input.*` | Job-level input |
| `<step_name>.output.*` | Output from completed upstream steps |
| `secret.*` | Workspace secrets |

**Example with templating:**

```yaml
actions:
  generate-report:
    type: agent
    provider: anthropic-main
    system_prompt: |
      You are a report generator for the {{ input.department }} department.
      Use a formal, professional tone.
    prompt: |
      Generate a report for Q{{ input.quarter }} {{ input.year }}.
      Previous metrics: {{ previous-step.output.metrics }}
      API key for data service: {{ secret.data_api_key }}
    input:
      department: { type: string }
      quarter: { type: integer }
      year: { type: integer }
```

**Step name rules**: Step names with hyphens become underscores in templates. A step named `previous-step` is referenced as `previous_step.output.*`.

## Error Handling

Agent actions can fail for several reasons. Each produces a specific error message:

| Error | Cause | Resolution |
|-------|-------|-----------|
| `Provider '{id}' not found` | `provider` ID doesn't exist in config | Check provider ID in action and config |
| `Template rendering error: ...` | `prompt` or `system_prompt` syntax invalid | Fix Tera template syntax or variable references |
| `LLM API error: ...` | Network error, API quota exceeded, invalid key | Check API credentials, rate limits, network connectivity |
| `JSON parsing failed: ...` | Response doesn't match `output_schema` | Adjust schema or prompt to guide LLM response format |
| `Step timeout exceeded` | LLM call took longer than `timeout` | Increase timeout or reduce `max_tokens` |

**Example: Handling LLM failures with conditions**

```yaml
tasks:
  robust-workflow:
    flow:
      classify:
        action: classify-ticket

      handle-failure:
        action: escalate-to-human
        depends_on: [classify]
        when: "{{ classify.status == 'failed' }}"
        # Runs only if classify step failed

      process-result:
        action: process-classification
        depends_on: [classify]
        when: "{{ classify.status == 'completed' }}"
        # Runs only if classify succeeded
```

## Limitations (Phase 7A)

- **Single-turn only**: Agents cannot have multi-turn conversations. Each step is an independent LLM call. Multi-turn support with tools is planned for Phase 7B.
- **No streaming**: Responses are fully buffered before being captured. Streaming output is not yet supported.
- **No request/response logging**: Full LLM conversation history is not logged (only metadata like token count and latency).

## Common Patterns

### Classification with routing

Classify input and route to different steps based on the result:

```yaml
actions:
  classify-issue:
    type: agent
    provider: anthropic-main
    prompt: "Classify this GitHub issue: {{ input.issue_body }}"
    output_schema:
      type: object
      properties:
        category: { type: string, enum: [bug, feature, documentation, wontfix] }

tasks:
  issue-workflow:
    input:
      issue_body: { type: string }
    flow:
      classify:
        action: classify-issue
        input:
          issue_body: "{{ input.issue_body }}"

      create-bug-ticket:
        action: create-jira-ticket
        depends_on: [classify]
        when: "{{ classify.output.category == 'bug' }}"

      add-feature-label:
        action: add-github-label
        depends_on: [classify]
        when: "{{ classify.output.category == 'feature' }}"
```

### Data extraction

Extract structured data from unstructured text:

```yaml
actions:
  extract-contact:
    type: agent
    provider: anthropic-main
    prompt: |
      Extract contact information from this text:
      {{ input.text }}

      Return as JSON with name, email, phone.
    output_schema:
      type: object
      properties:
        name: { type: string }
        email: { type: string }
        phone: { type: string }

tasks:
  contact-extraction:
    flow:
      extract:
        action: extract-contact
        input:
          text: "{{ input.unstructured_data }}"

      save-contact:
        action: save-to-db
        depends_on: [extract]
        input:
          name: "{{ extract.output.name }}"
          email: "{{ extract.output.email }}"
```

### Conditional processing based on analysis

Analyze content and conditionally trigger different workflows:

```yaml
actions:
  analyze-sentiment:
    type: agent
    provider: anthropic-main
    prompt: "Analyze sentiment of: {{ input.text }}"
    output_schema:
      type: object
      properties:
        sentiment: { type: string, enum: [positive, negative, neutral] }
        score: { type: number }

tasks:
  feedback-workflow:
    input:
      feedback: { type: string }
    flow:
      analyze:
        action: analyze-sentiment
        input:
          text: "{{ input.feedback }}"

      positive-branch:
        action: send-thank-you
        depends_on: [analyze]
        when: "{{ analyze.output.sentiment == 'positive' }}"

      negative-branch:
        action: create-support-ticket
        depends_on: [analyze]
        when: "{{ analyze.output.sentiment == 'negative' }}"
```

## Security Considerations

### Secrets in prompts

Workspace secrets are available in prompt templates via `{{ secret.NAME }}`. When an agent step renders a secret into a prompt, that secret is sent to the external LLM API (Anthropic, OpenAI, or whatever is configured). This is the same access model as script steps (which can read secrets via environment variables), but with agent steps the data leaves your infrastructure.

**Recommendations:**
- Only reference secrets in agent prompts when necessary
- Use the most restrictive LLM provider data policies available
- Consider using separate, limited-scope API keys as secrets for agent-accessible workflows
- Review workflow YAML for unintended secret references before deploying

### Custom API endpoints

The `api_endpoint` field in provider configuration accepts arbitrary URLs. Administrators should ensure endpoints point only to trusted LLM-compatible API servers. The field is only configurable via `server-config.yaml` (not by workflow authors).

## Cost Optimization

Agent steps incur LLM API costs. Keep these strategies in mind:

- **Reduce max_tokens**: Lower `max_tokens` for shorter responses (e.g., classification often needs < 100 tokens)
- **Reuse for multiple steps**: Design workflows so one agent step can answer multiple questions, if possible
- **Cache results**: For repeated analyses, consider caching outputs in a database step and checking before calling the agent
- **Batch operations**: Where possible, batch multiple items into one prompt instead of separate calls
- **Model selection**: Use faster/cheaper models for simple tasks; reserve advanced models for complex analysis

**Example cost-optimized classification:**

```yaml
actions:
  quick-classify:
    type: agent
    provider: anthropic-main
    model: claude-haiku-3-5-20241022  # Faster, cheaper model
    max_tokens: 50  # Minimal output
    prompt: |
      Classify as: [bug, feature, wontfix]
      {{ input.title }}
```
