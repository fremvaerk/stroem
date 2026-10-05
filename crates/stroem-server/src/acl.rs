use crate::config::{AclAction, AclConfig};
use crate::state::AppState;
use std::collections::HashSet;
use stroem_db::{JobAclScope, JobRow};

/// Task-level permission result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPermission {
    Run,
    View,
    Deny,
}

impl From<AclAction> for TaskPermission {
    fn from(a: AclAction) -> Self {
        match a {
            AclAction::Run => TaskPermission::Run,
            AclAction::View => TaskPermission::View,
            AclAction::Deny => TaskPermission::Deny,
        }
    }
}

/// Result of enumerating allowed tasks for a user
pub enum AllowedScope {
    /// Admin or no ACL configured — everything is allowed
    All,
    /// Filtered list of (workspace, task_name, permission) tuples
    Filtered(Vec<(String, String, TaskPermission)>),
}

pub struct AclEvaluator {
    config: Option<AclConfig>,
}

impl AclEvaluator {
    pub fn new(config: Option<AclConfig>) -> Self {
        Self { config }
    }

    /// Whether ACL is configured
    pub fn is_configured(&self) -> bool {
        self.config.is_some()
    }

    /// Evaluate permission for a single task.
    /// `task_path` is the folder/task path for glob matching — if task has a folder,
    /// it's "{folder}/{task_name}", otherwise just "{task_name}".
    pub fn evaluate(
        &self,
        workspace: &str,
        task_path: &str,
        email: &str,
        groups: &HashSet<String>,
        is_admin: bool,
    ) -> TaskPermission {
        // Admin always has full access
        if is_admin {
            return TaskPermission::Run;
        }

        let config = match &self.config {
            Some(c) => c,
            // No ACL configured = backward compat, everyone gets Run
            None => return TaskPermission::Run,
        };

        let mut highest_priority: u8 = 0;

        for rule in &config.rules {
            // Check workspace match
            if !glob_match(&rule.workspace, workspace) {
                continue;
            }

            // Check task match (any task pattern must match)
            let task_matches = rule
                .tasks
                .iter()
                .any(|pattern| glob_match(pattern, task_path));
            if !task_matches {
                continue;
            }

            // Check user/group match (OR'd)
            let user_matches = rule.users.iter().any(|u| u == email);
            let group_matches = rule.groups.iter().any(|g| groups.contains(g));
            if !user_matches && !group_matches {
                continue;
            }

            // Track highest permission
            let priority = rule.action.priority();
            if priority > highest_priority {
                highest_priority = priority;
            }
        }

        if highest_priority > 0 {
            // At least one rule matched — return the highest permission
            match highest_priority {
                3 => TaskPermission::Run,
                2 => TaskPermission::View,
                _ => TaskPermission::Deny,
            }
        } else {
            // No rule matched — use default
            config.default.into()
        }
    }

    /// Build the allowed scope for a user across all workspaces/tasks.
    /// Returns `All` for admins or when no ACL is configured.
    pub fn allowed_scope(
        &self,
        workspace_tasks: &[(String, String, Option<String>)], // (workspace, task_name, folder)
        email: &str,
        groups: &HashSet<String>,
        is_admin: bool,
    ) -> AllowedScope {
        if is_admin || self.config.is_none() {
            return AllowedScope::All;
        }

        let mut result = Vec::new();
        for (ws, task_name, folder) in workspace_tasks {
            let task_path = make_task_path(folder.as_deref(), task_name);
            let perm = self.evaluate(ws, &task_path, email, groups, false);
            if !matches!(perm, TaskPermission::Deny) {
                result.push((ws.clone(), task_name.clone(), perm));
            }
        }
        AllowedScope::Filtered(result)
    }

