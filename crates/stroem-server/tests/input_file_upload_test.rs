//! Upload route for a live task and the user download route for a job's
//! input file (spec 2026-10-29-file-inputs-design.md § 4.2, § 7.2).

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::repos::input_file::AttachRule;
use stroem_db::{
    ApiKeyRepo, InputFileRepo, JobInputFileRepo, NewInputFile, UserGroupRepo, UserRepo,
};
use stroem_server::auth::generate_api_key;
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    AclAction, AclConfig, AclRule, AuthConfig, DbConfig, InputFilesConfig, LogStorageConfig,
    RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
tasks:
  files:
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  plain:
    flow: { run: { action: noop } }
  run-files:
    folder: run
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  view-files:
    folder: view
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  deny-files:
    folder: hidden
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
"#;

#[derive(Default)]
struct Opts {
    acl: bool,
    max_file_bytes: Option<u64>,
    no_archive: bool,
}

struct TestApp {
    router: Router,
    pool: PgPool,
    archive_dir: std::path::PathBuf,
    _tmp: TempDir,
}

async fn build_app(opts: Opts) -> Result<TestApp> {
    let pool = stroem_test_support::test_pool().await;
    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let archive_dir = tmp.path().join("input-archive");
    std::fs::create_dir_all(&archive_dir)?;

    let input_files = opts.max_file_bytes.map(|m| InputFilesConfig {
        max_file_bytes: m,
        ..Default::default()
    });
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
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                display_name: None,
                description: None,
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: opts.acl.then(|| AuthConfig {
            jwt_secret: "test-jwt-secret-key".to_string(),
            refresh_secret: "test-refresh-secret-key".to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: None,
            rate_limit: Default::default(),
        }),
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: opts.acl.then(|| AclConfig {
            default: AclAction::Deny,
            rules: vec![
                AclRule {
                    workspace: "default".to_string(),
                    tasks: vec!["run/*".to_string()],
                    action: AclAction::Run,
                    groups: vec!["runners".to_string()],
                    users: vec![],
                },
                AclRule {
                    workspace: "default".to_string(),
                    tasks: vec!["view/*".to_string()],
                    action: AclAction::View,
                    groups: vec!["viewers".to_string()],
                    users: vec![],
                },
            ],
        }),
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        input_files,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        leader: Default::default(),
        pin_store: None,
    };

    let workspace: WorkspaceConfig = serde_yaml::from_str(YAML)?;
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let archive: Option<Arc<dyn BlobArchive>> = if opts.no_archive {
        None
    } else {
        Some(Arc::new(LocalBlobArchive::new(archive_dir.clone())))
    };
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_input_blob(archive);
    let router = build_router(state, CancellationToken::new());
    Ok(TestApp {
        router,
        pool,
        archive_dir,
        _tmp: tmp,
    })
}

async fn user(pool: &PgPool, group: &str) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    let email = format!("{user_id}@test.com");
    UserRepo::create(pool, user_id, &email, None, None)
        .await
        .unwrap();
    UserGroupRepo::add(pool, user_id, group).await.unwrap();
    let (raw, hash) = generate_api_key();
    ApiKeyRepo::create(pool, &hash, user_id, "test", &raw[..12], None)
        .await
        .unwrap();
    (user_id, raw)
}

fn upload(task: &str, name: &str, key: Option<&str>, body: Body) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/workspaces/default/tasks/{task}/files?name={name}"
        ))
        .header("content-type", "text/csv");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(body).unwrap()
}

async fn send(app: &TestApp, req: Request<Body>) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let resp = app.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

async fn input_file_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM input_file")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn files_under(dir: &std::path::Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

