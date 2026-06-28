use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::SqlitePool;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::auth::{
    AuthError, AuthSettings, UserContext, cookie_value, oidc_identity, split_groups,
    verify_session_payload,
};

#[derive(Debug)]
pub struct CallbackRequest<'a> {
    pub code: &'a str,
    pub state: &'a str,
    pub cookie_header: Option<&'a str>,
    pub redirect_uri: &'a str,
}

#[derive(Debug)]
pub struct CallbackResult {
    pub user: UserContext,
    pub redirect_path: String,
}

#[derive(Debug, Error)]
pub enum OidcError {
    #[error("OIDC is not configured")]
    NotConfigured,
    #[error("OIDC authorization endpoint is missing")]
    MissingAuthorizationEndpoint,
    #[error("OIDC token endpoint is missing")]
    MissingTokenEndpoint,
    #[error("OIDC JWKS endpoint is missing")]
    MissingJwksEndpoint,
    #[error("OIDC state validation failed")]
    StateValidationFailed,
    #[error("OIDC provider did not return an ID token")]
    MissingIdToken,
    #[error("OIDC ID token validation failed")]
    InvalidIdToken,
    #[error("OIDC userinfo subject mismatch")]
    UserinfoSubjectMismatch,
    #[error("OIDC {endpoint} endpoint must use HTTPS")]
    InsecureEndpoint { endpoint: &'static str },
    #[error("OIDC provider URL is invalid")]
    ProviderUrlInvalid,
    #[error("OIDC {endpoint} endpoint request failed")]
    ProviderRequest { endpoint: &'static str },
    #[error("OIDC provider returned invalid JSON")]
    ProviderJson,
    #[error(transparent)]
    Auth(#[from] AuthError),
}

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryDocument {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    jwks_uri: Option<String>,
    userinfo_endpoint: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedDiscoveryDocument {
    expires_at_unix: i64,
    document: DiscoveryDocument,
}

static DISCOVERY_CACHE: OnceLock<Mutex<BTreeMap<String, CachedDiscoveryDocument>>> =
    OnceLock::new();

pub async fn authorization_endpoint(auth: &AuthSettings) -> Result<String, OidcError> {
    let configured = auth.oidc_authorization_endpoint.trim();
    if !configured.is_empty() {
        validate_provider_url(auth, "authorization", configured)?;
        return Ok(configured.to_string());
    }
    let discovery = discovery(auth).await?;
    let endpoint = discovery
        .authorization_endpoint
        .ok_or(OidcError::MissingAuthorizationEndpoint)?;
    validate_provider_url(auth, "authorization", &endpoint)?;
    Ok(endpoint)
}

pub async fn complete_callback(
    auth: &AuthSettings,
    pool: &SqlitePool,
    request: CallbackRequest<'_>,
) -> Result<CallbackResult, OidcError> {
    if request.code.trim().is_empty() || request.state.trim().is_empty() {
        return Err(OidcError::StateValidationFailed);
    }
    let state_cookie = cookie_value(request.cookie_header, &auth.oidc_state_cookie_name)
        .ok_or(OidcError::StateValidationFailed)?;
    let state_payload =
        verify_session_payload(auth, &state_cookie).ok_or(OidcError::StateValidationFailed)?;
    if string_claim(&state_payload, "state").as_deref() != Some(request.state) {
        return Err(OidcError::StateValidationFailed);
    }
    let nonce = string_claim(&state_payload, "nonce").ok_or(OidcError::StateValidationFailed)?;
    let redirect_path = safe_redirect(string_claim(&state_payload, "rd").as_deref());

    let discovery = discovery(auth).await?;
    let token =
        exchange_code_for_token(auth, &discovery, request.code, request.redirect_uri).await?;
    let id_token = string_claim(&token, "id_token").ok_or(OidcError::MissingIdToken)?;
    let mut identity = verified_id_claims(auth, &discovery, &id_token, &nonce).await?;
    let userinfo = userinfo(
        auth,
        &discovery,
        string_claim(&token, "access_token")
            .as_deref()
            .unwrap_or_default(),
    )
    .await?;
    if !userinfo.is_empty() && userinfo.get("sub") != identity.get("sub") {
        return Err(OidcError::UserinfoSubjectMismatch);
    }
    identity.extend(userinfo);

    let subject = string_claim(&identity, "sub").ok_or(AuthError::MissingSubject)?;
    let groups = groups_from_claim(identity.get(&auth.oidc_groups_claim));
    let email = string_claim(&identity, &auth.oidc_email_claim);
    let name = string_claim(&identity, &auth.oidc_name_claim)
        .or_else(|| string_claim(&identity, &auth.oidc_username_claim))
        .or_else(|| email.clone())
        .unwrap_or_else(|| subject.clone());
    let user = oidc_identity(auth, pool, &subject, email.as_deref(), Some(&name), &groups).await?;

    Ok(CallbackResult {
        user,
        redirect_path,
    })
}

#[must_use]
pub fn url_uses_secure_transport(url: &str, allow_insecure_http: bool) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => true,
        "http" => allow_insecure_http || parsed.host_str().is_some_and(is_local_hostname),
        _ => false,
    }
}

async fn discovery(auth: &AuthSettings) -> Result<DiscoveryDocument, OidcError> {
    if auth.oidc_issuer.trim().is_empty() || auth.oidc_client_id.trim().is_empty() {
        return Err(OidcError::NotConfigured);
    }
    let issuer = auth.oidc_issuer.trim_end_matches('/').to_string();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let cache = discovery_cache();
    {
        let cached = cache.lock().await;
        if let Some(cached) = cached.get(&issuer)
            && cached.expires_at_unix > now
        {
            return Ok(cached.document.clone());
        }
    }
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    let document = fetch_json_object(auth, "discovery", &discovery_url).await?;
    let document: DiscoveryDocument =
        serde_json::from_value(Value::Object(document)).map_err(|_| OidcError::ProviderJson)?;

    // Provider metadata is canonical but mostly static. Cache it like the Python service
    // so login/callback bursts do not add avoidable network latency or IdP load.
    let mut cached = cache.lock().await;
    cached.insert(
        issuer,
        CachedDiscoveryDocument {
            expires_at_unix: now.saturating_add(auth.oidc_discovery_ttl_seconds),
            document: document.clone(),
        },
    );
    Ok(document)
}

fn discovery_cache() -> &'static Mutex<BTreeMap<String, CachedDiscoveryDocument>> {
    DISCOVERY_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

async fn exchange_code_for_token(
    auth: &AuthSettings,
    discovery: &DiscoveryDocument,
    code: &str,
    redirect_uri: &str,
) -> Result<Map<String, Value>, OidcError> {
    let endpoint = discovery
        .token_endpoint
        .as_deref()
        .ok_or(OidcError::MissingTokenEndpoint)?;
    validate_provider_url(auth, "token", endpoint)?;

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", auth.oidc_client_id.as_str()),
    ];
    if auth.oidc_client_auth == "client_secret_post" {
        form.push(("client_secret", auth.oidc_client_secret.as_str()));
    }

    let client = http_client(auth)?;
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form);
    if auth.oidc_client_auth == "client_secret_basic" && !auth.oidc_client_secret.is_empty() {
        request = request.basic_auth(&auth.oidc_client_id, Some(&auth.oidc_client_secret));
    }
    let response = request
        .send()
        .await
        .map_err(|_| OidcError::ProviderRequest { endpoint: "token" })?;
    json_response(response).await
}

