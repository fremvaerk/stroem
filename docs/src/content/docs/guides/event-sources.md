---
title: Event Source Triggers
description: Long-running queue consumers that create jobs via stdout JSON-line protocol
---

Event source triggers enable integration with external message queues and event streams. An event source is a long-running process that consumes messages from a queue (SQS, Kafka, RabbitMQ, Pub/Sub, etc.) and creates jobs by emitting `OUTPUT: <json>` lines to stdout.

Strøm is **queue-agnostic** — you bring your own consumer logic. The event source trigger references a task that runs the consumer process. When the consumer emits lines to stdout using the `OUTPUT: ` prefix protocol, Strøm creates jobs for the target task. Other stdout and stderr output is captured as log entries in the consumer's logs.

## Overview

Event sources differ from other trigger types:

| Feature | Scheduler | Webhook | Event Source |
|---------|-----------|---------|--------------|
| Trigger method | Cron schedule | HTTP POST | Long-running process (OUTPUT: prefix) |
| Execution | Periodic | On-demand | Continuous (with restart policy) |
| Duration | Minutes to hours | On-demand | Hours/days (permanent fixture) |
| Use case | Backups, reports, ETL | Deployments, CI/CD | Queue consumption, streaming |

## Configuration

Event sources are defined in the `triggers:` section of your workspace YAML. The event source trigger references a **consumer task** (a regular task whose flow runs the long-lived consumer process) and specifies a **target task** where jobs are created from emitted events:

```yaml
actions:
  sqs-poll:
    type: script
    language: python
    dependencies: ["boto3"]
    script: |
      # Your queue consumer code here
      # Emit events via: print("OUTPUT: " + json.dumps(event))

tasks:
  sqs-consumer:
    flow:
      poll:
        action: sqs-poll

triggers:
  sqs-events:
    type: event_source
    task: sqs-consumer              # Consumer task (runs the long-lived process)
    target_task: process-order      # Target task for emitted jobs
    env:                            # Tera-templated environment variables
      QUEUE_URL: "{{ secret.queue_url }}"
    input:                          # Default input merged into each job
      priority: high
    restart_policy: always          # always, on_failure, never
    backoff_secs: 5                 # Initial restart delay (exponential backoff)
    max_in_flight: 10               # Backpressure: pause reading when limit reached
    enabled: true
```

**Key differences from regular actions:**

- **`task:`** specifies the consumer task (contains the flow with your consumer action)
- **`target_task:`** specifies which task to create jobs for (receives the emitted JSON as input)
- **No inline execution fields** on the trigger — the consumer's execution is defined in the referenced task
- **`env:`** provides environment overrides for the consumer execution
- **`input:`** provides defaults merged into each emitted job

## Stdout protocol

Event source scripts communicate with the worker using a line-based protocol over stdout:

- Lines beginning with `OUTPUT: ` (note the space after the colon) followed by a JSON object trigger a job.
- All other stdout lines are treated as regular log output and appear in the event source job's logs.
- Empty lines are ignored.

**Example:** A consumer reading from an SQS queue might emit:

```
Starting consumer for queue: https://sqs.eu-west-1.amazonaws.com/123456789/orders
Connected successfully. Waiting for messages...
OUTPUT: {"orderId": "123", "amount": 99.99, "customer": "alice"}
Received message, deleting from queue...
OUTPUT: {"orderId": "124", "amount": 150.00, "customer": "bob"}
OUTPUT: {"orderId": "125", "amount": 45.50, "customer": "charlie"}
```

Each `OUTPUT: ` line becomes a separate job, with the JSON object merged into the task's input. Fields from the trigger's `input` section are defaults; JSON fields override them. Regular text lines appear in the worker's job log so you can monitor consumer activity.

### Why OUTPUT: prefix?

This protocol is consistent with how regular script actions work in Strøm. It lets you:

- Emit regular log output without it being misinterpreted as a job trigger
- Mix diagnostic messages, status updates, and event emissions freely in a single script
- Use `print()` / `echo` for debugging without side effects

### Parsing and errors

- **`OUTPUT: <json>`**: JSON parsed and merged with default input to create a job
- **Other non-empty lines**: Treated as log output; appear in job logs
- **Empty lines**: Ignored
- **Malformed JSON after `OUTPUT: `**: Logged as a warning; line skipped; event source continues

This design ensures robust operation — a single malformed message doesn't crash the consumer.

## Runner examples

### Local script with Python and SQS