#[tokio::test]
async fn upload_returns_201_with_metadata() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let (status, _, body) = send(&app, upload("files", "in.csv", None, Body::from("hello"))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v = json(&body);
    assert_eq!(v["size_bytes"], 5);
    assert_eq!(
        v["sha256"],
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!(v["name"], "in.csv");
    assert_eq!(v["content_type"], "text/csv");
    let id = v["file_id"].as_str().unwrap().to_string();
    assert!(
        id.len() == 37
            && id.starts_with("file_")
            && id[5..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "bad file id {id}"
    );
    assert!(v["expires_at"].is_string());

    let row = InputFileRepo::get(&app.pool, &id).await?.expect("row");
    assert_eq!(row.origin, "api");
    assert_eq!(row.workspace, "default");
    assert_eq!(row.task_name, "files");
    assert_eq!(row.original_name, "in.csv");
    assert_eq!(row.storage_key, format!("inputs/default/{id}"));
    assert!(files_under(&app.archive_dir.join("inputs/default")) > 0);
    Ok(())
}

#[tokio::test]
async fn upload_acl_run_view_deny() -> Result<()> {
    let app = build_app(Opts {
        acl: true,
        ..Default::default()
    })
    .await?;
    let (run_id, run_key) = user(&app.pool, "runners").await;
    let (_, view_key) = user(&app.pool, "viewers").await;

    let (s, _, b) = send(
        &app,
        upload("run-files", "a.csv", Some(&run_key), Body::from("x")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let row = InputFileRepo::get(&app.pool, json(&b)["file_id"].as_str().unwrap())
        .await?
        .unwrap();
    assert_eq!(row.uploaded_by_user_id, Some(run_id));

    let (s, _, _) = send(
        &app,
        upload("view-files", "a.csv", Some(&view_key), Body::from("x")),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, _, _) = send(
        &app,
        upload("deny-files", "a.csv", Some(&run_key), Body::from("x")),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _, _) = send(&app, upload("run-files", "a.csv", None, Body::from("x"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn upload_auth_off_records_null_owner() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let (s, _, b) = send(&app, upload("files", "a.csv", None, Body::from("x"))).await;
    assert_eq!(s, StatusCode::CREATED);
    let row = InputFileRepo::get(&app.pool, json(&b)["file_id"].as_str().unwrap())
        .await?
        .unwrap();
    assert_eq!(row.uploaded_by_user_id, None);
    Ok(())
}

#[tokio::test]
async fn upload_task_without_file_field_is_400() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let (s, _, b) = send(&app, upload("plain", "a.csv", None, Body::from("x"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json(&b)["error"], "task has no file input");
    assert_eq!(input_file_count(&app.pool).await, 0);
    Ok(())
}

#[tokio::test]
async fn upload_unknown_task_is_404() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let (s, _, _) = send(&app, upload("nope", "a.csv", None, Body::from("x"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn upload_invalid_name_is_400() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let (s, _, b) = send(&app, upload("files", "..", None, Body::from("x"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json(&b)["error"], "invalid file name");
    assert_eq!(input_file_count(&app.pool).await, 0);
    Ok(())
}

#[tokio::test]
async fn upload_missing_name_is_400() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let req = Request::builder()
        .method("POST")
        .uri("/api/workspaces/default/tasks/files/files")
        .body(Body::from("x"))?;
    let (s, _, b) = send(&app, req).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json(&b)["error"], "invalid file name");
    Ok(())
}

#[tokio::test]
async fn upload_over_limit_is_413_and_keeps_nothing() -> Result<()> {
    let app = build_app(Opts {
        max_file_bytes: Some(1024),
        ..Default::default()
    })
    .await?;
    // Chunked: no Content-Length, so only the streaming count can trip.
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
        Ok(bytes::Bytes::from(vec![b'a'; 1024])),
        Ok(bytes::Bytes::from(vec![b'b'; 1024])),
    ];
    let body = Body::from_stream(futures_util::stream::iter(chunks));
    let (s, _, b) = send(&app, upload("files", "big.bin", None, body)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json(&b)["error"], "file is larger than 1024 bytes");
    assert_eq!(input_file_count(&app.pool).await, 0);
    assert_eq!(files_under(&app.archive_dir), 0);

    // A declared length over the limit is refused before any storage.
    let req = Request::builder()
        .method("POST")
        .uri("/api/workspaces/default/tasks/files/files?name=big.bin")
        .header("content-length", "2048")
        .body(Body::from(vec![b'a'; 2048]))?;
    let (s, _, b) = send(&app, req).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json(&b)["error"], "file is larger than 1024 bytes");
    assert_eq!(files_under(&app.archive_dir), 0);
    Ok(())
}

#[tokio::test]
async fn upload_without_archive_is_503() -> Result<()> {
    let app = build_app(Opts {
        no_archive: true,
        ..Default::default()
    })
    .await?;
    let (s, _, b) = send(&app, upload("files", "a.csv", None, Body::from("x"))).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&b)["error"], "file storage is not configured");
    Ok(())
}

/// Store a file through the archive + repo and attach it to a new job of
/// `task` as `data_file`.
async fn job_with_file(app: &TestApp, task: &str, original_name: &str, ct: &str) -> Result<Uuid> {
    let archive = LocalBlobArchive::new(app.archive_dir.clone());
    let file_id = stroem_common::file_field::new_file_id();
    let key = format!("inputs/default/{file_id}");
    archive
        .put(&key, ct, bytes::Bytes::from_static(b"col\n1\n"))
        .await?;
    InputFileRepo::insert(
        &app.pool,
        &NewInputFile {
            file_id: &file_id,
            workspace: "default",
            task_name: task,
            uploaded_by_user_id: None,
            origin: "api",
            original_name,
            safe_name: "x.csv",
            content_type: ct,
            size_bytes: 6,
            sha256: "00",
            storage_key: &key,
        },
    )
    .await?;
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, 'default', $2, 'completed', 'api', NOW())",
    )
    .bind(job_id)
    .bind(task)
    .execute(&app.pool)
    .await?;
    let mut tx = app.pool.begin().await?;
    let ok = JobInputFileRepo::attach_tx(
        &mut tx,
        job_id,
        "data_file",
        &file_id,
        &[AttachRule::Own {
            workspace: "default",
            task_name: task,
            user_id: None,
            ttl_secs: 3600,
        }],
    )
    .await?;
    assert!(ok);
    tx.commit().await?;
    Ok(job_id)
}

fn download(job: Uuid, field: &str, key: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job}/input-files/{field}"));
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn download_input_file_acl_and_headers() -> Result<()> {
    let app = build_app(Opts {
        acl: true,
        ..Default::default()
    })
    .await?;
    let (_, run_key) = user(&app.pool, "runners").await;
    let (_, view_key) = user(&app.pool, "viewers").await;
    let (_, none_key) = user(&app.pool, "nobody").await;
    let job = job_with_file(&app, "view-files", "in.csv", "text/csv").await?;

    for key in [&run_key, &view_key] {
        let (s, h, b) = send(&app, download(job, "data_file", Some(key))).await;
        // `run` group has no rule on folder `view`: Deny for it.
        if key == &run_key {
            assert_eq!(s, StatusCode::NOT_FOUND);
            continue;
        }
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, b"col\n1\n");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["content-length"], "6");
        assert_eq!(h["content-type"], "text/csv");
        assert!(h["content-disposition"].to_str()?.starts_with("attachment"));
    }
    let (s, _, _) = send(&app, download(job, "data_file", Some(&none_key))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = send(&app, download(job, "other_field", Some(&view_key))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn download_run_permission_and_unsafe_names() -> Result<()> {
    let app = build_app(Opts {
        acl: true,
        ..Default::default()
    })
    .await?;
    let (_, run_key) = user(&app.pool, "runners").await;
    let job = job_with_file(&app, "run-files", "in.csv", "text/html").await?;
    let (s, h, _) = send(&app, download(job, "data_file", Some(&run_key))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h["content-disposition"].to_str()?.starts_with("attachment"));

    let job = job_with_file(&app, "run-files", "rapport \"é\";.csv", "text/csv").await?;
    let (s, h, _) = send(&app, download(job, "data_file", Some(&run_key))).await;
    assert_eq!(s, StatusCode::OK);
    let d = h["content-disposition"].to_str()?.to_string();
    let (quoted, ext) = d.split_once("filename*=").expect("filename* present");
    assert!(ext.starts_with("UTF-8''"), "{d}");
    // Only the two delimiting quotes of `filename="…"` remain.
    assert_eq!(quoted.matches('"').count(), 2, "{d}");
    assert!(!ext.contains('"') && !ext.contains(';'), "{d}");
    Ok(())
}