async fn verified_id_claims(
    auth: &AuthSettings,
    discovery: &DiscoveryDocument,
    id_token: &str,
    nonce: &str,
) -> Result<Map<String, Value>, OidcError> {
    let endpoint = discovery
        .jwks_uri
        .as_deref()
        .ok_or(OidcError::MissingJwksEndpoint)?;
    let jwks = fetch_jwks(auth, endpoint).await?;
    let header = decode_header(id_token).map_err(|_| OidcError::InvalidIdToken)?;
    let algorithms = algorithms_for_header(header.alg).ok_or(OidcError::InvalidIdToken)?;
    let kid = header.kid.as_deref();

    for jwk in jwks.keys.iter().filter(|jwk| jwk_matches_kid(jwk, kid)) {
        let Ok(key) = DecodingKey::from_jwk(jwk) else {
            continue;
        };
        let mut validation = Validation::new(header.alg);
        validation.algorithms.clone_from(&algorithms);
        validation.validate_aud = false;
        let Ok(token) = decode::<Map<String, Value>>(id_token, &key, &validation) else {
            continue;
        };
        let claims = token.claims;
        if string_claim(&claims, "iss").as_deref() != Some(auth.oidc_issuer.as_str()) {
            continue;
        }
        if !audience_matches(claims.get("aud"), &auth.oidc_client_id) {
            continue;
        }
        if string_claim(&claims, "nonce").as_deref() != Some(nonce) {
            continue;
        }
        return Ok(claims);
    }

    Err(OidcError::InvalidIdToken)
}

