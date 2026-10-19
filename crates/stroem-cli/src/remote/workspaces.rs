use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

pub async fn cmd_workspaces(client: &Client, server: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/workspaces", server))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let workspaces = body.as_array().context("Expected array response")?;

    if workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("{}", header());
    println!("{}", "-".repeat(88));
    for ws in workspaces {
        println!("{}", format_row(ws));
    }

    Ok(())
}

fn header() -> String {
    format!(
        "{:20} {:10} {:10} {:10} {:20} DESCRIPTION",
        "NAME", "TASKS", "ACTIONS", "TRIGGERS", "DISPLAY NAME"
    )
}

/// One table row. `display_name` / `description` come from the server
/// config; they are absent when unset and on older servers.
fn format_row(ws: &Value) -> String {
    let name = ws.get("name").and_then(|v| v.as_str()).unwrap_or("-");
    let tasks = ws.get("tasks_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let actions = ws
        .get("actions_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let triggers = ws
        .get("triggers_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Older servers omit `triggers_enabled`; treat absent as enabled.
    let triggers_enabled = ws
        .get("triggers_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let triggers_col = if triggers_enabled {
        triggers.to_string()
    } else {
        format!("{} (off)", triggers)
    };
    let display_name = ws
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let description = ws
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    format!(
        "{:20} {:10} {:10} {:10} {:20} {}",
        name, tasks, actions, triggers_col, display_name, description
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(extra: Value) -> String {
        let mut ws = json!({
            "name": "data",
            "tasks_count": 3,
            "actions_count": 5,
            "triggers_count": 2,
            "triggers_enabled": true,
        });
        ws.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        format_row(&ws)
    }

    fn columns(line: &str) -> Vec<&str> {
        line.split_whitespace().collect()
    }

    #[test]
    fn row_without_display_name_or_description_shows_dashes() {
        let line = row(json!({}));
        assert_eq!(columns(&line), ["data", "3", "5", "2", "-", "-"]);
    }

    #[test]
    fn row_shows_display_name_and_description() {
        let line = row(json!({
            "display_name": "Data platform",
            "description": "Nightly ETL jobs",
        }));
        let col = header().find("DESCRIPTION").unwrap();
        assert_eq!(line.find("Nightly ETL jobs"), Some(col), "{line}");
        assert!(line.contains(" Data platform "), "{line}");
    }

    #[test]
    fn row_shows_description_without_display_name() {
        let line = row(json!({"description": "Nightly ETL jobs"}));
        assert_eq!(
            columns(&line),
            ["data", "3", "5", "2", "-", "Nightly", "ETL", "jobs"]
        );
    }

    #[test]
    fn row_marks_disabled_triggers() {
        let line = row(json!({"triggers_enabled": false}));
        assert!(line.contains("2 (off)"), "{line}");
    }

    #[test]
    fn header_and_rows_align() {
        let line = row(json!({"display_name": "Data platform"}));
        let col = header().find("DISPLAY NAME").unwrap();
        assert_eq!(line.find("Data platform"), Some(col), "{line}");
    }
}
