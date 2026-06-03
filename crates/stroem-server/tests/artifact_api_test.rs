//! User-facing artifact API: list + download.
//!
//! Exercises `GET /api/jobs/{id}/artifacts` and `GET /api/jobs/{id}/artifacts/{name}`
//! end-to-end against the real router, a testcontainers Postgres, and a
//! local-filesystem `BlobArchive` backend.
//!
//! The plan called for a `common::TestHarness` helper, but that abstraction
//! does not exist in this crate's test suite yet. To stay within the task's
//! listed scope (artifact-only changes) we inline the harness here, mirroring
//! the pattern already established in `artifact_upload_test.rs`.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{create_pool, run_migrations};
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    ArtifactStorageConfig, DbConfig, LogStorageConfig, RetentionConfig, ServerConfig,
    WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const WORKER_TOKEN: &str = "test-worker-token";

struct TestApp {
    router: Router,
    pool: PgPool,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _tmp: TempDir,
}

async fn spawn_pg() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

async fn build_test_app() -> Result<TestApp> {
    let (pool, _pg) = spawn_pg().await?;

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
        },
        workspaces: HashMap::from([(
            "ws1".to_string(),
            WorkspaceSourceDef::Folder {
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: WORKER_TOKEN.to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: Some(ArtifactStorageConfig::default()),
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let workspace = WorkspaceConfig::new();
    let mgr = WorkspaceManager::from_config("ws1", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);

    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(artifact_dir));

    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_artifact_blob(Some(archive));
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        _pg,
        _tmp: tmp,
    })
}

async fn create_test_job(pool: &PgPool, workspace: &str, task_name: &str) -> Result<Uuid> {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, $2, $3, 'running', 'api', NOW())",
    )
    .bind(job_id)
    .bind(workspace)
    .bind(task_name)
    .execute(pool)
    .await?;
    Ok(job_id)
}

fn upload_request(
    job_id: Uuid,
    step_name: &str,
    artifact_name: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!(
            "/worker/jobs/{job_id}/steps/{step_name}/artifacts/{artifact_name}"
        ))
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap()
}

