//! The caller of a job action (restart, re-run, approve) that REST and MCP
//! share: both reduce their authentication to the token [`Claims`].

use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use stroem_common::models::auth::Claims;
use uuid::Uuid;

/// Who asks: a REST [`AuthUser`] or an MCP client (`McpAuthContext`). `None`
/// claims means auth is off. `claims.is_admin` is trusted as given: REST reads
/// it from the token or the API key's user row, MCP re-reads it from the DB
/// on every request.
#[derive(Clone, Copy, Default)]
pub(crate) struct Caller<'a> {
    pub claims: Option<&'a Claims>,
}

impl<'a> Caller<'a> {
    pub(crate) fn of(auth_user: &'a Option<AuthUser>) -> Self {
        Self {
            claims: auth_user.as_ref().map(|a| &a.claims),
        }
    }

    /// The audit identity: the user's email (the new job's `source_id`, the
    /// approver), `None` when auth is off.
    pub(crate) fn email(&self) -> Option<&'a str> {
        self.claims.map(|c| c.email.as_str())
    }

    /// The caller's user id (`None` when auth is off). A malformed `sub` is
    /// the same 401 as [`AuthUser::user_id`].
    pub(crate) fn user_id(&self) -> Result<Option<Uuid>, AppError> {
        self.claims.map(claims_user_id).transpose()
    }
}

/// `sub` as a user id; malformed → 401 "Invalid token".
pub(crate) fn claims_user_id(claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .sub
        .parse::<Uuid>()
        .map_err(|_| AppError::Unauthorized("Invalid token".into()))
}
