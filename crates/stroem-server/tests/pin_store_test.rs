//! Pin eviction runs on every replica and keeps what active jobs pin.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::JobRepo;
use stroem_server::config::{
    DbConfig, LogStorageConfig, RecoveryConfig, RetentionConfig, ServerConfig,
};
use stroem_server::events::EventBus;
use stroem_server::leader::LeaderElection;
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::workspace::availability::ReloadSettings;
use stroem_server::workspace::pins::{PinSource, PinStore, PinStoreConfig};
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;

fn remote_with_two_commits() -> (TempDir, String, String, String) {
    let dir = TempDir::new().unwrap();
    let repo = git2::Repository::init_bare(dir.path()).unwrap();
    let sig = git2::Signature::now("t", "t@t.t").unwrap();
    let mut parents: Vec<git2::Commit> = Vec::new();
    let mut shas = Vec::new();
    for marker in ["v1", "v2"] {
        let mut tb = repo.treebuilder(None).unwrap();
        let yaml = format!("actions:\n  greet:\n    type: script\n    script: echo {marker}\n");
        tb.insert("wf.yaml", repo.blob(yaml.as_bytes()).unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let oid = repo
            .commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                marker,
                &tree,
                &parent_refs,
            )
            .unwrap();
        parents = vec![repo.find_commit(oid).unwrap()];
        shas.push(oid.to_string());
    }
    let url = format!("file://{}", dir.path().display());
    (dir, url, shas[0].clone(), shas[1].clone())
}

fn server_config(url: &str, log_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: url.to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-must-be-long-enough-32".to_string(),
        auth: None,
        recovery: RecoveryConfig {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
            unmatched_step_timeout_secs: 30,
        },
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        pin_store: None,
    }
}

#[tokio::test]
async fn pin_eviction_keeps_commits_of_active_pinned_jobs_on_a_follower_too() -> Result<()> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let db_url = test_db.url;
    let tmp = TempDir::new()?;

    let (_remote, url, c1, c2) = remote_with_two_commits();
    let store = PinStore::open(
        PinStoreConfig {
            dir: tmp.path().join("pins"),
            keep_recent_per_workspace: 0,
        },
        HashMap::from([(
            "w".to_string(),
            PinSource {
                url,
                auth: None,
                poll_interval: Duration::from_secs(60),
            },
        )]),
        Arc::new(HashMap::new()),
        ReloadSettings::default(),
    )?;
    let config = server_config(&db_url, tmp.path());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(
        pool.clone(),
        WorkspaceManager::from_config("w", WorkspaceConfig::new()).with_pin_store(store),
        config,
        log_storage,
        HashMap::new(),
        None,
    )
    .with_event_bus(EventBus::noop())
    // Eviction is replica-local: it must run on a follower too.
    .with_leader(LeaderElection::never());

    drop(state.workspaces.pins().ensure("w", &c1).await?);
    drop(state.workspaces.pins().ensure("w", &c2).await?);

    let job_id = JobRepo::create(
        &pool,
        "w",
        "t",
        "distributed",
        None,
        "api",
        None,
        Some(&c1),
        None,
    )
    .await?;
    sqlx::query("UPDATE job SET git_ref = 'release/1' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;

    stroem_server::recovery::pin_eviction_once(&state).await;
    assert_eq!(
        state.workspaces.pins().cached_commits("w"),
        vec![c1.clone()]
    );

    sqlx::query("UPDATE job SET status = 'completed', completed_at = NOW() WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;
    stroem_server::recovery::pin_eviction_once(&state).await;
    assert!(state.workspaces.pins().cached_commits("w").is_empty());
    Ok(())
}