async fn fetch_jwks(auth: &AuthSettings, endpoint: &str) -> Result<JwkSet, OidcError> {
    validate_provider_url(auth, "JWKS", endpoint)?;
    let object = fetch_json_object(auth, "JWKS", endpoint).await?;
    serde_json::from_value(Value::Object(object)).map_err(|_| OidcError::ProviderJson)
}

async fn userinfo(
    auth: &AuthSettings,
    discovery: &DiscoveryDocument,
    access_token: &str,
) -> Result<Map<String, Value>, OidcError> {
    let Some(endpoint) = discovery.userinfo_endpoint.as_deref() else {
        return Ok(Map::new());
    };
    if access_token.trim().is_empty() {
        return Ok(Map::new());
    }
    validate_provider_url(auth, "userinfo", endpoint)?;
    let client = http_client(auth)?;
    let response = client
        .get(endpoint)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {access_token}"),
        )
        .send()
        .await
        .map_err(|_| OidcError::ProviderRequest {
            endpoint: "userinfo",
        })?;
    json_response(response).await
}

async fn fetch_json_object(
    auth: &AuthSettings,
    endpoint_name: &'static str,
    url: &str,
) -> Result<Map<String, Value>, OidcError> {
    validate_provider_url(auth, endpoint_name, url)?;
    let client = http_client(auth)?;
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| OidcError::ProviderRequest {
            endpoint: endpoint_name,
        })?;
    json_response(response).await
}

async fn json_response(response: reqwest::Response) -> Result<Map<String, Value>, OidcError> {
    if !response.status().is_success() {
        return Err(OidcError::ProviderRequest {
            endpoint: "provider",
        });
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|_| OidcError::ProviderJson)?;
    let Value::Object(object) = value else {
        return Err(OidcError::ProviderJson);
    };
    Ok(object)
}

fn http_client(auth: &AuthSettings) -> Result<reqwest::Client, OidcError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(auth.oidc_http_timeout_seconds))
        .build()
        .map_err(|_| OidcError::ProviderRequest { endpoint: "client" })
}

fn validate_provider_url(
    auth: &AuthSettings,
    endpoint: &'static str,
    url: &str,
) -> Result<(), OidcError> {
    let parsed = Url::parse(url).map_err(|_| OidcError::ProviderUrlInvalid)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(OidcError::ProviderUrlInvalid);
    }
    if !url_uses_secure_transport(url, auth.oidc_allow_insecure_http) {
        return Err(OidcError::InsecureEndpoint { endpoint });
    }
    Ok(())
}

fn algorithms_for_header(algorithm: Algorithm) -> Option<Vec<Algorithm>> {
    match algorithm {
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
            Some(vec![Algorithm::RS256, Algorithm::RS384, Algorithm::RS512])
        }
        Algorithm::ES256 | Algorithm::ES384 => Some(vec![Algorithm::ES256, Algorithm::ES384]),
        _ => None,
    }
}

fn jwk_matches_kid(jwk: &Jwk, kid: Option<&str>) -> bool {
    let Some(kid) = kid else {
        return true;
    };
    jwk.common.key_id.as_deref() == Some(kid)
}

fn groups_from_claim(value: Option<&Value>) -> BTreeSet<String> {
    match value {
        Some(Value::String(groups)) => split_groups(groups),
        Some(Value::Array(groups)) => groups
            .iter()
            .filter_map(value_to_string)
            .map(|group| group.trim().to_ascii_lowercase())
            .filter(|group| !group.is_empty())
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn audience_matches(value: Option<&Value>, client_id: &str) -> bool {
    match value {
        Some(Value::String(audience)) => audience == client_id,
        Some(Value::Array(audiences)) => audiences
            .iter()
            .any(|audience| audience.as_str() == Some(client_id)),
        _ => false,
    }
}

fn string_claim(claims: &Map<String, Value>, name: &str) -> Option<String> {
    claims
        .get(name)
        .and_then(value_to_string)
        .and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        })
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn safe_redirect(value: Option<&str>) -> String {
    value
        .filter(|item| item.starts_with('/') && !item.starts_with("//"))
        .unwrap_or("/")
        .to_string()
}

fn is_local_hostname(hostname: &str) -> bool {
    let hostname = hostname.trim().to_ascii_lowercase();
    matches!(hostname.as_str(), "localhost" | "127.0.0.1" | "::1")
        || hostname.ends_with(".localhost")
}
