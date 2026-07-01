//! End-to-end OAuth 2.1 Authorization Code + PKCE flow tests.
//!
//! These tests exercise the real HTTP surface: /oauth/authorize, the SPA
//! consent JSON endpoint, /oauth/token, and the refresh-token rotation
//! path. Each test runs against a fresh Postgres container.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{create_pool, run_migrations, OAuthClientRepo, UserRepo};
use stroem_server::auth::hash_password;
use stroem_server::config::{
    AuthConfig, DbConfig, InitialUserConfig, LogStorageConfig, McpConfig, RetentionConfig,
    ServerConfig, WorkspaceSourceDef,
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

const USER_EMAIL: &str = "oauth-user@test.com";
const USER_PASSWORD: &str = "oauth-test-pw-long-enough";
const JWT_SECRET: &str = "oauth-test-jwt-secret-key-long-enough";
const REFRESH_SECRET: &str = "oauth-test-refresh-secret-long-enough";
const BASE_URL: &str = "http://stroem.test";
const REDIRECT_URI: &str = "http://127.0.0.1:33891/callback";

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn setup() -> Result<(
    Router,
    PgPool,
    TempDir,
    testcontainers::ContainerAsync<Postgres>,
)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: url.clone() },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "oauth-test-worker-token-long-enough-32".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: JWT_SECRET.to_string(),
            refresh_secret: REFRESH_SECRET.to_string(),
            base_url: Some(BASE_URL.to_string()),
            providers: HashMap::new(),
            initial_user: Some(InitialUserConfig {
                email: USER_EMAIL.to_string(),
                password: USER_PASSWORD.to_string(),
            }),
        }),
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: Some(McpConfig { enabled: true }),
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let password_hash = hash_password(USER_PASSWORD)?;
    UserRepo::create(
        &pool,
        Uuid::new_v4(),
        USER_EMAIL,
        Some(&password_hash),
        None,
    )
    .await?;

    let mgr = WorkspaceManager::from_config("default", WorkspaceConfig::default());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

/// Register a test client directly via the DB repo so we don't need the
/// Phase-3 DCR endpoint to run these tests.
async fn register_public_client(pool: &PgPool) -> Result<String> {
    let client_id = format!("test-{}", Uuid::new_v4());
    OAuthClientRepo::create(
        pool,
        &client_id,
        None,
        "Test Client",
        &[REDIRECT_URI.to_string()],
        &[
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ],
        "mcp",
        false,
        None,
    )
    .await?;
    Ok(client_id)
}

/// PKCE: derive S256 challenge from verifier.
fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Login → access token.
async fn login(router: &Router) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({"email": USER_EMAIL, "password": USER_PASSWORD}).to_string(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    body["access_token"].as_str().unwrap().to_string()
}

/// `/oauth/authorize` with a valid PKCE request redirects to the SPA
/// consent page (preserving every param) — no consent UI implemented in
/// the server itself.
#[tokio::test]
async fn test_authorize_redirects_to_consent_page() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let verifier = "a".repeat(64);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");

    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri={ru}&code_challenge={c}&code_challenge_method=S256\
         &scope=mcp&resource={r}&state=xyz",
        ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
        c = challenge,
        r = url::form_urlencoded::byte_serialize(resource.as_bytes()).collect::<String>(),
    );
    let req = Request::builder()
        .method("GET")
        .uri(&uri)
        .header("Host", "stroem.test")
        .body(Body::empty())?;
    let resp = router.oneshot(req).await?;

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp.headers().get("location").unwrap().to_str()?;
    assert!(
        loc.starts_with("/consent?"),
        "expected redirect to SPA consent, got {loc}"
    );
    assert!(loc.contains(&format!("client_id={client_id}")));
    assert!(loc.contains("code_challenge_method=S256"));
    assert!(loc.contains("scope=mcp"));
    assert!(loc.contains("state=xyz"));
    Ok(())
}

/// Unknown client_id is reported inline (NOT redirected) — RFC 6749 §4.1.2.1.
#[tokio::test]
async fn test_authorize_rejects_unknown_client_inline() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;
    let verifier = "a".repeat(64);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id=bogus\
         &redirect_uri={ru}&code_challenge={c}&code_challenge_method=S256\
         &scope=mcp&resource={r}",
        ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
        c = challenge,
        r = url::form_urlencoded::byte_serialize(resource.as_bytes()).collect::<String>(),
    );
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("Host", "stroem.test")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "invalid_client");
    Ok(())
}

