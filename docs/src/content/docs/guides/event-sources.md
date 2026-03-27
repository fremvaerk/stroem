---
title: Event Source Triggers
description: Long-running queue consumers that create jobs via stdout JSON-line protocol
---

Event source triggers enable integration with external message queues and event streams. An event source is a long-running process that consumes messages from a queue (SQS, Kafka, RabbitMQ, Pub/Sub, etc.) and creates jobs by emitting JSON lines to stdout.

Strøm is **queue-agnostic** — you bring your own consumer logic. The worker simply runs your script or container and reads JSON events from stdout, creating a job for each one.

## Overview

Event sources differ from other trigger types:

| Feature | Scheduler | Webhook | Event Source |
|---------|-----------|---------|--------------|
| Trigger method | Cron schedule | HTTP POST | Long-running process stdout |
| Execution | Periodic | On-demand | Continuous (with restart policy) |
| Duration | Minutes to hours | On-demand | Hours/days (permanent fixture) |
| Use case | Backups, reports, ETL | Deployments, CI/CD | Queue consumption, streaming |

## Configuration

Event sources are defined in the `triggers:` section of your workspace YAML:

```yaml
triggers:
  my-queue-consumer:
    type: event_source
    task: process-message
    # Choose a runner and provide your consumer logic
    script: |
      # Your queue consumer code here
    runner: local          # "local" (default), "docker", or "pod"
    language: shell        # For runner: local, specify language
    dependencies: []       # For runner: local with dependencies
    env:                   # Tera-templated environment variables
      QUEUE_URL: "{{ secret.queue_url }}"
    input:                 # Default input merged with each job
      priority: high
    restart_policy: always # always, on_failure, never
    backoff_secs: 5        # Initial restart delay (exponential backoff)
    max_in_flight: 10      # Backpressure: pause reading when limit reached
    enabled: true
```

## Stdout protocol

Each JSON line emitted to stdout triggers a job. Non-JSON lines are silently skipped and logged.

**Example:** A consumer reading from an SQS queue might emit:

```json
{"orderId": "123", "amount": 99.99, "customer": "alice"}
{"orderId": "124", "amount": 150.00, "customer": "bob"}
{"orderId": "125", "amount": 45.50, "customer": "charlie"}
```

Each line becomes a separate job, with the JSON object merged into the task's input. Fields from the trigger's `input` section are defaults; JSON fields override them.

### Parsing and errors

- **Valid JSON lines**: Parsed and merged with default input to create a job
- **Non-JSON lines**: Ignored (no error; logged at debug level in worker logs)
- **Empty lines**: Ignored
- **Malformed JSON**: Logged as a warning; line skipped; event source continues

This design ensures robust operation — a single malformed message doesn't crash the consumer.

## Runner examples

### Local script with Python and SQS

```yaml
triggers:
  sqs-consumer:
    type: event_source
    task: process-order
    language: python
    dependencies: ["boto3"]
    script: |
      import boto3
      import json
      import sys
      import os
      import time

      sqs = boto3.client("sqs")
      queue_url = os.environ["QUEUE_URL"]

      while True:
          resp = sqs.receive_message(
              QueueUrl=queue_url,
              WaitTimeSeconds=20,
              MaxNumberOfMessages=1
          )
          for msg in resp.get("Messages", []):
              # Parse and emit
              body = json.loads(msg["Body"])
              print(json.dumps(body))
              sys.stdout.flush()
              # Delete from queue
              sqs.delete_message(
                  QueueUrl=queue_url,
                  ReceiptHandle=msg["ReceiptHandle"]
              )
    env:
      QUEUE_URL: "{{ secret.sqs_queue_url }}"
      AWS_REGION: "eu-west-1"
    restart_policy: always
    backoff_secs: 5
    max_in_flight: 10

tasks:
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
```

### Docker container with Kafka

```yaml
triggers:
  kafka-consumer:
    type: event_source
    task: handle-event
    runner: docker
    image: myorg/kafka-consumer:1.0
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
        fmt.Println(string(output))
    }
}
```

### Kubernetes pod with RabbitMQ

```yaml
triggers:
  rabbitmq-consumer:
    type: event_source
    task: process-message
    runner: pod
    image: myorg/amqp-consumer:latest
    env:
      AMQP_URL: "{{ secret.amqp_url }}"
      QUEUE_NAME: "orders"
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
    restart_policy: always
    backoff_secs: 5
    max_in_flight: 15
```

## Worker configuration

Workers that claim event source steps must declare the `event_source` tag in their configuration:

```yaml
# worker-config.yaml
worker_token: "wk_abc123..."
tags:
  - script
  - docker
  - event_source
max_event_sources: 5  # How many concurrent event sources this worker can run
```

The `max_event_sources` setting limits how many long-running event source triggers the worker will claim simultaneously. Default is 1. Increase it if you have multiple independent queue consumers.

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

1. **Event source process starts**: Worker claims the event source trigger (a special recurring step)
2. **Event source emits JSON**: Consumer writes `{"field": "value"}` to stdout
3. **Worker reads and parses**: Each JSON line is parsed and merged with trigger `input` defaults
4. **Job is created**: API call creates a new job with the merged input, `source_type: "event_source"`, `source_id: "{workspace}/{trigger_name}"`
5. **Existing jobs tracked**: Worker maintains `max_in_flight` counter to pause reading if limit reached

### EventSourceManager

The server runs an `EventSourceManager` background task that:

- Periodically checks for event source triggers in workspace configs
- Creates placeholder "event source controller" jobs that workers claim
- Monitors controller job lifespan
- Restarts controllers according to `restart_policy` and backoff

Workers interact with this via:
- `POST /worker/event-source/emit` — emit a JSON event (creates a job)
- Claim lifecycle — when claiming an event source, worker gets configuration and backoff state

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
- **Error logging**: Write detailed errors to stderr; worker logs them
- **Resource limits**: Set `resources:` in pod `manifest` to prevent runaway consumers
- **Rate limiting**: Control burst via `max_in_flight`; use `sequential: true` in task flow for strict ordering

## Example: Multi-message batch consumer

Process multiple messages per event source "tick":

```yaml
triggers:
  batch-processor:
    type: event_source
    task: process-batch
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

          # Emit a single event with batch of messages
          batch = [json.loads(msg["Body"]) for msg in messages]
          print(json.dumps({"messages": batch, "count": len(batch)}))

          # Clean up
          for msg in messages:
              sqs.delete_message(
                  QueueUrl=queue_url,
                  ReceiptHandle=msg["ReceiptHandle"]
              )
    env:
      QUEUE_URL: "{{ secret.sqs_queue_url }}"
    max_in_flight: 5  # Limit concurrent batches

tasks:
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

**Event source not starting?**
- Check worker has `event_source` tag in config
- Verify trigger is `enabled: true`
- Check server logs for EventSourceManager errors

**Events not being processed?**
- Verify consumer is emitting valid JSON lines
- Check `max_in_flight` isn't pausing the reader
- Look at worker logs for parsing errors

**Consumer keeps restarting?**
- Check exit codes and logs (stderr goes to worker logs)
- Verify environment variables are set correctly
- Consider increasing `backoff_secs`
- Check resource limits aren't being hit

**High latency between event and job creation?**
- Increase `max_in_flight` to allow more parallelism
- Set `sequential: false` in task flow (default)
- Consider batching events if appropriate
