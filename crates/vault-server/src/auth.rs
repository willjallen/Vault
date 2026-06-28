use std::collections::{BTreeSet, HashSet};
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use sqlx::{FromRow, Row, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

type HmacSha256 = Hmac<Sha256>;

const IDENTITY_UPSERT_RETRY_DELAYS_MS: [u64; 6] = [5, 10, 20, 40, 80, 160];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Headers,
    Oidc,
    Dev,
}

impl AuthMode {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "oidc" => Self::Oidc,
            "dev" => Self::Dev,
            _ => Self::Headers,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headers => "headers",
            Self::Oidc => "oidc",
            Self::Dev => "dev",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSecretSource {
    Explicit,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSecretRequirement {
    Optional,
    Required,
}

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub mode: AuthMode,
    pub auth_mode_raw: String,
    pub dev_mode: bool,
    pub dev_auth_enabled: bool,
    pub base_domain: String,
    pub public_url: String,
    pub session_secret: String,
    pub session_secret_source: SessionSecretSource,
    pub session_secret_requirement: SessionSecretRequirement,
    pub session_cookie_name: String,
    pub session_cookie_secure: String,
    pub session_max_age_seconds: i64,
    pub header_auth_issuer: String,
    pub dev_auth_issuer: String,
    pub admin_groups: HashSet<String>,
    pub bootstrap_admin_emails: HashSet<String>,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub oidc_scopes: String,
    pub oidc_redirect_uri: String,
    pub oidc_client_auth: String,
    pub oidc_state_cookie_name: String,
    pub oidc_authorization_endpoint: String,
    pub oidc_allow_insecure_http: bool,
    pub oidc_groups_claim: String,
    pub oidc_email_claim: String,
    pub oidc_name_claim: String,
    pub oidc_username_claim: String,
    pub oidc_nonce_bytes: i64,
    pub oidc_discovery_ttl_seconds: i64,
    pub oidc_http_timeout_seconds: f64,
    pub security_headers: SecurityHeaderSettings,
    pub default_user_email: String,
    pub dev_user: String,
    pub dev_name: String,
    pub dev_email: String,
    pub dev_groups: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct SecurityHeaderSettings {
    pub enabled: bool,
    pub content_security_policy: String,
    pub hsts_max_age_seconds: i64,
    pub hsts_include_subdomains: bool,
    pub hsts_preload: bool,
}

#[derive(Debug, Error)]
#[error("Invalid Vault runtime configuration: {}", .errors.join("; "))]
pub struct AuthConfigError {
    errors: Vec<String>,
}

impl Default for SecurityHeaderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            content_security_policy: String::new(),
            hsts_max_age_seconds: 31_536_000,
            hsts_include_subdomains: false,
            hsts_preload: false,
        }
    }
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            mode: AuthMode::Headers,
            auth_mode_raw: "headers".to_string(),
            dev_mode: false,
            dev_auth_enabled: false,
            base_domain: "localhost".to_string(),
            public_url: String::new(),
            session_secret: "dev-insecure-session-secret".to_string(),
            session_secret_source: SessionSecretSource::Fallback,
            session_secret_requirement: SessionSecretRequirement::Optional,
            session_cookie_name: "vault_session".to_string(),
            session_cookie_secure: "auto".to_string(),
            session_max_age_seconds: 604_800,
            header_auth_issuer: "headers".to_string(),
            dev_auth_issuer: "dev".to_string(),
            admin_groups: split_groups_set("admin,vault-admin"),
            bootstrap_admin_emails: HashSet::new(),
            oidc_issuer: String::new(),
            oidc_client_id: String::new(),
            oidc_client_secret: String::new(),
            oidc_scopes: "openid email profile".to_string(),
            oidc_redirect_uri: String::new(),
            oidc_client_auth: "client_secret_basic".to_string(),
            oidc_state_cookie_name: "vault_oidc_state".to_string(),
            oidc_authorization_endpoint: String::new(),
            oidc_allow_insecure_http: false,
            oidc_groups_claim: "groups".to_string(),
            oidc_email_claim: "email".to_string(),
            oidc_name_claim: "name".to_string(),
            oidc_username_claim: "preferred_username".to_string(),
            oidc_nonce_bytes: 24,
            oidc_discovery_ttl_seconds: 3600,
            oidc_http_timeout_seconds: 8.0,
            security_headers: SecurityHeaderSettings::default(),
            default_user_email: "admin@example.com".to_string(),
            dev_user: "local-admin".to_string(),
            dev_name: "Local Admin".to_string(),
            dev_email: "admin@example.com".to_string(),
            dev_groups: split_groups("admin,vault-admin"),
        }
    }
}

