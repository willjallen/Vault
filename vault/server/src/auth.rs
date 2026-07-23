use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fmt;
use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use sqlx::{FromRow, Row, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

type HmacSha256 = Hmac<Sha256>;

const IDENTITY_UPSERT_RETRY_DELAYS_MS: [u64; 6] = [5, 10, 20, 40, 80, 160];
const LAST_SEEN_REFRESH_INTERVAL_SECONDS: i64 = 5 * 60;
const MAX_PREVIOUS_SESSION_SECRETS: usize = 4;
const SESSION_SECRET_HEX_LENGTH: usize = 64;
const MIN_REPEATED_PATTERN_BYTES: usize = 8;
const SIGNING_HKDF_SALT: &[u8] = b"vault/signing-root/v1";
const SESSION_HKDF_INFO: &[u8] = b"vault/session-hmac/v1";
const UPLOAD_HKDF_INFO: &[u8] = b"vault/upload-hmac/v1";

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

#[derive(Clone)]
struct DerivedSigningKeys {
    configured: String,
    root: Option<[u8; 32]>,
    session: [u8; 32],
    upload: [u8; 32],
}

impl DerivedSigningKeys {
    fn new(configured: String) -> Self {
        let root = decode_session_secret(&configured);
        let input = root
            .as_ref()
            .map_or_else(|| configured.as_bytes(), |root| root.as_slice());
        let hkdf = Hkdf::<Sha256>::new(Some(SIGNING_HKDF_SALT), input);
        let mut session = [0_u8; 32];
        let mut upload = [0_u8; 32];
        hkdf.expand(SESSION_HKDF_INFO, &mut session)
            .expect("a 32-byte HKDF output is always valid");
        hkdf.expand(UPLOAD_HKDF_INFO, &mut upload)
            .expect("a 32-byte HKDF output is always valid");
        Self {
            configured,
            root,
            session,
            upload,
        }
    }
}

/// Prederived, domain-separated keys for signed browser and upload tokens.
#[derive(Clone)]
pub struct SigningKeyring {
    current: DerivedSigningKeys,
    previous: Vec<DerivedSigningKeys>,
    too_many_previous: bool,
}

impl SigningKeyring {
    #[must_use]
    pub fn from_configured(current: impl Into<String>, previous: Vec<String>) -> Self {
        let too_many_previous = previous.len() > MAX_PREVIOUS_SESSION_SECRETS;
        Self {
            current: DerivedSigningKeys::new(current.into()),
            previous: previous
                .into_iter()
                .take(MAX_PREVIOUS_SESSION_SECRETS)
                .map(DerivedSigningKeys::new)
                .collect(),
            too_many_previous,
        }
    }

    fn sign_session(&self, body: &[u8]) -> Option<Vec<u8>> {
        hmac_signature(&self.current.session, body)
    }

    fn verify_session(&self, body: &[u8], signature: &[u8]) -> bool {
        self.keys()
            .any(|keys| hmac_signature_matches(&keys.session, body, signature))
    }

    pub(crate) fn sign_upload(&self, body: &[u8]) -> Option<Vec<u8>> {
        hmac_signature(&self.current.upload, body)
    }

    pub(crate) fn verify_upload(&self, body: &[u8], signature: &[u8]) -> bool {
        self.keys()
            .any(|keys| hmac_signature_matches(&keys.upload, body, signature))
    }

    fn keys(&self) -> impl Iterator<Item = &DerivedSigningKeys> {
        std::iter::once(&self.current).chain(self.previous.iter())
    }

    fn validate_explicit_roots(&self, errors: &mut Vec<String>) {
        if !valid_session_secret(&self.current) {
            errors.push(session_secret_validation_message("VAULT_SESSION_SECRET"));
        }
        self.validate_previous_roots(errors, self.current.root);
    }

    fn validate_previous_roots(&self, errors: &mut Vec<String>, current_root: Option<[u8; 32]>) {
        if self.too_many_previous {
            errors.push(format!(
                "VAULT_SESSION_SECRET_PREVIOUS may contain at most {MAX_PREVIOUS_SESSION_SECRETS} secrets",
            ));
        }
        for previous in &self.previous {
            if !valid_session_secret(previous) {
                errors.push(session_secret_validation_message(
                    "VAULT_SESSION_SECRET_PREVIOUS",
                ));
                break;
            }
        }

        let mut seen = current_root.into_iter().collect::<HashSet<_>>();
        for previous in &self.previous {
            let Some(root) = previous.root else {
                continue;
            };
            if !seen.insert(root) {
                errors.push(
                    "VAULT_SESSION_SECRET_PREVIOUS must not repeat the current or another previous secret"
                        .to_string(),
                );
                break;
            }
        }
    }
}

impl fmt::Debug for SigningKeyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SigningKeyring")
            .field("current", &"[REDACTED]")
            .field("previous_count", &self.previous.len())
            .field("too_many_previous", &self.too_many_previous)
            .finish()
    }
}