    /// Pinned jobs' `(workspace, task, task_folder)` triples this user may
    /// see (spec § 7.8): the rules evaluated on each triple's OWN folder.
    /// The folder comes back as `""` for none, matching the SQL
    /// `COALESCE(task_folder, '')`.
    pub fn allowed_triples(
        &self,
        triples: &[(String, String, Option<String>)],
        email: &str,
        groups: &HashSet<String>,
    ) -> Vec<(String, String, String)> {
        triples
            .iter()
            .filter(|(ws, task, folder)| {
                let path = make_task_path(folder.as_deref(), task);
                !matches!(
                    self.evaluate(ws, &path, email, groups, false),
                    TaskPermission::Deny
                )
            })
            .map(|(ws, task, folder)| {
                (ws.clone(), task.clone(), folder.clone().unwrap_or_default())
            })
            .collect()
    }
}

/// Build the task path used for ACL glob matching.
/// If the task has a folder, returns "{folder}/{task_name}", otherwise just "{task_name}".
pub fn make_task_path(folder: Option<&str>, task_name: &str) -> String {
    match folder {
        Some(f) if !f.is_empty() => format!("{f}/{task_name}"),
        _ => task_name.to_string(),
    }
}

/// The folder half of a job's ACL path (spec § 7.8). A pinned job
/// (`git_ref` set) always uses the folder its own commit declared — never the
/// live config's, which may describe a different task of the same name. An
/// unpinned job uses the live task's folder, as before.
pub fn acl_folder(
    git_ref: Option<&str>,
    task_folder: Option<&str>,
    live_folder: Option<&str>,
) -> Option<String> {
    if git_ref.is_some() {
        task_folder.map(str::to_string)
    } else {
        live_folder.map(str::to_string)
    }
}

/// [`acl_folder`] for a job row; reads the live config only for an unpinned job.
#[tracing::instrument(skip_all, fields(job_id = %job.job_id))]
pub async fn job_folder(state: &AppState, job: &JobRow) -> Option<String> {
    let live = if job.git_ref.is_some() {
        None
    } else {
        state
            .get_workspace(&job.workspace)
            .await
            .and_then(|ws| ws.tasks.get(&job.task_name).and_then(|t| t.folder.clone()))
    };
    acl_folder(
        job.git_ref.as_deref(),
        job.task_folder.as_deref(),
        live.as_deref(),
    )
}

/// `{folder}/{task}` for a job — THE task path every job-scoped read path
/// authorises with. A new read path that exposes a job must use this (or
/// `web::api::jobs::check_job_acl`), never a live folder lookup.
#[tracing::instrument(skip_all, fields(job_id = %job.job_id))]
pub async fn job_task_path(state: &AppState, job: &JobRow) -> String {
    make_task_path(job_folder(state, job).await.as_deref(), &job.task_name)
}

/// Every live task as `(workspace, task_name, folder)`, the input of
/// [`AclEvaluator::allowed_scope`]. Shared by the job-list scope (REST and
/// MCP) and MCP's task-list scope.
#[tracing::instrument(skip_all)]
pub async fn live_task_folders(state: &AppState) -> Vec<(String, String, Option<String>)> {
    let mut tasks = Vec::new();
    for (ws_name, ws_config) in state.workspaces.get_all_configs().await {
        for (task_name, task_def) in &ws_config.tasks {
            tasks.push((ws_name.clone(), task_name.clone(), task_def.folder.clone()));
        }
    }
    tasks
}

/// Restrict a scope to a list query's workspace / task filter.
pub fn narrow_job_scope(
    scope: &JobAclScope,
    workspace: Option<&str>,
    task: Option<&str>,
) -> JobAclScope {
    let keep =
        |w: &str, t: &str| workspace.is_none_or(|ws| ws == w) && task.is_none_or(|tn| tn == t);
    JobAclScope {
        live_pairs: scope
            .live_pairs
            .iter()
            .filter(|(w, t)| keep(w, t))
            .cloned()
            .collect(),
        pinned_triples: scope
            .pinned_triples
            .iter()
            .filter(|(w, t, _)| keep(w, t))
            .cloned()
            .collect(),
    }
}

