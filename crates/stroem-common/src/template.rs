use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use tera::Tera;

use crate::models::workflow::{ConnectionDef, InputFieldDef, WorkspaceConfig};

/// Tera filter that resolves `ref+` secret references via the vals CLI.
///
/// Usage in templates: `{{ secret.KEY | vals }}`
/// - Non-string values pass through unchanged
/// - Strings not starting with `ref+` pass through unchanged
/// - Strings starting with `ref+` are resolved via `vals eval`
fn vals_filter(
    value: &tera::Value,
    _args: &HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let s = match value.as_str() {
        Some(s) => s,
        None => return Ok(value.clone()),
    };

    if !s.starts_with("ref+") {
        return Ok(value.clone());
    }

    let input = serde_json::json!({"_v": s});
    let input_str = serde_json::to_string(&input)
        .map_err(|e| tera::Error::msg(format!("vals: serialize failed: {e}")))?;

    let mut child = std::process::Command::new("vals")
        .args(["eval", "-f", "-", "-o", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            tera::Error::msg(format!(
                "vals CLI not found. Install vals to use ref+ secrets: {e}"
            ))
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(input_str.as_bytes())
            .map_err(|e| tera::Error::msg(format!("vals: stdin write failed: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| tera::Error::msg(format!("vals: process failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tera::Error::msg(format!(
            "vals eval failed (exit {}): {}",
            output.status,
            stderr.trim()
        )));
    }

    let resolved: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| tera::Error::msg(format!("vals: invalid output JSON: {e}")))?;

    match resolved.get("_v").and_then(|v| v.as_str()) {
        Some(resolved_str) => Ok(tera::Value::String(resolved_str.to_string())),
        None => Err(tera::Error::msg("vals: resolved output missing '_v' key")),
    }
}

/// Renders a single Tera template string against a JSON context
pub fn render_template(template: &str, context: &serde_json::Value) -> Result<String> {
    let mut tera = Tera::default();
    let template_name = "__template__";

    tera.add_raw_template(template_name, template)
        .context("Failed to parse template")?;

    tera.register_filter("vals", vals_filter);

    let tera_context =
        tera::Context::from_serialize(context).context("Failed to convert JSON to Tera context")?;

    tera.render(template_name, &tera_context)
        .context("Failed to render template")
}

/// Split a possibly-qualified reference `workspace.item` on the FIRST `.`.
/// Returns `(Some(workspace), item)` for a qualified name, or `(None, name)`
/// for a local name or a degenerate form (empty workspace/item).
pub fn parse_qualified_ref(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((ws, item)) if !ws.is_empty() && !item.is_empty() => (Some(ws), item),
        _ => (None, name),
    }
}

/// Outcome of asking a [`WorkspaceLookup`] for a workspace by name.
pub enum Lookup<'a> {
    Found(&'a WorkspaceConfig),
    /// No workspace of that name is configured — an author error.
    Unknown,
    /// Configured, but not loaded / unhealthy right now — a server condition.
    Unavailable,
}

/// Access to workspace configs by name. The server implements this over its
/// in-memory `WorkspaceManager` snapshot; the CLI over the single local config.
///
/// `Send + Sync` supertraits: server-side callers hold a `&dyn WorkspaceLookup`
/// (e.g. `RenderContext::lookup`) across `.await` points in async handlers —
/// without these bounds the trait object itself isn't `Send`/`Sync` even when
/// every concrete implementor is, and the handler's future fails to be `Send`.
pub trait WorkspaceLookup: Send + Sync {
    /// The workspace the caller's YAML lives in.
    fn local_name(&self) -> &str;
    fn get(&self, name: &str) -> Lookup<'_>;
    /// `true` when no server is present, so a qualified reference can never be
    /// satisfied. Changes the error wording only.
    fn offline(&self) -> bool {
        false
    }
}

/// A [`WorkspaceLookup`] over exactly one config (CLI, unit tests).
pub struct SingleWorkspace<'a> {
    pub name: &'a str,
    pub config: &'a WorkspaceConfig,
}

impl WorkspaceLookup for SingleWorkspace<'_> {
    fn local_name(&self) -> &str {
        self.name
    }
    fn get(&self, name: &str) -> Lookup<'_> {
        if name == self.name {
            Lookup::Found(self.config)
        } else {
            Lookup::Unknown
        }
    }
    fn offline(&self) -> bool {
        true
    }
}

/// A connection-type reference normalised to the workspace that defines it.
/// Two references match only if both fields are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalType {
    pub workspace: String,
    pub name: String,
}

impl std::fmt::Display for CanonicalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.workspace, self.name)
    }
}

fn found_config<'a>(
    lookup: &'a dyn WorkspaceLookup,
    ws: &str,
    what: &str,
) -> Result<&'a WorkspaceConfig> {
    match lookup.get(ws) {
        Lookup::Found(c) => Ok(c),
        Lookup::Unknown => bail!("{}: unknown workspace '{}'", what, ws),
        Lookup::Unavailable => bail!("{}: workspace '{}' is not available", what, ws),
    }
}

/// Canonicalise a connection-type reference written in `defining_ws`.
///
/// Precedence: a literal key in `defining_ws` (covers library-flattened
/// `lib.type`), then `ws.type` split on the first dot against a known
/// workspace, then — if the prefix is not a configured workspace — an opaque
/// local name (keeps pre-existing `type: foo.bar` string-compare configs
/// working). A bare name is always `(defining_ws, name)`.
pub fn canonical_type_ref(
    type_ref: &str,
    defining_ws: &str,
    lookup: &dyn WorkspaceLookup,
) -> Result<CanonicalType> {
    let local = |name: &str| CanonicalType {
        workspace: defining_ws.to_string(),
        name: name.to_string(),
    };
    let defining_cfg = found_config(
        lookup,
        defining_ws,
        &format!("connection type '{}'", type_ref),
    )?;
    if defining_cfg.connection_types.contains_key(type_ref) {
        return Ok(local(type_ref));
    }
    match parse_qualified_ref(type_ref) {
        (Some(ws), item) => match lookup.get(ws) {
            Lookup::Found(cfg) => {
                if cfg.connection_types.contains_key(item) {
                    Ok(CanonicalType {
                        workspace: ws.to_string(),
                        name: item.to_string(),
                    })
                } else {
                    bail!(
                        "connection type '{}': workspace '{}' has no connection type '{}'",
                        type_ref,
                        ws,
                        item
                    )
                }
            }
            Lookup::Unknown => Ok(local(type_ref)),
            Lookup::Unavailable => bail!(
                "connection type '{}': workspace '{}' is not available",
                type_ref,
                ws
            ),
        },
        (None, _) => Ok(local(type_ref)),
    }
}

/// Where a connection-typed input is resolved.
pub struct ResolveScope<'a> {
    pub lookup: &'a dyn WorkspaceLookup,
    /// Workspace whose YAML declares the input schema (types canonicalise here).
    pub schema_ws: &'a str,
    /// Workspace in which a bare connection name is looked up first.
    pub value_ws: &'a str,
    /// Second workspace tried for a bare name on a miss — only `shared`
    /// connections qualify. Used for caller-supplied values on a
    /// cross-workspace action step.
    pub fallback_ws: Option<&'a str>,
}

/// A connection located by [`resolve_connection_ref`].
pub struct ResolvedConnection<'a> {
    pub workspace: String,
    pub name: String,
    pub def: &'a ConnectionDef,
}

/// Locate a connection by bare or qualified name, enforcing the `shared` gate
/// for any reference that crosses a workspace boundary.
pub fn resolve_connection_ref<'a>(
    conn_ref: &str,
    scope: &ResolveScope<'a>,
) -> Result<ResolvedConnection<'a>> {
    let lookup = scope.lookup;
    let value_cfg = found_config(
        lookup,
        scope.value_ws,
        &format!("connection '{}'", conn_ref),
    )?;

    // 1. Literal local key (bare name, or a connection literally named "a.b").
    if let Some(def) = value_cfg.connections.get(conn_ref) {
        return Ok(ResolvedConnection {
            workspace: scope.value_ws.to_string(),
            name: conn_ref.to_string(),
            def,
        });
    }

    // 2. Qualified `ws.name`.
    if let (Some(ws), item) = parse_qualified_ref(conn_ref) {
        return match lookup.get(ws) {
            Lookup::Found(cfg) => match cfg.connections.get(item) {
                Some(def) if ws == scope.value_ws || def.shared => Ok(ResolvedConnection {
                    workspace: ws.to_string(),
                    name: item.to_string(),
                    def,
                }),
                Some(_) => bail!(
                    "connection '{}' exists in workspace '{}' but is not shared (set `shared: true` on it in workspace '{}')",
                    conn_ref,
                    ws,
                    ws
                ),
                None => bail!(
                    "connection '{}' does not exist: workspace '{}' has no connection '{}'",
                    conn_ref,
                    ws,
                    item
                ),
            },
            Lookup::Unknown if lookup.offline() => bail!(
                "connection '{}': cross-workspace connection references require a server (run this task through `stroem-api trigger`)",
                conn_ref
            ),
            Lookup::Unknown => bail!("connection '{}': unknown workspace '{}'", conn_ref, ws),
            Lookup::Unavailable => bail!(
                "connection '{}': workspace '{}' is not available",
                conn_ref,
                ws
            ),
        };
    }

    // 3. Bare name missed locally: try the fallback workspace, gated by `shared`.
    if let Some(fb) = scope.fallback_ws.filter(|fb| *fb != scope.value_ws) {
        if let Lookup::Found(cfg) = lookup.get(fb) {
            match cfg.connections.get(conn_ref) {
                Some(def) if def.shared => {
                    return Ok(ResolvedConnection {
                        workspace: fb.to_string(),
                        name: conn_ref.to_string(),
                        def,
                    })
                }
                Some(_) => bail!(
                    "connection '{}' not found in workspace '{}'; '{}.{}' exists but is not shared",
                    conn_ref,
                    scope.value_ws,
                    fb,
                    conn_ref
                ),
                None => {}
            }
        }
    }

    bail!(
        "connection '{}' does not exist in workspace '{}'",
        conn_ref,
        scope.value_ws
    )
}