fn hmac_signature(key: &[u8; 32], body: &[u8]) -> Option<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(body);
    Some(mac.finalize().into_bytes().to_vec())
}

fn hmac_signature_matches(key: &[u8; 32], body: &[u8], signature: &[u8]) -> bool {
    HmacSha256::new_from_slice(key).is_ok_and(|mut mac| {
        mac.update(body);
        mac.verify_slice(signature).is_ok()
    })
}

fn decode_session_secret(value: &str) -> Option<[u8; 32]> {
    if value.len() != SESSION_SECRET_HEX_LENGTH
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(decoded)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn valid_session_secret(keys: &DerivedSigningKeys) -> bool {
    keys.root.is_some_and(|root| !obviously_low_entropy(&root))
}

fn obviously_low_entropy(root: &[u8; 32]) -> bool {
    let mut counts = [0_u8; 256];
    for byte in root {
        counts[usize::from(*byte)] = counts[usize::from(*byte)].saturating_add(1);
    }
    let distinct = counts.iter().filter(|count| **count > 0).count();
    let highest_frequency = counts.iter().copied().max().unwrap_or_default();
    if distinct < 16 || highest_frequency > 8 {
        return true;
    }
    if (1..=root.len() - MIN_REPEATED_PATTERN_BYTES)
        .any(|period| root[period..] == root[..root.len() - period])
    {
        return true;
    }
    let difference = root[1].wrapping_sub(root[0]);
    root.windows(2)
        .all(|pair| pair[1].wrapping_sub(pair[0]) == difference)
}

fn session_secret_validation_message(name: &str) -> String {
    format!(
        "{name} must be exactly 64 hexadecimal characters generated from 32 random bytes and must not be repeated or low-diversity",
    )
}

#[derive(Debug, Clone, Default)]
pub struct TrustedProxySet {
    networks: Vec<IpNet>,
    invalid_entries: Vec<String>,
    configured: bool,
}

impl TrustedProxySet {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let value = value.trim();
        if value.is_empty() {
            return Self::default();
        }

        let mut networks = Vec::new();
        let mut invalid_entries = Vec::new();
        for entry in value.split(',') {
            let entry = entry.trim();
            let network = if entry.is_empty() || entry == "*" {
                None
            } else if let Ok(address) = entry.parse::<IpAddr>() {
                Some(IpNet::from(normalize_peer_ip(address)))
            } else {
                entry
                    .parse::<IpNet>()
                    .ok()
                    .and_then(normalize_proxy_network)
            };
            match network {
                Some(network) if network.prefix_len() != 0 => {
                    if !networks.contains(&network) {
                        networks.push(network);
                    }
                }
                _ => invalid_entries.push(if entry.is_empty() {
                    "<empty>".to_string()
                } else {
                    entry.to_string()
                }),
            }
        }

        Self {
            networks,
            invalid_entries,
            configured: true,
        }
    }

    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = normalize_peer_ip(address);
        self.networks
            .iter()
            .any(|network| network.contains(&address))
    }

    fn validation_error(&self, required: bool) -> Option<String> {
        if !self.configured {
            return required.then(|| {
                "FORWARDED_ALLOW_IPS is required when VAULT_AUTH_MODE=headers".to_string()
            });
        }
        if !self.invalid_entries.is_empty() {
            return Some(format!(
                "FORWARDED_ALLOW_IPS contains invalid IP/CIDR entries: {}",
                self.invalid_entries.join(", ")
            ));
        }
        if self.networks.is_empty() {
            return Some(
                "FORWARDED_ALLOW_IPS must contain at least one trusted proxy IP or CIDR"
                    .to_string(),
            );
        }
        None
    }
}

