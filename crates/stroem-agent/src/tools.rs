//! Tool definition generation for multi-turn agent steps.
//!
//! Converts Strøm task schemas and built-in tools (ask_user) into
//! rig-core `ToolDefinition` objects for LLM tool calling.

use rig_core::completion::ToolDefinition;
use std::collections::HashMap;
use stroem_common::models::workflow::{InputFieldDef, TaskDef};

/// Convert a Strøm task definition to a rig `ToolDefinition`.
///
/// Tool name uses `strom_task_` prefix with hyphens converted to underscores
/// (LLM tool names typically don't allow hyphens).
pub fn task_to_tool_definition(task_name: &str, task: &TaskDef) -> ToolDefinition {
    let tool_name = format!("strom_task_{}", task_name.replace('-', "_"));
    let description = task
        .description
        .clone()
        .unwrap_or_else(|| format!("Execute the '{}' task", task_name));

    let parameters = input_schema_to_json_schema(&task.input);

    ToolDefinition {
        name: tool_name,
        description,
        parameters,
    }
}

/// Built-in `ask_user` tool definition.
///
/// Only injected when `interactive: true` is set on the agent action.
/// Suspends the agent step and waits for human input via the approve endpoint.
pub fn ask_user_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "ask_user".to_string(),
        description: "Ask the user a question and wait for their response. Use this when you need human input, clarification, or approval to proceed.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The question or message to show to the user"
                }
            },
            "required": ["message"]
        }),
    }
}

/// Convert a Strøm `InputFieldDef` map to a JSON Schema object for tool parameters.
///
/// Maps Strøm field types to JSON Schema types:
/// - `string`, `text` → `"string"`
/// - `integer` → `"integer"`
/// - `number` → `"number"`
/// - `boolean` → `"boolean"`
/// - `date`, `datetime` → `"string"` (with format)
/// - `json` → no type (any JSON value)
/// - anything else (connection types) → `"object"`
pub fn input_schema_to_json_schema(input: &HashMap<String, InputFieldDef>) -> serde_json::Value {
    if input.is_empty() {
        return serde_json::json!({
            "type": "object",
            "properties": {},
        });
    }

    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for (name, field) in input {
        let mut prop = serde_json::Map::new();

        let (json_type, format) = map_field_type(&field.field_type);
        if let Some(json_type) = json_type {
            prop.insert("type".to_string(), serde_json::Value::String(json_type));
        }
        if let Some(fmt) = format {
            prop.insert("format".to_string(), serde_json::Value::String(fmt));
        }

        let is_json = field.field_type == stroem_common::template::JSON_TYPE;
        match (&field.description, is_json) {
            (Some(desc), true) => {
                prop.insert(
                    "description".to_string(),
                    serde_json::Value::String(format!("{desc} Any JSON value.")),
                );
            }
            (None, true) => {
                prop.insert(
                    "description".to_string(),
                    serde_json::Value::String("Any JSON value.".to_string()),
                );
            }
            (Some(desc), false) => {
                prop.insert(
                    "description".to_string(),
                    serde_json::Value::String(desc.clone()),
                );
            }
            (None, false) => {}
        }

        if let Some(ref default) = field.default {
            prop.insert("default".to_string(), default.clone());
        }

        if let Some(ref options) = field.options {
            let enum_vals: Vec<serde_json::Value> = options
                .iter()
                .map(|o| serde_json::Value::String(o.clone()))
                .collect();
            prop.insert("enum".to_string(), serde_json::Value::Array(enum_vals));
        }

        properties.insert(name.clone(), serde_json::Value::Object(prop));

        if field.required {
            required.push(serde_json::Value::String(name.clone()));
        }
    }

    let mut schema = serde_json::json!({
        "type": "object",
        "properties": properties,
    });
    if !required.is_empty() {
        schema["required"] = serde_json::Value::Array(required);
    }
    schema
}

