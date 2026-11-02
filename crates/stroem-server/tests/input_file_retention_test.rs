//! Retention: job deletion drops attachments; the orphan sweep removes unused
//! input files (spec 2026-10-29-file-inputs-design.md § 4.7).
//!
//! Mirrors the inline-harness pattern used by `artifact_api_test.rs` and
//! `artifact_upload_test.rs` — no shared `common::TestHarness` exists in this
//! crate yet, so the relevant setup is duplicated here for clarity.

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{InputFileRepo, NewInputFile, UploadTokenRepo};
use stroem_server::blob_storage::{Blob, BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    DbConfig, LogStorageConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WORKER_TOKEN: &str = "test-worker-token";

struct TestApp {
    wrapper_fail: Arc<AtomicBool>,
    // Kept around so existing fixture wiring keeps the router built; the two
    // retention tests below bypass the HTTP layer entirely (the upload
    // endpoint's status gate rejects writes to terminal jobs, so we seed
    // artifact rows + blobs via the repo and BlobArchive directly).
    #[allow(dead_code)]
    router: Router,
    pool: PgPool,
    state: AppState,
    blob: Arc<dyn BlobArchive>,
    _tmp: TempDir,
}

async fn spawn_pg() -> Result<PgPool> {
    let pool = stroem_test_support::test_pool().await;
    Ok(pool)
}

async fn build_test_app_with_retention(job_days: u64) -> Result<TestApp> {
    build_test_app_inner(job_days, true).await
}

/// Delegates to a real archive; `delete` fails once while `fail` is set.
struct FlakyArchive {
    inner: Arc<dyn BlobArchive>,
    fail: Arc<AtomicBool>,
}

#[async_trait]
impl BlobArchive for FlakyArchive {
    async fn put(&self, key: &str, content_type: &str, data: bytes::Bytes) -> Result<()> {
        self.inner.put(key, content_type, data).await
    }
    async fn get(&self, key: &str) -> Result<Option<Blob>> {
        self.inner.get(key).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        if self.fail.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected delete failure");
        }
        self.inner.delete(key).await
    }
    async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        self.inner.delete_prefix(prefix).await
    }
}

