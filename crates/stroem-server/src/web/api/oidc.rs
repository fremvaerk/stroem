use crate::auth::{create_access_token, generate_refresh_token};
use crate::oidc::{create_state_jwt, provision_user, validate_state_jwt, OidcStateClaims};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use chrono::{Duration, Utc};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient};
use openidconnect::{
    AuthorizationCode, CsrfToken, Nonce, PkceCodeChallenge, PkceCodeVerifier, Scope, TokenResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use stroem_db::{RefreshTokenRepo, UserRepo};

const STATE_COOKIE_NAME: &str = "stroem_oidc_state";

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// GET /api/auth/oidc/{provider} — Initiate OIDC login flow
#[tracing::instrument(skip(state, jar))]
pub async fn oidc_start(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Path(provider_name): Path<String>,
) -> Response {
    let auth_config = match &state.config.auth {
        Some(cfg) => cfg,
        None => {
            return (
                StatusCode::NOT_FOUND,
                "Authentication not configured".to_string(),
            )
                .into_response()
        }
    };

    let provider = match state.oidc_providers.get(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("Unknown OIDC provider: {}", provider_name),
            )
                .into_response()
        }
    };

    // Build client with redirect URI
    let client = CoreClient::from_provider_metadata(
        provider.metadata.clone(),
        provider.client_id.clone(),
        Some(provider.client_secret.clone()),
    )
    .set_redirect_uri(provider.redirect_url.clone());

    // Generate PKCE challenge
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Build authorization URL
    let (auth_url, csrf_token, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    // Create state JWT (stored in cookie)
    let state_claims = OidcStateClaims {
        state: csrf_token.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: pkce_verifier.secret().clone(),
        provider: provider_name,
        exp: Utc::now().timestamp() + 600, // 10 minutes
    };

    let state_jwt = match create_state_jwt(&state_claims, &auth_config.jwt_secret) {
        Ok(jwt) => jwt,
        Err(e) => {
            tracing::error!("Failed to create state JWT: {}", e);
            return error_redirect("Internal server error");
        }
    };

    // Set state cookie (HttpOnly, SameSite=Lax, 10min)
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600",
        STATE_COOKIE_NAME, state_jwt
    );

    let jar = jar.add(axum_extra::extract::cookie::Cookie::parse(cookie).unwrap());

    (jar, Redirect::to(auth_url.as_str())).into_response()
}

/// GET /api/auth/oidc/{provider}/callback — Handle OIDC callback
#[tracing::instrument(skip(state, jar, query))]
pub async fn oidc_callback(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Path(provider_name): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    // Handle error from IdP
    if let Some(error) = &query.error {
        let msg = query.error_description.as_deref().unwrap_or(error.as_str());
        return error_redirect(msg);
    }

    let auth_config = match &state.config.auth {
        Some(cfg) => cfg,
        None => return error_redirect("Authentication not configured"),
    };

    // Read and validate state cookie
    let state_jwt = match jar.get(STATE_COOKIE_NAME) {
        Some(cookie) => cookie.value().to_string(),
        None => return error_redirect("Missing OIDC state cookie"),
    };

    let state_claims = match validate_state_jwt(&state_jwt, &auth_config.jwt_secret) {
        Ok(c) => c,
        Err(_) => return error_redirect("Invalid or expired OIDC state"),
    };

    // Verify provider matches
    if state_claims.provider != provider_name {
        return error_redirect("OIDC provider mismatch");
    }

    // Verify state parameter
    let query_state = match &query.state {
        Some(s) => s,
        None => return error_redirect("Missing state parameter"),
    };
    if query_state != &state_claims.state {
        return error_redirect("OIDC state mismatch");
    }

    // Get authorization code
    let code = match &query.code {
        Some(c) => c,
        None => return error_redirect("Missing authorization code"),
    };

    let provider = match state.oidc_providers.get(&provider_name) {
        Some(p) => p,
        None => return error_redirect("Unknown OIDC provider"),
    };

    // Build client with redirect URI
    let client = CoreClient::from_provider_metadata(
        provider.metadata.clone(),
        provider.client_id.clone(),
        Some(provider.client_secret.clone()),
    )
    .set_redirect_uri(provider.redirect_url.clone());

    let http_client = match openidconnect::reqwest::ClientBuilder::new()
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to build HTTP client: {}", e);
            return error_redirect("Internal server error");
        }
    };

    // Exchange code for tokens
    let pkce_verifier = PkceCodeVerifier::new(state_claims.pkce_verifier);
    let token_response = match client.exchange_code(AuthorizationCode::new(code.clone())) {
        Ok(req) => match req
            .set_pkce_verifier(pkce_verifier)
            .request_async(&http_client)
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                tracing::error!("Token exchange failed: {}", e);
                return error_redirect("Token exchange failed");
            }
        },
        Err(e) => {
            tracing::error!("Failed to build token exchange request: {}", e);
            return error_redirect("Token exchange configuration error");
        }
    };

    // Validate ID token
    let id_token = match token_response.id_token() {
        Some(t) => t,
        None => return error_redirect("No ID token in response"),
    };

    let nonce = Nonce::new(state_claims.nonce);
    let id_token_verifier = client.id_token_verifier();
    let claims = match id_token.claims(&id_token_verifier, &nonce) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("ID token validation failed: {}", e);
            return error_redirect("ID token validation failed");
        }
    };

    // Extract user info from claims
    let subject = claims.subject().as_str();
    let email = claims
        .email()
        .map(|e| e.as_str().to_string())
        .unwrap_or_default();
    let name = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.as_str().to_string());

    if email.is_empty() {
        return error_redirect("No email in ID token claims");
    }

    // JIT user provisioning
    let user = match provision_user(
        &state.pool,
        &provider_name,
        subject,
        &email,
        name.as_deref(),
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("User provisioning failed: {}", e);
            return error_redirect("User provisioning failed");
        }
    };

    if let Err(e) = UserRepo::touch_last_login(&state.pool, user.user_id).await {
        tracing::warn!("Failed to update last_login_at: {}", e);
    }

    // Issue internal JWT tokens
    let access_token = match create_access_token(
        &user.user_id.to_string(),
        &user.email,
        &auth_config.jwt_secret,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to create access token: {}", e);
            return error_redirect("Internal server error");
        }
    };

    let (raw_refresh, refresh_hash) = generate_refresh_token();
    let expires_at = Utc::now() + Duration::days(30);

    if let Err(e) =
        RefreshTokenRepo::create(&state.pool, &refresh_hash, user.user_id, expires_at).await
    {
        tracing::error!("Failed to store refresh token: {}", e);
        return error_redirect("Internal server error");
    }

    // Clear state cookie
    let clear_cookie = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        STATE_COOKIE_NAME
    );

    // Redirect to frontend callback with tokens in hash fragment
    let redirect_url = format!(
        "/login/callback#access_token={}&refresh_token={}",
        url::form_urlencoded::byte_serialize(access_token.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(raw_refresh.as_bytes()).collect::<String>()
    );

    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, &redirect_url)
        .header(header::SET_COOKIE, clear_cookie)
        .body(axum::body::Body::empty())
        .unwrap()
        .into_response()
}

/// Redirect to /login/callback with error in hash fragment
fn error_redirect(message: &str) -> Response {
    let redirect_url = format!(
        "/login/callback#error={}",
        url::form_urlencoded::byte_serialize(message.as_bytes()).collect::<String>()
    );

    Redirect::to(&redirect_url).into_response()
}