/// Map a Strøm field type to a (JSON Schema type, optional format) pair.
/// `json` has no type: any JSON value (spec 2026-10-06-json-input-type D9).
fn map_field_type(field_type: &str) -> (Option<String>, Option<String>) {
    let t = |s: &str| Some(s.to_string());
    match field_type {
        "string" | "text" => (t("string"), None),
        "integer" => (t("integer"), None),
        "number" => (t("number"), None),
        "boolean" => (t("boolean"), None),
        "date" => (t("string"), t("date")),
        "datetime" => (t("string"), t("date-time")),
        stroem_common::template::JSON_TYPE => (None, None),
        _ => (t("object"), None), // Connection types or unknown
    }
}

/// Extract the task name from a Strøm tool name (reverse of `task_to_tool_definition`).
///
/// Returns `None` if the tool name doesn't have the `strom_task_` prefix.
pub fn task_name_from_tool_name(tool_name: &str) -> Option<String> {
    tool_name.strip_prefix("strom_task_").map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_to_tool_definition_basic() {
        let task = TaskDef {
            description: Some("Deploy to production".to_string()),
            ..default_task()
        };
        let td = task_to_tool_definition("deploy-app", &task);
        assert_eq!(td.name, "strom_task_deploy_app");
        assert_eq!(td.description, "Deploy to production");
        assert!(td.parameters["type"] == "object");
    }

    #[test]
    fn test_task_to_tool_definition_with_input() {
        let mut input = HashMap::new();
        input.insert(
            "environment".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                name: None,
                description: Some("Target environment".to_string()),
                required: true,
                secret: false,
                default: None,
                options: Some(vec!["staging".to_string(), "production".to_string()]),
                allow_custom: false,
                multiple: false,
                order: None,
            },
        );
        let task = TaskDef {
            description: Some("Deploy".to_string()),
            input,
            ..default_task()
        };
        let td = task_to_tool_definition("deploy", &task);
        let props = &td.parameters["properties"];
        assert!(props["environment"]["type"] == "string");
        assert!(props["environment"]["description"] == "Target environment");
        let required = td.parameters["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("environment")));
    }

    #[test]
    fn test_ask_user_tool_definition() {
        let td = ask_user_tool_definition();
        assert_eq!(td.name, "ask_user");
        assert!(td.parameters["properties"]["message"]["type"] == "string");
        let required = td.parameters["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("message")));
    }

    #[test]
    fn test_task_name_from_tool_name() {
        assert_eq!(
            task_name_from_tool_name("strom_task_deploy_app"),
            Some("deploy_app".to_string())
        );
        assert_eq!(task_name_from_tool_name("ask_user"), None);
        assert_eq!(task_name_from_tool_name("other_tool"), None);
    }

    #[test]
    fn test_input_schema_empty() {
        let schema = input_schema_to_json_schema(&HashMap::new());
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn test_map_field_type_coverage() {
        let t = |s: &str| Some(s.to_string());
        assert_eq!(map_field_type("string"), (t("string"), None));
        assert_eq!(map_field_type("text"), (t("string"), None));
        assert_eq!(map_field_type("integer"), (t("integer"), None));
        assert_eq!(map_field_type("number"), (t("number"), None));
        assert_eq!(map_field_type("boolean"), (t("boolean"), None));
        assert_eq!(map_field_type("date"), (t("string"), t("date")));
        assert_eq!(map_field_type("datetime"), (t("string"), t("date-time")));
        assert_eq!(map_field_type("my_connection_type"), (t("object"), None));
    }

    #[test]
    fn json_field_has_no_type_keyword() {
        assert_eq!(map_field_type("json"), (None, None));
        let mut input = HashMap::new();
        input.insert(
            "cfg".to_string(),
            serde_yaml::from_str::<InputFieldDef>("{ type: json, description: Config }").unwrap(),
        );
        let schema = input_schema_to_json_schema(&input);
        let cfg = &schema["properties"]["cfg"];
        assert!(cfg.get("type").is_none(), "{schema}");
        assert_eq!(cfg["description"], "Config Any JSON value.");
    }

    fn default_task() -> TaskDef {
        serde_yaml::from_str(
            r#"
            flow:
              step1:
                action: dummy
            "#,
        )
        .unwrap()
    }
}