async fn build_test_app_inner(job_days: u64, with_blob: bool) -> Result<TestApp> {
    let pool = spawn_pg().await?;

    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let artifact_dir = tmp.path().join("artifact-archive");
    std::fs::create_dir_all(&artifact_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: "postgres://unused".to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::from([(
            "ws1".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                display_name: None,
                description: None,
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: WORKER_TOKEN.to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig {
            worker_hours: None,
            job_days: Some(job_days),
            interval_secs: 3600,
        },
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        input_files: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        leader: Default::default(),
        pin_store: None,
    };

    let workspace = WorkspaceConfig::new();
    let mgr = WorkspaceManager::from_config("ws1", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);

    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(artifact_dir));
    let wrapper_fail = Arc::new(AtomicBool::new(false));
    let flaky: Arc<dyn BlobArchive> = Arc::new(FlakyArchive {
        inner: archive.clone(),
        fail: wrapper_fail.clone(),
    });

    let mut state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    if with_blob {
        state = state.with_input_blob(Some(flaky));
    }
    let router = build_router(state.clone(), CancellationToken::new());

    Ok(TestApp {
        wrapper_fail,
        router,
        pool,
        state,
        blob: archive,
        _tmp: tmp,
    })
}

/// Insert a terminal job with a back-dated `created_at`.
async fn create_old_terminal_job(pool: &PgPool, age_days: i64) -> Result<Uuid> {
    let job_id = Uuid::new_v4();
    let created_at = chrono::Utc::now() - chrono::Duration::days(age_days);
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, 'ws1', 'task1', 'completed', 'api', $2)",
    )
    .bind(job_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(job_id)
}

/// Insert an `input_file` row plus its blob; `age_days` back-dates `created_at`.
async fn seed_file(app: &TestApp, age_days: i64) -> Result<String> {
    let file_id = format!("file_{}", Uuid::new_v4().simple());
    let key = app.state.input_file_storage_key("ws1", &file_id);
    app.blob
        .put(&key, "text/plain", bytes::Bytes::from_static(b"x"))
        .await?;
    InputFileRepo::insert(
        &app.pool,
        &NewInputFile {
            file_id: &file_id,
            workspace: "ws1",
            task_name: "task1",
            uploaded_by_user_id: None,
            origin: "api",
            original_name: "a.txt",
            safe_name: "a.txt",
            content_type: "text/plain",
            size_bytes: 1,
            sha256: "00",
            storage_key: &key,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE input_file SET created_at = NOW() - make_interval(days => $2) WHERE file_id = $1",
    )
    .bind(&file_id)
    .bind(age_days as i32)
    .execute(&app.pool)
    .await?;
    Ok(file_id)
}

async fn attach(app: &TestApp, job_id: Uuid, file_id: &str) -> Result<()> {
    sqlx::query("INSERT INTO job_input_file (job_id, field, file_id) VALUES ($1, 'f', $2)")
        .bind(job_id)
        .bind(file_id)
        .execute(&app.pool)
        .await?;
    Ok(())
}

async fn file_row_exists(app: &TestApp, file_id: &str) -> Result<bool> {
    Ok(InputFileRepo::get(&app.pool, file_id).await?.is_some())
}

async fn blob_exists(app: &TestApp, file_id: &str) -> Result<bool> {
    let key = app.state.input_file_storage_key("ws1", file_id);
    Ok(app.blob.get(&key).await?.is_some())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_deletes_job_attachments_keeps_file_until_ttl() -> Result<()> {
    let app = build_test_app_with_retention(1).await?;
    let job_id = create_old_terminal_job(&app.pool, 2).await?;
    // Young file (inside the 24 h upload TTL), attached to the old job.
    let file_id = seed_file(&app, 0).await?;
    attach(&app, job_id, &file_id).await?;

    stroem_server::recovery::retention_cleanup(&app.state).await;

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&app.pool)
        .await?;
    assert_eq!(n, 0, "old job deleted");
    let (links,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM job_input_file WHERE file_id = $1")
        .bind(&file_id)
        .fetch_one(&app.pool)
        .await?;
    assert_eq!(links, 0, "attachment rows removed with the job");
    assert!(file_row_exists(&app, &file_id).await?, "young file kept");
    assert!(blob_exists(&app, &file_id).await?);

    // Once past the TTL, the next sweep removes the now unattached file.
    sqlx::query("UPDATE input_file SET created_at = NOW() - interval '2 days' WHERE file_id = $1")
        .bind(&file_id)
        .execute(&app.pool)
        .await?;
    stroem_server::recovery::retention_cleanup(&app.state).await;
    assert!(!file_row_exists(&app, &file_id).await?);
    assert!(!blob_exists(&app, &file_id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_deletes_unattached_expired_upload_blob_and_row() -> Result<()> {
    let app = build_test_app_with_retention(365).await?;
    let file_id = seed_file(&app, 2).await?;

    stroem_server::recovery::retention_cleanup(&app.state).await;

    assert!(!file_row_exists(&app, &file_id).await?);
    assert!(!blob_exists(&app, &file_id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_keeps_young_or_attached_upload() -> Result<()> {
    let app = build_test_app_with_retention(365).await?;
    let young = seed_file(&app, 0).await?;
    let attached = seed_file(&app, 2).await?;
    let job_id = create_old_terminal_job(&app.pool, 0).await?;
    attach(&app, job_id, &attached).await?;

    stroem_server::recovery::retention_cleanup(&app.state).await;

    for id in [&young, &attached] {
        assert!(file_row_exists(&app, id).await?);
        assert!(blob_exists(&app, id).await?);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_retries_blob_delete_after_failure() -> Result<()> {
    let app = build_test_app_with_retention(365).await?;
    let file_id = seed_file(&app, 2).await?;
    app.wrapper_fail.store(true, Ordering::SeqCst);

    stroem_server::recovery::retention_cleanup(&app.state).await;
    let row = InputFileRepo::get(&app.pool, &file_id)
        .await?
        .expect("row kept after a failed blob delete");
    assert!(row.deleting_at.is_some(), "row marked for deletion");
    assert!(blob_exists(&app, &file_id).await?);

    stroem_server::recovery::retention_cleanup(&app.state).await;
    assert!(!file_row_exists(&app, &file_id).await?);
    assert!(!blob_exists(&app, &file_id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_removes_expired_upload_tokens() -> Result<()> {
    let app = build_test_app_with_retention(365).await?;
    let user_id = Uuid::new_v4();
    stroem_db::UserRepo::create(
        &app.pool,
        user_id,
        &format!("{user_id}@test.com"),
        None,
        None,
    )
    .await?;
    let now = chrono::Utc::now();
    UploadTokenRepo::insert(
        &app.pool,
        "tok-expired",
        user_id,
        "ws1",
        "task1",
        "a",
        now - chrono::Duration::hours(1),
    )
    .await?;
    UploadTokenRepo::insert(
        &app.pool,
        "tok-live",
        user_id,
        "ws1",
        "task1",
        "a",
        now + chrono::Duration::hours(1),
    )
    .await?;

    stroem_server::recovery::retention_cleanup(&app.state).await;

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT token_sha256 FROM input_file_upload_token WHERE token_sha256 LIKE 'tok-%'",
    )
    .fetch_all(&app.pool)
    .await?;
    assert_eq!(rows, vec![("tok-live".to_string(),)]);
    Ok(())
}