async fn upload_artifact(
    app: &TestApp,
    job_id: Uuid,
    step_name: &str,
    artifact_name: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let req = upload_request(
        job_id,
        step_name,
        artifact_name,
        content_type,
        body.to_vec(),
    );
    let resp = app.router.clone().oneshot(req).await?;
    let status = resp.status();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "upload {} failed",
        artifact_name
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn list_artifacts_returns_uploaded() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(&app, job_id, "s1", "a.txt", "text/plain", b"hi").await?;
    upload_artifact(&app, job_id, "s2", "b.png", "image/png", b"PNGDATA").await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await?.to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let names: Vec<&str> = items.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.png"));

    // url field should look like /api/jobs/{id}/artifacts/{name}
    for item in items {
        let url = item["url"].as_str().unwrap();
        assert!(
            url.starts_with(&format!("/api/jobs/{job_id}/artifacts/")),
            "unexpected url {url}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_safe_mime_serves_inline() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(&app, job_id, "s1", "shot.png", "image/png", b"PNG").await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/shot.png"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(resp.headers()["content-type"], "image/png");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    // Cache-Control must be present on every successful download response so
    // the browser disk cache cannot replay it to a subsequent unauthenticated
    // session on the same machine.
    assert_eq!(
        resp.headers()["cache-control"],
        "private, max-age=0, must-revalidate"
    );
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(disp.starts_with("inline"), "expected inline, got {disp}");

    let bytes = resp.into_body().collect().await?.to_bytes();
    assert_eq!(&bytes[..], b"PNG");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_attachment_path_sets_cache_control() -> Result<()> {
    // Regression: every successful download — inline or attachment — must set
    // `Cache-Control: private, max-age=0, must-revalidate`.
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(
        &app,
        job_id,
        "s1",
        "report.bin",
        "application/octet-stream",
        b"BINARY",
    )
    .await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/report.bin"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["cache-control"],
        "private, max-age=0, must-revalidate"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_html_forced_to_attachment() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(
        &app,
        job_id,
        "s1",
        "evil.html",
        "text/html",
        b"<script>alert(1)</script>",
    )
    .await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/evil.html"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.starts_with("attachment"),
        "expected attachment, got {disp}"
    );
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_pdf_forced_to_attachment() -> Result<()> {
    // Regression: PDFs were on the inline allow-list, but PDF.js has a history
    // of script-execution CVEs via embedded JS. They must download as
    // attachments now.
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(
        &app,
        job_id,
        "s1",
        "report.pdf",
        "application/pdf",
        b"%PDF-1.4 stub",
    )
    .await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/report.pdf"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.starts_with("attachment"),
        "expected attachment for PDF, got {disp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_markdown_forced_to_attachment() -> Result<()> {
    // Regression: Markdown was on the inline allow-list, but no major browser
    // renders it natively and some intermediaries render it as HTML — which
    // would reintroduce the XSS surface we close for HTML.
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(&app, job_id, "s1", "notes.md", "text/markdown", b"# notes").await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/notes.md"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.starts_with("attachment"),
        "expected attachment for Markdown, got {disp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_svg_forced_to_attachment() -> Result<()> {
    // SVG can carry inline <script> and event handlers — must download.
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(
        &app,
        job_id,
        "s1",
        "logo.svg",
        "image/svg+xml",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>",
    )
    .await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/logo.svg"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.starts_with("attachment"),
        "expected attachment for SVG, got {disp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_javascript_forced_to_attachment() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    upload_artifact(
        &app,
        job_id,
        "s1",
        "evil.js",
        "application/javascript",
        b"alert(1)",
    )
    .await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/evil.js"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.starts_with("attachment"),
        "expected attachment for JavaScript, got {disp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_streams_body_and_returns_full_payload() -> Result<()> {
    // Regression: the handler previously buffered the full blob via
    // `BlobArchive::get` before returning a response. It now uses
    // `get_stream` + `Body::from_stream`. Verify that a moderately-sized
    // payload round-trips intact.
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let payload: Vec<u8> = (0..(256 * 1024)).map(|i| (i % 256) as u8).collect();
    upload_artifact(
        &app,
        job_id,
        "s1",
        "big.bin",
        "application/octet-stream",
        &payload,
    )
    .await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/big.bin"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await?.to_bytes();
    assert_eq!(bytes.len(), payload.len());
    assert_eq!(&bytes[..], &payload[..]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn download_unknown_artifact_is_404() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/missing.txt"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn list_unknown_job_is_404() -> Result<()> {
    let app = build_test_app().await?;
    let bogus = Uuid::new_v4();

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{bogus}/artifacts"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Round-trip an artifact whose name contains a `/` (i.e. lives in a
/// subdirectory under `/artifacts/`). The upload accepts `reports/q1.html`,
/// the list endpoint preserves the slash, and the download endpoint accepts
/// the URL-encoded form (`reports%2Fq1.html`) and returns the original bytes.
#[tokio::test(flavor = "multi_thread")]
async fn subdirectory_artifact_round_trips_list_and_download() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let payload = b"<html><body>Q1 report</body></html>";
    upload_artifact(
        &app,
        job_id,
        "build",
        "reports/q1.html",
        "text/html",
        payload,
    )
    .await?;

    // List shows the full `reports/q1.html` name (no escaping in the JSON).
    let list_req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(list_req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let items = body.as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"].as_str().unwrap(), "reports/q1.html");

    // The `url` field is the percent-encoded download path — the slash is
    // encoded as `%2F` so the artifact name is a single URL segment.
    let url = items[0]["url"].as_str().unwrap();
    assert!(
        url.ends_with("/reports%2Fq1.html"),
        "expected percent-encoded slash, got {url}"
    );

    // Download via the percent-encoded form. Browsers don't decode `%2F` to
    // `/` in path segments, so the router resolves this to the single
    // artifact name `reports/q1.html`.
    let dl_req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{job_id}/artifacts/reports%2Fq1.html"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(dl_req).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // HTML is forced to attachment regardless of subdirectory placement.
    let disp = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.starts_with("attachment"),
        "subdir HTML must still be attachment, got {disp}"
    );

    let dl_bytes = resp.into_body().collect().await?.to_bytes();
    assert_eq!(&dl_bytes[..], &payload[..]);
    Ok(())
}