/// Unregistered redirect_uri must NOT round-trip to the client (per spec) —
/// inline error is the only safe response.
#[tokio::test]
async fn test_authorize_rejects_bad_redirect_inline() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let verifier = "a".repeat(64);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri={ru}&code_challenge={c}&code_challenge_method=S256\
         &scope=mcp&resource={r}",
        ru = url::form_urlencoded::byte_serialize(b"http://evil.example/").collect::<String>(),
        c = challenge,
        r = url::form_urlencoded::byte_serialize(resource.as_bytes()).collect::<String>(),
    );
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("Host", "stroem.test")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// Full happy-path: authorize → consent → token → /mcp call succeeds.
#[tokio::test]
async fn test_full_pkce_flow_yields_mcp_capable_token() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let access_jwt = login(&router).await;
    let verifier = "AaBbCcDdEeFfGgHhIiJjKkLlMmNnOoPpQqRrSsTtUuVvWwXxYy".to_string();
    assert!(verifier.len() >= 43 && verifier.len() <= 128);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");

    // 1. SPA posts consent decision → server mints auth code.
    let consent_body = json!({
        "client_id": client_id,
        "redirect_uri": REDIRECT_URI,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "scope": "mcp",
        "resource": resource,
        "state": "csrf-state"
    });
    let consent_req = Request::builder()
        .method("POST")
        .uri("/api/oauth/consent")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {access_jwt}"))
        .body(Body::from(consent_body.to_string()))?;
    let consent_resp = router.clone().oneshot(consent_req).await?;
    assert_eq!(consent_resp.status(), StatusCode::OK);
    let consent_data = body_json(consent_resp).await;
    let redirect_url = consent_data["redirect_url"].as_str().unwrap();
    assert!(redirect_url.starts_with(REDIRECT_URI));
    assert!(redirect_url.contains("&state=csrf-state"));

    // Extract code from the redirect URL.
    let code = redirect_url
        .split_once("code=")
        .unwrap()
        .1
        .split('&')
        .next()
        .unwrap();

    // 2. Token exchange.
    let token_body = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={ru}\
         &code_verifier={v}&client_id={client_id}",
        ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
        v = verifier,
    );
    let token_req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("Host", "stroem.test")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(token_body))?;
    let token_resp = router.clone().oneshot(token_req).await?;
    assert_eq!(token_resp.status(), StatusCode::OK, "{:?}", token_resp);
    let token_data = body_json(token_resp).await;
    let access_token = token_data["access_token"].as_str().unwrap();
    let refresh_token = token_data["refresh_token"].as_str().unwrap();
    assert_eq!(token_data["token_type"], "Bearer");
    assert_eq!(token_data["expires_in"], 3600);
    assert_eq!(token_data["scope"], "mcp");

    // 3. Use the OAuth-issued access token at /mcp.
    // rmcp restricts Host to localhost/127.0.0.1/::1 by default (DNS-rebinding
    // protection). The audience-bound JWT still validates because the server
    // derives the canonical issuer from auth.base_url, not from the request
    // Host — so aud="http://stroem.test/mcp" matches what the middleware
    // expects, regardless of which Host header the client sent.
    let mcp_body = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": 0,
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    });
    let mcp_req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {access_token}"))
        .body(Body::from(mcp_body.to_string()))?;
    let mcp_resp = router.clone().oneshot(mcp_req).await?;
    assert_eq!(
        mcp_resp.status(),
        StatusCode::OK,
        "OAuth-minted token must work at /mcp"
    );

    // 4. Replay attempt: the auth code is single-use.
    let replay_body = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={ru}&code_verifier={v}&client_id={client_id}",
        ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
        v = verifier,
    );
    let replay_req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("Host", "stroem.test")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(replay_body))?;
    let replay_resp = router.clone().oneshot(replay_req).await?;
    assert_eq!(replay_resp.status(), StatusCode::BAD_REQUEST);
    let replay_data = body_json(replay_resp).await;
    assert_eq!(replay_data["error"], "invalid_grant");

    // 5. Refresh-token rotation: old refresh is revoked, new one works.
    let refresh_body =
        format!("grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}");
    let refresh_req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("Host", "stroem.test")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(refresh_body))?;
    let refresh_resp = router.clone().oneshot(refresh_req).await?;
    assert_eq!(refresh_resp.status(), StatusCode::OK);
    let refresh_data = body_json(refresh_resp).await;
    let new_refresh = refresh_data["refresh_token"].as_str().unwrap();
    assert_ne!(
        new_refresh, refresh_token,
        "refresh-token rotation must yield a new token"
    );

    // Old refresh now rejected (token-theft sentinel).
    let reuse_body =
        format!("grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}");
    let reuse_req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("Host", "stroem.test")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(reuse_body))?;
    let reuse_resp = router.oneshot(reuse_req).await?;
    assert_eq!(reuse_resp.status(), StatusCode::BAD_REQUEST);
    let reuse_data = body_json(reuse_resp).await;
    assert_eq!(reuse_data["error"], "invalid_grant");

    Ok(())
}

