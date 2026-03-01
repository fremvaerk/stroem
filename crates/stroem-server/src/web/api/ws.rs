use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::parse_uuid_param;
use axum::{
    extract::{ws::WebSocket, Path, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
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
        Err(resp) => return resp,
    };

    // Validate auth when enabled
    if let Some(auth_config) = &state.config.auth {
        // Try Authorization header first, then ?token= query param
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.to_string())
            .or_else(|| query.token.clone());

        match token {
            Some(t) if t.starts_with("strm_") => {
                if let Err(resp) = crate::web::api::middleware::validate_api_key(&t, &state).await {
                    return resp;
                }
            }
            Some(t) => {
                if crate::auth::validate_access_token(&t, &auth_config.jwt_secret).is_err() {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "Invalid or expired token"})),
                    )
                        .into_response();
                }
            }
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "Authentication required"})),
                )
                    .into_response();
            }
        }
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
