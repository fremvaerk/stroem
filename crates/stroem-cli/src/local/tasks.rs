use anyhow::Result;
use stroem_common::models::workflow::WorkspaceConfig;

fn truncate_desc(s: &str, max_len: usize) -> String {
    let clean = s.replace(['\n', '\r'], " ");
    if clean.len() <= max_len {
        return clean;
    }
    let suffix_len = 3; // "..."
    let target = max_len - suffix_len;
    let end = clean
        .char_indices()
        .take_while(|(i, _)| *i <= target)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(target);
    format!("{}...", &clean[..end])
}

pub fn cmd_tasks(config: &WorkspaceConfig) -> Result<()> {
    if config.tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    let mut rows: Vec<(&str, &str, usize, &str, &str)> = config
        .tasks
        .iter()
        .map(|(name, task)| {
            (
                name.as_str(),
                task.folder.as_deref().unwrap_or("-"),
                task.flow.len(),
                task.mode.as_str(),
                task.description.as_deref().unwrap_or(""),
            )
        })
        .collect();
    rows.sort_by_key(|r| r.0);

    println!(
        "{:<30} {:<15} {:>5}  {:<15} DESCRIPTION",
        "NAME", "FOLDER", "STEPS", "MODE"
    );
    println!("{}", "-".repeat(85));
    for (name, folder, steps, mode, desc) in &rows {
        let desc_truncated = truncate_desc(desc, 40);
        println!(
            "{:<30} {:<15} {:>5}  {:<15} {}",
            name, folder, steps, mode, desc_truncated
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::{FlowStep, TaskDef};

    fn make_task(folder: Option<&str>, steps: usize, desc: Option<&str>) -> TaskDef {
        let mut flow = HashMap::new();
        for i in 0..steps {
            flow.insert(
                format!("step{}", i),
                FlowStep {
                    action: "act".to_string(),
                    name: None,
                    description: None,
                    depends_on: vec![],
                    input: HashMap::new(),
                    continue_on_failure: false,
                    timeout: None,
                    when: None,
                    for_each: None,
                    sequential: false,
                    retry: None,
                    inline_action: None,
                },
            );
        }
        TaskDef {
            name: None,
            description: desc.map(|s| s.to_string()),
            mode: "distributed".to_string(),
            folder: folder.map(|s| s.to_string()),
            input: HashMap::new(),
            flow,
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        }
    }

    #[test]
    fn tasks_empty_workspace() {
        let config = WorkspaceConfig::new();
        assert!(cmd_tasks(&config).is_ok());
    }

    #[test]
    fn tasks_lists_all() {
        let mut config = WorkspaceConfig::new();
        config
            .tasks
            .insert("deploy".to_string(), make_task(Some("infra"), 3, None));
        config
            .tasks
            .insert("test".to_string(), make_task(Some("ci"), 5, None));
        assert!(cmd_tasks(&config).is_ok());
    }

    #[test]
    fn truncate_desc_ascii_within_limit() {
        assert_eq!(truncate_desc("short", 40), "short");
    }

    #[test]
    fn truncate_desc_ascii_at_boundary() {
        let s = "a".repeat(40);
        assert_eq!(truncate_desc(&s, 40), s);
    }

    #[test]
    fn truncate_desc_ascii_over_boundary() {
        // target = 40 - 3 = 37; take_while i <= 37 keeps the char at index 37
        // (38th char), so 38 chars are kept before "...".
        let s = "a".repeat(41);
        let result = truncate_desc(&s, 40);
        assert_eq!(result, format!("{}...", "a".repeat(38)));
    }

    #[test]
    fn truncate_desc_multibyte_safe() {
        let s = "Strøm æøå 日本語 description here!";
        let result = truncate_desc(s, 40);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(result.len() <= 40 || result.ends_with("..."));
    }

    #[test]
    fn truncate_desc_newlines_stripped() {
        let result = truncate_desc("line1\nline2\r\nline3", 40);
        assert!(!result.contains('\n'));
        assert!(!result.contains('\r'));
        assert_eq!(result, "line1 line2  line3");
    }

    #[test]
    fn tasks_no_folder_shows_dash() {
        let mut config = WorkspaceConfig::new();
        config
            .tasks
            .insert("orphan".to_string(), make_task(None, 1, None));
        assert!(cmd_tasks(&config).is_ok());
    }

    #[test]
    fn tasks_sorted_alphabetically() {
        let mut config = WorkspaceConfig::new();
        config
            .tasks
            .insert("zebra".to_string(), make_task(Some("z"), 1, None));
        config
            .tasks
            .insert("alpha".to_string(), make_task(Some("a"), 2, None));
        assert!(cmd_tasks(&config).is_ok());
    }
}
