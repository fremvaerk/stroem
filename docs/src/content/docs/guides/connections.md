---
title: Connections
description: Named, typed objects for storing external system configurations like database credentials and API endpoints
---

Connections are named, typed configuration objects that store external system details (database credentials, API endpoints, etc.). They live in workspace YAML and are resolved automatically when used as task inputs.

## Connection Types

Define reusable schemas for connections using `connection_types`. Each property has a type, and can be marked as required, have a default value, or be flagged as secret.

```yaml
connection_types:
  postgres:
    properties:
      host:
        type: string
        required: true
      port:
        type: integer
        default: 5432
      database:
        type: string
        required: true
      user:
        type: string
        required: true
      password:
        type: string
        required: true
        secret: true
```

Supported property types: `string`, `text`, `integer`, `number`, `boolean`, `date`, `datetime`.

## Connections

Define named connection instances under `connections`. The `type` field references a connection type; all other fields are values.

```yaml
connections:
  prod_db:
    type: postgres
    host: "db.example.com"
    port: 5432
    database: "myapp"
    user: "admin"
    password: "{{ 'ref+awsssm:///prod/db/password' | vals }}"

  staging_db:
    type: postgres
    host: "staging-db.internal"
    database: "myapp_staging"
    user: "app"
    password: "{{ 'ref+awsssm:///staging/db/password' | vals }}"
```

Connection values support Tera templates (e.g., `{{ 'ref+...' | vals }}` for secret resolution). Default values from the connection type are applied automatically for missing fields — in this example, `staging_db.port` will default to `5432`.

### Untyped Connections

Connections without a `type` field are also valid. Untyped connections skip type validation entirely and can be used for any connection-type input field:

```yaml
connections:
  custom_api:
    url: "https://api.example.com"
    token: "abc123"
```

## Using Connections as Task Inputs

When a task input field's `type` matches a connection type name, the system treats it as a connection reference. At job creation time, the connection name string is automatically resolved to the full connection object.

```yaml
actions:
  run-migration:
    type: script
    script: "migrate --host {{ input.db.host }} --port {{ input.db.port }} --db {{ input.db.database }}"
    input:
      db:
        type: postgres        # Expects a resolved connection object

tasks:
  deploy:
    input:
      db:
        type: postgres        # References the connection type
      env:
        type: string           # Normal input
    flow:
      migrate:
        action: run-migration
        input:
          db: "{{ input.db }}"
```

When triggering the task, pass the connection name as the input value:

```json
{ "db": "prod_db", "env": "production" }
```

The system resolves `"prod_db"` to `{ "host": "db.example.com", "port": 5432, "database": "myapp", ... }`, making the full connection object available in templates as `{{ input.db.host }}`, `{{ input.db.port }}`, etc.

In the web UI, connection-type inputs render as a searchable dropdown listing all connections of that type in the workspace — no manual typing required.

When a connection type comes from a library, use the full namespaced name: `type: common.postgres` (where `common` is the library name).

## Resolution Flow

**Task-level** (at job creation):
1. `merge_defaults()` runs as normal (fills missing fields, applies defaults)
2. `resolve_connection_inputs()` scans each task input field:
   - If the field type is a connection type (not `string`/`text`/`integer`/`number`/`boolean`), the provided string value is looked up in workspace connections
   - The string is replaced with the connection's values object
3. The resolved input is stored in the job — available in all templates

**Action-level** (at step execution):
Connection inputs on individual actions within a task are resolved when the step is about to execute, during the claim and render phase.

### Direct Object Pass-Through

Instead of passing a connection name string, you can pass a connection object directly:

```json
{ "db": { "host": "localhost", "port": 5432, "database": "test" } }
```

When an object is passed instead of a string, the resolution lookup is skipped and the object is used as-is.

## Validation

At YAML parse time (`stroem validate`):

- Connection type property types must be `string`, `text`, `integer`, `number`, or `boolean`
- Typed connections must reference an existing `connection_type`
- Required fields without defaults must be present in the connection
- Unknown fields produce warnings
- Empty string values produce errors
- Task input fields with non-primitive types must reference a known connection type