/// Job-list scope for a user (spec § 7.8), shared by REST and MCP. `None` =
/// no filtering (admin, or no ACL configured). Unpinned jobs are authorised
/// by live `(workspace, task)` pairs; pinned jobs by their own
/// `(workspace, task, task_folder)` triple.
#[tracing::instrument(skip_all)]
pub async fn build_job_acl_scope(
    state: &AppState,
    email: &str,
    groups: &HashSet<String>,
    is_admin: bool,
) -> anyhow::Result<Option<JobAclScope>> {
    if is_admin || !state.acl.is_configured() {
        return Ok(None);
    }
    let live_tasks = live_task_folders(state).await;
    let live_pairs = match state.acl.allowed_scope(&live_tasks, email, groups, false) {
        AllowedScope::All => return Ok(None),
        AllowedScope::Filtered(items) => {
            items.into_iter().map(|(ws, task, _)| (ws, task)).collect()
        }
    };
    let pinned = stroem_db::JobRepo::pinned_task_triples(&state.pool).await?;
    let pinned_triples = state.acl.allowed_triples(&pinned, email, groups);
    Ok(Some(JobAclScope {
        live_pairs,
        pinned_triples,
    }))
}

/// Simple glob matching supporting `*` as wildcard (including multiple wildcards).
/// - `*` alone matches everything
/// - `prefix/*` matches strings starting with prefix/
/// - `*suffix` matches strings ending with suffix
/// - `pre*suf` matches strings starting with pre and ending with suf
/// - `a/*/b*` handles multiple wildcards via recursive matching
/// - exact match otherwise
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    match pattern.find('*') {
        None => pattern == value,
        Some(star_pos) => {
            let prefix = &pattern[..star_pos];
            let rest_pattern = &pattern[star_pos + 1..];

            if !value.starts_with(prefix) {
                return false;
            }

            let remaining = &value[prefix.len()..];

            // If no more wildcards in rest_pattern, match suffix directly
            if !rest_pattern.contains('*') {
                return remaining.ends_with(rest_pattern) && remaining.len() >= rest_pattern.len();
            }

            // Multiple wildcards: try matching rest_pattern at every position
            for i in 0..=remaining.len() {
                if glob_match(rest_pattern, &remaining[i..]) {
                    return true;
                }
            }
            false
        }
    }
}