```yaml
actions:
  sqs-poll:
    type: script
    language: python
    dependencies: ["boto3"]
    script: |
      import boto3
      import json
      import sys
      import os

      sqs = boto3.client("sqs")
      queue_url = os.environ["QUEUE_URL"]

      while True:
          resp = sqs.receive_message(
              QueueUrl=queue_url,
              WaitTimeSeconds=20,
              MaxNumberOfMessages=1
          )
          for msg in resp.get("Messages", []):
              # Parse and emit using OUTPUT: prefix
              body = json.loads(msg["Body"])
              print("OUTPUT: " + json.dumps(body))
              sys.stdout.flush()
              # Delete from queue
              sqs.delete_message(
                  QueueUrl=queue_url,
                  ReceiptHandle=msg["ReceiptHandle"]
              )

tasks:
  sqs-consumer:
    flow:
      poll:
        action: sqs-poll

  process-order:
    mode: distributed
    input:
      orderId: { type: string }
      amount: { type: number }
    flow:
      validate:
        action: validate-order
        input:
          order_id: "{{ input.orderId }}"
      process:
        action: charge-and-ship
        depends_on: [validate]
        input:
          order_id: "{{ input.orderId }}"
          amount: "{{ input.amount }}"

triggers:
  sqs-events:
    type: event_source
    task: sqs-consumer
    target_task: process-order
    env:
      QUEUE_URL: "{{ secret.sqs_queue_url }}"
      AWS_REGION: "eu-west-1"
    restart_policy: always
    backoff_secs: 5
    max_in_flight: 10
```

### Docker container with Kafka

```yaml
actions:
  kafka-poll:
    type: script
    runner: docker
    image: myorg/kafka-consumer:1.0
    # No script field — the image runs as-is

tasks:
  kafka-consumer:
    flow:
      poll:
        action: kafka-poll

  handle-event:
    flow:
      process:
        action: process-kafka-event

triggers:
  kafka-events:
    type: event_source
    task: kafka-consumer
    target_task: handle-event
    env:
      KAFKA_BROKERS: "{{ secret.kafka_brokers }}"
      KAFKA_TOPIC: "events"
      KAFKA_GROUP: "stroem-worker-1"
    input:
      source: kafka
    restart_policy: always
    max_in_flight: 20
    backoff_secs: 10
```

Your Docker image should accept configuration via environment variables and emit JSON lines to stdout. Example Dockerfile:

```dockerfile
FROM golang:1.21-alpine
WORKDIR /app
COPY . .
RUN go build -o consumer .
CMD ["./consumer"]
```

And your consumer code:

```go
package main

import (
    "context"
    "encoding/json"
    "fmt"
    "os"
    "strings"

    "github.com/segmentio/kafka-go"
)

func main() {
    brokers := strings.Split(os.Getenv("KAFKA_BROKERS"), ",")
    topic := os.Getenv("KAFKA_TOPIC")
    group := os.Getenv("KAFKA_GROUP")

    reader := kafka.NewReader(kafka.ReaderConfig{
        Brokers: brokers,
        Topic:   topic,
        GroupID: group,
    })

    for {
        msg, _ := reader.ReadMessage(context.Background())
        var event map[string]interface{}
        json.Unmarshal(msg.Value, &event)
        output, _ := json.Marshal(event)
        fmt.Println("OUTPUT: " + string(output))
    }
}
```

### Kubernetes pod with RabbitMQ

```yaml
actions:
  rabbitmq-poll:
    type: script
    runner: pod
    image: myorg/amqp-consumer:latest
    manifest:
      metadata:
        labels:
          app: stroem-rabbitmq-consumer
      spec:
        serviceAccountName: stroem-worker
        imagePullSecrets:
          - name: myorg-registry
        containers:
          - name: main
            resources:
              requests:
                memory: "256Mi"
                cpu: "250m"
              limits:
                memory: "512Mi"
                cpu: "500m"

tasks:
  rabbitmq-consumer:
    flow:
      poll:
        action: rabbitmq-poll

  process-message:
    flow:
      handle:
        action: handle-message

triggers:
  rabbitmq-events:
    type: event_source
    task: rabbitmq-consumer
    target_task: process-message
    env:
      AMQP_URL: "{{ secret.amqp_url }}"
      QUEUE_NAME: "orders"
    restart_policy: always
    backoff_secs: 5
    max_in_flight: 15
```

## Worker configuration

