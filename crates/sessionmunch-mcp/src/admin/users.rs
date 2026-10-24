//! Admin routes: user management (root-only).

use super::*;

/// JSON request body for `POST /admin/users`.
#[derive(Debug, Deserialize)]
pub(super) struct CreateUserRequest {
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    role: Option<sessionmunch_core::UserRole>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PatchUserRequest {
    #[serde(default)]
    name: Option<Option<String>>,
    #[serde(default)]
    email: Option<Option<String>>,
    #[serde(default)]
    role: Option<sessionmunch_core::UserRole>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateApiCredentialRequest {
    username: String,
    label: String,
}

#[derive(Debug, Serialize)]
pub(super) struct UserWithTokenResponse {
    user: sessionmunch_core::User,
    token: String,
}

#[derive(Debug, Serialize)]
pub(super) struct UserWithPasswordResponse {
    user: sessionmunch_core::User,
    temporary_password: String,
}

#[derive(Debug, Serialize)]
pub(super) struct UserResponse {
    user: sessionmunch_core::User,
}

#[derive(Debug, Serialize)]
pub(super) struct UserListResponse {
    users: Vec<sessionmunch_core::User>,
}

#[derive(Debug, Serialize)]
pub(super) struct ApiCredentialWithTokenResponse {
    credential: sessionmunch_core::ApiCredential,
    token: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ApiCredentialListResponse {
    credentials: Vec<sessionmunch_core::ApiCredential>,
}

pub(super) fn require_root(
    level: sessionmunch_core::AuthLevel,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    level
        .authorize(sessionmunch_core::Capability::UserManagement, true)
        .map_err(|e| {
            let status = if e.is_authentication_required() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            (status, Json(serde_json::json!({ "error": e.message() })))
        })
}

pub(super) fn require_pepper(
    state: &AdminState,
) -> Result<&sessionmunch_store::TokenPepper, (StatusCode, Json<serde_json::Value>)> {
    state.token_pepper.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "multi-user not enabled (set [auth].token_pepper in config or run `sessionmunch init`)"
            })),
        )
    })
}

pub(super) fn reserved_password(
    auth: Option<axum::Extension<std::sync::Arc<crate::auth::AuthState>>>,
    password: &str,
) -> bool {
    auth.is_some_and(|axum::Extension(state)| {
        crate::human_auth::password_is_reserved(&state, password)
    })
}

pub(super) async fn hash_temp_password() -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let raw = sessionmunch_store::password::generate_temporary_password()
        .map_err(|e| internal_err(e.to_string()))?;
    sessionmunch_core::validate_human_password(&raw, None, &[])
        .map_err(|e| validation_error(e.to_string()))?;
    Ok(raw)
}

pub(super) async fn mint_temporary_password(
    state: &AdminState,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    for _ in 0..8 {
        let raw = hash_temp_password().await?;
        if reserved_password(auth.clone(), &raw) {
            continue;
        }
        if let Some(pepper) = state.token_pepper.as_ref() {
            let hash = sessionmunch_store::hash_token(&raw, pepper);
            match state.reader.token_hash_exists(hash).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return Err(internal_err(e.to_string())),
            }
        }
        return Ok(raw);
    }
    Err(internal_err(
        "could not mint a unique temporary password".to_string(),
    ))
}

pub(super) async fn create_human_user_response(
    state: &AdminState,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    req: CreateUserRequest,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let mut new_user = sessionmunch_core::NewUser {
        username: req.username,
        name: req.name,
        email: req.email,
    };
    new_user
        .validate()
        .map_err(|e| validation_error(e.to_string()))?;
    let role = req.role.unwrap_or(sessionmunch_core::UserRole::User);
    let raw = mint_temporary_password(state, auth).await?;
    let phc = sessionmunch_store::password::hash_password(raw.clone())
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let user_id = state
        .writer
        .create_human_user(new_user, role, Some(phc), true)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("created user vanished from store".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(UserWithPasswordResponse {
                user,
                temporary_password: raw,
            })
            .unwrap_or_default(),
        ),
    ))
}

pub(super) async fn handle_create_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    if req.role.is_some() {
        return create_human_user_response(&state, auth, req).await;
    }
    let pepper = require_pepper(&state)?;
    let mut new_user = sessionmunch_core::NewUser {
        username: req.username,
        name: req.name,
        email: req.email,
    };
    new_user
        .validate()
        .map_err(|e| validation_error(e.to_string()))?;
    let token = sessionmunch_store::generate_token().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = sessionmunch_store::hash_token(&token, pepper);
    let user_id = state
        .writer
        .create_user(new_user, token_hash)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("created user vanished from store".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserWithTokenResponse { user, token }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_create_human_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    create_human_user_response(&state, auth, req).await
}