/// Evaluate a `when` condition template against a JSON context.
///
/// Returns `true` (step should run) if the rendered result is truthy:
/// non-empty and not `"false"`, `"0"`, `"null"`, or `"none"` (all
/// comparisons are case-insensitive). Returns `false` (step should be
/// skipped) otherwise. Template render errors propagate as `Err`.
pub fn evaluate_condition(template: &str, context: &serde_json::Value) -> Result<bool> {
    let rendered = render_template(template, context)?;
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    let lower = trimmed.to_lowercase();
    Ok(lower != "false" && lower != "0" && lower != "null" && lower != "none")
}

/// Recursively renders all string values in a JSON value as Tera templates.
/// Objects and arrays are traversed; non-string leaves pass through unchanged.
pub fn render_json_strings(
    value: &serde_json::Value,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    match value {
        serde_json::Value::String(s) => {
            let rendered = render_template(s, context)
                .with_context(|| format!("Failed to render template in JSON string: {}", s))?;
            Ok(serde_json::Value::String(rendered))
        }
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), render_json_strings(v, context)?);
            }
            Ok(serde_json::Value::Object(result))
        }
        serde_json::Value::Array(arr) => {
            let result: Result<Vec<_>> = arr
                .iter()
                .map(|v| render_json_strings(v, context))
                .collect();
            Ok(serde_json::Value::Array(result?))
        }
        other => Ok(other.clone()),
    }
}

/// Renders all values in a String→String env map, returns a new HashMap.
/// Each value is treated as a Tera template.
pub fn render_env_map(
    env_map: &HashMap<String, String>,
    context: &serde_json::Value,
) -> Result<HashMap<String, String>> {
    let mut result = HashMap::new();

    for (key, value) in env_map {
        let rendered = render_template(value, context)
            .with_context(|| format!("Failed to render env template for key '{}'", key))?;
        result.insert(key.clone(), rendered);
    }

    Ok(result)
}

/// Renders an optional string template. Returns None if input is None.
pub fn render_string_opt(
    template: &Option<String>,
    context: &serde_json::Value,
) -> Result<Option<String>> {
    match template {
        Some(s) => {
            let rendered =
                render_template(s, context).context("Failed to render string template")?;
            Ok(Some(rendered))
        }
        None => Ok(None),
    }
}

/// Renders all string values in an input map, returns a new JSON object.
/// Non-string values pass through unchanged.
///
/// For simple variable references like `{{ input.db }}` where the context value is
/// a non-string (object, array, number, boolean), the raw value is extracted directly
/// from the context. This preserves structured data (e.g. connection objects) flowing
/// through step input templates, so downstream action spec templates can access nested
/// fields like `{{ input.db.host }}`.
pub fn render_input_map(
    input_map: &HashMap<String, serde_json::Value>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut result = serde_json::Map::new();

    for (key, value) in input_map {
        let rendered_value = match value {
            serde_json::Value::String(s) => {
                // For simple variable references ({{ path.to.value }}), extract the
                // raw value from context to preserve structured data (objects, arrays).
                // This is essential for connection objects flowing through step templates.
                // Only objects and arrays are extracted directly — primitives (strings,
                // numbers, booleans) continue through Tera to produce string output,
                // preserving backward compatibility.
                if let Some(path) = extract_simple_variable_path(s) {
                    if let Some(raw_value) = resolve_context_path(context, path) {
                        if raw_value.is_object() || raw_value.is_array() {
                            raw_value
                        } else {
                            // Primitive value — render through Tera for string output
                            let rendered = render_template(s, context).with_context(|| {
                                format!("Failed to render template for key '{}'", key)
                            })?;
                            serde_json::Value::String(rendered)
                        }
                    } else {
                        // Path not found — fall through to Tera for proper error message
                        let rendered = render_template(s, context).with_context(|| {
                            format!("Failed to render template for key '{}'", key)
                        })?;
                        serde_json::Value::String(rendered)
                    }
                } else {
                    let rendered = render_template(s, context)
                        .with_context(|| format!("Failed to render template for key '{}'", key))?;
                    serde_json::Value::String(rendered)
                }
            }
            // Pass through non-string values unchanged
            other => other.clone(),
        };
        result.insert(key.clone(), rendered_value);
    }

    Ok(serde_json::Value::Object(result))
}

/// Check if a template string is a simple variable reference like `{{ input.db }}`.
/// Returns the variable path (e.g. "input.db") if it matches, None otherwise.
/// Does NOT match templates with filters, surrounding text, or multiple expressions.
fn extract_simple_variable_path(template: &str) -> Option<&str> {
    let trimmed = template.trim();
    let inner = trimmed.strip_prefix("{{")?.strip_suffix("}}")?.trim();
    // Must be a simple dot-path: alphanumeric, dots, underscores only
    if !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '_')
    {
        Some(inner)
    } else {
        None
    }
}

/// Resolve a dot-separated path against a JSON context value.
/// E.g. "input.db.host" resolves through context["input"]["db"]["host"].
fn resolve_context_path(context: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut current = context;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

/// Merge task input defaults into user-provided input.
///
/// For each field in the input schema that is missing from `user_input`:
/// - If the field has a `default`, insert it (rendering string defaults through Tera with `context`)
/// - If the field is `required` and has no default, return an error
///
/// User-provided fields always take precedence. Extra user fields not in the schema pass through.
pub fn merge_defaults(
    user_input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let user_map = user_input.as_object().unwrap_or(&empty);

    let mut result = user_map.clone();

    // Fill in defaults for schema fields not provided by the user
    for (field_name, field_def) in input_schema {
        if result.contains_key(field_name) {
            continue;
        }

        if let Some(ref default_value) = field_def.default {
            let resolved = match default_value {
                serde_json::Value::String(s) => {
                    if s.contains("{{") {
                        let rendered = render_template(s, context).with_context(|| {
                            format!(
                                "Failed to render default template for input field '{}'",
                                field_name
                            )
                        })?;
                        serde_json::Value::String(rendered)
                    } else {
                        default_value.clone()
                    }
                }
                _ => default_value.clone(),
            };
            result.insert(field_name.clone(), resolved);
        }
        // Note: required-field validation is intentionally not done here.
        // Webhooks and triggers supply different input shapes (body, headers, etc.)
        // that don't match the task's input schema.
    }

    Ok(serde_json::Value::Object(result))
}

/// Primitive type names that are NOT connection type references.
pub const PRIMITIVE_TYPES: &[&str] = &[
    "string", "text", "integer", "number", "boolean", "date", "datetime",
];

/// Resolve connection inputs: replace connection name strings with the full
/// connection object. See [`resolve_connection_inputs_scoped`]; this wrapper
/// resolves everything in `lookup.local_name()`.
pub fn resolve_connection_inputs(
    input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
) -> Result<serde_json::Value> {
    let local = lookup.local_name();
    resolve_connection_inputs_scoped(
        input,
        input_schema,
        &ResolveScope {
            lookup,
            schema_ws: local,
            value_ws: local,
            fallback_ws: None,
        },
    )
}

/// Resolve connection inputs within an explicit [`ResolveScope`].
///
/// For each field in `input_schema` whose `field_type` is not a primitive, the
/// value must be a connection name (string) or an already-resolved object
/// (passed through). The name is located by [`resolve_connection_ref`], its
/// declared type is canonicalised in the connection's own workspace and must
/// equal the field's canonical type (untyped connections match anything —
/// existing rule). A connection whose type lives in a different workspace than
/// the connection itself gets that type's property defaults applied and its
/// values checked here, because the owner's load-time pass never saw the type.
pub fn resolve_connection_inputs_scoped(
    input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    scope: &ResolveScope,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let input_map = input.as_object().unwrap_or(&empty);
    let mut result = input_map.clone();

    for (field_name, field_def) in input_schema {
        if PRIMITIVE_TYPES.contains(&field_def.field_type.as_str()) {
            continue;
        }
        let value = match result.get(field_name) {
            Some(v) => v.clone(),
            None => continue,
        };
        let conn_name = match value.as_str() {
            Some(s) => s,
            None => {
                if value.is_object() {
                    continue;
                }
                bail!(
                    "Input field '{}' expects a connection name (string), got {}",
                    field_name,
                    value
                );
            }
        };

        let field_ct = canonical_type_ref(&field_def.field_type, scope.schema_ws, scope.lookup)
            .with_context(|| format!("Input field '{}'", field_name))?;
        let resolved = resolve_connection_ref(conn_name, scope).with_context(|| {
            format!(
                "Input field '{}' references connection '{}'",
                field_name, conn_name
            )
        })?;

        let values = match resolved.def.connection_type {
            None => resolved.def.values.clone(),
            Some(ref declared) => {
                let conn_ct = canonical_type_ref(declared, &resolved.workspace, scope.lookup)
                    .with_context(|| format!("connection '{}'", conn_name))?;
                if conn_ct != field_ct {
                    bail!(
                        "Input field '{}' expects type '{}' but connection '{}' is type '{}'",
                        field_name,
                        field_ct,
                        conn_name,
                        conn_ct
                    );
                }
                if conn_ct.workspace != resolved.workspace {
                    // Foreign-typed connection: defaults + checks not done at load.
                    let type_cfg =
                        found_config(scope.lookup, &conn_ct.workspace, "connection type")?;
                    let type_def = type_cfg
                        .connection_types
                        .get(&conn_ct.name)
                        .with_context(|| format!("connection type '{}' vanished", conn_ct))?;
                    let with_defaults = resolved.def.values_with_type_defaults(type_def);
                    let warnings = crate::validation::check_connection_values(
                        &format!("{}.{}", resolved.workspace, resolved.name),
                        &with_defaults,
                        &conn_ct.to_string(),
                        type_def,
                    )?;
                    for w in warnings {
                        tracing::warn!(
                            connection = %format!("{}.{}", resolved.workspace, resolved.name),
                            "{}",
                            w
                        );
                    }
                    with_defaults
                } else {
                    resolved.def.values.clone()
                }
            }
        };

        let values_json =
            serde_json::to_value(&values).context("Failed to serialize connection values")?;
        result.insert(field_name.clone(), values_json);
    }

    Ok(serde_json::Value::Object(result))
}

/// Sentinel string used by the server to redact secret values in API responses.
/// The UI sends this exact byte sequence on Re-run for fields the user did not
/// edit, signalling "replay this from the source job's `raw_input`".
pub const REDACTED_SENTINEL: &str = "••••••";

/// Resolve Re-run "reuse from source" sentinels in the incoming input.
///
/// For each field in the schema that is a secret or connection type, if the
/// incoming value equals [`REDACTED_SENTINEL`]:
/// - If the source job's `raw_input` has that field, replace the sentinel with
///   the source value.
/// - Otherwise, remove the field entirely so `merge_defaults` fills it from
///   the schema default (supports secret rotation).
///
/// All other fields pass through unchanged. The sentinel only has meaning for
/// secret and connection-typed fields — a user typing literal bullets into a
/// plain field is preserved verbatim.
pub fn resolve_rerun_sentinels(
    incoming: &serde_json::Value,
    source_raw_input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let incoming_map = incoming.as_object().unwrap_or(&empty);
    let source_map = source_raw_input.as_object().unwrap_or(&empty);

    let mut result = incoming_map.clone();

    for (field_name, field_def) in input_schema {
        let is_connection = !PRIMITIVE_TYPES.contains(&field_def.field_type.as_str());
        let is_secret_or_connection = field_def.secret || is_connection;
        if !is_secret_or_connection {
            continue;
        }

        let value = match result.get(field_name) {
            Some(v) => v,
            None => continue,
        };
        if value.as_str() != Some(REDACTED_SENTINEL) {
            continue;
        }

        // Sentinel detected on a secret/connection field — replay or drop.
        match source_map.get(field_name) {
            Some(source_value) => {
                result.insert(field_name.clone(), source_value.clone());
            }
            None => {
                result.remove(field_name);
            }
        }
    }

    Ok(serde_json::Value::Object(result))
}

/// Recursively walk a JSON value tree and render all string values containing `{{`
/// through Tera. Objects and arrays are traversed recursively; other types are cloned as-is.
pub fn render_value_deep(
    value: &serde_json::Value,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    match value {
        serde_json::Value::String(s) => {
            if s.contains("{{") {
                let rendered = render_template(s, context)
                    .with_context(|| format!("Failed to render template in value: {}", s))?;
                Ok(serde_json::Value::String(rendered))
            } else {
                Ok(value.clone())
            }
        }
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), render_value_deep(v, context)?);
            }
            Ok(serde_json::Value::Object(result))
        }
        serde_json::Value::Array(arr) => {
            let mut result = Vec::new();
            for v in arr {
                result.push(render_value_deep(v, context)?);
            }
            Ok(serde_json::Value::Array(result))
        }
        _ => Ok(value.clone()),
    }
}

