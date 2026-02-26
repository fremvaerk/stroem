---
title: Hello World
description: A minimal two-step workflow demonstrating actions, tasks, and data passing
---

This example creates a two-step workflow that greets someone and then shouts the greeting in uppercase.

## Workflow file

Create `workspace/.workflows/hello.yaml`:

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }} && echo 'OUTPUT: {\"greeting\": \"Hello {{ input.name }}\"}'"
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
      name: { type: string, default: "World" }
    flow:
      say-hello:
        action: greet
        input:
          name: "{{ input.name }}"
      shout-it:
        action: shout
        depends_on: [say-hello]
        input:
          message: "{{ say_hello.output.greeting }}"
```

## What this does

1. **`greet` action** prints "Hello World" and emits structured output with `OUTPUT: {json}`
2. **`shout` action** takes a message and converts it to uppercase
3. **`hello-world` task** runs `say-hello` first, then passes its output to `shout-it`

The two steps form a dependency chain: `say-hello` → `shout-it`.

## Key concepts demonstrated

- **Actions** define reusable commands with typed input parameters
- **Tasks** compose actions into a DAG via `depends_on`
- **Structured output** uses `OUTPUT: {json}` prefix in stdout
- **Data passing** uses Tera templates: `{{ say_hello.output.greeting }}`
- **Step name sanitization**: `say-hello` becomes `say_hello` in templates (hyphens → underscores)

## Running it

```bash
# Via API
curl -s -X POST http://localhost:8080/api/workspaces/default/tasks/hello-world/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"name": "World"}}' | jq .

# Via CLI
stroem trigger hello-world --input '{"name": "World"}'
```

## Expected output

Step `say-hello`:
```
Hello World
OUTPUT: {"greeting": "Hello World"}
```

Step `shout-it`:
```
HELLO WORLD
```