pub(super) async fn handle_list_users(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let users = state
        .reader
        .list_users()
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserListResponse { users }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_expire_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .expire_user_token(user.id)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after token expiry".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_revive_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .revive_user_token(user.id)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after token revival".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_rotate_user_token(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let pepper = require_pepper(&state)?;
    let user = lookup_user_by_username(&state, &username).await?;
    let token = sessionmunch_store::generate_token().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = sessionmunch_store::hash_token(&token, pepper);
    let updated = state
        .writer
        .rotate_user_token(user.id, token_hash)
        .await
        .map_err(map_user_store_err)?;
    if !updated {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "user vanished mid-rotation" })),
        ));
    }
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after token rotation".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserWithTokenResponse { user, token }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_patch_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
    Json(req): Json<PatchUserRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .patch_user(user.id, req.name, req.email, req.role)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after patch".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_reset_password(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    let raw = mint_temporary_password(&state, auth).await?;
    let phc = sessionmunch_store::password::hash_password(raw.clone())
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    state
        .writer
        .reset_human_password(user.id, phc, true)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after reset".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(UserWithPasswordResponse {
                user,
                temporary_password: raw,
            })
            .unwrap_or_default(),
        ),
    ))
}

pub(super) async fn handle_disable_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .set_user_disabled(user.id, true)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after disable".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_enable_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .set_user_disabled(user.id, false)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after enable".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_create_api_credential(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    Json(req): Json<CreateApiCredentialRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let pepper = require_pepper(&state)?;
    let user = lookup_user_by_username(&state, &req.username).await?;
    let token = sessionmunch_store::generate_api_key().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = sessionmunch_store::hash_token(&token, pepper);
    let preview = sessionmunch_store::api_key_preview(&token);
    let id = state
        .writer
        .create_api_credential(
            sessionmunch_core::ApiCredentialId::new(),
            user.id,
            req.label,
            token_hash,
            Some(preview),
        )
        .await
        .map_err(map_user_store_err)?;
    let credential = state
        .reader
        .find_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("credential vanished after create".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(ApiCredentialWithTokenResponse { credential, token })
                .unwrap_or_default(),
        ),
    ))
}

pub(super) async fn handle_list_api_credentials(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let credentials = state
        .reader
        .list_api_credentials()
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(ApiCredentialListResponse { credentials }).unwrap_or_default()),
    ))
}

pub(super) async fn handle_rotate_api_credential(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let pepper = require_pepper(&state)?;
    let id: sessionmunch_core::ApiCredentialId = id
        .parse()
        .map_err(|e: sessionmunch_core::MemoryError| validation_error(e.to_string()))?;
    let token = sessionmunch_store::generate_api_key().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = sessionmunch_store::hash_token(&token, pepper);
    let preview = sessionmunch_store::api_key_preview(&token);
    let updated = state
        .writer
        .rotate_api_credential(id, token_hash, Some(preview))
        .await
        .map_err(map_user_store_err)?;
    if !updated {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such credential" })),
        ));
    }
    let credential = state
        .reader
        .find_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("credential vanished after rotate".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(ApiCredentialWithTokenResponse { credential, token })
                .unwrap_or_default(),
        ),
    ))
}

pub(super) async fn handle_revoke_api_credential(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<sessionmunch_core::AuthLevel>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let id: sessionmunch_core::ApiCredentialId = id
        .parse()
        .map_err(|e: sessionmunch_core::MemoryError| validation_error(e.to_string()))?;
    state
        .writer
        .revoke_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let credential = state
        .reader
        .find_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "no such credential" })),
            )
        })?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "credential": credential })),
    ))
}

pub(super) async fn lookup_user_by_username(
    state: &AdminState,
    username: &str,
) -> Result<sessionmunch_core::User, (StatusCode, Json<serde_json::Value>)> {
    let found = state
        .reader
        .find_user_by_username(username.to_string())
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    found.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no such user: {username}") })),
        )
    })
}

pub(super) fn validation_error(msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

pub(super) fn map_user_store_err(
    e: sessionmunch_store::StoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        sessionmunch_store::StoreError::Duplicate(msg)
        | sessionmunch_store::StoreError::InvalidState(msg) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": msg })),
        ),
        sessionmunch_store::StoreError::Memory(
            sessionmunch_core::MemoryError::InvalidUsername(msg),
        )
        | sessionmunch_store::StoreError::Memory(sessionmunch_core::MemoryError::InvalidEmail(
            msg,
        ))
        | sessionmunch_store::StoreError::Memory(
            sessionmunch_core::MemoryError::InvalidPassword(msg),
        ) => validation_error(msg),
        other => internal_err(other.to_string()),
    }
}
