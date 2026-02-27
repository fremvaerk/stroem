use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::parse_uuid_param;
use axum::{
    extract::{ws::WebSocket, Path, State, WebSocketUpgrade},
    response::IntoResponse,
};
use std::sync::Arc;
use stroem_db::JobRepo;
use uuid::Uuid;

/// GET /api/jobs/{id}/logs/stream -- WebSocket upgrade for live log streaming
pub async fn job_log_stream(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let job_id = match parse_uuid_param(&id, "job") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    ws.on_upgrade(move |socket| handle_ws(socket, state, job_id))
        .into_response()
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>, job_id: Uuid) {
    // 1. Send backfill (existing log content)
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