impl AuthSettings {
    #[must_use]
    pub fn from_env() -> Self {
        let dev_auth_enabled = env_flag("VAULT_DEV_AUTH");
        let auth_mode = env::var("VAULT_AUTH_MODE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                if dev_auth_enabled {
                    "dev".to_string()
                } else {
                    "headers".to_string()
                }
            });
        let default_user_email = env_string("VAULT_DEFAULT_USER_EMAIL", "admin@example.com");
        let dev_email = env::var("VAULT_DEV_EMAIL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| default_user_email.clone());
        let mode = AuthMode::parse(&auth_mode);
        let (session_secret, session_secret_source) = session_secret_from_env();
        Self {
            mode,
            auth_mode_raw: auth_mode.trim().to_ascii_lowercase(),
            dev_mode: env_flag("VAULT_DEV_MODE") || mode == AuthMode::Dev || dev_auth_enabled,
            dev_auth_enabled,
            base_domain: env_string("BASE_DOMAIN", "localhost"),
            public_url: env_string("VAULT_PUBLIC_URL", ""),
            session_secret,
            session_secret_source,
            session_secret_requirement: require_session_secret_from_env(),
            session_cookie_name: env_string("VAULT_SESSION_COOKIE_NAME", "vault_session"),
            session_cookie_secure: env_string("VAULT_SESSION_COOKIE_SECURE", "auto")
                .to_ascii_lowercase(),
            session_max_age_seconds: env_i64("VAULT_SESSION_MAX_AGE_SECONDS", 604_800),
            header_auth_issuer: env_string("VAULT_HEADER_AUTH_ISSUER", "headers"),
            dev_auth_issuer: env_string("VAULT_DEV_AUTH_ISSUER", "dev"),
            admin_groups: split_groups_set(&env_string("VAULT_ADMIN_GROUPS", "admin,vault-admin")),
            bootstrap_admin_emails: split_groups_set(&env_string(
                "VAULT_BOOTSTRAP_ADMIN_EMAILS",
                "",
            )),
            oidc_issuer: env_string("VAULT_OIDC_ISSUER", "")
                .trim_end_matches('/')
                .to_string(),
            oidc_client_id: env_string("VAULT_OIDC_CLIENT_ID", ""),
            oidc_client_secret: env_string("VAULT_OIDC_CLIENT_SECRET", ""),
            oidc_scopes: env_string("VAULT_OIDC_SCOPES", "openid email profile"),
            oidc_redirect_uri: env_string("VAULT_OIDC_REDIRECT_URI", ""),
            oidc_client_auth: env_string("VAULT_OIDC_CLIENT_AUTH", "client_secret_basic")
                .to_ascii_lowercase(),
            oidc_state_cookie_name: env_string("VAULT_OIDC_STATE_COOKIE_NAME", "vault_oidc_state"),
            oidc_authorization_endpoint: env_string("VAULT_OIDC_AUTHORIZATION_ENDPOINT", ""),
            oidc_allow_insecure_http: env_flag("VAULT_OIDC_ALLOW_INSECURE_HTTP"),
            oidc_groups_claim: env_string("VAULT_OIDC_GROUPS_CLAIM", "groups"),
            oidc_email_claim: env_string("VAULT_OIDC_EMAIL_CLAIM", "email"),
            oidc_name_claim: env_string("VAULT_OIDC_NAME_CLAIM", "name"),
            oidc_username_claim: env_string("VAULT_OIDC_USERNAME_CLAIM", "preferred_username"),
            oidc_nonce_bytes: env_i64("VAULT_OIDC_NONCE_BYTES", 24).max(16),
            oidc_discovery_ttl_seconds: env_i64("VAULT_OIDC_DISCOVERY_TTL_SECONDS", 3600),
            oidc_http_timeout_seconds: env_f64("VAULT_OIDC_HTTP_TIMEOUT_SECONDS", 8.0),
            security_headers: SecurityHeaderSettings {
                enabled: env_flag_default("VAULT_SECURITY_HEADERS_ENABLED", true),
                content_security_policy: env_string("VAULT_CONTENT_SECURITY_POLICY", ""),
                hsts_max_age_seconds: env_i64("VAULT_HSTS_MAX_AGE_SECONDS", 31_536_000).max(0),
                hsts_include_subdomains: env_flag("VAULT_HSTS_INCLUDE_SUBDOMAINS"),
                hsts_preload: env_flag("VAULT_HSTS_PRELOAD"),
            },
            default_user_email,
            dev_user: env_string("VAULT_DEV_USER", "local-admin"),
            dev_name: env_string("VAULT_DEV_NAME", "Local Admin"),
            dev_email,
            dev_groups: split_groups(&env_string("VAULT_DEV_GROUPS", "admin,vault-admin")),
        }
    }

    pub fn validate_runtime_config(&self) -> Result<(), AuthConfigError> {
        let mut errors = Vec::new();
        if !matches!(self.auth_mode_raw.trim(), "headers" | "oidc" | "dev") {
            errors.push("VAULT_AUTH_MODE must be one of dev, headers, oidc".to_string());
        }
        if !valid_cookie_secure_mode(&self.session_cookie_secure) {
            errors.push("VAULT_SESSION_COOKIE_SECURE must be auto, true, or false".to_string());
        }
        if !valid_cookie_name(&self.session_cookie_name) {
            errors.push(
                "VAULT_SESSION_COOKIE_NAME must contain only letters, digits, underscores, hyphens, or dots"
                    .to_string(),
            );
        }
        if !valid_cookie_name(&self.oidc_state_cookie_name) {
            errors.push(
                "VAULT_OIDC_STATE_COOKIE_NAME must contain only letters, digits, underscores, hyphens, or dots"
                    .to_string(),
            );
        }
        if !valid_oidc_client_auth_mode(&self.oidc_client_auth) {
            errors.push(
                "VAULT_OIDC_CLIENT_AUTH must be client_secret_basic, client_secret_post, or none"
                    .to_string(),
            );
        }
        if self.session_secret_requirement == SessionSecretRequirement::Required
            && self.session_secret_source != SessionSecretSource::Explicit
        {
            errors.push(
                "VAULT_SESSION_SECRET is required when VAULT_REQUIRE_SESSION_SECRET=1".to_string(),
            );
        }
        if !self.dev_mode && self.session_secret == development_session_secret() {
            errors.push("VAULT_SESSION_SECRET is required outside development mode".to_string());
        }
        validate_public_url(self, &mut errors);
        if self.mode == AuthMode::Oidc {
            validate_oidc_runtime_config(self, &mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AuthConfigError { errors })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserContext {
    pub id: String,
    pub vault_user_id: i64,
    pub issuer: String,
    pub subject: String,
    pub name: String,
    pub email: String,
    pub groups: Vec<String>,
    pub is_admin: bool,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Authentication required")]
    AuthenticationRequired,
    #[error("User is disabled")]
    UserDisabled,
    #[error("Identity provider did not supply a subject")]
    MissingSubject,
    #[error("Could not sync user identity")]
    IdentitySync,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Time(#[from] time::error::Format),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, FromRow)]
struct VaultUserRecord {
    id: i64,
    issuer: String,
    subject: String,
    email: Option<String>,
    name: String,
    is_admin: i64,
    is_active: i64,
}

pub async fn header_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<UserContext, AuthError> {
    let remote_user = clean_header(headers.get("Remote-User"));
    if remote_user.is_empty() {
        if let Some(user) = dev_identity(settings, pool).await? {
            return Ok(user);
        }
        return Err(AuthError::AuthenticationRequired);
    }

    let groups = split_groups_header(headers.get("Remote-Groups"));
    let email = {
        let value = clean_header(headers.get("Remote-Email"));
        if value.is_empty() {
            settings.default_user_email.clone()
        } else {
            value
        }
    };
    let remote_name = {
        let value = clean_header(headers.get("Remote-Name"));
        if value.is_empty() {
            remote_user.clone()
        } else {
            value
        }
    };

    let user = upsert_vault_user(
        pool,
        &settings.header_auth_issuer,
        &remote_user,
        Some(&email),
        Some(&remote_name),
        Some(&groups),
        false,
    )
    .await?;
    context_for_user(settings, pool, &user).await
}

pub async fn dev_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
) -> Result<Option<UserContext>, AuthError> {
    if !settings.dev_auth_enabled || !dev_auth_allowed_for_domain(&settings.base_domain) {
        return Ok(None);
    }

    let user = upsert_vault_user(
        pool,
        &settings.dev_auth_issuer,
        &settings.dev_user,
        Some(&settings.dev_email),
        Some(&settings.dev_name),
        Some(&settings.dev_groups),
        false,
    )
    .await?;
    Ok(Some(context_for_user(settings, pool, &user).await?))
}

pub async fn session_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
    cookie_header: Option<&str>,
) -> Result<Option<UserContext>, AuthError> {
    let Some(cookie_value) = cookie_value(cookie_header, &settings.session_cookie_name) else {
        return Ok(None);
    };
    let Some(payload) = verify_session_payload(settings, &cookie_value) else {
        return Ok(None);
    };
    let Some(user_id) = payload.get("uid").and_then(value_as_i64) else {
        return Ok(None);
    };
    let Some(user) = fetch_user_by_id(pool, user_id).await? else {
        return Ok(None);
    };
    if user.is_active == 0 {
        return Ok(None);
    }
    sqlx::query("UPDATE vault_users SET last_seen_at = ? WHERE id = ?")
        .bind(now_string()?)
        .bind(user.id)
        .execute(pool)
        .await?;
    context_for_user(settings, pool, &user).await.map(Some)
}

pub async fn oidc_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    groups: &BTreeSet<String>,
) -> Result<UserContext, AuthError> {
    let user = upsert_vault_user(
        pool,
        &settings.oidc_issuer,
        subject,
        email,
        name,
        Some(groups),
        true,
    )
    .await?;
    context_for_user(settings, pool, &user).await
}

pub fn sign_session_payload(
    settings: &AuthSettings,
    payload: &Map<String, Value>,
) -> Result<String, AuthError> {
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload)?);
    let mut mac = HmacSha256::new_from_slice(settings.session_secret.as_bytes())
        .map_err(|_| AuthError::IdentitySync)?;
    mac.update(body.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{body}.{signature}"))
}