fn normalize_peer_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address @ IpAddr::V4(_) => address,
    }
}

fn normalize_proxy_network(network: IpNet) -> Option<IpNet> {
    let IpNet::V6(ipv6_network) = network else {
        return Some(network);
    };
    if ipv6_network.prefix_len() < 96 {
        return Some(IpNet::V6(ipv6_network));
    }
    let Some(ipv4_address) = ipv6_network.network().to_ipv4_mapped() else {
        return Some(IpNet::V6(ipv6_network));
    };
    Ipv4Net::new(ipv4_address, ipv6_network.prefix_len() - 96)
        .ok()
        .map(IpNet::V4)
}

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub mode: AuthMode,
    pub auth_mode_raw: String,
    pub dev_mode: bool,
    pub dev_auth_enabled: bool,
    pub base_domain: String,
    pub public_url: String,
    pub signing_keys: SigningKeyring,
    pub session_secret_source: SessionSecretSource,
    pub session_secret_requirement: SessionSecretRequirement,
    pub session_cookie_name: String,
    pub session_cookie_secure: String,
    pub session_max_age_seconds: i64,
    pub trusted_proxies: TrustedProxySet,
    pub header_auth_issuer: String,
    pub dev_auth_issuer: String,
    pub admin_groups: HashSet<String>,
    pub bootstrap_admin_emails: HashSet<String>,
    pub oidc_bootstrap_admin_subjects: HashSet<String>,
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
            signing_keys: SigningKeyring::from_configured(development_session_secret(), vec![]),
            session_secret_source: SessionSecretSource::Fallback,
            session_secret_requirement: SessionSecretRequirement::Optional,
            session_cookie_name: "vault_session".to_string(),
            session_cookie_secure: "auto".to_string(),
            session_max_age_seconds: 604_800,
            trusted_proxies: TrustedProxySet::default(),
            header_auth_issuer: "headers".to_string(),
            dev_auth_issuer: "dev".to_string(),
            admin_groups: split_groups_set("admin,vault-admin"),
            bootstrap_admin_emails: HashSet::new(),
            oidc_bootstrap_admin_subjects: HashSet::new(),
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
        let previous_session_secrets = previous_session_secrets_from_env();
        Self {
            mode,
            auth_mode_raw: auth_mode.trim().to_ascii_lowercase(),
            dev_mode: env_flag("VAULT_DEV_MODE") || mode == AuthMode::Dev || dev_auth_enabled,
            dev_auth_enabled,
            base_domain: env_string("BASE_DOMAIN", "localhost"),
            public_url: env_string("VAULT_PUBLIC_URL", ""),
            signing_keys: SigningKeyring::from_configured(session_secret, previous_session_secrets),
            session_secret_source,
            session_secret_requirement: require_session_secret_from_env(),
            session_cookie_name: env_string("VAULT_SESSION_COOKIE_NAME", "vault_session"),
            session_cookie_secure: env_string("VAULT_SESSION_COOKIE_SECURE", "auto")
                .to_ascii_lowercase(),
            session_max_age_seconds: env_i64("VAULT_SESSION_MAX_AGE_SECONDS", 604_800),
            trusted_proxies: TrustedProxySet::parse(
                &env::var("FORWARDED_ALLOW_IPS").unwrap_or_default(),
            ),
            header_auth_issuer: env_string("VAULT_HEADER_AUTH_ISSUER", "headers"),
            dev_auth_issuer: env_string("VAULT_DEV_AUTH_ISSUER", "dev"),
            admin_groups: split_groups_set(&env_string("VAULT_ADMIN_GROUPS", "admin,vault-admin")),
            bootstrap_admin_emails: split_groups_set(&env_string(
                "VAULT_BOOTSTRAP_ADMIN_EMAILS",
                "",
            )),
            oidc_bootstrap_admin_subjects: split_exact_set(&env_string(
                "VAULT_OIDC_BOOTSTRAP_ADMIN_SUBJECTS",
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
        if self.dev_auth_enabled && self.mode != AuthMode::Dev {
            errors.push("VAULT_DEV_AUTH requires VAULT_AUTH_MODE=dev".to_string());
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
        if !self.dev_mode && self.session_secret_source != SessionSecretSource::Explicit {
            errors.push("VAULT_SESSION_SECRET is required outside development mode".to_string());
        }
        if self.session_secret_source == SessionSecretSource::Explicit {
            self.signing_keys.validate_explicit_roots(&mut errors);
        } else {
            if self.signing_keys.current.configured != development_session_secret() {
                errors.push(
                    "VAULT_SESSION_SECRET fallback is available only for the built-in development secret"
                        .to_string(),
                );
            }
            self.signing_keys.validate_previous_roots(&mut errors, None);
        }
        if let Some(error) = self
            .trusted_proxies
            .validation_error(self.mode == AuthMode::Headers)
        {
            errors.push(error);
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

#[derive(Debug, FromRow)]
struct IdentitySnapshotRow {
    id: i64,
    issuer: String,
    subject: String,
    email: Option<String>,
    name: String,
    is_admin: i64,
    is_active: i64,
    last_seen_is_fresh: i64,
    group_name: Option<String>,
    root_permissions_complete: i64,
}

#[derive(Debug)]
struct IdentitySnapshot {
    user: VaultUserRecord,
    groups: Vec<String>,
    last_seen_is_fresh: bool,
    root_permissions_complete: bool,
}

impl IdentitySnapshot {
    fn from_rows(rows: Vec<IdentitySnapshotRow>) -> Option<Self> {
        let mut rows = rows.into_iter();
        let first = rows.next()?;
        let mut groups = first.group_name.into_iter().collect::<Vec<_>>();
        let mut root_permissions_complete = first.root_permissions_complete != 0;
        for row in rows {
            groups.extend(row.group_name);
            root_permissions_complete &= row.root_permissions_complete != 0;
        }
        Some(Self {
            user: VaultUserRecord {
                id: first.id,
                issuer: first.issuer,
                subject: first.subject,
                email: first.email,
                name: first.name,
                is_admin: first.is_admin,
                is_active: first.is_active,
            },
            groups,
            last_seen_is_fresh: first.last_seen_is_fresh != 0,
            root_permissions_complete,
        })
    }

    fn matches_authoritative_claims(
        &self,
        email: Option<&str>,
        name: &str,
        groups: &BTreeSet<String>,
    ) -> bool {
        self.user.email.as_deref() == email
            && self.user.name == name
            && self.groups.iter().eq(groups.iter())
            && self.root_permissions_complete
    }

    fn into_context(self, settings: &AuthSettings) -> UserContext {
        let is_admin = effective_admin_from_parts(
            settings,
            self.user.is_admin != 0,
            &self.user.issuer,
            &self.user.subject,
            self.user.email.as_deref(),
            &self.groups,
        );
        UserContext {
            id: self.user.id.to_string(),
            vault_user_id: self.user.id,
            issuer: self.user.issuer,
            subject: self.user.subject,
            name: self.user.name,
            email: self.user.email.unwrap_or_default(),
            groups: self.groups,
            is_admin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentitySyncMode {
    Authoritative,
    OidcLogin,
}

pub async fn header_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<UserContext, AuthError> {
    let remote_user = clean_header(headers.get("Remote-User"));
    if remote_user.is_empty() {
        return Err(AuthError::AuthenticationRequired);
    }

    let groups = split_groups_header(headers.get("Remote-Groups"));
    let email = clean_header(headers.get("Remote-Email"));
    let email = (!email.is_empty()).then_some(email);
    let remote_name = {
        let value = clean_header(headers.get("Remote-Name"));
        if value.is_empty() {
            remote_user.clone()
        } else {
            value
        }
    };

    authoritative_identity(
        settings,
        pool,
        &settings.header_auth_issuer,
        &remote_user,
        email.as_deref(),
        &remote_name,
        &groups,
    )
    .await
}

pub async fn dev_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
) -> Result<Option<UserContext>, AuthError> {
    if !settings.dev_auth_enabled || !dev_auth_allowed_for_domain(&settings.base_domain) {
        return Ok(None);
    }

    Ok(Some(
        authoritative_identity(
            settings,
            pool,
            &settings.dev_auth_issuer,
            &settings.dev_user,
            Some(&settings.dev_email),
            &settings.dev_name,
            &settings.dev_groups,
        )
        .await?,
    ))
}

async fn authoritative_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    name: &str,
    groups: &BTreeSet<String>,
) -> Result<UserContext, AuthError> {
    if issuer.trim().is_empty() || subject.trim().is_empty() {
        return Err(AuthError::MissingSubject);
    }

    let cutoff = last_seen_cutoff()?;
    if let Some(snapshot) = fetch_identity_snapshot(pool, issuer, subject, &cutoff).await? {
        if snapshot.user.is_active == 0 {
            return Err(AuthError::UserDisabled);
        }
        if snapshot.matches_authoritative_claims(email, name, groups) && snapshot.last_seen_is_fresh
        {
            return Ok(snapshot.into_context(settings));
        }
    }

    let snapshot = upsert_vault_user(
        pool,
        issuer,
        subject,
        email,
        Some(name),
        groups,
        IdentitySyncMode::Authoritative,
    )
    .await?;
    if snapshot.user.is_active == 0 {
        return Err(AuthError::UserDisabled);
    }
    Ok(snapshot.into_context(settings))
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
    let cutoff = last_seen_cutoff()?;
    let Some(snapshot) = fetch_identity_snapshot_by_id(pool, user_id, &cutoff).await? else {
        return Ok(None);
    };
    if snapshot.user.is_active == 0 {
        return Ok(None);
    }
    if snapshot.last_seen_is_fresh {
        return Ok(Some(snapshot.into_context(settings)));
    }
    let snapshot = refresh_last_seen_by_id(pool, user_id).await?;
    Ok(snapshot
        .filter(|snapshot| snapshot.user.is_active != 0)
        .map(|snapshot| snapshot.into_context(settings)))
}

pub async fn oidc_identity(
    settings: &AuthSettings,
    pool: &SqlitePool,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    groups: &BTreeSet<String>,
) -> Result<UserContext, AuthError> {
    let snapshot = upsert_vault_user(
        pool,
        &settings.oidc_issuer,
        subject,
        email,
        name,
        groups,
        IdentitySyncMode::OidcLogin,
    )
    .await?;
    if snapshot.user.is_active == 0 {
        return Err(AuthError::UserDisabled);
    }
    Ok(snapshot.into_context(settings))
}

pub fn sign_session_payload(
    settings: &AuthSettings,
    payload: &Map<String, Value>,
) -> Result<String, AuthError> {
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload)?);
    let signature = settings
        .signing_keys
        .sign_session(body.as_bytes())
        .map(|signature| URL_SAFE_NO_PAD.encode(signature))
        .ok_or(AuthError::IdentitySync)?;
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
    if !settings
        .signing_keys
        .verify_session(body.as_bytes(), &signature_bytes)
    {
        return None;
    }
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
    groups: &BTreeSet<String>,
    mode: IdentitySyncMode,
) -> Result<IdentitySnapshot, AuthError> {
    if issuer.trim().is_empty() || subject.trim().is_empty() {
        return Err(AuthError::MissingSubject);
    }
    for retry_delay_ms in IDENTITY_UPSERT_RETRY_DELAYS_MS
        .into_iter()
        .map(Some)
        .chain(std::iter::once(None))
    {
        match upsert_vault_user_once(pool, issuer, subject, email, name, groups, mode).await {
            Ok(snapshot) => return Ok(snapshot),
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
    groups: &BTreeSet<String>,
    mode: IdentitySyncMode,
) -> Result<IdentitySnapshot, AuthError> {
    // Identity sync is a short canonical write that can be hit by many fresh
    // sessions at once. BEGIN IMMEDIATE makes SQLite queue writers up front
    // instead of failing later during deferred read-to-write promotion.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let display_name = display_name(name, email, subject);
    let (now, cutoff) = last_seen_write_times()?;
    let existing = fetch_identity_snapshot_tx(&mut tx, issuer, subject, &cutoff).await?;

    let user_id = if let Some(snapshot) = existing {
        if snapshot.user.is_active == 0 {
            return Err(AuthError::UserDisabled);
        }
        match mode {
            IdentitySyncMode::OidcLogin => {
                // A real OIDC callback is a login even when its profile claims
                // are unchanged. Missing verified email remains preserve-only.
                sqlx::query(
                    "UPDATE vault_users SET email = COALESCE(?, email), name = ?, last_seen_at = ?, last_login_at = ? WHERE id = ?",
                )
                .bind(email)
                .bind(&display_name)
                .bind(&now)
                .bind(&now)
                .bind(snapshot.user.id)
                .execute(&mut *tx)
                .await?;
            }
            IdentitySyncMode::Authoritative => {
                // Header and development identity claims are authoritative. In
                // particular, None must clear an old email before bootstrap-admin
                // authorization is calculated.
                if snapshot.user.email.as_deref() != email || snapshot.user.name != display_name {
                    sqlx::query("UPDATE vault_users SET email = ?, name = ? WHERE id = ?")
                        .bind(email)
                        .bind(&display_name)
                        .bind(snapshot.user.id)
                        .execute(&mut *tx)
                        .await?;
                }
                if !snapshot.last_seen_is_fresh {
                    sqlx::query("UPDATE vault_users SET last_seen_at = ? WHERE id = ?")
                        .bind(&now)
                        .bind(snapshot.user.id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }

        if !snapshot.groups.iter().eq(groups.iter()) || !snapshot.root_permissions_complete {
            sync_vault_groups(&mut tx, snapshot.user.id, groups).await?;
        }
        snapshot.user.id
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
        .bind(if mode == IdentitySyncMode::OidcLogin {
            Some(now.as_str())
        } else {
            None
        })
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        let user_id = result.last_insert_rowid();
        sync_vault_groups(&mut tx, user_id, groups).await?;
        user_id
    };

    let snapshot = fetch_identity_snapshot_by_id_tx(&mut tx, user_id, &cutoff)
        .await?
        .ok_or(AuthError::IdentitySync)?;
    tx.commit().await?;
    Ok(snapshot)
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
        sync_group_root_permissions(tx, group_id).await?;
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

async fn sync_group_root_permissions(
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

const IDENTITY_SNAPSHOT_BY_IDENTITY_SQL: &str = r"
    SELECT
        vault_users.id,
        vault_users.issuer,
        vault_users.subject,
        vault_users.email,
        vault_users.name,
        vault_users.is_admin,
        vault_users.is_active,
        COALESCE(julianday(vault_users.last_seen_at) >= julianday(?), 0)
            AS last_seen_is_fresh,
        vault_groups.name AS group_name,
        CASE
            WHEN vault_groups.id IS NULL THEN 1
            ELSE NOT EXISTS (
                SELECT 1
                FROM folders AS root
                WHERE root.is_root = 1
                  AND NOT EXISTS (
                      SELECT 1
                      FROM folder_permissions
                      WHERE folder_permissions.folder_id = root.id
                        AND folder_permissions.group_id = vault_groups.id
                  )
            )
        END AS root_permissions_complete
    FROM vault_users
    LEFT JOIN vault_group_memberships
        ON vault_group_memberships.user_id = vault_users.id
    LEFT JOIN vault_groups
        ON vault_groups.id = vault_group_memberships.group_id
    WHERE vault_users.issuer = ? AND vault_users.subject = ?
    ORDER BY vault_groups.name
";

const IDENTITY_SNAPSHOT_BY_ID_SQL: &str = r"
    SELECT
        vault_users.id,
        vault_users.issuer,
        vault_users.subject,
        vault_users.email,
        vault_users.name,
        vault_users.is_admin,
        vault_users.is_active,
        COALESCE(julianday(vault_users.last_seen_at) >= julianday(?), 0)
            AS last_seen_is_fresh,
        vault_groups.name AS group_name,
        1 AS root_permissions_complete
    FROM vault_users
    LEFT JOIN vault_group_memberships
        ON vault_group_memberships.user_id = vault_users.id
    LEFT JOIN vault_groups
        ON vault_groups.id = vault_group_memberships.group_id
    WHERE vault_users.id = ?
    ORDER BY vault_groups.name
";

async fn fetch_identity_snapshot(
    pool: &SqlitePool,
    issuer: &str,
    subject: &str,
    cutoff: &str,
) -> Result<Option<IdentitySnapshot>, AuthError> {
    let rows = sqlx::query_as::<_, IdentitySnapshotRow>(IDENTITY_SNAPSHOT_BY_IDENTITY_SQL)
        .bind(cutoff)
        .bind(issuer)
        .bind(subject)
        .fetch_all(pool)
        .await?;
    Ok(IdentitySnapshot::from_rows(rows))
}

async fn fetch_identity_snapshot_tx(
    tx: &mut Transaction<'_, Sqlite>,
    issuer: &str,
    subject: &str,
    cutoff: &str,
) -> Result<Option<IdentitySnapshot>, AuthError> {
    let rows = sqlx::query_as::<_, IdentitySnapshotRow>(IDENTITY_SNAPSHOT_BY_IDENTITY_SQL)
        .bind(cutoff)
        .bind(issuer)
        .bind(subject)
        .fetch_all(&mut **tx)
        .await?;
    Ok(IdentitySnapshot::from_rows(rows))
}

async fn fetch_identity_snapshot_by_id(
    pool: &SqlitePool,
    user_id: i64,
    cutoff: &str,
) -> Result<Option<IdentitySnapshot>, AuthError> {
    let rows = sqlx::query_as::<_, IdentitySnapshotRow>(IDENTITY_SNAPSHOT_BY_ID_SQL)
        .bind(cutoff)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(IdentitySnapshot::from_rows(rows))
}

async fn fetch_identity_snapshot_by_id_tx(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: i64,
    cutoff: &str,
) -> Result<Option<IdentitySnapshot>, AuthError> {
    let rows = sqlx::query_as::<_, IdentitySnapshotRow>(IDENTITY_SNAPSHOT_BY_ID_SQL)
        .bind(cutoff)
        .bind(user_id)
        .fetch_all(&mut **tx)
        .await?;
    Ok(IdentitySnapshot::from_rows(rows))
}

async fn refresh_last_seen_by_id(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<IdentitySnapshot>, AuthError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (now, cutoff) = last_seen_write_times()?;
    let mut snapshot = fetch_identity_snapshot_by_id_tx(&mut tx, user_id, &cutoff).await?;
    if snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.user.is_active != 0 && !snapshot.last_seen_is_fresh)
    {
        sqlx::query("UPDATE vault_users SET last_seen_at = ? WHERE id = ? AND is_active = 1")
            .bind(now)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        snapshot = fetch_identity_snapshot_by_id_tx(&mut tx, user_id, &cutoff).await?;
    }
    tx.commit().await?;
    Ok(snapshot)
}

#[must_use]
pub fn effective_admin_from_parts(
    settings: &AuthSettings,
    is_stored_admin: bool,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    groups: &[String],
) -> bool {
    if is_stored_admin {
        return true;
    }
    let email = email.unwrap_or_default().trim().to_ascii_lowercase();
    let oidc_identity = settings.mode == AuthMode::Oidc
        && !settings.oidc_issuer.is_empty()
        && issuer == settings.oidc_issuer;
    let trusted_email_identity = match settings.mode {
        AuthMode::Headers => issuer == settings.header_auth_issuer,
        AuthMode::Dev => issuer == settings.dev_auth_issuer,
        AuthMode::Oidc => false,
    };
    (oidc_identity && settings.oidc_bootstrap_admin_subjects.contains(subject))
        || (trusted_email_identity && settings.bootstrap_admin_emails.contains(&email))
        || groups.iter().any(|group| {
            settings
                .admin_groups
                .contains(&group.trim().to_ascii_lowercase())
        })
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

fn split_exact_set(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect()
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
    (development_session_secret(), SessionSecretSource::Fallback)
}

fn previous_session_secrets_from_env() -> Vec<String> {
    env::var("VAULT_SESSION_SECRET_PREVIOUS").map_or_else(
        |_| Vec::new(),
        |value| {
            let value = value.trim();
            if value.is_empty() {
                Vec::new()
            } else {
                value
                    .split(',')
                    .take(MAX_PREVIOUS_SESSION_SECRETS + 1)
                    .map(|item| item.trim().to_string())
                    .collect()
            }
        },
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
    if !settings.bootstrap_admin_emails.is_empty() {
        errors.push(
            "VAULT_BOOTSTRAP_ADMIN_EMAILS does not apply to OIDC; use VAULT_OIDC_BOOTSTRAP_ADMIN_SUBJECTS"
                .to_string(),
        );
    }
    if settings.oidc_issuer.trim().is_empty() {
        errors.push("VAULT_OIDC_ISSUER is required when VAULT_AUTH_MODE=oidc".to_string());
    } else if !url_uses_secure_transport(&settings.oidc_issuer, settings.oidc_allow_insecure_http) {
        errors.push("VAULT_OIDC_ISSUER must use https outside local development".to_string());
    }
    if settings.oidc_issuer == settings.header_auth_issuer
        || settings.oidc_issuer == settings.dev_auth_issuer
    {
        errors.push(
            "VAULT_OIDC_ISSUER must differ from header and development identity issuers"
                .to_string(),
        );
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

fn last_seen_cutoff() -> Result<String, time::error::Format> {
    (OffsetDateTime::now_utc() - time::Duration::seconds(LAST_SEEN_REFRESH_INTERVAL_SECONDS))
        .format(&Rfc3339)
}

fn last_seen_write_times() -> Result<(String, String), time::error::Format> {
    let now = OffsetDateTime::now_utc();
    Ok((
        now.format(&Rfc3339)?,
        (now - time::Duration::seconds(LAST_SEEN_REFRESH_INTERVAL_SECONDS)).format(&Rfc3339)?,
    ))
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