/// Merge action-level input defaults into already-rendered step input.
///
/// 1. Calls `merge_defaults()` to fill missing fields from the action's input schema
/// 2. Calls `render_value_deep()` ONLY on the fields that step 1 filled in from
///    defaults (a field already present in `rendered_input` is never touched
///    again, no matter what `merge_defaults()` returns for it)
///
/// This handles the case where an action defines a default like:
/// ```yaml
/// input:
///   clickhouse:
///     type: object
///     default:
///       host: "{{ secret.clickhouse.host }}"
///       port: 8443
/// ```
/// The object default is inserted by `merge_defaults()` as-is, then
/// `render_value_deep()` walks it to render the `{{ secret.clickhouse.host }}`
/// template.
///
/// Values already present in `rendered_input` are deliberately excluded from
/// that second pass and passed through byte-for-byte: they were rendered once
/// already, in the caller's own context. Re-rendering them here — against
/// this call's context, which for a cross-workspace action is the *owner's*
/// `secret` map — would let a caller smuggle out the owner's secrets with a
/// Tera string-literal trick (a value like `'{{ "{{ secret.TOKEN }}" }}'`
/// renders to a literal `{{ secret.TOKEN }}` on the first, caller-side pass,
/// then would render again to the real secret value if this function
/// re-rendered already-resolved fields).
pub fn merge_action_defaults(
    rendered_input: &serde_json::Value,
    action_input_schema: &HashMap<String, InputFieldDef>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let input_map = rendered_input.as_object().unwrap_or(&empty);

    let merged = merge_defaults(rendered_input, action_input_schema, context)
        .context("Failed to merge action input defaults")?;
    let mut merged_map = merged.as_object().cloned().unwrap_or_default();

    // Only fields absent from the caller-supplied input were filled in from
    // schema defaults above; render templates in those alone. A value the
    // caller supplied (including a connection object an earlier resolver
    // pass already substituted in) must never be re-rendered here — doing so
    // against, e.g., the action owner's `secret` context would let a caller
    // smuggle out owner secrets via a Tera string-literal trick
    // (`'{{ "{{ secret.TOKEN }}" }}'`).
    let mut defaults_only = serde_json::Map::new();
    for (key, value) in merged_map.iter() {
        if !input_map.contains_key(key) {
            defaults_only.insert(key.clone(), value.clone());
        }
    }
    let rendered_defaults = render_value_deep(&serde_json::Value::Object(defaults_only), context)
        .context("Failed to render templates in action defaults")?;
    if let serde_json::Value::Object(rendered_map) = rendered_defaults {
        for (key, value) in rendered_map {
            merged_map.insert(key, value);
        }
    }

    Ok(serde_json::Value::Object(merged_map))
}

/// Prepare action input: merge defaults, resolve connection references, all
/// within `lookup.local_name()` (a local step: caller == owner).
pub fn prepare_action_input(
    rendered_input: &serde_json::Value,
    action_input_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
) -> Result<serde_json::Value> {
    let local = lookup.local_name();
    prepare_action_input_cross(rendered_input, action_input_schema, lookup, local, local)
}