/// PKCE failure: wrong verifier → invalid_grant.
#[tokio::test]
async fn test_token_rejects_bad_pkce_verifier() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let access_jwt = login(&router).await;
    let verifier = "x".repeat(48);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");

    let consent_req = Request::builder()
        .method("POST")
        .uri("/api/oauth/consent")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {access_jwt}"))
        .body(Body::from(
            json!({
                "client_id": client_id,
                "redirect_uri": REDIRECT_URI,
                "code_challenge": challenge,
                "code_challenge_method": "S256",
                "scope": "mcp",
                "resource": resource,
            })
            .to_string(),
        ))?;
    let consent_resp = router.clone().oneshot(consent_req).await?;
    assert_eq!(consent_resp.status(), StatusCode::OK);
    let consent_data = body_json(consent_resp).await;
    let redirect_url = consent_data["redirect_url"].as_str().unwrap();
    let code = redirect_url
        .split_once("code=")
        .unwrap()
        .1
        .split('&')
        .next()
        .unwrap();

    // Wrong verifier.
    let token_body = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={ru}&code_verifier={wrong}&client_id={client_id}",
        ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
        wrong = "y".repeat(48),
    );
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(token_body))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "invalid_grant");
    Ok(())
}

/// RFC 7591 Dynamic Client Registration — happy path with a public client.
#[tokio::test]
async fn test_dcr_register_public_client_then_authorize() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;

    let body = json!({
        "client_name": "Cursor Editor",
        "redirect_uris": ["http://127.0.0.1:33891/callback"],
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let data = body_json(resp).await;

    let client_id = data["client_id"].as_str().unwrap();
    assert!(
        client_id.starts_with("mcp_"),
        "DCR client_id prefix: got {client_id}"
    );
    assert!(
        data.get("client_secret").is_none() || data["client_secret"].is_null(),
        "public client must have no client_secret"
    );
    assert_eq!(data["token_endpoint_auth_method"], "none");
    assert_eq!(data["scope"], "mcp");
    assert_eq!(
        data["grant_types"],
        json!(["authorization_code", "refresh_token"])
    );
    assert!(data["client_secret_expires_at"].as_i64().unwrap() > 0);

    // Newly-registered client can now drive /oauth/authorize.
    let verifier = "z".repeat(64);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri={ru}&code_challenge={c}&code_challenge_method=S256\
         &scope=mcp&resource={r}",
        ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
        c = challenge,
        r = url::form_urlencoded::byte_serialize(resource.as_bytes()).collect::<String>(),
    );
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("Host", "stroem.test")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    Ok(())
}