Event source consumer tasks run through normal runners (local, Docker, or Kubernetes), so no special worker configuration is required. Workers simply need to declare the appropriate runner tags for the actions used in their consumer task:

```yaml
# worker-config.yaml
worker_token: "wk_abc123..."
tags:
  - script          # Required if consumer uses type: script with runner: local
  - docker          # Required if consumer uses runner: docker
  - pod             # Required if consumer uses runner: pod
```

The server-side `EventSourceManager` background task handles consumer lifecycle: creation, restart policy, exponential backoff, and monitoring. Consumers run indefinitely (or until failure, depending on `restart_policy`) as regular jobs executed through the normal claiming mechanism.

## Restart policies

When an event source process exits, the restart policy determines what happens next.

| Policy | Behavior |
|--------|----------|
| `always` (default) | Restart immediately, regardless of exit code. Use for permanent consumers. |
| `on_failure` | Restart only if exit code is non-zero. Stops automatically on clean exit. |
| `never` | Do not restart. Process exits once and the trigger becomes idle. |

### Exponential backoff

When `restart_policy: always` or `on_failure`, restarts use **exponential backoff** to prevent thundering herd:

- First restart: `backoff_secs` seconds (default 5)
- Second consecutive failure: `backoff_secs * 2` (10s)
- Third: `backoff_secs * 4` (20s)
- ... exponential growth until capped at 5 minutes

The counter resets on a successful run (exit code 0).

Example with 10-second base backoff:

```yaml
triggers:
  flaky-consumer:
    type: event_source
    task: my-task
    script: "./my_consumer.sh"
    restart_policy: on_failure
    backoff_secs: 10
```

## Backpressure and max_in_flight

The `max_in_flight` setting provides **natural Unix pipe backpressure**. When the specified number of jobs are pending or running:

1. The worker pauses reading from the event source's stdout
2. The pipe fills up, blocking the consumer process
3. Consumer naturally slows down or pauses
4. When jobs complete, the worker resumes reading

This prevents job queues from exploding when downstream tasks slow down.

```yaml
triggers:
  high-volume-stream:
    type: event_source
    task: process-event
    script: "./consume.sh"
    max_in_flight: 50  # Never create more than 50 pending/running jobs at once
```

Without `max_in_flight`, all events are immediately converted to jobs, potentially overwhelming the system.

## How it works internally

### Job creation flow

1. **Consumer task runs**: The server creates a job for the consumer task, which runs indefinitely
2. **Consumer emits events**: The consumer process writes `OUTPUT: {"field": "value"}` to stdout
3. **Events are captured**: The consumer's log streaming captures `OUTPUT: ` lines
4. **Jobs created for target**: When events are emitted via `POST /worker/event-source/emit`, they are parsed and merged with trigger `input` defaults
5. **Target task invoked**: A new job is created for the `target_task`, with the merged JSON as input, `source_type: "event_source"`, `source_id: "{workspace}/{trigger_name}"`
6. **Backpressure tracked**: The server maintains a count of pending/running jobs. When `max_in_flight` is reached, the worker pauses stdout reading, naturally slowing the consumer

### EventSourceManager

The server runs an `EventSourceManager` background task that:

- Periodically checks for event source triggers in workspace configs
- Creates and monitors consumer task jobs
- Restarts consumer jobs according to `restart_policy` and exponential backoff
- Handles consumer lifecycle (start, failure, restart with delay)

Consumer jobs are regular jobs created via the normal task dispatch mechanism, so they respect timeouts, recovery sweeps, and all standard job features.

### Error handling

- **Consumer crashes**: Handled by `restart_policy` + exponential backoff
- **Malformed JSON**: Logged; event skipped; consumer continues
- **Network errors**: Depends on consumer retry logic (your code)
- **Worker crashes**: EventSourceManager detects stale controller, restarts it
- **Server down**: Consumer keeps running; jobs are created when server comes back up

## Disabling event sources

Set `enabled: false` to pause an event source without removing it:

```yaml
triggers:
  my-consumer:
    type: event_source
    task: my-task
    script: "./consumer.sh"
    enabled: false  # Process won't start
```

## Secrets and templating

Event variables and `input` defaults support Tera templating, with access to `secret.*`:

```yaml
triggers:
  secure-consumer:
    type: event_source
    task: protected-task
    script: |
      #!/bin/bash
      # Will have AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY from env
      aws sqs receive-message --queue-url "$QUEUE_URL" | jq '.Messages[].Body | fromjson'
    env:
      QUEUE_URL: "{{ secret.queue_url }}"
      AWS_ACCESS_KEY_ID: "{{ secret.aws_access_key }}"
      AWS_SECRET_ACCESS_KEY: "{{ secret.aws_secret_key }}"
    input:
      region: "{{ secret.default_region }}"
```

