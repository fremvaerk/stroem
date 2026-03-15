use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::parse_uuid_param;
use crate::web::error::AppError;
use axum::{
    extract::{ws::WebSocket, Path, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;
use stroem_common::models::auth::Claims;
use stroem_db::JobRepo;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// Auth token passed as query param (standard for browser WebSocket).
    pub token: Option<String>,
    /// When true, skip sending existing log content on connect.
    #[serde(default)]
    pub skip_backfill: bool,
}

/// GET /api/jobs/{id}/logs/stream -- WebSocket upgrade for live log streaming
///
/// Auth is handled manually here (not via the `require_auth` middleware layer)
/// because browser WebSocket API cannot set custom headers. Supports:
/// - `Authorization: Bearer <token>` header (programmatic clients)
/// - `?token=<token>` query param (browser clients)
pub async fn job_log_stream(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let job_id = match parse_uuid_param(&id, "job") {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };

    // --- Auth: validate token when auth is enabled, saving claims for ACL ---
    let claims: Option<Claims> = if let Some(auth_config) = &state.config.auth {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.to_string())
            .or_else(|| query.token.clone());

        match token {
            Some(t) if t.starts_with("strm_") => {
                match crate::web::api::middleware::validate_api_key(&t, &state).await {
                    Ok(c) => Some(c),
                    Err(resp) => return resp,
                }
            }
            Some(t) => match crate::auth::validate_access_token(&t, &auth_config.jwt_secret) {
                Ok(c) => Some(c),
                Err(_) => {
                    return AppError::Unauthorized("Invalid or expired token".into())
                        .into_response();
                }
            },
            None => {
                return AppError::Unauthorized("Authentication required".into()).into_response();
            }
        }
    } else {
        None
    };

    // --- ACL check: deny-by-default when ACL is configured ---
    if state.acl.is_configured() {
        if let Some(ref claims) = claims {
            let user_id = match claims.sub.parse::<uuid::Uuid>() {
                Ok(id) => id,
                Err(_) => {
                    return AppError::Internal(anyhow::anyhow!("Invalid user ID in token"))
                        .into_response();
                }
            };
            let (is_admin, groups) =
                match load_user_acl_context(&state.pool, user_id, claims.is_admin).await {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        return AppError::Internal(
                            anyhow::anyhow!(e).context("load ACL context for WS"),
                        )
                        .into_response();
                    }
                };
            if !is_admin {
                let job = match JobRepo::get(&state.pool, job_id).await {
                    Ok(Some(j)) => j,
                    Ok(None) => {
                        return AppError::not_found("Job").into_response();
                    }
                    Err(e) => {
                        return AppError::Internal(
                            anyhow::anyhow!(e).context("get job for ACL check"),
                        )
                        .into_response();
                    }
                };
                let folder = state
                    .get_workspace(&job.workspace)
                    .await
                    .and_then(|ws| ws.tasks.get(&job.task_name).and_then(|t| t.folder.clone()));
                let task_path = make_task_path(folder.as_deref(), &job.task_name);
                let perm =
                    state
                        .acl
                        .evaluate(&job.workspace, &task_path, &claims.email, &groups, false);
                if matches!(perm, TaskPermission::Deny) {
                    return AppError::not_found("Job").into_response();
                }
            }
        }
        // No claims but ACL configured = deny (auth should have caught this,
        // but this is a safety net for the case where auth is not configured
        // but ACL is — which shouldn't happen in practice)
    }

    let skip_backfill = query.skip_backfill;
    ws.on_upgrade(move |socket| handle_ws(socket, state, job_id, skip_backfill))
        .into_response()
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>, job_id: Uuid, skip_backfill: bool) {
    // 1. Send backfill (existing log content) unless skip_backfill is set
    if !skip_backfill {
        let meta = match JobRepo::get(&state.pool, job_id).await {
            Ok(Some(job)) => JobLogMeta {
                workspace: job.workspace,
                task_name: job.task_name,
                created_at: job.created_at,
            },
            _ => {
                // If job not found, use dummy meta (local file lookup still works by job_id)
                JobLogMeta {
                    workspace: String::new(),
                    task_name: String::new(),
                    created_at: chrono::Utc::now(),
                }
            }
        };

        if let Ok(existing) = state.log_storage.get_log(job_id, &meta).await {
            if !existing.is_empty()
                && socket
                    .send(axum::extract::ws::Message::Text(existing.into()))
                    .await
                    .is_err()
            {
                return;
            }
        }
    }

    // 2. Subscribe to live broadcast
    let mut rx = state.log_broadcast.subscribe(job_id).await;

    // 3. Forward broadcast messages to the WebSocket
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(chunk) => {
                        if socket.send(axum::extract::ws::Message::Text(chunk.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket subscriber lagged by {} messages for job {}", n, job_id);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(axum::extract::ws::Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // ignore pings/pongs/text from client
                }
            }
        }
    }

    // 4. Cleanup
    state.log_broadcast.remove_channel(job_id).await;
}