/// Confidential client (token_endpoint_auth_method=client_secret_post) gets
/// a generated secret returned exactly once.
#[tokio::test]
async fn test_dcr_register_confidential_client_returns_secret() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;
    let body = json!({
        "client_name": "CI Pipeline",
        "redirect_uris": ["https://ci.example/oauth/cb"],
        "token_endpoint_auth_method": "client_secret_post",
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let data = body_json(resp).await;
    let secret = data["client_secret"]
        .as_str()
        .expect("client_secret present");
    assert!(
        secret.starts_with("strm_"),
        "confidential client secret reuses strm_ prefix: {secret}"
    );
    assert_eq!(data["token_endpoint_auth_method"], "client_secret_post");
    Ok(())
}

/// DCR rejects unsupported scope.
#[tokio::test]
async fn test_dcr_rejects_unsupported_scope() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;
    let body = json!({
        "client_name": "evil",
        "redirect_uris": ["http://127.0.0.1/cb"],
        "scope": "admin:everything",
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let data = body_json(resp).await;
    assert_eq!(data["error"], "invalid_client_metadata");
    Ok(())
}

/// DCR rejects HTTP redirect URIs to non-loopback hosts (phishing
/// prevention — OAuth 2.1 §4.1).
#[tokio::test]
async fn test_dcr_rejects_http_non_loopback_redirect() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;
    let body = json!({
        "client_name": "evil",
        "redirect_uris": ["http://attacker.example/cb"],
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let data = body_json(resp).await;
    assert_eq!(data["error"], "invalid_client_metadata");
    Ok(())
}

/// Regression for code-review #1: an OAuth /mcp token MUST be rejected by
/// `/api/*`. Without this, an admin clicking Allow on the consent screen
/// hands the MCP client (and anyone who reads its token storage) the full
/// REST API including /api/users/{id}/admin.
#[tokio::test]
async fn test_mcp_token_rejected_by_rest_api() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let access_jwt = login(&router).await;
    let verifier = "AaBbCcDdEeFfGgHhIiJjKkLlMmNnOoPpQqRrSsTtUuVvWwXxYy".to_string();
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");

    // Drive the consent flow to get an audience-bound token.
    let consent_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/oauth/consent")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {access_jwt}"))
                .body(Body::from(
                    json!({
                        "client_id": client_id,
                        "redirect_uri": REDIRECT_URI,
                        "code_challenge": challenge,
                        "code_challenge_method": "S256",
                        "scope": "mcp",
                        "resource": resource,
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(consent_resp.status(), StatusCode::OK);
    let code = body_json(consent_resp).await["redirect_url"]
        .as_str()
        .unwrap()
        .split_once("code=")
        .unwrap()
        .1
        .split('&')
        .next()
        .unwrap()
        .to_string();
    let token_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&code={code}&redirect_uri={ru}&code_verifier={v}&client_id={client_id}",
                    ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
                    v = verifier,
                )))?,
        )
        .await?;
    assert_eq!(token_resp.status(), StatusCode::OK);
    let mcp_access = body_json(token_resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Present the MCP-bound token to a REST endpoint — must be rejected.
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/workspaces")
                .header("Authorization", format!("Bearer {mcp_access}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "MCP-audience token must not authenticate /api/* — consent screen \
         disclosed only MCP tool access, not the full REST surface"
    );
    Ok(())
}

/// Regression for code-review #3: the consent endpoint must re-derive the
/// expected `resource` from the canonical issuer, not trust the SPA's
/// echo. A logged-in user posting a bogus `resource` directly must fail.
#[tokio::test]
async fn test_consent_rejects_unexpected_resource() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let access_jwt = login(&router).await;
    let verifier = "z".repeat(64);
    let challenge = s256_challenge(&verifier);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/oauth/consent")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {access_jwt}"))
                .body(Body::from(
                    json!({
                        "client_id": client_id,
                        "redirect_uri": REDIRECT_URI,
                        "code_challenge": challenge,
                        "code_challenge_method": "S256",
                        "scope": "mcp",
                        // attacker-supplied resource — not /mcp on the canonical issuer
                        "resource": "https://elsewhere.example/admin",
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// Regression for code-review #6: replaying a revoked refresh token MUST
/// revoke the entire chain — the legitimate rotated token must also stop
/// working. Without this, a stolen refresh token remains usable indefinitely
/// after rotation.
#[tokio::test]
async fn test_refresh_reuse_revokes_entire_chain() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_id = register_public_client(&pool).await?;
    let access_jwt = login(&router).await;
    let verifier = "x".repeat(64);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");

    // Get initial token pair.
    let consent_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/oauth/consent")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {access_jwt}"))
                .body(Body::from(
                    json!({
                        "client_id": client_id, "redirect_uri": REDIRECT_URI,
                        "code_challenge": challenge, "code_challenge_method": "S256",
                        "scope": "mcp", "resource": resource,
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    let code = body_json(consent_resp).await["redirect_url"]
        .as_str()
        .unwrap()
        .split_once("code=")
        .unwrap()
        .1
        .split('&')
        .next()
        .unwrap()
        .to_string();
    let token_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&code={code}&redirect_uri={ru}&code_verifier={v}&client_id={client_id}",
                    ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
                    v = verifier,
                )))?,
        )
        .await?;
    let data = body_json(token_resp).await;
    let r1 = data["refresh_token"].as_str().unwrap().to_string();

    // Legitimate rotation: r1 → r2.
    let rotate_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=refresh_token&refresh_token={r1}&client_id={client_id}"
                )))?,
        )
        .await?;
    assert_eq!(rotate_resp.status(), StatusCode::OK);
    let r2 = body_json(rotate_resp).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(r1, r2);

    // Attacker replays r1 — must trigger chain revocation.
    let replay_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=refresh_token&refresh_token={r1}&client_id={client_id}"
                )))?,
        )
        .await?;
    assert_eq!(replay_resp.status(), StatusCode::BAD_REQUEST);

    // r2 (the legitimate rotated token) must ALSO now be revoked.
    let r2_resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=refresh_token&refresh_token={r2}&client_id={client_id}"
                )))?,
        )
        .await?;
    assert_eq!(
        r2_resp.status(),
        StatusCode::BAD_REQUEST,
        "the legitimate rotated token must be invalidated when reuse is detected — \
         OAuth 2.1 §4.13 token-theft sentinel"
    );
    Ok(())
}

/// Regression for code-review #5: DCR must reject arbitrary URI schemes.
/// `intent://` and similar custom schemes can be used to launch malicious
/// apps via OS handlers, bypassing the same-app guarantees OAuth normally
/// provides. Allowed: https, http loopback, reverse-DNS private schemes.
#[tokio::test]
async fn test_dcr_rejects_intent_scheme() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({
                        "client_name": "evil",
                        "redirect_uris": ["intent://hijack/#Intent;scheme=https;end"],
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// Regression for the post-commit security review: client X must not be
/// able to refresh client Y's tokens by presenting Y's refresh_token
/// with X's client_id. RFC 6749 §6 / OAuth 2.1 §6 require the refresh
/// token to be bound to the authenticated client.
#[tokio::test]
async fn test_refresh_rejects_cross_client_binding() -> Result<()> {
    let (router, pool, _tmp, _c) = setup().await?;
    let client_y = register_public_client(&pool).await?;
    let client_x = register_public_client(&pool).await?;
    let access_jwt = login(&router).await;
    let verifier = "q".repeat(64);
    let challenge = s256_challenge(&verifier);
    let resource = format!("{BASE_URL}/mcp");

    // Get a refresh token belonging to client Y.
    let consent_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/oauth/consent")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {access_jwt}"))
                .body(Body::from(
                    json!({
                        "client_id": client_y, "redirect_uri": REDIRECT_URI,
                        "code_challenge": challenge, "code_challenge_method": "S256",
                        "scope": "mcp", "resource": resource,
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    let code = body_json(consent_resp).await["redirect_url"]
        .as_str()
        .unwrap()
        .split_once("code=")
        .unwrap()
        .1
        .split('&')
        .next()
        .unwrap()
        .to_string();
    let token_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&code={code}&redirect_uri={ru}&code_verifier={v}&client_id={client_y}",
                    ru = url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>(),
                    v = verifier,
                )))?,
        )
        .await?;
    let r_y = body_json(token_resp).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Attacker (client X, authenticated as itself) tries to refresh Y's token.
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=refresh_token&refresh_token={r_y}&client_id={client_x}"
                )))?,
        )
        .await?;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "client X must not be able to refresh a token bound to client Y"
    );
    let body = body_json(resp).await;
    assert_eq!(body["error"], "invalid_grant");
    Ok(())
}

/// Regression for #5 inverse: reverse-DNS private-use schemes (RFC 8252)
/// are valid and continue to work — Claude Desktop and similar native apps
/// rely on them.
#[tokio::test]
async fn test_dcr_accepts_reverse_dns_scheme() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("Host", "stroem.test")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({
                        "client_name": "Claude Desktop",
                        "redirect_uris": ["com.anthropic.claude:oauth/callback"],
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

/// A token issued for a *different* audience must not be accepted by /mcp.
/// We craft one manually by signing claims with the right secret but a wrong
/// `aud` — this is the case where someone tries to replay a token from
/// another resource server that trusts the same issuer.
#[tokio::test]
async fn test_mcp_rejects_token_for_different_audience() -> Result<()> {
    let (router, _pool, _tmp, _c) = setup().await?;

    // Forge a JWT with aud=<wrong>.
    use jsonwebtoken::{encode, EncodingKey, Header};
    use stroem_common::models::auth::Claims;
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: Uuid::new_v4().to_string(),
        email: "user@example.com".into(),
        is_admin: false,
        iat: now,
        exp: now + 3600,
        aud: Some("http://elsewhere.example/foo".to_string()), // wrong
        iss: Some(BASE_URL.to_string()),
        scope: Some("mcp".to_string()),
        client_id: None,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )?;

    let body = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": 0,
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))?;
    let resp = router.oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let text = body_string(resp).await;
    assert!(
        text.contains("audience"),
        "expected audience mismatch error, got: {text}"
    );
    Ok(())
}