Secrets are injected at runtime; never logged or exposed in job configs.

## Best practices

- **Idempotent consumption**: Emit an event ID (`job_id`) in each JSON line; task can check for duplicates
- **Structured input**: Emit consistent JSON schema; document expected fields
- **Graceful shutdown**: Handle `SIGTERM` to finish in-flight work before exiting
- **Health checks**: Include a heartbeat or status endpoint your alerting can monitor
- **Error logging**: Write errors and diagnostics to stderr; they appear in the event source job's log view
- **Resource limits**: Set `resources:` in pod `manifest` to prevent runaway consumers
- **Rate limiting**: Control burst via `max_in_flight`; use `sequential: true` in task flow for strict ordering

## Example: Multi-message batch consumer

Process multiple messages per event source "tick":

```yaml
actions:
  sqs-batch-poll:
    type: script
    language: python
    dependencies: ["boto3"]
    script: |
      import boto3
      import json
      import os

      sqs = boto3.client("sqs")
      queue_url = os.environ["QUEUE_URL"]

      while True:
          # Receive up to 10 messages at once
          resp = sqs.receive_message(
              QueueUrl=queue_url,
              MaxNumberOfMessages=10,
              WaitTimeSeconds=20
          )

          messages = resp.get("Messages", [])
          if not messages:
              continue

          # Emit a single event with batch of messages using OUTPUT: prefix
          batch = [json.loads(msg["Body"]) for msg in messages]
          print("OUTPUT: " + json.dumps({"messages": batch, "count": len(batch)}))

          # Clean up
          for msg in messages:
              sqs.delete_message(
                  QueueUrl=queue_url,
                  ReceiptHandle=msg["ReceiptHandle"]
              )

tasks:
  batch-consumer:
    flow:
      poll:
        action: sqs-batch-poll

  process-batch:
    mode: distributed
    input:
      messages: { type: array }
      count: { type: integer }
    flow:
      iterate:
        action: process-one-batch
        input:
          batch: "{{ input.messages }}"

triggers:
  batch-processor:
    type: event_source
    task: batch-consumer
    target_task: process-batch
    env:
      QUEUE_URL: "{{ secret.sqs_queue_url }}"
    max_in_flight: 5  # Limit concurrent batches
```

## Comparison with other triggers

**Cron scheduler**: Great for scheduled, periodic work (backups, reports). Fixed timing, simple configuration. Not suitable for reacting to external events.

**Webhook**: Excellent for integrations (GitHub, Slack, deployments). External system initiates the request. Real-time but requires inbound network access.

**Event source**: Ideal for queue-driven architecture. Decouples producers from consumers. Self-managed consumer lifecycle. Supports backpressure and rate limiting.

Choose event sources when:
- You have a dedicated message queue (SQS, Kafka, RabbitMQ, etc.)
- You need continuous consumption (not periodic)
- Ordering or batching matters
- You want to manage your own consumer logic

## Troubleshooting

**Consumer task not starting?**
- Check that the `task:` field references an existing task in the same workspace
- Verify the task contains at least one flow step with a valid action
- Check worker has the required tags for the runner used in the consumer action
- Verify `enabled: true` on the trigger

**Events not being processed?**
- Verify consumer is emitting lines with the `OUTPUT: ` prefix followed by valid JSON
- Check the consumer task's job logs — look for any errors or unexpected output
- Verify `target_task:` references an existing task
- Check `max_in_flight` — if the limit is reached, event emission will block

**Consumer keeps restarting?**
- Check the consumer task job logs for error messages (exit code, exceptions, etc.)
- Verify environment variables are set correctly (check with `env` debugging in consumer)
- Consider increasing `backoff_secs` to allow recovery time between restarts
- Check resource limits aren't being hit (memory, CPU, file descriptors)

**Target jobs failing?**
- Check the `input:` defaults on the trigger — verify required fields are present
- Verify the emitted JSON contains all required input fields for the target task
- Check the target task's action for validation or processing errors

**High latency between event and job creation?**
- Increase `max_in_flight` to allow more parallelism
- Set `sequential: false` in target task flow (default)
- Consider batching events in the consumer if appropriate
- Monitor `max_in_flight` — if consistently full, consumer is waiting for jobs to complete
