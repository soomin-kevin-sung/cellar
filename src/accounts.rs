//! Application accounts and cookie-backed sessions.

use std::sync::Arc;

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{app::RequestId, auth::OwnerIdentity, db::Database, error::AppError};

const SESSION_COOKIE: &str = "cellar_session";
const SESSION_SECONDS: i64 = 60 * 60 * 24 * 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    fn parse(value: &str) -> Result<Self, AccountError> {
        match value {
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            _ => Err(AccountError::Corrupt),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUser {
    id: Uuid,
    username: String,
    role: Role,
}

impl SessionUser {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn role(&self) -> Role {
        self.role
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserResponse {
    id: Uuid,
    username: String,
    role: Role,
    active: bool,
    created_at: i64,
}

#[derive(Debug)]
enum AccountError {
    Invalid,
    Unauthorized,
    Conflict,
    NotFound,
    Database,
    Corrupt,
}

#[derive(Clone)]
pub struct AccountService {
    database: Arc<Database>,
    bootstrap_password: Arc<str>,
}

impl AccountService {
    pub fn new(database: Arc<Database>, bootstrap_password: impl Into<Arc<str>>) -> Self {
        Self {
            database,
            bootstrap_password: bootstrap_password.into(),
        }
    }

    async fn login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(SessionUser, String), AccountError> {
        let username = normalize_username(username)?;
        if !valid_password(password) {
            return Err(AccountError::Unauthorized);
        }

        let mut row = self.user_by_username(&username).await?;
        if row.is_none() && username == "cellar" {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_user")
                .fetch_one(self.database.pool())
                .await
                .map_err(|_| AccountError::Database)?;
            if count == 0 && password.as_bytes() == self.bootstrap_password.as_bytes() {
                self.create_user_internal("cellar", password, Role::Admin)
                    .await?;
                row = self.user_by_username("cellar").await?;
            }
        }

        let row = row.ok_or(AccountError::Unauthorized)?;
        let active = row
            .try_get::<i64, _>("active")
            .map_err(|_| AccountError::Corrupt)?
            == 1;
        if !active {
            return Err(AccountError::Unauthorized);
        }
        let password_hash: String = row
            .try_get("password_hash")
            .map_err(|_| AccountError::Corrupt)?;
        verify_password(password.to_owned(), password_hash).await?;
        let user = decode_session_user(&row)?;
        let token = create_token();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query("INSERT INTO app_session (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)")
            .bind(token_hash(&token))
            .bind(user.id.to_string())
            .bind(now)
            .bind(now + SESSION_SECONDS)
            .execute(self.database.pool())
            .await
            .map_err(|_| AccountError::Database)?;
        Ok((user, token))
    }

    async fn authenticate(&self, token: &str) -> Result<SessionUser, AccountError> {
        if token.len() != 43
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(AccountError::Unauthorized);
        }
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let row = sqlx::query(
            "SELECT u.id, u.username, u.role FROM app_session s \
             JOIN app_user u ON u.id = s.user_id \
             WHERE s.token_hash = ? AND s.expires_at > ? AND u.active = 1",
        )
        .bind(token_hash(token))
        .bind(now)
        .fetch_optional(self.database.pool())
        .await
        .map_err(|_| AccountError::Database)?
        .ok_or(AccountError::Unauthorized)?;
        decode_session_user(&row)
    }

    async fn logout(&self, token: &str) -> Result<(), AccountError> {
        sqlx::query("DELETE FROM app_session WHERE token_hash = ?")
            .bind(token_hash(token))
            .execute(self.database.pool())
            .await
            .map_err(|_| AccountError::Database)?;
        Ok(())
    }

    async fn user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<sqlx::sqlite::SqliteRow>, AccountError> {
        sqlx::query("SELECT id, username, password_hash, role, active FROM app_user WHERE username = ? COLLATE NOCASE")
            .bind(username)
            .fetch_optional(self.database.pool())
            .await
            .map_err(|_| AccountError::Database)
    }

    async fn create_user_internal(
        &self,
        username: &str,
        password: &str,
        role: Role,
    ) -> Result<(), AccountError> {
        let username = normalize_username(username)?;
        if !valid_password(password) {
            return Err(AccountError::Invalid);
        }
        let hash = hash_password(password.to_owned()).await?;
        let result = sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role, active, created_at) VALUES (?, ?, ?, ?, 1, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(username)
        .bind(hash)
        .bind(role.as_str())
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .execute(self.database.pool())
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error)
                if error
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation) =>
            {
                Err(AccountError::Conflict)
            }
            Err(_) => Err(AccountError::Database),
        }
    }

    async fn list_users(&self) -> Result<Vec<UserResponse>, AccountError> {
        sqlx::query("SELECT id, username, role, active, created_at FROM app_user ORDER BY username COLLATE NOCASE")
            .fetch_all(self.database.pool())
            .await
            .map_err(|_| AccountError::Database)?
            .into_iter()
            .map(|row| {
                Ok(UserResponse {
                    id: Uuid::parse_str(&row.try_get::<String, _>("id").map_err(|_| AccountError::Corrupt)?).map_err(|_| AccountError::Corrupt)?,
                    username: row.try_get("username").map_err(|_| AccountError::Corrupt)?,
                    role: Role::parse(&row.try_get::<String, _>("role").map_err(|_| AccountError::Corrupt)?)?,
                    active: row.try_get::<i64, _>("active").map_err(|_| AccountError::Corrupt)? == 1,
                    created_at: row.try_get("created_at").map_err(|_| AccountError::Corrupt)?,
                })
            })
            .collect()
    }

    async fn update_user(
        &self,
        actor: &SessionUser,
        id: Uuid,
        request: UpdateUserRequest,
    ) -> Result<(), AccountError> {
        if actor.id == id && (request.active == Some(false) || request.role == Some(Role::Member)) {
            return Err(AccountError::Invalid);
        }
        if request.active.is_none() && request.role.is_none() && request.password.is_none() {
            return Err(AccountError::Invalid);
        }
        if let Some(password) = request.password {
            if !valid_password(&password) {
                return Err(AccountError::Invalid);
            }
            let hash = hash_password(password).await?;
            let changed = sqlx::query("UPDATE app_user SET password_hash = ? WHERE id = ?")
                .bind(hash)
                .bind(id.to_string())
                .execute(self.database.pool())
                .await
                .map_err(|_| AccountError::Database)?
                .rows_affected();
            if changed == 0 {
                return Err(AccountError::NotFound);
            }
            sqlx::query("DELETE FROM app_session WHERE user_id = ? AND user_id != ?")
                .bind(id.to_string())
                .bind(actor.id.to_string())
                .execute(self.database.pool())
                .await
                .map_err(|_| AccountError::Database)?;
        }
        if let Some(role) = request.role {
            let changed = sqlx::query("UPDATE app_user SET role = ? WHERE id = ?")
                .bind(role.as_str())
                .bind(id.to_string())
                .execute(self.database.pool())
                .await
                .map_err(|_| AccountError::Database)?
                .rows_affected();
            if changed == 0 {
                return Err(AccountError::NotFound);
            }
        }
        if let Some(active) = request.active {
            let changed = sqlx::query("UPDATE app_user SET active = ? WHERE id = ?")
                .bind(i64::from(active))
                .bind(id.to_string())
                .execute(self.database.pool())
                .await
                .map_err(|_| AccountError::Database)?
                .rows_affected();
            if changed == 0 {
                return Err(AccountError::NotFound);
            }
            if !active {
                sqlx::query("DELETE FROM app_session WHERE user_id = ?")
                    .bind(id.to_string())
                    .execute(self.database.pool())
                    .await
                    .map_err(|_| AccountError::Database)?;
            }
        }
        Ok(())
    }

    async fn delete_user(&self, actor: &SessionUser, id: Uuid) -> Result<(), AccountError> {
        if actor.id == id {
            return Err(AccountError::Invalid);
        }

        let mut transaction = self
            .database
            .pool()
            .begin()
            .await
            .map_err(|_| AccountError::Database)?;
        sqlx::query("DELETE FROM app_session WHERE user_id = ?")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|_| AccountError::Database)?;
        let changed = sqlx::query("DELETE FROM app_user WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|_| AccountError::Database)?
            .rows_affected();
        if changed == 0 {
            return Err(AccountError::NotFound);
        }
        transaction
            .commit()
            .await
            .map_err(|_| AccountError::Database)?;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUserRequest {
    username: String,
    password: String,
    role: Role,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateUserRequest {
    active: Option<bool>,
    role: Option<Role>,
    password: Option<String>,
}

pub fn public_router(service: AccountService) -> Router {
    Router::new()
        .route("/api/v1/auth/login", post(login))
        .with_state(service)
}

pub fn protected_router(service: AccountService) -> Router {
    Router::new()
        .route("/api/v1/auth/me", get(me))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/admin/users", get(list_users).post(create_user))
        .route("/api/v1/admin/users/{id}", patch(update_user).delete(delete_user))
        .with_state(service)
}

pub async fn session_middleware(
    State(service): State<AccountService>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .expect("request IDs run before sessions");
    let Some(token) = session_token(request.headers()) else {
        return AppError::unauthorized(request_id).into_response();
    };
    match service.authenticate(&token).await {
        Ok(user) => {
            let owner = OwnerIdentity::try_from_email(&format!("{}@cellar.local", user.username()))
                .expect("validated usernames form valid local identities");
            request.extensions_mut().insert(owner);
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        Err(AccountError::Database) => AppError::service_unavailable(
            request_id,
            "authentication_unavailable",
            "Authentication is temporarily unavailable.",
        )
        .into_response(),
        Err(_) => AppError::unauthorized(request_id).into_response(),
    }
}

async fn login(
    State(service): State<AccountService>,
    Extension(request_id): Extension<RequestId>,
    payload: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::bad_request(
            request_id.clone(),
            "invalid_request",
            "The request is invalid.",
        )
    })?;
    match service.login(&payload.username, &payload.password).await {
        Ok((user, token)) => {
            let mut response = Json(user).into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, session_cookie(&token));
            Ok(response)
        }
        Err(AccountError::Database) => Err(AppError::service_unavailable(
            request_id,
            "authentication_unavailable",
            "Authentication is temporarily unavailable.",
        )),
        Err(_) => Err(AppError::unauthorized(request_id)),
    }
}

async fn me(Extension(user): Extension<SessionUser>) -> Json<SessionUser> {
    Json(user)
}

async fn logout(
    State(service): State<AccountService>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(token) = session_token(&headers) {
        service.logout(&token).await.map_err(|_| {
            AppError::service_unavailable(
                request_id,
                "logout_failed",
                "Logout is temporarily unavailable.",
            )
        })?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_session_cookie());
    Ok(response)
}

fn require_admin(user: &SessionUser, request_id: RequestId) -> Result<(), AppError> {
    if user.role == Role::Admin {
        Ok(())
    } else {
        Err(AppError::forbidden(request_id))
    }
}

async fn list_users(
    State(service): State<AccountService>,
    Extension(request_id): Extension<RequestId>,
    Extension(user): Extension<SessionUser>,
) -> Result<Json<Vec<UserResponse>>, AppError> {
    require_admin(&user, request_id.clone())?;
    service.list_users().await.map(Json).map_err(|_| {
        AppError::service_unavailable(
            request_id,
            "user_list_failed",
            "Users are temporarily unavailable.",
        )
    })
}

async fn create_user(
    State(service): State<AccountService>,
    Extension(request_id): Extension<RequestId>,
    Extension(user): Extension<SessionUser>,
    payload: Result<Json<CreateUserRequest>, JsonRejection>,
) -> Result<StatusCode, AppError> {
    require_admin(&user, request_id.clone())?;
    let Json(payload) = payload.map_err(|_| {
        AppError::bad_request(
            request_id.clone(),
            "invalid_request",
            "The request is invalid.",
        )
    })?;
    match service
        .create_user_internal(&payload.username, &payload.password, payload.role)
        .await
    {
        Ok(()) => Ok(StatusCode::CREATED),
        Err(AccountError::Conflict) => Err(AppError::conflict(
            request_id,
            "username_exists",
            "That username already exists.",
            None,
        )),
        Err(AccountError::Invalid) => Err(AppError::bad_request(
            request_id,
            "invalid_user",
            "The user details are invalid.",
        )),
        Err(_) => Err(AppError::service_unavailable(
            request_id,
            "user_create_failed",
            "The user could not be created.",
        )),
    }
}

async fn update_user(
    State(service): State<AccountService>,
    Path(id): Path<Uuid>,
    Extension(request_id): Extension<RequestId>,
    Extension(user): Extension<SessionUser>,
    payload: Result<Json<UpdateUserRequest>, JsonRejection>,
) -> Result<StatusCode, AppError> {
    require_admin(&user, request_id.clone())?;
    let Json(payload) = payload.map_err(|_| {
        AppError::bad_request(
            request_id.clone(),
            "invalid_request",
            "The request is invalid.",
        )
    })?;
    match service.update_user(&user, id, payload).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(AccountError::Invalid) => Err(AppError::bad_request(
            request_id,
            "invalid_user_update",
            "That account change is not allowed.",
        )),
        Err(AccountError::NotFound) => Err(AppError::not_found(
            request_id,
            "user_not_found",
            "The user was not found.",
        )),
        Err(_) => Err(AppError::service_unavailable(
            request_id,
            "user_update_failed",
            "The user could not be updated.",
        )),
    }
}