pub fn oidc_token_urlsafe(nonce_bytes: i64) -> Result<String, getrandom::Error> {
    let byte_count = usize::try_from(nonce_bytes.max(16)).unwrap_or(16);
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[must_use]
pub fn verify_session_payload(settings: &AuthSettings, value: &str) -> Option<Map<String, Value>> {
    let (body, signature) = value.rsplit_once('.')?;
    if !body.is_ascii() || !signature.is_ascii() {
        return None;
    }
    let signature_bytes = URL_SAFE_NO_PAD.decode(signature.as_bytes()).ok()?;
    let mut mac = HmacSha256::new_from_slice(settings.session_secret.as_bytes()).ok()?;
    mac.update(body.as_bytes());
    mac.verify_slice(&signature_bytes).ok()?;
    let body_bytes = URL_SAFE_NO_PAD.decode(body.as_bytes()).ok()?;
    let Value::Object(payload) = serde_json::from_slice::<Value>(&body_bytes).ok()? else {
        return None;
    };
    let expires_at = payload.get("exp")?;
    let expires_at = value_as_f64(expires_at)?;
    if !expires_at.is_finite() || expires_at < unix_timestamp_now() {
        return None;
    }
    Some(payload)
}

async fn upsert_vault_user(
    pool: &SqlitePool,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    groups: Option<&BTreeSet<String>>,
    mark_login: bool,
) -> Result<VaultUserRecord, AuthError> {
    if issuer.trim().is_empty() || subject.trim().is_empty() {
        return Err(AuthError::MissingSubject);
    }
    for retry_delay_ms in IDENTITY_UPSERT_RETRY_DELAYS_MS
        .into_iter()
        .map(Some)
        .chain(std::iter::once(None))
    {
        match upsert_vault_user_once(pool, issuer, subject, email, name, groups, mark_login).await {
            Ok(user) => return Ok(user),
            Err(error) if retry_delay_ms.is_some() && retryable_identity_upsert_error(&error) => {
                if let Some(delay_ms) = retry_delay_ms {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(AuthError::IdentitySync)
}

async fn upsert_vault_user_once(
    pool: &SqlitePool,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    groups: Option<&BTreeSet<String>>,
    mark_login: bool,
) -> Result<VaultUserRecord, AuthError> {
    // Identity sync is a short canonical write that can be hit by many fresh
    // sessions at once. BEGIN IMMEDIATE makes SQLite queue writers up front
    // instead of failing later during deferred read-to-write promotion.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing = fetch_user_by_identity(&mut tx, issuer, subject).await?;
    let display_name = display_name(name, email, subject);
    let now = now_string()?;

    let user = if let Some(user) = existing {
        if user.is_active == 0 {
            return Err(AuthError::UserDisabled);
        }
        if mark_login {
            sqlx::query(
                "UPDATE vault_users SET email = ?, name = ?, last_seen_at = ?, last_login_at = ? WHERE id = ?",
            )
            .bind(email)
            .bind(&display_name)
            .bind(&now)
            .bind(&now)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE vault_users SET email = ?, name = ?, last_seen_at = ? WHERE id = ?",
            )
            .bind(email)
            .bind(&display_name)
            .bind(&now)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
        }
        fetch_user_by_id_tx(&mut tx, user.id)
            .await?
            .ok_or(AuthError::IdentitySync)?
    } else {
        let result = sqlx::query(
            r"
            INSERT INTO vault_users
                (issuer, subject, email, name, is_admin, is_active, last_login_at, last_seen_at)
            VALUES
                (?, ?, ?, ?, 0, 1, ?, ?)
            ",
        )
        .bind(issuer)
        .bind(subject)
        .bind(email)
        .bind(&display_name)
        .bind(if mark_login { Some(now.as_str()) } else { None })
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        fetch_user_by_id_tx(&mut tx, result.last_insert_rowid())
            .await?
            .ok_or(AuthError::IdentitySync)?
    };

    if let Some(groups) = groups {
        sync_vault_groups(&mut tx, user.id, groups).await?;
    }
    tx.commit().await?;
    fetch_user_by_id(pool, user.id)
        .await?
        .ok_or(AuthError::IdentitySync)
}

fn retryable_identity_upsert_error(error: &AuthError) -> bool {
    let AuthError::Database(sqlx::Error::Database(database_error)) = error else {
        return false;
    };
    database_error.is_unique_violation()
        || database_error.code().is_some_and(|code| {
            matches!(
                code.as_ref(),
                "5" | "6" | "261" | "262" | "517" | "SQLITE_BUSY" | "SQLITE_LOCKED"
            )
        })
        || database_error.message().contains("database is locked")
        || database_error
            .message()
            .contains("database table is locked")
}

async fn sync_vault_groups(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: i64,
    groups: &BTreeSet<String>,
) -> Result<(), AuthError> {
    let mut target_group_ids = BTreeSet::new();
    for group_name in groups {
        if group_name.trim().is_empty() {
            continue;
        }
        let group_id = ensure_group(tx, group_name).await?;
        ensure_group_root_permissions(tx, group_id).await?;
        target_group_ids.insert(group_id);
    }

    let existing_group_ids: Vec<i64> =
        sqlx::query_scalar("SELECT group_id FROM vault_group_memberships WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&mut **tx)
            .await?;
    for group_id in &existing_group_ids {
        if !target_group_ids.contains(group_id) {
            sqlx::query("DELETE FROM vault_group_memberships WHERE user_id = ? AND group_id = ?")
                .bind(user_id)
                .bind(group_id)
                .execute(&mut **tx)
                .await?;
        }
    }
    let existing: BTreeSet<i64> = existing_group_ids.into_iter().collect();
    for group_id in target_group_ids {
        if !existing.contains(&group_id) {
            sqlx::query("INSERT INTO vault_group_memberships (user_id, group_id) VALUES (?, ?)")
                .bind(user_id)
                .bind(group_id)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

async fn ensure_group(
    tx: &mut Transaction<'_, Sqlite>,
    group_name: &str,
) -> Result<i64, AuthError> {
    if let Some(group_id) =
        sqlx::query_scalar::<_, i64>("SELECT id FROM vault_groups WHERE name = ?")
            .bind(group_name)
            .fetch_optional(&mut **tx)
            .await?
    {
        return Ok(group_id);
    }
    let result = sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(group_name)
        .execute(&mut **tx)
        .await?;
    Ok(result.last_insert_rowid())
}

async fn ensure_group_root_permissions(
    tx: &mut Transaction<'_, Sqlite>,
    group_id: i64,
) -> Result<(), AuthError> {
    let root_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM folders WHERE is_root = 1")
        .fetch_all(&mut **tx)
        .await?;
    for root_id in root_ids {
        sqlx::query(
            r"
            INSERT INTO folder_permissions
                (folder_id, group_id, can_view, can_read, can_write)
            SELECT ?, ?, 1, 1, 1
            WHERE NOT EXISTS (
                SELECT 1 FROM folder_permissions WHERE folder_id = ? AND group_id = ?
            )
            ",
        )
        .bind(root_id)
        .bind(group_id)
        .bind(root_id)
        .bind(group_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn context_for_user(
    settings: &AuthSettings,
    pool: &SqlitePool,
    user: &VaultUserRecord,
) -> Result<UserContext, AuthError> {
    let groups = group_names_for_user(pool, user.id).await?;
    let is_admin =
        effective_admin_from_parts(settings, user.is_admin != 0, user.email.as_deref(), &groups);
    Ok(UserContext {
        id: user.id.to_string(),
        vault_user_id: user.id,
        issuer: user.issuer.clone(),
        subject: user.subject.clone(),
        name: user.name.clone(),
        email: user.email.clone().unwrap_or_default(),
        groups,
        is_admin,
    })
}

async fn group_names_for_user(pool: &SqlitePool, user_id: i64) -> Result<Vec<String>, AuthError> {
    Ok(sqlx::query_scalar(
        r"
        SELECT vault_groups.name
        FROM vault_groups
        JOIN vault_group_memberships ON vault_group_memberships.group_id = vault_groups.id
        WHERE vault_group_memberships.user_id = ?
        ORDER BY vault_groups.name
        ",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

#[must_use]
pub fn effective_admin_from_parts(
    settings: &AuthSettings,
    is_stored_admin: bool,
    email: Option<&str>,
    groups: &[String],
) -> bool {
    if is_stored_admin {
        return true;
    }
    let email = email.unwrap_or_default().trim().to_ascii_lowercase();
    settings.bootstrap_admin_emails.contains(&email)
        || groups.iter().any(|group| {
            settings
                .admin_groups
                .contains(&group.trim().to_ascii_lowercase())
        })
}

async fn fetch_user_by_identity(
    tx: &mut Transaction<'_, Sqlite>,
    issuer: &str,
    subject: &str,
) -> Result<Option<VaultUserRecord>, AuthError> {
    Ok(sqlx::query_as::<_, VaultUserRecord>(
        r"
        SELECT id, issuer, subject, email, name, is_admin, is_active
        FROM vault_users
        WHERE issuer = ? AND subject = ?
        ",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn fetch_user_by_id_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: i64,
) -> Result<Option<VaultUserRecord>, AuthError> {
    Ok(sqlx::query_as::<_, VaultUserRecord>(
        "SELECT id, issuer, subject, email, name, is_admin, is_active FROM vault_users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn fetch_user_by_id(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<VaultUserRecord>, AuthError> {
    Ok(sqlx::query_as::<_, VaultUserRecord>(
        "SELECT id, issuer, subject, email, name, is_admin, is_active FROM vault_users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

fn clean_header(value: Option<&axum::http::HeaderValue>) -> String {
    value
        .and_then(|item| item.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn split_groups_header(value: Option<&axum::http::HeaderValue>) -> BTreeSet<String> {
    split_groups(
        value
            .and_then(|item| item.to_str().ok())
            .unwrap_or_default(),
    )
}

#[must_use]
pub fn split_groups(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

fn split_groups_set(value: &str) -> HashSet<String> {
    split_groups(value).into_iter().collect()
}

fn env_string(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_i64(name: &str, default: i64) -> i64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
}

fn env_flag(name: &str) -> bool {
    env_flag_default(name, false)
}

fn env_flag_default(name: &str, default: bool) -> bool {
    env_value_is_truthy(&env::var(name).unwrap_or_else(|_| {
        if default {
            "1".to_string()
        } else {
            "0".to_string()
        }
    }))
}

fn env_value_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn session_secret_from_env() -> (String, SessionSecretSource) {
    let session_secret = env::var("VAULT_SESSION_SECRET")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(value) = session_secret {
        return (value, SessionSecretSource::Explicit);
    }
    let oidc_secret = env::var("VAULT_OIDC_CLIENT_SECRET")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    (
        oidc_secret.unwrap_or_else(development_session_secret),
        SessionSecretSource::Fallback,
    )
}

fn require_session_secret_from_env() -> SessionSecretRequirement {
    let required = match env::var("VAULT_REQUIRE_SESSION_SECRET") {
        Ok(value) => env_value_is_truthy(&value),
        Err(_) => env_flag("VAULT_DOCKER_RUNTIME"),
    };
    if required {
        SessionSecretRequirement::Required
    } else {
        SessionSecretRequirement::Optional
    }
}

fn development_session_secret() -> String {
    "dev-insecure-session-secret".to_string()
}

fn valid_cookie_secure_mode(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "auto" | "1" | "true" | "yes" | "on" | "0" | "false" | "no" | "off"
    )
}

fn valid_cookie_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
}

fn valid_oidc_client_auth_mode(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "client_secret_basic" | "client_secret_post" | "none"
    )
}

fn validate_public_url(settings: &AuthSettings, errors: &mut Vec<String>) {
    let public_url = settings.public_url.trim();
    if public_url.is_empty() {
        return;
    }
    match parse_http_url(public_url) {
        Some(parsed) => {
            if !settings.dev_mode && parsed.scheme != "https" && !is_local_hostname(&parsed.host) {
                errors
                    .push("VAULT_PUBLIC_URL must use https outside local development".to_string());
            }
        }
        None => errors.push("VAULT_PUBLIC_URL must be an absolute http(s) URL".to_string()),
    }
}

fn validate_oidc_runtime_config(settings: &AuthSettings, errors: &mut Vec<String>) {
    if settings.oidc_issuer.trim().is_empty() {
        errors.push("VAULT_OIDC_ISSUER is required when VAULT_AUTH_MODE=oidc".to_string());
    } else if !url_uses_secure_transport(&settings.oidc_issuer, settings.oidc_allow_insecure_http) {
        errors.push("VAULT_OIDC_ISSUER must use https outside local development".to_string());
    }
    if settings.oidc_client_id.trim().is_empty() {
        errors.push("VAULT_OIDC_CLIENT_ID is required when VAULT_AUTH_MODE=oidc".to_string());
    }
    if matches!(
        settings
            .oidc_client_auth
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "client_secret_basic" | "client_secret_post"
    ) && settings.oidc_client_secret.trim().is_empty()
    {
        errors.push(
            "VAULT_OIDC_CLIENT_SECRET is required for confidential OIDC client auth".to_string(),
        );
    }
    let redirect_origin = if settings.oidc_redirect_uri.trim().is_empty() {
        settings.public_url.trim()
    } else {
        settings.oidc_redirect_uri.trim()
    };
    if redirect_origin.is_empty() {
        return;
    }
    match parse_http_url(redirect_origin) {
        Some(parsed) => {
            if !settings.dev_mode && parsed.scheme != "https" && !is_local_hostname(&parsed.host) {
                errors.push(
                    "OIDC redirect/public URL must use https outside local development".to_string(),
                );
            }
        }
        None => errors.push("OIDC redirect/public URL must be an absolute http(s) URL".to_string()),
    }
}

fn url_uses_secure_transport(value: &str, allow_insecure_http: bool) -> bool {
    parse_http_url(value).is_some_and(|parsed| {
        parsed.scheme == "https"
            || (parsed.scheme == "http" && (allow_insecure_http || is_local_hostname(&parsed.host)))
    })
}

struct ParsedHttpUrl {
    scheme: String,
    host: String,
}

fn parse_http_url(value: &str) -> Option<ParsedHttpUrl> {
    let (scheme, rest) = value.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    let host = hostname_from_authority(authority)?;
    Some(ParsedHttpUrl { scheme, host })
}

fn hostname_from_authority(authority: &str) -> Option<String> {
    let host_port = authority.rsplit('@').next()?;
    if let Some(rest) = host_port.strip_prefix('[') {
        return rest
            .split_once(']')
            .map(|(host, _)| host.to_ascii_lowercase());
    }
    host_port
        .split(':')
        .next()
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase)
}

fn is_local_hostname(hostname: &str) -> bool {
    let normalized = hostname
        .trim()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    matches!(normalized.as_str(), "localhost" | "127.0.0.1" | "::1")
        || normalized.ends_with(".localhost")
}

fn display_name(name: Option<&str>, email: Option<&str>, subject: &str) -> String {
    [name, email, Some(subject)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or(subject)
        .to_string()
}

fn dev_auth_allowed_for_domain(base_domain: &str) -> bool {
    let normalized = base_domain.trim().to_ascii_lowercase();
    matches!(normalized.as_str(), "localhost" | "127.0.0.1" | "::1")
        || normalized.ends_with(".localhost")
}

#[must_use]
pub fn cookie_value(cookie_header: Option<&str>, name: &str) -> Option<String> {
    cookie_header?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_string()))
}

fn value_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

fn unix_timestamp_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn now_string() -> Result<String, time::error::Format> {
    OffsetDateTime::now_utc().format(&Rfc3339)
}

pub async fn folder_permission_count_for_group(
    pool: &SqlitePool,
    group_name: &str,
) -> Result<i64, AuthError> {
    Ok(sqlx::query(
        r"
        SELECT COUNT(*) AS count
        FROM folder_permissions
        JOIN vault_groups ON vault_groups.id = folder_permissions.group_id
        WHERE vault_groups.name = ?
        ",
    )
    .bind(group_name)
    .fetch_one(pool)
    .await?
    .get("count"))
}
