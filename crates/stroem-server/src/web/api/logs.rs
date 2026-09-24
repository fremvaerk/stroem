//! `GET /api/jobs/{id}/logs` and `GET /api/jobs/{id}/steps/{step}/logs`:
//! a bounded tail by default, the whole log streamed with `?full=true`
//! (spec § 3.1).

use crate::acl::TaskPermission;
use crate::config::LogReadConfig;
use crate::log_read::{tail_body, StepFilter, LOG_SOURCE_HEADER};
use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::jobs::check_job_acl;
use crate::web::api::middleware::AuthUser;
use crate::web::api::parse_uuid_param;
use crate::web::error::AppError;
use anyhow::Context;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;
use stroem_db::JobRepo;

/// Raw query values, so malformed input becomes our JSON 400 rather than
/// axum's plain-text rejection.
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub tail_bytes: Option<String>,
    pub full: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LogReadMode {
    Tail(u64),
    Full,
}

pub fn parse_log_mode(q: &LogQuery, cfg: &LogReadConfig) -> Result<LogReadMode, AppError> {
    let full = match q.full.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "full must be true or false, got '{other}'"
            )))
        }
    };
    match (full, q.tail_bytes.as_deref()) {
        (true, Some(_)) => Err(AppError::BadRequest(
            "tail_bytes and full=true are mutually exclusive".into(),
        )),
        (true, None) => Ok(LogReadMode::Full),
        (false, None) => Ok(LogReadMode::Tail(cfg.tail_default_bytes)),
        (false, Some(raw)) => match raw.parse::<u64>() {
            Ok(n) if (1..=cfg.tail_max_bytes).contains(&n) => Ok(LogReadMode::Tail(n)),
            _ => Err(AppError::BadRequest(format!(
                "tail_bytes must be an integer between 1 and {}",
                cfg.tail_max_bytes
            ))),
        },
    }
}

fn source_header(source: &'static str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static(LOG_SOURCE_HEADER),
        HeaderValue::from_static(source),
    )
}

async fn read_logs(
    state: Arc<AppState>,
    auth_user: Option<AuthUser>,
    id: String,
    step: Option<String>,
    query: LogQuery,
) -> Result<Response, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;
    let mode = parse_log_mode(&query, &state.config.log_storage.read)?;
    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }
    let is_terminal = stroem_common::models::job::is_terminal_status(&job.status);
    let meta = JobLogMeta {
        workspace: job.workspace,
        task_name: job.task_name,
        created_at: job.created_at,
    };
    let filter = step.as_deref().map_or(StepFilter::All, StepFilter::Step);
    match mode {
        LogReadMode::Tail(n) => {
            let tail = state
                .log_storage
                .read_tail(job_id, &meta, is_terminal, filter, n)
                .await
                .context("read log tail")?;
            let body = tail_body(&tail).context("serialize log tail")?;
            let headers = [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                ),
                source_header(tail.source.as_str()),
            ];
            Ok((headers, body).into_response())
        }
        LogReadMode::Full => {
            let (source, stream) = state
                .log_storage
                .stream_full(job_id, &meta, is_terminal, filter)
                .await
                .context("stream log")?;
            let headers = [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
                ),
                source_header(source.as_str()),
            ];
            Ok((headers, Body::from_stream(stream)).into_response())
        }
    }
}

/// A `Query<LogQuery>` rejection (e.g. a duplicated parameter) becomes our
/// JSON 400 like every other bad query, instead of axum's plain-text one.
fn query_rejection(err: QueryRejection) -> AppError {
    AppError::BadRequest(err.body_text())
}

/// GET /api/jobs/{id}/logs
#[tracing::instrument(skip(state))]
pub async fn get_job_logs(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let Query(query) = query.map_err(query_rejection)?;
    read_logs(state, auth_user, id, None, query).await
}

/// GET /api/jobs/{id}/steps/{step}/logs — `_server` is a pseudo-step.
#[tracing::instrument(skip(state))]
pub async fn get_step_logs(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((id, step_name)): Path<(String, String)>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let Query(query) = query.map_err(query_rejection)?;
    read_logs(state, auth_user, id, Some(step_name), query).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(tail: Option<&str>, full: Option<&str>) -> LogQuery {
        LogQuery {
            tail_bytes: tail.map(Into::into),
            full: full.map(Into::into),
        }
    }

    #[test]
    fn parse_log_mode_defaults_bounds_and_exclusivity() {
        let cfg = LogReadConfig::default();
        assert_eq!(
            parse_log_mode(&q(None, None), &cfg).unwrap(),
            LogReadMode::Tail(262_144)
        );
        assert_eq!(
            parse_log_mode(&q(Some("1024"), None), &cfg).unwrap(),
            LogReadMode::Tail(1024)
        );
        assert_eq!(
            parse_log_mode(&q(Some("4194304"), None), &cfg).unwrap(),
            LogReadMode::Tail(4_194_304)
        );
        assert_eq!(
            parse_log_mode(&q(None, Some("true")), &cfg).unwrap(),
            LogReadMode::Full
        );
        assert_eq!(
            parse_log_mode(&q(None, Some("false")), &cfg).unwrap(),
            LogReadMode::Tail(262_144)
        );
        for (tail, full) in [
            (Some("0"), None),
            (Some("4194305"), None),
            (Some("-1"), None),
            (Some("abc"), None),
            (Some("10"), Some("true")),
            (None, Some("yes")),
        ] {
            assert!(
                matches!(
                    parse_log_mode(&q(tail, full), &cfg),
                    Err(AppError::BadRequest(_))
                ),
                "{tail:?} {full:?}"
            );
        }
    }
}