async fn delete_user(
    State(service): State<AccountService>,
    Path(id): Path<Uuid>,
    Extension(request_id): Extension<RequestId>,
    Extension(user): Extension<SessionUser>,
) -> Result<StatusCode, AppError> {
    require_admin(&user, request_id.clone())?;
    match service.delete_user(&user, id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(AccountError::Invalid) => Err(AppError::bad_request(
            request_id,
            "invalid_user_delete",
            "Your own account cannot be deleted.",
        )),
        Err(AccountError::NotFound) => Err(AppError::not_found(
            request_id,
            "user_not_found",
            "The user was not found.",
        )),
        Err(_) => Err(AppError::service_unavailable(
            request_id,
            "user_delete_failed",
            "The user could not be deleted.",
        )),
    }
}

fn normalize_username(value: &str) -> Result<String, AccountError> {
    let value = value.trim().to_ascii_lowercase();
    if (3..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Ok(value)
    } else {
        Err(AccountError::Invalid)
    }
}

fn valid_password(value: &str) -> bool {
    (10..=128).contains(&value.chars().count()) && !value.chars().any(char::is_control)
}

async fn hash_password(password: String) -> Result<String, AccountError> {
    tokio::task::spawn_blocking(move || {
        let mut salt = [0_u8; 16];
        getrandom::fill(&mut salt).map_err(|_| AccountError::Database)?;
        let salt = SaltString::encode_b64(&salt).map_err(|_| AccountError::Database)?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| AccountError::Database)
    })
    .await
    .map_err(|_| AccountError::Database)?
}

