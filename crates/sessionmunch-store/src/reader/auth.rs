//! Users, login sessions and API credentials.

use super::*;

impl ReaderPool {
    /// Hot path for Bearer auth: native `api_credentials` where
    /// `revoked_at IS NULL`. Does not filter `users.disabled_at`.
    /// Always authenticates as [`sessionmunch_core::AuthLevel::User`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error. Returns `Ok(None)` when no
    /// active credential matches.
    pub async fn find_active_user_by_token_hash(
        &self,
        token_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<Option<crate::AuthenticatedApiUser>> {
        let now = now_us();
        self.with_conn(move |conn| {
            crate::api_credentials::find_active_user_by_token_hash(conn, &token_hash, now)
        })
        .await
    }

    /// Look up a user by exact-match username. Used by admin endpoints
    /// that accept username on the wire.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_user_by_username(&self, username: String) -> StoreResult<Option<User>> {
        self.with_conn(move |conn| crate::users::find_user_by_username(conn, &username))
            .await
    }

    /// Login lookup including the stored Argon2id PHC.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_login_user_by_username(
        &self,
        username: String,
    ) -> StoreResult<Option<crate::LoginUser>> {
        self.with_conn(move |conn| crate::users::find_login_user_by_username(conn, &username))
            .await
    }

    /// Live (unrevoked, unexpired) web session by secret hash.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_live_session_by_hash(
        &self,
        session_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<Option<crate::LiveWebSession>> {
        let now = now_us();
        self.with_conn(move |conn| {
            crate::web_sessions::find_live_session_by_hash(conn, &session_hash, now)
        })
        .await
    }

    /// Look up a user by id, including disabled and password-less rows.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_user_by_id(&self, id: UserId) -> StoreResult<Option<User>> {
        self.with_conn(move |conn| crate::users::find_user_by_id(conn, id))
            .await
    }

    /// All registered users, ordered by `created_at` ascending.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_users(&self) -> StoreResult<Vec<User>> {
        self.with_conn(crate::users::list_users).await
    }

    /// Whether bootstrap has been marked complete.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn bootstrap_completed(&self) -> StoreResult<bool> {
        self.with_conn(crate::users::bootstrap_completed).await
    }

    /// Whether any user has a password hash.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn any_password_hash(&self) -> StoreResult<bool> {
        self.with_conn(crate::users::any_password_hash).await
    }

    /// Recoverable-root count for startup validation.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn count_recoverable_roots(&self) -> StoreResult<i64> {
        self.with_conn(crate::users::count_recoverable_roots).await
    }

    /// Whether any native API credential exists (pepper required).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn api_credentials_exist(&self) -> StoreResult<bool> {
        self.with_conn(crate::api_credentials::api_credentials_exist)
            .await
    }

    /// True when any credential (active or revoked) stores this hash.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn token_hash_exists(&self, token_hash: [u8; TOKEN_HASH_LEN]) -> StoreResult<bool> {
        self.with_conn(move |conn| crate::api_credentials::token_hash_exists(conn, &token_hash))
            .await
    }

    /// List native API credentials, newest first.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_api_credentials(&self) -> StoreResult<Vec<sessionmunch_core::ApiCredential>> {
        self.with_conn(crate::api_credentials::list_api_credentials)
            .await
    }

    /// List credentials for one user.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_api_credentials_for_user(
        &self,
        user_id: UserId,
    ) -> StoreResult<Vec<sessionmunch_core::ApiCredential>> {
        self.with_conn(move |conn| {
            crate::api_credentials::list_api_credentials_for_user(conn, user_id)
        })
        .await
    }

    /// Look up one credential by id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_api_credential(
        &self,
        id: sessionmunch_core::ApiCredentialId,
    ) -> StoreResult<Option<sessionmunch_core::ApiCredential>> {
        self.with_conn(move |conn| crate::api_credentials::find_api_credential(conn, id))
            .await
    }

    /// Return whether any user row exists, including expired users. This cheap
    /// predicate is used at the admin authorization boundary to switch from
    /// bootstrap compatibility mode to root-only multi-user administration.
    ///
    /// # Errors
    /// Propagates any SQL or pool error so callers can fail closed.
    pub async fn users_exist(&self) -> StoreResult<bool> {
        self.with_conn(crate::users::users_exist).await
    }

    /// Does this deployment tell its operators apart?
    ///
    /// Two independent routes produce distinct operators: rows in `users`, and
    /// usernames asserted by a trusted authenticating proxy. Only the first is
    /// visible from the store, so the caller passes the second in — a
    /// proxy-only deployment reports `users_exist() == false` forever.
    ///
    /// One notion, several gates: the admin authorization boundaries and the
    /// per-author bucketing of pending auto-improve proposals all key on this,
    /// so a single-operator server keeps the exact behaviour it had before
    /// either route existed.
    ///
    /// # Errors
    /// Propagates any SQL or pool error so callers can fail closed.
    pub async fn distinguishes_operators(&self, trusted_proxy_identity: bool) -> StoreResult<bool> {
        // Short-circuits the DB round-trip when the static config bit already
        // settles it. The users table is otherwise consulted per call rather
        // than cached, so committing a first user tightens access without a
        // restart.
        if trusted_proxy_identity {
            return Ok(true);
        }
        self.users_exist().await
    }
}