/// Prepare action input for a step whose action is owned by `owner_ws` while
/// the flow step (and `rendered_input`) belong to `caller_ws`.
///
/// Provenance-aware two-pass:
/// 1. Fields present in `rendered_input` were supplied by the caller: resolve
///    them with bare names in the caller first, falling back to the owner only
///    for `shared` connections.
/// 2. Merge the owner's action defaults (rendered with the owner's secrets).
/// 3. Fields filled by defaults are the owner reading its own config: resolve
///    ungated in the owner. Fields resolved in pass 1 are objects by now and
///    pass through.
///
/// When `caller_ws == owner_ws` the passes collapse to the local behaviour.
pub fn prepare_action_input_cross(
    rendered_input: &serde_json::Value,
    action_input_schema: &HashMap<String, InputFieldDef>,
    lookup: &dyn WorkspaceLookup,
    caller_ws: &str,
    owner_ws: &str,
) -> Result<serde_json::Value> {
    let owner_cfg = found_config(lookup, owner_ws, "action owner")?;

    let caller_resolved = resolve_connection_inputs_scoped(
        rendered_input,
        action_input_schema,
        &ResolveScope {
            lookup,
            schema_ws: owner_ws,
            value_ws: caller_ws,
            fallback_ws: if caller_ws == owner_ws {
                None
            } else {
                Some(owner_ws)
            },
        },
    )
    .context("Failed to resolve action connection inputs")?;

    let secrets_ctx = serde_json::json!({ "secret": &owner_cfg.secrets });
    let merged = merge_action_defaults(&caller_resolved, action_input_schema, &secrets_ctx)
        .context("Failed to merge action input defaults")?;

    resolve_connection_inputs_scoped(
        &merged,
        action_input_schema,
        &ResolveScope {
            lookup,
            schema_ws: owner_ws,
            value_ws: owner_ws,
            fallback_ws: None,
        },
    )
    .context("Failed to resolve action connection inputs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_simple_variable() {
        let template = "Hello {{ name }}";
        let context = json!({"name": "World"});

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_render_nested_context() {
        let template = "{{ user.name }} lives in {{ user.city }}";
        let context = json!({
            "user": {
                "name": "Alice",
                "city": "Copenhagen"
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Alice lives in Copenhagen");
    }

    #[test]
    fn test_render_step_output() {
        let template = "Previous step returned: {{ step1.output.result }}";
        let context = json!({
            "step1": {
                "output": {
                    "result": "success"
                }
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Previous step returned: success");
    }

    #[test]
    fn test_render_missing_variable() {
        let template = "Hello {{ missing }}";
        let context = json!({});

        let result = render_template(template, &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_render_input_map_all_strings() {
        let mut input_map = HashMap::new();
        input_map.insert("name".to_string(), json!("{{ input.name }}"));
        input_map.insert("greeting".to_string(), json!("Hello {{ input.name }}"));

        let context = json!({
            "input": {
                "name": "Alice"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result["name"], "Alice");
        assert_eq!(result["greeting"], "Hello Alice");
    }

    #[test]
    fn test_render_input_map_mixed_types() {
        let mut input_map = HashMap::new();
        input_map.insert("name".to_string(), json!("{{ input.name }}"));
        input_map.insert("count".to_string(), json!(42));
        input_map.insert("enabled".to_string(), json!(true));
        input_map.insert("items".to_string(), json!(["a", "b", "c"]));
        input_map.insert("config".to_string(), json!({"key": "value"}));

        let context = json!({
            "input": {
                "name": "Bob"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result["name"], "Bob");
        assert_eq!(result["count"], 42);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["items"], json!(["a", "b", "c"]));
        assert_eq!(result["config"], json!({"key": "value"}));
    }

    #[test]
    fn test_render_input_map_empty() {
        let input_map = HashMap::new();
        let context = json!({});

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_render_input_map_template_error() {
        let mut input_map = HashMap::new();
        input_map.insert("bad".to_string(), json!("{{ missing.value }}"));

        let context = json!({});

        let result = render_input_map(&input_map, &context);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("bad"));
    }

    #[test]
    fn test_render_with_filters() {
        let template = "{{ name | upper }}";
        let context = json!({"name": "alice"});

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "ALICE");
    }

    #[test]
    fn test_render_with_conditionals() {
        let template = "{% if enabled %}Active{% else %}Inactive{% endif %}";
        let context = json!({"enabled": true});

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Active");
    }

    #[test]
    fn test_render_workflow_like_context() {
        let template = "Deploy {{ input.env }} with tag {{ build.output.tag }}";
        let context = json!({
            "input": {
                "env": "production",
                "repo": "https://github.com/org/app.git"
            },
            "build": {
                "output": {
                    "tag": "v1.2.3"
                }
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Deploy production with tag v1.2.3");
    }

    #[test]
    fn test_render_input_map_workflow_step() {
        let mut input_map = HashMap::new();
        input_map.insert("repo".to_string(), json!("{{ input.repo }}"));
        input_map.insert("tag".to_string(), json!("{{ build.output.tag }}"));
        input_map.insert("replicas".to_string(), json!(3));

        let context = json!({
            "input": {
                "repo": "company/app"
            },
            "build": {
                "output": {
                    "tag": "v2.0.0"
                }
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result["repo"], "company/app");
        assert_eq!(result["tag"], "v2.0.0");
        assert_eq!(result["replicas"], 3);
    }

    #[test]
    fn test_render_env_map_simple() {
        let mut env = HashMap::new();
        env.insert("HOST".to_string(), "{{ input.host }}".to_string());
        env.insert("PORT".to_string(), "5432".to_string());

        let context = json!({ "input": { "host": "localhost" } });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(result["HOST"], "localhost");
        assert_eq!(result["PORT"], "5432");
    }

    #[test]
    fn test_render_env_map_with_secrets() {
        let mut env = HashMap::new();
        env.insert("DB_PASSWORD".to_string(), "{{ secret.db_pw }}".to_string());
        env.insert("API_KEY".to_string(), "{{ secret.api_key }}".to_string());

        let context = json!({
            "secret": {
                "db_pw": "ref+awsssm:///prod/db/password",
                "api_key": "ref+vault://secret/data/api#key"
            }
        });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(result["DB_PASSWORD"], "ref+awsssm:///prod/db/password");
        assert_eq!(result["API_KEY"], "ref+vault://secret/data/api#key");
    }

    #[test]
    fn test_render_env_map_with_nested_secrets() {
        let mut env = HashMap::new();
        env.insert(
            "DB_PASSWORD".to_string(),
            "{{ secret.db.password }}".to_string(),
        );
        env.insert("DB_HOST".to_string(), "{{ secret.db.host }}".to_string());
        env.insert("API_KEY".to_string(), "{{ secret.api_key }}".to_string());

        let context = json!({
            "secret": {
                "db": {
                    "password": "ref+sops://secrets.enc.yaml#/db/password",
                    "host": "ref+sops://secrets.enc.yaml#/db/host"
                },
                "api_key": "ref+vault://secret/data/api#key"
            }
        });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(
            result["DB_PASSWORD"],
            "ref+sops://secrets.enc.yaml#/db/password"
        );
        assert_eq!(result["DB_HOST"], "ref+sops://secrets.enc.yaml#/db/host");
        assert_eq!(result["API_KEY"], "ref+vault://secret/data/api#key");
    }

    #[test]
    fn test_render_env_map_mixed() {
        let mut env = HashMap::new();
        env.insert("STATIC".to_string(), "plain-value".to_string());
        env.insert("DYNAMIC".to_string(), "{{ input.name }}".to_string());
        env.insert("SECRET".to_string(), "{{ secret.token }}".to_string());

        let context = json!({
            "input": { "name": "alice" },
            "secret": { "token": "ref+vault://x" }
        });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(result["STATIC"], "plain-value");
        assert_eq!(result["DYNAMIC"], "alice");
        assert_eq!(result["SECRET"], "ref+vault://x");
    }

    #[test]
    fn test_render_env_map_error() {
        let mut env = HashMap::new();
        env.insert("BAD".to_string(), "{{ missing.var }}".to_string());

        let context = json!({});
        let result = render_env_map(&env, &context);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("BAD"));
    }

    #[test]
    fn test_render_string_opt_some() {
        let template = Some("echo {{ input.name }}".to_string());
        let context = json!({ "input": { "name": "world" } });
        let result = render_string_opt(&template, &context).unwrap();
        assert_eq!(result, Some("echo world".to_string()));
    }

    #[test]
    fn test_render_step_output_with_sanitized_hyphen() {
        // Step names like "say-hello" must be sanitized to "say_hello" in context
        // because Tera interprets hyphens as subtraction
        let template = "{{ say_hello.output.greeting }}";
        let context = json!({
            "say_hello": {
                "output": {
                    "greeting": "Hello World"
                }
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_render_hyphenated_name_fails() {
        // Proves that hyphens in variable names DON'T work in Tera
        let template = "{{ say-hello.output.greeting }}";
        let context = json!({
            "say-hello": {
                "output": {
                    "greeting": "Hello World"
                }
            }
        });

        let result = render_template(template, &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_render_string_opt_none() {
        let template: Option<String> = None;
        let context = json!({});
        let result = render_string_opt(&template, &context).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_vals_filter_passthrough_non_string() {
        let value = json!(42);
        let args = HashMap::new();
        let result = vals_filter(&value, &args).unwrap();
        assert_eq!(result, json!(42));
    }

    #[test]
    fn test_vals_filter_passthrough_no_ref() {
        let value = json!("plain-text");
        let args = HashMap::new();
        let result = vals_filter(&value, &args).unwrap();
        assert_eq!(result, json!("plain-text"));
    }

    #[test]
    fn test_vals_filter_passthrough_empty() {
        let value = json!("");
        let args = HashMap::new();
        let result = vals_filter(&value, &args).unwrap();
        assert_eq!(result, json!(""));
    }

    #[test]
    fn test_vals_filter_passthrough_boolean() {
        let args = HashMap::new();
        let result = vals_filter(&json!(true), &args).unwrap();
        assert_eq!(result, json!(true));
    }

    #[test]
    fn test_vals_filter_passthrough_null() {
        let args = HashMap::new();
        let result = vals_filter(&json!(null), &args).unwrap();
        assert_eq!(result, json!(null));
    }

    #[test]
    fn test_vals_filter_ref_value_errors() {
        // When vals is not installed: "vals CLI not found"
        // When vals is installed but backend unreachable: "vals eval failed"
        let value = json!("ref+vault://secret/key");
        let args = HashMap::new();
        let result = vals_filter(&value, &args);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("vals CLI not found") || err.contains("vals eval failed"),
            "Expected vals-related error, got: {err}"
        );
    }

    #[test]
    fn test_vals_filter_in_template_passthrough() {
        let template = "password={{ db_pw | vals }}";
        let context = json!({"db_pw": "plain-value"});
        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "password=plain-value");
    }

    #[test]
    fn test_vals_filter_in_template_ref_errors() {
        // With a ref+ value, vals is invoked — either it's missing or the backend is unreachable
        let template = "password={{ secret.KEY | vals }}";
        let context = json!({"secret": {"KEY": "ref+awsssm:///prod/db/password"}});
        let result = render_template(template, &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_vals_filter_chained_with_other_filters() {
        // vals on a non-ref+ string passes through, then upper transforms it
        let template = "{{ name | vals | upper }}";
        let context = json!({"name": "hello"});
        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "HELLO");
    }

    #[test]
    fn test_vals_filter_with_literal_string() {
        // Inline literal strings that don't start with ref+ pass through vals unchanged
        let template = "{{ 'plain-value' | vals }}";
        let context = json!({});
        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "plain-value");
    }

    // --- merge_defaults tests ---

    fn field(
        field_type: &str,
        required: bool,
        default: Option<serde_json::Value>,
    ) -> InputFieldDef {
        InputFieldDef {
            field_type: field_type.to_string(),
            name: None,
            description: None,
            required,
            secret: false,
            default,
            options: None,
            allow_custom: false,
            multiple: false,
            order: None,
        }
    }

    #[test]
    fn test_merge_defaults_fills_missing_fields() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );
        schema.insert(
            "retries".to_string(),
            field("number", false, Some(json!(3))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "staging");
        assert_eq!(result["retries"], 3);
    }

    #[test]
    fn test_merge_defaults_user_values_take_precedence() {
        let user_input = json!({"env": "production"});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "production");
    }

    #[test]
    fn test_merge_defaults_template_rendered_with_secrets() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "api_key".to_string(),
            field("string", false, Some(json!("{{ secret.API_KEY }}"))),
        );

        let context = json!({ "secret": { "API_KEY": "sk-12345" } });
        let result = merge_defaults(&user_input, &schema, &context).unwrap();
        assert_eq!(result["api_key"], "sk-12345");
    }

    #[test]
    fn test_merge_defaults_required_field_missing_skipped() {
        // Required fields without defaults are silently skipped (not an error).
        // Webhooks and triggers pass different input shapes that don't match
        // the task schema, so strict validation would break those flows.
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert("name".to_string(), field("string", true, None));

        let result = merge_defaults(&user_input, &schema, &json!({}));
        assert!(result.is_ok());
        // The field is simply absent in the result
        let obj = result.unwrap();
        assert!(obj.get("name").is_none());
    }

    #[test]
    fn test_merge_defaults_required_field_with_default_ok() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "name".to_string(),
            field("string", true, Some(json!("default-name"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["name"], "default-name");
    }

    #[test]
    fn test_merge_defaults_extra_user_fields_preserved() {
        let user_input = json!({"env": "prod", "custom_flag": true});
        let mut schema = HashMap::new();
        schema.insert("env".to_string(), field("string", false, None));

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "prod");
        assert_eq!(result["custom_flag"], true);
    }

    #[test]
    fn test_merge_defaults_empty_schema_empty_input() {
        let result = merge_defaults(&json!({}), &HashMap::new(), &json!({})).unwrap();
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_merge_defaults_non_string_defaults_passthrough() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert("count".to_string(), field("number", false, Some(json!(42))));
        schema.insert(
            "enabled".to_string(),
            field("boolean", false, Some(json!(true))),
        );
        schema.insert(
            "tags".to_string(),
            field("string", false, Some(json!(["a", "b"]))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["count"], 42);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["tags"], json!(["a", "b"]));
    }

    #[test]
    fn test_merge_defaults_string_without_template_passthrough() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "staging");
    }

    #[test]
    fn test_merge_defaults_template_render_error() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "key".to_string(),
            field("string", false, Some(json!("{{ missing.var }}"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({}));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("key"),
            "Error should mention field name: {err}"
        );
    }

    #[test]
    fn test_merge_defaults_optional_field_no_default_omitted() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert("optional_field".to_string(), field("string", false, None));

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert!(result.get("optional_field").is_none());
    }

    #[test]
    fn test_merge_defaults_null_input() {
        // null input treated as empty object
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let result = merge_defaults(&json!(null), &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "staging");
    }

    #[test]
    fn test_merge_defaults_required_field_provided_by_user() {
        let user_input = json!({"name": "alice"});
        let mut schema = HashMap::new();
        schema.insert("name".to_string(), field("string", true, None));

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["name"], "alice");
    }

    #[test]
    fn test_merge_defaults_mix_provided_and_defaulted() {
        let user_input = json!({"env": "production"});
        let mut schema = HashMap::new();
        schema.insert("env".to_string(), field("string", true, None));
        schema.insert(
            "retries".to_string(),
            field("number", false, Some(json!(3))),
        );
        schema.insert(
            "notify".to_string(),
            field("boolean", false, Some(json!(true))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "production");
        assert_eq!(result["retries"], 3);
        assert_eq!(result["notify"], true);
    }

    #[test]
    fn test_merge_defaults_secret_field_omitted_uses_default() {
        // Simulates the UI omitting a secret field (user didn't change the sentinel).
        // merge_defaults should fill in the default and render the template.
        let mut schema = HashMap::new();
        schema.insert(
            "api_key".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                name: None,
                description: None,
                required: false,
                secret: true,
                default: Some(json!("the-secret-value")),
                options: None,
                allow_custom: false,
                multiple: false,
                order: None,
            },
        );
        schema.insert(
            "env".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                name: None,
                description: None,
                required: false,
                secret: false,
                default: Some(json!("staging")),
                options: None,
                allow_custom: false,
                multiple: false,
                order: None,
            },
        );

        // User provides env but omits api_key (secret sentinel was unchanged)
        let user_input = json!({"env": "production"});
        let context = json!({});
        let result = merge_defaults(&user_input, &schema, &context).unwrap();

        assert_eq!(result["env"], "production");
        assert_eq!(result["api_key"], "the-secret-value");
    }

    #[test]
    fn test_merge_defaults_array_default_for_multiple_field() {
        // multiple:true field with an array default — user omits it, default fills in verbatim.
        let mut schema = HashMap::new();
        schema.insert(
            "environments".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                multiple: true,
                options: Some(vec!["dev".into(), "staging".into(), "prod".into()]),
                default: Some(json!(["dev", "staging"])),
                ..Default::default()
            },
        );

        let result = merge_defaults(&json!({}), &schema, &json!({})).unwrap();
        assert_eq!(result["environments"], json!(["dev", "staging"]));
        // Crucially: the value is an array, not a stringified one.
        assert!(result["environments"].is_array());
    }

    #[test]
    fn test_render_template_join_filter_on_array_input() {
        // The multi-select.yaml fixture relies on `{{ input.environments | join(sep=', ') }}`
        // rendering correctly when `input.environments` is a JSON array.
        let context = json!({
            "input": {
                "environments": ["dev", "staging"],
            }
        });
        let result =
            render_template("{{ input.environments | join(sep=', ') }}", &context).unwrap();
        assert_eq!(result, "dev, staging");
    }

    #[test]
    fn test_resolve_rerun_sentinels_array_passthrough_for_multiple_field() {
        // A plain (non-secret, non-connection) multiple-true field with an array value
        // must pass through resolve_rerun_sentinels unchanged.
        let mut schema = HashMap::new();
        schema.insert(
            "environments".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                multiple: true,
                options: Some(vec!["dev".into(), "staging".into()]),
                ..Default::default()
            },
        );

        let incoming = json!({ "environments": ["dev", "staging"] });
        let source_raw_input = json!({ "environments": ["prod"] });

        let result = resolve_rerun_sentinels(&incoming, &source_raw_input, &schema).unwrap();
        // The incoming array wins — no sentinel was sent, so the source value is not consulted.
        assert_eq!(result["environments"], json!(["dev", "staging"]));
    }

    // --- resolve_connection_inputs tests ---

    use crate::models::workflow::{ConnectionDef, ConnectionPropertyDef, ConnectionTypeDef};

    fn make_ws_with_connection() -> WorkspaceConfig {
        let mut ws = WorkspaceConfig::new();
        ws.connection_types.insert(
            "postgres".to_string(),
            ConnectionTypeDef {
                properties: {
                    let mut props = HashMap::new();
                    props.insert(
                        "host".to_string(),
                        ConnectionPropertyDef {
                            property_type: "string".to_string(),
                            required: true,
                            default: None,
                            secret: false,
                        },
                    );
                    props.insert(
                        "port".to_string(),
                        ConnectionPropertyDef {
                            property_type: "integer".to_string(),
                            required: false,
                            default: Some(json!(5432)),
                            secret: false,
                        },
                    );
                    props
                },
            },
        );
        ws.connections.insert(
            "prod_db".to_string(),
            ConnectionDef {
                connection_type: Some("postgres".to_string()),
                shared: false,
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("db.example.com"));
                    v.insert("port".to_string(), json!(5432));
                    v.insert("database".to_string(), json!("myapp"));
                    v
                },
            },
        );
        ws
    }

    /// Test lookup over several named workspaces. `unavailable` names are
    /// configured-but-unloaded (Lookup::Unavailable); everything else unknown.
    struct MultiWs {
        local: String,
        configs: HashMap<String, WorkspaceConfig>,
        unavailable: Vec<String>,
    }
    impl WorkspaceLookup for MultiWs {
        fn local_name(&self) -> &str {
            &self.local
        }
        fn get(&self, name: &str) -> Lookup<'_> {
            if let Some(c) = self.configs.get(name) {
                Lookup::Found(c)
            } else if self.unavailable.iter().any(|u| u == name) {
                Lookup::Unavailable
            } else {
                Lookup::Unknown
            }
        }
    }

    fn conn(type_name: Option<&str>, shared: bool, host: &str) -> ConnectionDef {
        ConnectionDef {
            connection_type: type_name.map(|s| s.to_string()),
            shared,
            values: HashMap::from([("host".to_string(), json!(host))]),
        }
    }

    fn empty_type() -> ConnectionTypeDef {
        ConnectionTypeDef {
            properties: HashMap::new(),
        }
    }

    /// caller: no types/connections. jobs: type `clickhouse`, connections
    /// `clickhouse-prod` (shared) and `private-ch` (not shared).
    /// infra: connection `ch-eu` with `type: jobs.clickhouse`, shared.
    fn three_workspaces() -> MultiWs {
        let caller = WorkspaceConfig::default();
        let mut jobs = WorkspaceConfig::default();
        jobs.connection_types
            .insert("clickhouse".to_string(), empty_type());
        jobs.connections.insert(
            "clickhouse-prod".to_string(),
            conn(Some("clickhouse"), true, "ch.jobs.internal"),
        );
        jobs.connections.insert(
            "private-ch".to_string(),
            conn(Some("clickhouse"), false, "ch.private.internal"),
        );
        let mut infra = WorkspaceConfig::default();
        infra.connections.insert(
            "ch-eu".to_string(),
            conn(Some("jobs.clickhouse"), true, "ch.eu.internal"),
        );
        MultiWs {
            local: "caller".to_string(),
            configs: HashMap::from([
                ("caller".to_string(), caller),
                ("jobs".to_string(), jobs),
                ("infra".to_string(), infra),
            ]),
            unavailable: vec!["broken".to_string()],
        }
    }

    fn schema_of(field_type: &str) -> HashMap<String, InputFieldDef> {
        HashMap::from([("ch".to_string(), field(field_type, false, None))])
    }

    // ── canonical_type_ref ──────────────────────────────────────────────

    #[test]
    fn test_canonical_bare_type_is_local() {
        let ws = three_workspaces();
        let ct = canonical_type_ref("clickhouse", "jobs", &ws).unwrap();
        assert_eq!(
            ct,
            CanonicalType {
                workspace: "jobs".into(),
                name: "clickhouse".into()
            }
        );
        assert_eq!(ct.to_string(), "jobs.clickhouse");
    }

    #[test]
    fn test_canonical_dotted_type_resolves_to_owner() {
        let ws = three_workspaces();
        let ct = canonical_type_ref("jobs.clickhouse", "caller", &ws).unwrap();
        assert_eq!(ct.workspace, "jobs");
        assert_eq!(ct.name, "clickhouse");
    }

    #[test]
    fn test_canonical_literal_local_key_wins_over_split() {
        // Library-flattened type `common.pg` is a literal local key.
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("caller")
            .unwrap()
            .connection_types
            .insert("common.pg".to_string(), empty_type());
        let ct = canonical_type_ref("common.pg", "caller", &ws).unwrap();
        assert_eq!(
            ct,
            CanonicalType {
                workspace: "caller".into(),
                name: "common.pg".into()
            }
        );
    }

    #[test]
    fn test_canonical_unknown_prefix_is_opaque_local() {
        let ws = three_workspaces();
        let ct = canonical_type_ref("nope.thing", "caller", &ws).unwrap();
        assert_eq!(
            ct,
            CanonicalType {
                workspace: "caller".into(),
                name: "nope.thing".into()
            }
        );
    }

    #[test]
    fn test_canonical_known_ws_missing_type_is_error() {
        let ws = three_workspaces();
        let err = canonical_type_ref("jobs.mysql", "caller", &ws).unwrap_err();
        assert!(
            err.to_string().contains("has no connection type 'mysql'"),
            "{err}"
        );
    }

    #[test]
    fn test_canonical_unavailable_ws_is_error() {
        let ws = three_workspaces();
        let err = canonical_type_ref("broken.x", "caller", &ws).unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }

    // ── resolve_connection_inputs (qualified refs) ─────────────────────

    #[test]
    fn test_resolve_qualified_shared_connection_from_owner() {
        let ws = three_workspaces();
        let out = resolve_connection_inputs(
            &json!({"ch": "jobs.clickhouse-prod"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.jobs.internal");
    }

    #[test]
    fn test_resolve_qualified_unshared_connection_is_rejected() {
        let ws = three_workspaces();
        let err = resolve_connection_inputs(
            &json!({"ch": "jobs.private-ch"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");
    }

    #[test]
    fn test_resolve_two_hop_connection_declaring_foreign_type() {
        // infra.ch-eu declares `type: jobs.clickhouse` → canonical (jobs, clickhouse)
        let ws = three_workspaces();
        let out = resolve_connection_inputs(
            &json!({"ch": "infra.ch-eu"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.eu.internal");
    }

    #[test]
    fn test_resolve_local_bare_type_rejects_foreign_connection() {
        // caller declares its OWN `clickhouse` type → (caller, clickhouse) ≠ (jobs, clickhouse)
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("caller")
            .unwrap()
            .connection_types
            .insert("clickhouse".to_string(), empty_type());
        let err = resolve_connection_inputs(
            &json!({"ch": "jobs.clickhouse-prod"}),
            &schema_of("clickhouse"),
            &ws,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expects type 'caller.clickhouse'"), "{msg}");
        assert!(msg.contains("is type 'jobs.clickhouse'"), "{msg}");
    }

    #[test]
    fn test_resolve_unknown_workspace_is_user_error() {
        let ws = three_workspaces();
        let err =
            resolve_connection_inputs(&json!({"ch": "nope.x"}), &schema_of("jobs.clickhouse"), &ws)
                .unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown workspace 'nope'"),
            "{err:#}"
        );
    }

    #[test]
    fn test_resolve_unavailable_workspace_is_distinct_error() {
        let ws = three_workspaces();
        let err = resolve_connection_inputs(
            &json!({"ch": "broken.x"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not available"), "{msg}");
        assert!(!msg.contains("unknown workspace"), "{msg}");
    }

    #[test]
    fn test_resolve_local_connection_declaring_foreign_type_matches() {
        // caller's own `local-ch` says `type: jobs.clickhouse` → satisfies `type: jobs.clickhouse`
        let mut ws = three_workspaces();
        ws.configs.get_mut("caller").unwrap().connections.insert(
            "local-ch".to_string(),
            conn(Some("jobs.clickhouse"), false, "ch.local"),
        );
        let out = resolve_connection_inputs(
            &json!({"ch": "local-ch"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.local");
    }

    #[test]
    fn test_resolve_untyped_shared_connection_matches_any_type() {
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("jobs")
            .unwrap()
            .connections
            .insert("loose".to_string(), conn(None, true, "loose.host"));
        let out = resolve_connection_inputs(
            &json!({"ch": "jobs.loose"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "loose.host");
    }

    #[test]
    fn test_resolve_foreign_typed_connection_gets_type_defaults_and_is_checked() {
        let mut ws = three_workspaces();
        // jobs.clickhouse gains a defaulted `port` and a required `host`.
        let type_def = ConnectionTypeDef {
            properties: HashMap::from([
                (
                    "port".to_string(),
                    crate::models::workflow::ConnectionPropertyDef {
                        property_type: "integer".into(),
                        required: false,
                        default: Some(json!(8443)),
                        secret: false,
                    },
                ),
                (
                    "host".to_string(),
                    crate::models::workflow::ConnectionPropertyDef {
                        property_type: "string".into(),
                        required: true,
                        default: None,
                        secret: false,
                    },
                ),
            ]),
        };
        ws.configs
            .get_mut("jobs")
            .unwrap()
            .connection_types
            .insert("clickhouse".to_string(), type_def);
        // Two-hop infra.ch-eu: gets port default applied at resolution.
        let out = resolve_connection_inputs(
            &json!({"ch": "infra.ch-eu"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap();
        assert_eq!(out["ch"]["port"], 8443);
        // A foreign-typed connection missing a required field is rejected.
        ws.configs.get_mut("infra").unwrap().connections.insert(
            "bad".to_string(),
            ConnectionDef {
                connection_type: Some("jobs.clickhouse".into()),
                shared: true,
                values: HashMap::new(),
            },
        );
        let err = resolve_connection_inputs(
            &json!({"ch": "infra.bad"}),
            &schema_of("jobs.clickhouse"),
            &ws,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("missing required field 'host'"),
            "{err:#}"
        );
    }

    #[test]
    fn test_resolve_opaque_dotted_type_string_compare_compat() {
        // Pre-existing configs: input and connection both say `type: foo.bar`,
        // no such type and no workspace `foo`. Must keep working.
        let mut ws = three_workspaces();
        ws.configs.get_mut("caller").unwrap().connections.insert(
            "legacy".to_string(),
            conn(Some("foo.bar"), false, "legacy.host"),
        );
        let out = resolve_connection_inputs(&json!({"ch": "legacy"}), &schema_of("foo.bar"), &ws)
            .unwrap();
        assert_eq!(out["ch"]["host"], "legacy.host");
    }

    #[test]
    fn test_single_workspace_rejects_qualified_refs_with_server_hint() {
        let ws = make_ws_with_connection();
        let single = SingleWorkspace {
            name: "local",
            config: &ws,
        };
        let err = resolve_connection_inputs(
            &json!({"db": "other.prod_db"}),
            &{
                let mut s = HashMap::new();
                s.insert("db".to_string(), field("postgres", false, None));
                s
            },
            &single,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("require a server"), "{err:#}");
    }

    // ── prepare_action_input_cross (provenance-aware two-pass) ─────────

    #[test]
    fn test_cross_caller_supplied_bare_name_prefers_caller_then_shared_owner() {
        let mut ws = three_workspaces();
        // Owner action schema (written in jobs): ch: type clickhouse, default private-ch
        let mut schema = HashMap::new();
        schema.insert(
            "ch".to_string(),
            field("clickhouse", false, Some(json!("private-ch"))),
        );
        // (1) caller supplies shared owner name bare → falls back to owner, ok
        let out = prepare_action_input_cross(
            &json!({"ch": "clickhouse-prod"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.jobs.internal");
        // (2) caller supplies UNSHARED owner name bare → rejected
        let err = prepare_action_input_cross(
            &json!({"ch": "private-ch"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found in workspace 'caller'"), "{msg}");
        assert!(
            msg.contains("'jobs.private-ch' exists but is not shared"),
            "{msg}"
        );
        // (3) caller-local bare name wins over an owner name of the same spelling
        ws.configs.get_mut("caller").unwrap().connections.insert(
            "clickhouse-prod".to_string(),
            conn(Some("jobs.clickhouse"), false, "ch.caller.local"),
        );
        let out = prepare_action_input_cross(
            &json!({"ch": "clickhouse-prod"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap();
        assert_eq!(out["ch"]["host"], "ch.caller.local");
    }

    #[test]
    fn test_cross_owner_default_resolves_ungated_in_owner() {
        let ws = three_workspaces();
        let mut schema = HashMap::new();
        schema.insert(
            "ch".to_string(),
            field("clickhouse", false, Some(json!("private-ch"))),
        );
        // Caller supplies nothing → owner's default `private-ch` (unshared) resolves.
        let out = prepare_action_input_cross(&json!({}), &schema, &ws, "caller", "jobs").unwrap();
        assert_eq!(out["ch"]["host"], "ch.private.internal");
    }

    #[test]
    fn test_cross_caller_value_is_not_rendered_against_owner_secrets() {
        let mut ws = three_workspaces();
        ws.configs
            .get_mut("jobs")
            .unwrap()
            .secrets
            .insert("TOKEN".to_string(), json!("owner-secret"));

        let mut schema = HashMap::new();
        schema.insert("ch".to_string(), field("clickhouse", false, None));
        schema.insert("note".to_string(), field("string", false, None));

        let out = prepare_action_input_cross(
            &json!({"ch": "clickhouse-prod", "note": "{{ secret.TOKEN }}"}),
            &schema,
            &ws,
            "caller",
            "jobs",
        )
        .unwrap();

        // Caller-supplied "note" must pass through verbatim, never rendered
        // against the owner's secrets.
        assert_eq!(out["note"], "{{ secret.TOKEN }}");
        assert_eq!(out["ch"]["host"], "ch.jobs.internal");
    }

    #[test]
    fn test_prepare_action_input_local_unchanged() {
        let ws = make_ws_with_connection();
        let single = SingleWorkspace {
            name: "local",
            config: &ws,
        };
        let mut schema = HashMap::new();
        schema.insert(
            "db".to_string(),
            field("postgres", false, Some(json!("prod_db"))),
        );
        let out = prepare_action_input(&json!({}), &schema, &single).unwrap();
        assert_eq!(out["db"]["host"], "db.example.com");
    }

    #[test]
    fn test_resolve_connection_inputs_valid() {
        let ws = make_ws_with_connection();
        let input = json!({"db": "prod_db", "env": "production"});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));
        schema.insert("env".to_string(), field("string", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        // db should be resolved to the connection's values
        assert_eq!(result["db"]["host"], "db.example.com");
        assert_eq!(result["db"]["port"], 5432);
        assert_eq!(result["db"]["database"], "myapp");
        // env should pass through unchanged
        assert_eq!(result["env"], "production");
    }

    #[test]
    fn test_resolve_connection_inputs_missing_connection() {
        let ws = make_ws_with_connection();
        let input = json!({"db": "nonexistent"});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        );
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("nonexistent"));
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn test_resolve_connection_inputs_type_mismatch() {
        let mut ws = make_ws_with_connection();
        ws.connection_types.insert(
            "redis".to_string(),
            ConnectionTypeDef {
                properties: HashMap::new(),
            },
        );
        // prod_db is type: postgres, but schema expects redis
        let input = json!({"cache": "prod_db"});
        let mut schema = HashMap::new();
        schema.insert("cache".to_string(), field("redis", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        );
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("expects type") && err.contains("redis"));
        assert!(err.contains("is type") && err.contains("postgres"));
    }

    #[test]
    fn test_resolve_connection_inputs_primitives_passthrough() {
        let ws = WorkspaceConfig::new();
        let input = json!({"name": "alice", "count": 5});
        let mut schema = HashMap::new();
        schema.insert("name".to_string(), field("string", false, None));
        schema.insert("count".to_string(), field("integer", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["name"], "alice");
        assert_eq!(result["count"], 5);
    }

    #[test]
    fn test_resolve_connection_inputs_text_type_is_primitive() {
        let ws = WorkspaceConfig::new();
        let input = json!({"query": "SELECT *\nFROM users\nWHERE active = true"});
        let mut schema = HashMap::new();
        schema.insert("query".to_string(), field("text", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["query"], "SELECT *\nFROM users\nWHERE active = true");
    }

    #[test]
    fn test_resolve_connection_inputs_empty_input() {
        let ws = make_ws_with_connection();
        let input = json!({});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        // Missing field should be skipped (not an error)
        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert!(result.get("db").is_none());
    }

    #[test]
    fn test_resolve_connection_inputs_object_passthrough() {
        let ws = make_ws_with_connection();
        // Already an object (inline connection data) — should pass through
        let input = json!({"db": {"host": "inline.example.com", "port": 5433}});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["db"]["host"], "inline.example.com");
        assert_eq!(result["db"]["port"], 5433);
    }

    #[test]
    fn test_render_input_map_preserves_connection_object() {
        // When a step input template references a connection object (e.g. {{ input.db }}),
        // Tera renders it as a JSON string. render_input_map should parse it back to
        // preserve the structured value, so downstream action spec templates can access
        // nested fields like {{ input.db.host }}.
        let mut input_map = HashMap::new();
        input_map.insert("db".to_string(), json!("{{ input.db }}"));
        input_map.insert("env".to_string(), json!("{{ input.env }}"));

        let context = json!({
            "input": {
                "db": {
                    "host": "db.example.com",
                    "port": 5432,
                    "database": "myapp"
                },
                "env": "production"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        // Connection object should be preserved as a structured value
        assert!(
            result["db"].is_object(),
            "db should be an object, got: {}",
            result["db"]
        );
        assert_eq!(result["db"]["host"], "db.example.com");
        assert_eq!(result["db"]["port"], 5432);
        assert_eq!(result["db"]["database"], "myapp");
        // Simple string should stay as string
        assert!(result["env"].is_string());
        assert_eq!(result["env"], "production");
    }

    #[test]
    fn test_render_input_map_preserves_array() {
        let mut input_map = HashMap::new();
        input_map.insert("tags".to_string(), json!("{{ input.tags }}"));

        let context = json!({
            "input": {
                "tags": ["web", "backend"]
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert!(
            result["tags"].is_array(),
            "tags should be an array, got: {}",
            result["tags"]
        );
    }

    #[test]
    fn test_render_input_map_string_starting_with_brace_stays_string() {
        // A rendered string that starts with '{' but isn't valid JSON should stay as string
        let mut input_map = HashMap::new();
        input_map.insert("msg".to_string(), json!("{prefix} {{ input.name }}"));

        let context = json!({
            "input": {
                "name": "alice"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert!(result["msg"].is_string());
        assert_eq!(result["msg"], "{prefix} alice");
    }

    #[test]
    fn test_render_input_map_connection_then_action_spec() {
        // Full chain: resolved connection in job input → step input template → action spec context
        // Step 1: Simulate job input with resolved connection
        let job_input = json!({
            "db": {
                "host": "db.example.com",
                "port": 5432,
                "database": "myapp"
            },
            "env": "production"
        });

        // Step 2: Render step input template (flow_step.input)
        let mut step_input = HashMap::new();
        step_input.insert("db".to_string(), json!("{{ input.db }}"));
        step_input.insert("env".to_string(), json!("{{ input.env }}"));

        let step_context = json!({ "input": job_input });
        let rendered_step_input = render_input_map(&step_input, &step_context).unwrap();

        // Step 3: Build action spec context from rendered step input
        let spec_context = json!({ "input": rendered_step_input });

        // Step 4: Action cmd template can access nested fields
        let cmd = render_template(
            "migrate --host {{ input.db.host }} --port {{ input.db.port }} --env {{ input.env }}",
            &spec_context,
        )
        .unwrap();
        assert_eq!(
            cmd,
            "migrate --host db.example.com --port 5432 --env production"
        );
    }

    #[test]
    fn test_resolve_connection_inputs_non_string_non_object_value() {
        // Passing a number/boolean/array as connection input should error
        let ws = make_ws_with_connection();
        let input = json!({"db": 42});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("expects a connection name"));
    }

    #[test]
    fn test_resolve_connection_inputs_mixed_primitive_and_connection() {
        let ws = make_ws_with_connection();
        let input = json!({"db": "prod_db", "env": "production", "retries": 3, "debug": false});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));
        schema.insert("env".to_string(), field("string", false, None));
        schema.insert("retries".to_string(), field("integer", false, None));
        schema.insert("debug".to_string(), field("boolean", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert!(result["db"].is_object());
        assert_eq!(result["db"]["host"], "db.example.com");
        assert_eq!(result["env"], "production");
        assert_eq!(result["retries"], 3);
        assert_eq!(result["debug"], false);
    }

    #[test]
    fn test_resolve_connection_inputs_date_datetime_are_primitives() {
        let ws = WorkspaceConfig::new();
        let input = json!({"start": "2024-01-01", "ts": "2024-01-01T00:00:00Z"});
        let mut schema = HashMap::new();
        schema.insert("start".to_string(), field("date", false, None));
        schema.insert("ts".to_string(), field("datetime", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["start"], "2024-01-01");
        assert_eq!(result["ts"], "2024-01-01T00:00:00Z");
    }

    #[test]
    fn test_resolve_connection_inputs_number_primitive_passthrough() {
        let ws = WorkspaceConfig::new();
        let input = json!({"ratio": 0.75});
        let mut schema = HashMap::new();
        schema.insert("ratio".to_string(), field("number", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["ratio"], 0.75);
    }

    #[test]
    fn test_resolve_connection_inputs_field_not_in_schema_passes_through() {
        // Fields not present in the schema should be left in the output untouched.
        let ws = make_ws_with_connection();
        let input = json!({"extra_field": "some-value", "count": 99});
        let schema = HashMap::new();

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["extra_field"], "some-value");
        assert_eq!(result["count"], 99);
    }

    #[test]
    fn test_resolve_connection_inputs_null_input_treated_as_empty() {
        let ws = make_ws_with_connection();
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        // null input is treated as empty object — missing field is silently skipped
        let result = resolve_connection_inputs(
            &json!(null),
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert!(result.get("db").is_none());
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_resolve_connection_inputs_boolean_value_for_connection_field_errors() {
        let ws = make_ws_with_connection();
        let input = json!({"db": true});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("expects a connection name"));
    }

    #[test]
    fn test_resolve_connection_inputs_multiple_connection_fields() {
        let mut ws = WorkspaceConfig::new();
        ws.connections.insert(
            "primary_db".to_string(),
            ConnectionDef {
                connection_type: None,
                shared: false,
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("primary.example.com"));
                    v
                },
            },
        );
        ws.connections.insert(
            "replica_db".to_string(),
            ConnectionDef {
                connection_type: None,
                shared: false,
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("replica.example.com"));
                    v
                },
            },
        );

        let input = json!({"primary": "primary_db", "replica": "replica_db"});
        let mut schema = HashMap::new();
        schema.insert("primary".to_string(), field("postgres", false, None));
        schema.insert("replica".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["primary"]["host"], "primary.example.com");
        assert_eq!(result["replica"]["host"], "replica.example.com");
    }

    #[test]
    fn test_resolve_connection_inputs_empty_schema_passes_input_through() {
        let ws = WorkspaceConfig::new();
        let input = json!({"key": "value", "count": 5});
        let schema = HashMap::new();

        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["key"], "value");
        assert_eq!(result["count"], 5);
    }

    #[test]
    fn test_resolve_connection_inputs_untyped_connection() {
        let mut ws = WorkspaceConfig::new();
        ws.connection_types.insert(
            "custom".to_string(),
            ConnectionTypeDef {
                properties: HashMap::new(),
            },
        );
        ws.connections.insert(
            "my_api".to_string(),
            ConnectionDef {
                connection_type: None, // untyped
                shared: false,
                values: {
                    let mut v = HashMap::new();
                    v.insert("url".to_string(), json!("https://api.example.com"));
                    v
                },
            },
        );

        let input = json!({"api": "my_api"});
        let mut schema = HashMap::new();
        schema.insert("api".to_string(), field("custom", false, None));

        // Untyped connection: no type mismatch check, just resolve
        let result = resolve_connection_inputs(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();
        assert_eq!(result["api"]["url"], "https://api.example.com");
    }

    // --- render_value_deep tests ---

    #[test]
    fn test_render_value_deep_string_with_template() {
        let value = json!("Hello {{ name }}");
        let context = json!({"name": "World"});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result, json!("Hello World"));
    }

    #[test]
    fn test_render_value_deep_string_without_template() {
        let value = json!("plain text");
        let context = json!({});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result, json!("plain text"));
    }

    #[test]
    fn test_render_value_deep_object_with_nested_templates() {
        let value = json!({
            "host": "{{ secret.db.host }}",
            "port": 8443,
            "nested": {
                "url": "https://{{ secret.db.host }}:8443"
            }
        });
        let context = json!({"secret": {"db": {"host": "clickhouse.example.com"}}});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result["host"], "clickhouse.example.com");
        assert_eq!(result["port"], 8443);
        assert_eq!(
            result["nested"]["url"],
            "https://clickhouse.example.com:8443"
        );
    }

    #[test]
    fn test_render_value_deep_array_with_templates() {
        let value = json!(["{{ name }}", "plain", 42]);
        let context = json!({"name": "alice"});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result, json!(["alice", "plain", 42]));
    }

    #[test]
    fn test_render_value_deep_primitives_passthrough() {
        let context = json!({});
        assert_eq!(render_value_deep(&json!(42), &context).unwrap(), json!(42));
        assert_eq!(
            render_value_deep(&json!(true), &context).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value_deep(&json!(null), &context).unwrap(),
            json!(null)
        );
        assert_eq!(
            render_value_deep(&json!(2.72), &context).unwrap(),
            json!(2.72)
        );
    }

    #[test]
    fn test_render_value_deep_error_propagates() {
        let value = json!({"key": "{{ missing.var }}"});
        let context = json!({});
        let result = render_value_deep(&value, &context);
        assert!(result.is_err());
    }

    // --- merge_action_defaults tests ---

    #[test]
    fn test_merge_action_defaults_object_default_with_templates() {
        let rendered_input = json!({"sql": "SELECT 1"});
        let mut schema = HashMap::new();
        schema.insert(
            "clickhouse".to_string(),
            field(
                "object",
                false,
                Some(json!({
                    "host": "{{ secret.clickhouse.host }}",
                    "port": 8443
                })),
            ),
        );
        schema.insert("sql".to_string(), field("string", true, None));

        let context = json!({"secret": {"clickhouse": {"host": "ch.example.com"}}});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result["sql"], "SELECT 1");
        assert_eq!(result["clickhouse"]["host"], "ch.example.com");
        assert_eq!(result["clickhouse"]["port"], 8443);
    }

    #[test]
    fn test_merge_action_defaults_user_value_takes_precedence() {
        let rendered_input = json!({"clickhouse": {"host": "custom.host", "port": 9000}});
        let mut schema = HashMap::new();
        schema.insert(
            "clickhouse".to_string(),
            field(
                "object",
                false,
                Some(json!({
                    "host": "{{ secret.clickhouse.host }}",
                    "port": 8443
                })),
            ),
        );

        let context = json!({"secret": {"clickhouse": {"host": "ch.example.com"}}});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        // User-provided value should win
        assert_eq!(result["clickhouse"]["host"], "custom.host");
        assert_eq!(result["clickhouse"]["port"], 9000);
    }

    #[test]
    fn test_merge_action_defaults_plain_string_default() {
        let rendered_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let context = json!({});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result["env"], "staging");
    }

    #[test]
    fn test_merge_action_defaults_empty_schema_noop() {
        let rendered_input = json!({"key": "value"});
        let schema = HashMap::new();
        let context = json!({});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result, json!({"key": "value"}));
    }

    #[test]
    fn test_merge_action_defaults_does_not_rerender_caller_values() {
        let mut schema = HashMap::new();
        schema.insert(
            "a".to_string(),
            field("string", false, Some(json!("{{ secret.x }}"))),
        );
        schema.insert("b".to_string(), field("string", false, None));

        let context = json!({"secret": {"x": "OWNER_SECRET", "y": "OTHER"}});
        let rendered_input = json!({"b": "{{ secret.y }}"});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();

        // "a" was filled from the schema default → rendered.
        assert_eq!(result["a"], "OWNER_SECRET");
        // "b" was caller-supplied → passed through verbatim, NOT re-rendered.
        assert_eq!(result["b"], "{{ secret.y }}");
    }

    #[test]
    fn test_merge_action_defaults_deeply_nested_templates() {
        let rendered_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "config".to_string(),
            field(
                "object",
                false,
                Some(json!({
                    "level1": {
                        "level2": {
                            "value": "{{ secret.deep }}"
                        }
                    }
                })),
            ),
        );

        let context = json!({"secret": {"deep": "resolved"}});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result["config"]["level1"]["level2"]["value"], "resolved");
    }

    #[test]
    fn test_prepare_action_input_merges_defaults_and_resolves_connections() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert(
            "ch".to_string(),
            json!({"host": "ch.example.com", "pass": "s3cret"}),
        );
        ws.connections.insert(
            "ch-prod".to_string(),
            crate::models::workflow::ConnectionDef {
                connection_type: None,
                shared: false,
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("ch.example.com"));
                    v.insert("port".to_string(), json!(8123));
                    v.insert("password".to_string(), json!("s3cret"));
                    v
                },
            },
        );

        // Action input: connection type with default, plus a plain string
        let mut schema = HashMap::new();
        schema.insert(
            "db".to_string(),
            field("clickhouse", false, Some(json!("ch-prod"))),
        );
        schema.insert("query".to_string(), field("string", true, None));

        // User provides query but not db — default "ch-prod" should be filled and resolved
        let input = json!({"query": "SELECT 1"});
        let result = prepare_action_input(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();

        assert_eq!(result["query"], "SELECT 1");
        assert_eq!(result["db"]["host"], "ch.example.com");
        assert_eq!(result["db"]["port"], 8123);
        assert_eq!(result["db"]["password"], "s3cret");
    }

    #[test]
    fn test_prepare_action_input_user_provided_connection_name() {
        let mut ws = WorkspaceConfig::new();
        ws.connections.insert(
            "my-conn".to_string(),
            crate::models::workflow::ConnectionDef {
                connection_type: None,
                shared: false,
                values: {
                    let mut v = HashMap::new();
                    v.insert("url".to_string(), json!("https://example.com"));
                    v
                },
            },
        );

        let mut schema = HashMap::new();
        schema.insert("api".to_string(), field("custom_api", false, None));

        // User explicitly provides the connection name
        let input = json!({"api": "my-conn"});
        let result = prepare_action_input(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();

        assert_eq!(result["api"]["url"], "https://example.com");
    }

    #[test]
    fn test_prepare_action_input_secret_in_default() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets
            .insert("api_key".to_string(), json!("secret-key-123"));

        let mut schema = HashMap::new();
        schema.insert(
            "token".to_string(),
            field("string", false, Some(json!("{{ secret.api_key }}"))),
        );

        let input = json!({});
        let result = prepare_action_input(
            &input,
            &schema,
            &SingleWorkspace {
                name: "local",
                config: &ws,
            },
        )
        .unwrap();

        assert_eq!(result["token"], "secret-key-123");
    }

    #[test]
    fn test_render_json_strings_nested() {
        let value = json!({
            "metadata": {
                "namespace": "production"
            },
            "spec": {
                "serviceAccountName": "{{ input.service_account }}"
            }
        });
        let context = json!({ "input": { "service_account": "my-sa" } });
        let result = render_json_strings(&value, &context).unwrap();
        assert_eq!(result["metadata"]["namespace"], "production");
        assert_eq!(result["spec"]["serviceAccountName"], "my-sa");
    }

    #[test]
    fn test_render_json_strings_preserves_non_strings() {
        let value = json!({
            "replicas": 3,
            "enabled": true,
            "name": "{{ input.name }}",
            "items": [1, "{{ input.label }}", null]
        });
        let context = json!({ "input": { "name": "test", "label": "prod" } });
        let result = render_json_strings(&value, &context).unwrap();
        assert_eq!(result["replicas"], 3);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["name"], "test");
        assert_eq!(result["items"][0], 1);
        assert_eq!(result["items"][1], "prod");
        assert!(result["items"][2].is_null());
    }

    #[test]
    fn test_render_json_strings_error_on_bad_template() {
        let value = json!({ "key": "{{ missing.var }}" });
        let context = json!({});
        let result = render_json_strings(&value, &context);
        assert!(result.is_err());
    }

    // ─── evaluate_condition tests ────────────────────────────────────────

    #[test]
    fn test_evaluate_condition_true_string() {
        let ctx = json!({});
        assert!(evaluate_condition("true", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_false_string() {
        let ctx = json!({});
        assert!(!evaluate_condition("false", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_zero_string() {
        let ctx = json!({});
        assert!(!evaluate_condition("0", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_empty_string() {
        let ctx = json!({});
        assert!(!evaluate_condition("", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_nonempty_string() {
        let ctx = json!({});
        assert!(evaluate_condition("yes", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_one_string() {
        let ctx = json!({});
        assert!(evaluate_condition("1", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_template_true() {
        let ctx = json!({"input": {"deploy": true}});
        assert!(evaluate_condition("{{ input.deploy }}", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_template_false() {
        let ctx = json!({"input": {"deploy": false}});
        assert!(!evaluate_condition("{{ input.deploy }}", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_tera_comparison() {
        let ctx = json!({"input": {"env": "production"}});
        assert!(
            evaluate_condition("{% if input.env == \"production\" %}true{% endif %}", &ctx,)
                .unwrap()
        );
        assert!(
            !evaluate_condition("{% if input.env == \"staging\" %}true{% endif %}", &ctx,).unwrap()
        );
    }

    #[test]
    fn test_evaluate_condition_step_output() {
        let ctx = json!({"check": {"output": {"has_data": true}}});
        assert!(evaluate_condition("{{ check.output.has_data }}", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_step_output_false() {
        let ctx = json!({"check": {"output": {"has_data": false}}});
        assert!(!evaluate_condition("{{ check.output.has_data }}", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_undefined_variable_is_error() {
        let ctx = json!({});
        assert!(evaluate_condition("{{ missing.var }}", &ctx).is_err());
    }

    #[test]
    fn test_evaluate_condition_whitespace_trimmed() {
        let ctx = json!({});
        assert!(evaluate_condition("  true  ", &ctx).unwrap());
        assert!(!evaluate_condition("  false  ", &ctx).unwrap());
        assert!(!evaluate_condition("  0  ", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_not_operator() {
        let ctx = json!({"check": {"output": {"has_data": false}}});
        // Tera `not` filter
        assert!(
            evaluate_condition("{% if not check.output.has_data %}true{% endif %}", &ctx,).unwrap()
        );
    }

    #[test]
    fn test_evaluate_condition_false_case_insensitive() {
        let ctx = json!({});
        assert!(!evaluate_condition("False", &ctx).unwrap());
        assert!(!evaluate_condition("FALSE", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_null_is_falsy() {
        let ctx = json!({});
        assert!(!evaluate_condition("null", &ctx).unwrap());
        assert!(!evaluate_condition("Null", &ctx).unwrap());
        assert!(!evaluate_condition("NULL", &ctx).unwrap());
    }

    #[test]
    fn test_evaluate_condition_none_is_falsy() {
        let ctx = json!({});
        assert!(!evaluate_condition("none", &ctx).unwrap());
        assert!(!evaluate_condition("None", &ctx).unwrap());
        assert!(!evaluate_condition("NONE", &ctx).unwrap());
    }

    // --- resolve_rerun_sentinels tests ---

    #[test]
    fn test_resolve_rerun_sentinels_secret_with_source_value() {
        let mut schema = HashMap::new();
        schema.insert(
            "password".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: true,
                ..Default::default()
            },
        );
        let incoming = json!({"password": "••••••"});
        let source_raw = json!({"password": "real-secret-value"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        assert_eq!(result, json!({"password": "real-secret-value"}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_secret_without_source_value() {
        let mut schema = HashMap::new();
        schema.insert(
            "password".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: true,
                ..Default::default()
            },
        );
        let incoming = json!({"password": "••••••"});
        let source_raw = json!({}); // source didn't override the secret
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        // Field removed so merge_defaults will fill from schema default.
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_connection_with_source_value() {
        let mut schema = HashMap::new();
        schema.insert(
            "db".to_string(),
            InputFieldDef {
                field_type: "Postgres".to_string(), // non-primitive => connection type
                secret: false,
                ..Default::default()
            },
        );
        let incoming = json!({"db": "••••••"});
        let source_raw = json!({"db": "production-db"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        assert_eq!(result, json!({"db": "production-db"}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_non_sentinel_passthrough() {
        let mut schema = HashMap::new();
        schema.insert(
            "name".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: false,
                ..Default::default()
            },
        );
        let incoming = json!({"name": "alice"});
        let source_raw = json!({"name": "bob"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        // Non-secret, non-connection field with non-sentinel value: untouched.
        assert_eq!(result, json!({"name": "alice"}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_plain_field_with_sentinel_untouched() {
        let mut schema = HashMap::new();
        schema.insert(
            "note".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: false,
                ..Default::default()
            },
        );
        // User literally typed bullets into a plain string field. Server should
        // store as-is — sentinel only meaningful for secret/connection types.
        let incoming = json!({"note": "••••••"});
        let source_raw = json!({"note": "previous"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        assert_eq!(result, json!({"note": "••••••"}));
    }

    #[test]
    fn test_parse_qualified_ref() {
        assert_eq!(
            parse_qualified_ref("jobs.recalc-agg-sessions"),
            (Some("jobs"), "recalc-agg-sessions")
        );
        assert_eq!(parse_qualified_ref("score-all"), (None, "score-all"));
        // Only the first dot splits (action names may contain dots via libraries).
        assert_eq!(parse_qualified_ref("a.b.c"), (Some("a"), "b.c"));
        // Degenerate forms are treated as local.
        assert_eq!(parse_qualified_ref(".foo"), (None, ".foo"));
        assert_eq!(parse_qualified_ref("foo."), (None, "foo."));
    }
}