async fn verify_password(password: String, hash: String) -> Result<(), AccountError> {
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&hash).map_err(|_| AccountError::Corrupt)?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AccountError::Unauthorized)
    })
    .await
    .map_err(|_| AccountError::Database)?
}

fn create_token() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system random source is available");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn decode_session_user(row: &sqlx::sqlite::SqliteRow) -> Result<SessionUser, AccountError> {
    Ok(SessionUser {
        id: Uuid::parse_str(
            &row.try_get::<String, _>("id")
                .map_err(|_| AccountError::Corrupt)?,
        )
        .map_err(|_| AccountError::Corrupt)?,
        username: row.try_get("username").map_err(|_| AccountError::Corrupt)?,
        role: Role::parse(
            &row.try_get::<String, _>("role")
                .map_err(|_| AccountError::Corrupt)?,
        )?,
    })
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let value = header.to_str().ok()?;
        for pair in value.split(';') {
            if let Some(token) = pair.trim().strip_prefix("cellar_session=") {
                if found.is_some() {
                    return None;
                }
                found = Some(token.to_owned());
            }
        }
    }
    found
}

fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={SESSION_SECONDS}"))
        .expect("generated session cookies are valid")
}

fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "cellar_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{AccountError, AccountService, Role, UpdateUserRequest};
    use crate::db::Database;

    #[tokio::test]
    async fn bootstraps_admin_and_manages_a_member_account() {
        let temporary = tempfile::tempdir().unwrap();
        let database = Arc::new(
            Database::open(temporary.path().join("cellar.db"))
                .await
                .unwrap(),
        );
        let service = AccountService::new(database, "bootstrap-password");

        let (admin, _) = service.login("cellar", "bootstrap-password").await.unwrap();
        assert_eq!(admin.role(), Role::Admin);

        service
            .create_user_internal("member.one", "member-password", Role::Member)
            .await
            .unwrap();
        let (member, _) = service
            .login("member.one", "member-password")
            .await
            .unwrap();
        assert_eq!(member.role(), Role::Member);
        assert_eq!(service.list_users().await.unwrap().len(), 2);

        service
            .update_user(
                &admin,
                member.id(),
                UpdateUserRequest {
                    active: Some(false),
                    role: None,
                    password: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            service.login("member.one", "member-password").await,
            Err(AccountError::Unauthorized)
        ));
        assert!(matches!(
            service.delete_user(&admin, admin.id()).await,
            Err(AccountError::Invalid)
        ));
        service.delete_user(&admin, member.id()).await.unwrap();
        assert_eq!(service.list_users().await.unwrap().len(), 1);
    }
}