/// Load the ACL context (admin flag + group memberships) for a user.
///
/// Returns `(is_admin, groups)`. Admins skip the group lookup.
#[tracing::instrument(skip(pool))]
pub async fn load_user_acl_context(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    is_admin: bool,
) -> anyhow::Result<(bool, HashSet<String>)> {
    if is_admin {
        return Ok((true, HashSet::new()));
    }
    let groups = stroem_db::UserGroupRepo::get_groups_for_user(pool, user_id).await?;
    Ok((false, groups))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AclAction, AclConfig, AclRule};

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("production", "production"));
        assert!(!glob_match("production", "staging"));
    }

    #[test]
    fn test_glob_match_wildcard_all() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_prefix_wildcard() {
        assert!(glob_match("deploy/*", "deploy/web"));
        assert!(glob_match("deploy/*", "deploy/api"));
        assert!(!glob_match("deploy/*", "build/web"));
    }

    #[test]
    fn test_glob_match_suffix_wildcard() {
        assert!(glob_match("*-deploy", "web-deploy"));
        assert!(glob_match("*-deploy", "api-deploy"));
        assert!(!glob_match("*-deploy", "web-build"));
    }

    #[test]
    fn test_glob_match_middle_wildcard() {
        assert!(glob_match("pre*suf", "pre-middle-suf"));
        assert!(glob_match("pre*suf", "presuf"));
        assert!(!glob_match("pre*suf", "pre-middle-other"));
    }

    #[test]
    fn test_glob_match_no_match() {
        assert!(!glob_match("abc", "xyz"));
        assert!(!glob_match("abc*", "xyz"));
    }

    #[test]
    fn test_evaluate_no_config_returns_run() {
        let acl = AclEvaluator::new(None);
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "user@example.com", &groups, false),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_admin_bypass() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "admin@example.com", &groups, true),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_default_deny_no_matching_rules() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "staging".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec!["other@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "user@example.com", &groups, false),
            TaskPermission::Deny
        );
    }

    #[test]
    fn test_evaluate_email_match() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec!["dev@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "dev@example.com", &groups, false),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_group_match() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::View,
                groups: vec!["engineering".to_string()],
                users: vec![],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let mut groups = HashSet::new();
        groups.insert("engineering".to_string());
        assert_eq!(
            acl.evaluate("production", "deploy", "user@example.com", &groups, false),
            TaskPermission::View
        );
    }

    #[test]
    fn test_evaluate_highest_wins() {
        // Two rules match: one gives View, one gives Run. Run should win.
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![
                AclRule {
                    workspace: "*".to_string(),
                    tasks: vec!["*".to_string()],
                    action: AclAction::View,
                    groups: vec!["engineering".to_string()],
                    users: vec![],
                },
                AclRule {
                    workspace: "production".to_string(),
                    tasks: vec!["deploy/*".to_string()],
                    action: AclAction::Run,
                    groups: vec!["devops".to_string()],
                    users: vec![],
                },
            ],
        };
        let acl = AclEvaluator::new(Some(config));
        let mut groups = HashSet::new();
        groups.insert("engineering".to_string());
        groups.insert("devops".to_string());
        assert_eq!(
            acl.evaluate(
                "production",
                "deploy/web",
                "user@example.com",
                &groups,
                false
            ),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_folder_task_path() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["infra/*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec!["user@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();

        let path = make_task_path(Some("infra"), "deploy");
        assert_eq!(
            acl.evaluate("prod", &path, "user@example.com", &groups, false),
            TaskPermission::Run
        );

        let path2 = make_task_path(Some("other"), "deploy");
        assert_eq!(
            acl.evaluate("prod", &path2, "user@example.com", &groups, false),
            TaskPermission::Deny
        );
    }

    #[test]
    fn test_evaluate_default_view() {
        let config = AclConfig {
            default: AclAction::View,
            rules: vec![],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("prod", "deploy", "user@example.com", &groups, false),
            TaskPermission::View
        );
    }

    #[test]
    fn test_is_configured() {
        assert!(!AclEvaluator::new(None).is_configured());
        assert!(AclEvaluator::new(Some(AclConfig {
            default: AclAction::Deny,
            rules: vec![]
        }))
        .is_configured());
    }

    #[test]
    fn test_make_task_path() {
        assert_eq!(make_task_path(None, "deploy"), "deploy");
        assert_eq!(make_task_path(Some(""), "deploy"), "deploy");
        assert_eq!(make_task_path(Some("infra"), "deploy"), "infra/deploy");
    }

    #[test]
    fn test_allowed_scope_admin() {
        let acl = AclEvaluator::new(Some(AclConfig {
            default: AclAction::Deny,
            rules: vec![],
        }));
        let groups = HashSet::new();
        let tasks = vec![("ws".to_string(), "task1".to_string(), None)];
        assert!(matches!(
            acl.allowed_scope(&tasks, "admin@example.com", &groups, true),
            AllowedScope::All
        ));
    }

    #[test]
    fn test_allowed_scope_no_config() {
        let acl = AclEvaluator::new(None);
        let groups = HashSet::new();
        let tasks = vec![("ws".to_string(), "task1".to_string(), None)];
        assert!(matches!(
            acl.allowed_scope(&tasks, "user@example.com", &groups, false),
            AllowedScope::All
        ));
    }

    #[test]
    fn test_allowed_scope_filtered() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["visible*".to_string()],
                action: AclAction::View,
                groups: vec![],
                users: vec!["user@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        let tasks = vec![
            ("ws".to_string(), "visible-task".to_string(), None),
            ("ws".to_string(), "hidden-task".to_string(), None),
        ];
        match acl.allowed_scope(&tasks, "user@example.com", &groups, false) {
            AllowedScope::Filtered(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].1, "visible-task");
                assert_eq!(items[0].2, TaskPermission::View);
            }
            AllowedScope::All => panic!("expected Filtered"),
        }
    }

    #[test]
    fn test_glob_match_multiple_wildcards() {
        assert!(glob_match("deploy/*/prod*", "deploy/web/production"));
        assert!(glob_match("*/api/*", "staging/api/v2"));
        assert!(!glob_match("*/api/*", "staging/web/v2"));
        assert!(glob_match("*/*", "a/b"));
        assert!(!glob_match("*/*", "abc"));
    }

    fn folder_rules() -> AclEvaluator {
        AclEvaluator::new(Some(AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "etl".to_string(),
                tasks: vec!["public/*".to_string()],
                action: AclAction::View,
                groups: vec!["viewers".to_string()],
                users: vec![],
            }],
        }))
    }

    #[test]
    fn acl_folder_pinned_uses_own_folder_even_when_live_differs() {
        assert_eq!(
            acl_folder(Some("release/2.3"), Some("restricted"), Some("public")),
            Some("restricted".to_string())
        );
        // A pinned job whose commit declared no folder has none — never the live one.
        assert_eq!(acl_folder(Some("release/2.3"), None, Some("public")), None);
    }

    #[test]
    fn acl_folder_unpinned_uses_live_folder() {
        assert_eq!(
            acl_folder(None, Some("ignored"), Some("public")),
            Some("public".to_string())
        );
        assert_eq!(acl_folder(None, None, None), None);
    }

    #[test]
    fn allowed_triples_evaluates_each_triples_own_folder() {
        let acl = folder_rules();
        let groups: HashSet<String> = ["viewers".to_string()].into();
        let triples = vec![
            (
                "etl".to_string(),
                "nightly".to_string(),
                Some("restricted".to_string()),
            ),
            (
                "etl".to_string(),
                "nightly".to_string(),
                Some("public".to_string()),
            ),
            ("etl".to_string(), "loose".to_string(), None),
        ];
        assert_eq!(
            acl.allowed_triples(&triples, "v@test", &groups),
            vec![(
                "etl".to_string(),
                "nightly".to_string(),
                "public".to_string()
            )]
        );
    }

    #[test]
    fn allowed_triples_returns_no_folder_as_empty_string() {
        // A folder-less pinned job matches the SQL's `COALESCE(task_folder, '')`.
        let acl = AclEvaluator::new(Some(AclConfig {
            default: AclAction::View,
            rules: vec![],
        }));
        let triples = vec![("etl".to_string(), "loose".to_string(), None)];
        assert_eq!(
            acl.allowed_triples(&triples, "v@test", &HashSet::new()),
            vec![("etl".to_string(), "loose".to_string(), String::new())]
        );
    }

    #[test]
    fn narrow_job_scope_filters_both_halves() {
        let scope = stroem_db::JobAclScope {
            live_pairs: vec![
                ("etl".to_string(), "a".to_string()),
                ("ops".to_string(), "a".to_string()),
            ],
            pinned_triples: vec![
                ("etl".to_string(), "a".to_string(), "public".to_string()),
                ("etl".to_string(), "b".to_string(), "".to_string()),
            ],
        };
        let n = narrow_job_scope(&scope, Some("etl"), Some("a"));
        assert_eq!(n.live_pairs, vec![("etl".to_string(), "a".to_string())]);
        assert_eq!(
            n.pinned_triples,
            vec![("etl".to_string(), "a".to_string(), "public".to_string())]
        );
        let ws_only = narrow_job_scope(&scope, Some("etl"), None);
        assert_eq!(
            ws_only.live_pairs,
            vec![("etl".to_string(), "a".to_string())]
        );
        assert_eq!(ws_only.pinned_triples.len(), 2);
        let all = narrow_job_scope(&scope, None, None);
        assert_eq!(all.live_pairs.len(), 2);
        assert_eq!(all.pinned_triples.len(), 2);
    }
}
