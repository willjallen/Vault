use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json as AxumJson, Router as AxumRouter};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Map, Value, json};
use sqlx::Row;
use tokio::net::TcpListener;
use tower::ServiceExt;
use vault_server::auth::{
    AuthMode, AuthSettings, SigningKeyring, sign_session_payload, verify_session_payload,
};
use vault_server::config::Config;
use vault_server::db;
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

const TEST_SIGNING_ROOT: &str = "a3f1c9e72b840d56ff196ab30ce2d785914b8c6230e7fa5d4921bc68e30fd754";
const PREVIOUS_SIGNING_ROOT: &str =
    "7d92b4e10a6fc83531de709bca4825f06e13d97a58c02bf46a91e53c7bd80462";
const TEST_RSA_PRIVATE_KEY: &str = r"-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAybIoSnqZmI2yqV/8RVupZccKfs4b/dTCqJ2rz6D3hPOiS/fY
gMMVfLQ6YVpOeJUXoBkQGbGCX4A0T+hOR2xC1j9vDQ23GF4M7wQBeRKhBi+RzQXX
Cm4naTffAaUCHXa5X7TRbpd3CdRFu3pwd8hXHc4N+v/6npTbRo2I6X5IPZwH91Hk
TkAJhZa2ySQVJWLoanJAEzF+PMTFIMBIkbG/zWp8FkjqR49i0gXXohZWKmBv2qVr
VIV+9blWmJFvEsAUvc3TMNWZYCUfeevTyCJk9io89GnT/MnS4BjbxZGI/XPl7IbA
4bAabbEGWIijWUk/wpbZ5p9AIX2kkRHZvrFJEwIDAQABAoIBADtXkYcoPxylRBWV
ShHWACcTwsDAP3gVKxiVG0HBaFHTpMKZLzfjLeU82ZhfC4tqwkK2XQhSM7uJatq/
zJgzAA8tJq0+hcpDkaaZFR3cH0hEoq8hsr0835eTeqdvNwoYLj48YwoYwktACyw3
v/NeHFOGlpJs0f3qagF+DvQz1WlaglU32mmAV8RlgzLFebyFqEcSI7rUa3UPS7ii
Km6dRK2vNLTU9Or2W0d54sCDDthuk6j0C09gSn5ttP/mlHAiIllUxwIjpixFZysR
HF4JvgAqLBTHkaXcg6khuHFBLZ+Xc035lMlg8HwCW/JDTx+OXy6uk5lwmqnz4hgP
IFue1XECgYEA/2Yi6Oy9At5K9PDyZUZ+AbOdI3rLZ5iP4WXOjuq0HPAkmE/k3fAE
kpYCG+syOBOVy+I8gWEkKtEjWxZuGwbSXjS+2I8T5oivoJM7PEfD6OAtoxBWzth4
4SjdZdd6JJzpvAJli1Vi2jqyU6ExQb8qXvTdxlAOKq1Li+fc7QgYSUcCgYEAyiuq
/Zfo0HmbebjaL+/lcr8Vwh2PuFuSzE+rqd6LTqa78LuCKbYAd7wQxJPzE1d7pLGL
CARF+wVG40wHLWbyD7VVmy/wzMkFBgZCCBJv5zP2izG/Sr7aoRQEgEXa3ZUgoRES
VEizZyqy+AMN+Dop8OhOVCOP1ZzJ0jB6ukdZp9UCgYEA/aA2Js2CXijWkyv762r3
k0UFVciJ2lT8/T8Ww4J8XwhzrvYYN/Y09EUXzxXgByQb7B69K1aGjiamT7yUly5N
FtSWeYSMpLE0h+fuOUyjVs3ZREfjjQIX+LGWO56iY12YF+bhZF7lDgagNMCso7ft
oeLVoiy6BNOXZFZbZOBXDd0CgYBaxjSmXLjqMk/+3WMKNxq85NNuLzvCuUs2dWdM
hGHkVLT6KBcPh2q6WDTnLs7rllIr5pPYa6LITNxBXneyiRCSwQbJAUOLj46z38dy
PGUGWKyQXyvW8c7UmFpVBgh5iWX3K+Ug9uumnONyvFxfYi5Gvue8m6MPdLChsabJ
URQOaQKBgDwf55HywIpn2JScLC6KVXGcCyhp/L4o/EJZJ+sq8hldX4lEwa2pCno6
MC19Ejq/Yi4ipxWb97I3za9r1HUIIuJQGKnXAhdFGQe6Wh++ysBLNb2fWiHfwwq0
2ByLzUbduA60iPoS7Uaxxy8frMIT9BmOYFRd5xu/re5q1FiDR5vA
-----END RSA PRIVATE KEY-----";

const TEST_RSA_N: &str = "ybIoSnqZmI2yqV_8RVupZccKfs4b_dTCqJ2rz6D3hPOiS_fYgMMVfLQ6YVpOeJUXoBkQGbGCX4A0T-hOR2xC1j9vDQ23GF4M7wQBeRKhBi-RzQXXCm4naTffAaUCHXa5X7TRbpd3CdRFu3pwd8hXHc4N-v_6npTbRo2I6X5IPZwH91HkTkAJhZa2ySQVJWLoanJAEzF-PMTFIMBIkbG_zWp8FkjqR49i0gXXohZWKmBv2qVrVIV-9blWmJFvEsAUvc3TMNWZYCUfeevTyCJk9io89GnT_MnS4BjbxZGI_XPl7IbA4bAabbEGWIijWUk_wpbZ5p9AIX2kkRHZvrFJEw";
const TEST_RSA_E: &str = "AQAB";

async fn test_state(auth: AuthSettings) -> (AppState, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        data_dir: temp_dir.path().to_path_buf(),
        db_path: Some(temp_dir.path().join("vault.db")),
        objects_path: None,
        transfers_path: None,
        static_dir: "vault/client".into(),
        storage_backend: "local".to_string(),
        storage_prefix: String::new(),
        site_name: "Vault".to_string(),
        max_upload_bytes: 5 * 1024 * 1024 * 1024,
        transfer_chunk_bytes: 32 * 1024 * 1024,
        transfer_session_ttl_seconds: 86_400,
        export_ttl_seconds: 86_400,
        export_workers: 1,
        export_max_active_jobs: 256,
        export_max_active_jobs_per_user: 16,
        export_zip_compression_threshold_bytes: 3 * 1024 * 1024 * 1024,
        export_zip_compresslevel: 1,
        ttl_sweep_interval_seconds: 60,
        gzip_minimum_size: 1024,
        gzip_compresslevel: 6,
    };
    let db = db::connect(&config.db_path()).await.expect("db");
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let state = AppState::new(config, auth, db, Arc::new(storage));
    (state, temp_dir)
}

fn oidc_auth(endpoint: &str) -> AuthSettings {
    AuthSettings {
        mode: AuthMode::Oidc,
        oidc_authorization_endpoint: endpoint.to_string(),
        oidc_client_id: "vault-client".to_string(),
        oidc_redirect_uri: "https://vault.example.com/auth/callback".to_string(),
        signing_keys: SigningKeyring::from_configured(TEST_SIGNING_ROOT, vec![]),
        ..AuthSettings::default()
    }
}

fn oidc_auth_without_configured_redirect(endpoint: &str) -> AuthSettings {
    let mut auth = oidc_auth(endpoint);
    auth.oidc_redirect_uri.clear();
    auth
}

fn oidc_provider_auth(issuer: &str) -> AuthSettings {
    AuthSettings {
        mode: AuthMode::Oidc,
        oidc_issuer: issuer.to_string(),
        oidc_client_id: "vault-client".to_string(),
        oidc_client_secret: "vault-secret".to_string(),
        oidc_redirect_uri: "https://vault.example.com/auth/callback".to_string(),
        signing_keys: SigningKeyring::from_configured(TEST_SIGNING_ROOT, vec![]),
        ..AuthSettings::default()
    }
}

fn request(method: Method, uri: &str) -> Request<Body> {
    request_with_headers(method, uri, &[])
}

fn request_with_headers(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Host", "vault.example.com");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

fn same_origin_post(uri: &str) -> Request<Body> {
    request_with_headers(Method::POST, uri, &[("Origin", "http://vault.example.com")])
}

async fn login_state_cookie(auth: AuthSettings, headers: &[(&str, &str)]) -> String {
    let (state, _temp_dir) = test_state(auth).await;
    let app = http::router(state);
    let response = app
        .oneshot(request_with_headers(Method::GET, "/login", headers))
        .await
        .expect("login");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    first_set_cookie(&response)
}

async fn callback_session_cookie(auth: AuthSettings, headers: &[(&str, &str)]) -> String {
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);
    let response = app
        .oneshot(callback_request_with_headers(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
            headers,
        ))
        .await
        .expect("callback");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    all_set_cookies(&response)
        .into_iter()
        .find(|cookie| cookie.starts_with(&format!("{}=", auth.session_cookie_name)))
        .expect("session cookie")
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn location(response: &axum::response::Response) -> &str {
    response
        .headers()
        .get(header::LOCATION)
        .expect("location")
        .to_str()
        .expect("location str")
}

fn first_set_cookie(response: &axum::response::Response) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .next()
        .expect("set-cookie")
        .to_str()
        .expect("cookie str")
        .to_string()
}

fn all_set_cookies(response: &axum::response::Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().expect("cookie str").to_string())
        .collect()
}

fn query_pairs(location: &str) -> HashMap<String, String> {
    location
        .split_once('?')
        .map_or("", |(_, query)| query)
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn token_urlsafe_len(nbytes: usize) -> usize {
    (nbytes * 4).div_ceil(3)
}

fn oidc_state_cookie_value(cookie: &str) -> String {
    cookie
        .split(';')
        .next()
        .expect("cookie value")
        .split_once('=')
        .expect("cookie pair")
        .1
        .to_string()
}

#[derive(Clone)]
struct MockProviderState {
    issuer: String,
    discovery: Option<Value>,
    token_response: Value,
    userinfo: Value,
    discovery_requests: Arc<Mutex<usize>>,
    token_requests: Arc<Mutex<Vec<TokenRequest>>>,
}

#[derive(Debug, Clone)]
struct TokenRequest {
    authorization: Option<String>,
    body: String,
}

struct MockProvider {
    issuer: String,
    discovery_requests: Arc<Mutex<usize>>,
    token_requests: Arc<Mutex<Vec<TokenRequest>>>,
}

async fn start_mock_provider(nonce: &str, userinfo: Value) -> MockProvider {
    start_mock_provider_with_token_response(userinfo, |issuer| {
        json!({
            "id_token": signed_id_token(issuer, nonce),
            "access_token": "access-token",
        })
    })
    .await
}

async fn start_mock_provider_with_token_response(
    userinfo: Value,
    token_response: impl FnOnce(&str) -> Value,
) -> MockProvider {
    start_mock_provider_with_discovery(userinfo, token_response, |_| None).await
}

async fn start_mock_provider_with_discovery(
    userinfo: Value,
    token_response: impl FnOnce(&str) -> Value,
    discovery: impl FnOnce(&str) -> Option<Value>,
) -> MockProvider {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("provider bind");
    let issuer = format!("http://{}", listener.local_addr().expect("provider addr"));
    let discovery_requests = Arc::new(Mutex::new(0));
    let token_requests = Arc::new(Mutex::new(Vec::new()));
    let provider_state = MockProviderState {
        issuer: issuer.clone(),
        discovery: discovery(&issuer),
        token_response: token_response(&issuer),
        userinfo,
        discovery_requests: Arc::clone(&discovery_requests),
        token_requests: Arc::clone(&token_requests),
    };
    let app = AxumRouter::new()
        .route("/.well-known/openid-configuration", get(mock_discovery))
        .route("/token", post(mock_token))
        .route("/jwks", get(mock_jwks))
        .route("/userinfo", get(mock_userinfo))
        .with_state(provider_state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("provider serve");
    });
    MockProvider {
        issuer,
        discovery_requests,
        token_requests,
    }
}

async fn mock_discovery(AxumState(state): AxumState<MockProviderState>) -> AxumJson<Value> {
    *state.discovery_requests.lock().expect("discovery requests") += 1;
    if let Some(discovery) = state.discovery {
        return AxumJson(discovery);
    }
    AxumJson(json!({
        "authorization_endpoint": format!("{}/authorize", state.issuer),
        "token_endpoint": format!("{}/token", state.issuer),
        "jwks_uri": format!("{}/jwks", state.issuer),
        "userinfo_endpoint": format!("{}/userinfo", state.issuer),
    }))
}

async fn mock_token(
    AxumState(state): AxumState<MockProviderState>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumJson<Value> {
    let body = String::from_utf8(body.to_vec()).expect("form body");
    state
        .token_requests
        .lock()
        .expect("token requests")
        .push(TokenRequest {
            authorization: headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            body,
        });
    AxumJson(state.token_response)
}

async fn mock_jwks() -> AxumJson<Value> {
    AxumJson(json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "kid": "test-key",
            "alg": "RS256",
            "n": TEST_RSA_N,
            "e": TEST_RSA_E,
        }],
    }))
}

async fn mock_userinfo(AxumState(state): AxumState<MockProviderState>) -> AxumJson<Value> {
    AxumJson(state.userinfo)
}

fn signed_id_token(issuer: &str, nonce: &str) -> String {
    signed_id_token_with_profile(
        issuer,
        nonce,
        &json!({
            "email": "claims@example.com",
            "email_verified": true,
            "name": "Claims Name",
            "groups": ["claims-group"],
        }),
    )
}

fn signed_id_token_with_profile(issuer: &str, nonce: &str, profile: &Value) -> String {
    let expires_at = unix_timestamp_now() + 600;
    let mut claims = json!({
        "iss": issuer,
        "sub": "kevin",
        "aud": "vault-client",
        "exp": expires_at,
        "iat": expires_at - 60,
        "nonce": nonce,
    })
    .as_object()
    .expect("base claims")
    .clone();
    claims.extend(profile.as_object().expect("profile claims object").clone());
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key".to_string());
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).expect("rsa key"),
    )
    .expect("id token")
}

fn signed_state_cookie(auth: &AuthSettings, state: &str, nonce: &str, rd: &str) -> String {
    let mut payload = Map::new();
    payload.insert("state".to_string(), Value::String(state.to_string()));
    payload.insert("nonce".to_string(), Value::String(nonce.to_string()));
    payload.insert("rd".to_string(), Value::String(rd.to_string()));
    payload.insert("exp".to_string(), json!(unix_timestamp_now() + 600));
    sign_session_payload(auth, &payload).expect("state cookie")
}

fn callback_request(uri: &str, auth: &AuthSettings, state_cookie: &str) -> Request<Body> {
    callback_request_with_headers(uri, auth, state_cookie, &[])
}

fn session_request(uri: &str, auth: &AuthSettings, session_cookie: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(
            header::COOKIE,
            format!("{}={session_cookie}", auth.session_cookie_name),
        )
        .body(Body::empty())
        .expect("session request")
}

fn callback_request_with_headers(
    uri: &str,
    auth: &AuthSettings,
    state_cookie: &str,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Host", "vault.example.com")
        .header(
            header::COOKIE,
            format!("{}={}", auth.oidc_state_cookie_name, state_cookie),
        );
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

fn set_cookie_value(response: &axum::response::Response, name: &str) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|value| {
            let cookie = value.to_str().ok()?;
            cookie
                .strip_prefix(&format!("{name}="))
                .and_then(|rest| rest.split(';').next())
                .map(str::to_string)
        })
        .expect("set-cookie value")
}

fn unix_timestamp_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .try_into()
        .expect("timestamp")
}

#[tokio::test]
async fn oidc_mode_returns_api_401_and_browser_login_redirect_without_session() {
    /*
     * Missing OIDC sessions are handled differently for machine and browser surfaces.
     * APIs return a JSON authentication error, while a share page redirects to login with its
     * path and query preserved as the return destination.
     */
    let (state, _temp_dir) = test_state(oidc_auth("https://idp.example.com/auth")).await;
    let app = http::router(state);

    let api = app
        .clone()
        .oneshot(request(Method::GET, "/api/bootstrap"))
        .await
        .expect("api response");
    assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(api).await["detail"],
        "Authentication required"
    );

    let browser = app
        .oneshot(request(Method::GET, "/s/share-code?preview=1"))
        .await
        .expect("browser response");
    assert_eq!(browser.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&browser), "/login?rd=/s/share-code%3Fpreview%3D1");
}

#[tokio::test]
async fn non_oidc_auth_routes_preserve_python_redirect_and_cookie_behavior() {
    /*
     * Header-auth deployments keep the legacy Python behavior for OIDC-shaped routes: login and
     * callback simply return home without setting cookies. Logout still honors a safe local
     * destination and expires both conventional authentication cookies.
     */
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let app = http::router(state);

    let login = app
        .clone()
        .oneshot(request(Method::GET, "/login?rd=/Project"))
        .await
        .expect("login");
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&login), "/");
    assert!(all_set_cookies(&login).is_empty());

    let callback = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/auth/callback?error=access_denied&code=auth-code&state=state-123",
        ))
        .await
        .expect("callback");
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&callback), "/");
    assert!(all_set_cookies(&callback).is_empty());

    let logout = app
        .oneshot(same_origin_post("/logout?rd=/Project"))
        .await
        .expect("logout");
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&logout), "/Project");
    let cookies = all_set_cookies(&logout);
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_session=; Max-Age=0"))
    );
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_oidc_state=; Max-Age=0"))
    );
}

#[tokio::test]
async fn oidc_login_redirects_to_provider_and_sets_signed_state_cookie() {
    /*
     * Login constructs the provider authorization request with the configured client, callback,
     * scopes, and fresh state and nonce values. A signed, HTTP-only, short-lived state
     * cookie binds those values to the local return path with the expected SameSite and
     * transport attributes.
     */
    let auth = oidc_auth("https://idp.example.com/auth");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(request(Method::GET, "/login?rd=/Project"))
        .await
        .expect("login");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = location(&response);
    assert!(location.starts_with("https://idp.example.com/auth?"));
    let query = query_pairs(location);
    assert_eq!(query["client_id"], "vault-client");
    assert_eq!(
        query["redirect_uri"],
        "https%3A%2F%2Fvault.example.com%2Fauth%2Fcallback"
    );
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["scope"], "openid%20email%20profile");
    assert!(query["state"].len() >= 16);
    assert!(query["nonce"].len() >= 16);

    let cookie = first_set_cookie(&response);
    assert!(cookie.starts_with("vault_oidc_state="));
    assert!(cookie.contains("Max-Age=600"));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(!cookie.contains("Secure"));
    let payload = verify_session_payload(&auth, &oidc_state_cookie_value(&cookie))
        .expect("signed state payload");
    assert_eq!(payload["state"], query["state"]);
    assert_eq!(payload["nonce"], query["nonce"]);
    assert_eq!(payload["rd"], "/Project");
}

#[tokio::test]
async fn oidc_login_preserves_safe_redirects_and_rejects_ambiguous_or_external_forms() {
    /*
     * A normal local path may retain its query and fragment through the signed login state.
     * Scheme-relative, credentialed, absolute, backslash, multiply encoded, and header-injection
     * destinations are all reduced to the safe root fallback.
     */
    let auth = oidc_auth("https://idp.example.com/auth");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let safe = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/login?rd=%2FProject%2FPlan%3Ftab%3Dactivity%23history",
        ))
        .await
        .expect("safe login redirect");
    let safe_cookie = first_set_cookie(&safe);
    let safe_payload = verify_session_payload(&auth, &oidc_state_cookie_value(&safe_cookie))
        .expect("safe state payload");
    assert_eq!(safe_payload["rd"], "/Project/Plan?tab=activity#history");

    for encoded in [
        "%2F%2Fevil.example.com",
        "%2F%2Fuser%3Apassword%40evil.example.com",
        "https%3A%2F%2Fevil.example.com",
        "%2F%5Cevil.example.com",
        "%2F%255cevil.example.com",
        "%2F%25255cevil.example.com",
        "%2F%252fevil.example.com",
        "%2F%25252fevil.example.com",
        "%2F%250d%250aLocation%3A%252f%252fevil.example.com",
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, &format!("/login?rd={encoded}")))
            .await
            .expect("unsafe login redirect");
        let cookie = first_set_cookie(&response);
        let payload = verify_session_payload(&auth, &oidc_state_cookie_value(&cookie))
            .expect("sanitized state payload");
        assert_eq!(payload["rd"], "/", "{encoded}");
    }
}

#[test]
fn oidc_state_cookie_accepts_a_bounded_previous_signing_root() {
    /*
     * Rotating the signing root can keep existing login state valid through an explicitly
     * configured previous key. The same cookie fails verification once that bounded
     * compatibility key is no longer present.
     */
    let mut old_auth = oidc_auth("https://idp.example.com/auth");
    old_auth.signing_keys = SigningKeyring::from_configured(PREVIOUS_SIGNING_ROOT, vec![]);
    let state_cookie = signed_state_cookie(&old_auth, "state-123", "nonce-123", "/Project");

    let mut rotated_auth = oidc_auth("https://idp.example.com/auth");
    rotated_auth.signing_keys =
        SigningKeyring::from_configured(TEST_SIGNING_ROOT, vec![PREVIOUS_SIGNING_ROOT.to_string()]);
    assert!(verify_session_payload(&rotated_auth, &state_cookie).is_some());

    let current_only = oidc_auth("https://idp.example.com/auth");
    assert!(verify_session_payload(&current_only, &state_cookie).is_none());
}

#[tokio::test]
async fn oidc_login_uses_configured_nonce_byte_count_for_state_and_nonce() {
    /*
     * The configured entropy byte count controls both CSRF state and OpenID nonce generation.
     * Their URL-safe encodings have the exact unpadded length and are copied unchanged into the
     * signed cookie.
     */
    let mut auth = oidc_auth("https://idp.example.com/auth");
    auth.oidc_nonce_bytes = 18;
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let response = http::router(state)
        .oneshot(request(Method::GET, "/login?rd=/Project"))
        .await
        .expect("login");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let query = query_pairs(location(&response));
    assert_eq!(query["state"].len(), token_urlsafe_len(18));
    assert_eq!(query["nonce"].len(), token_urlsafe_len(18));
    assert!(!query["state"].contains('='));
    assert!(!query["nonce"].contains('='));

    let cookie = first_set_cookie(&response);
    let payload = verify_session_payload(&auth, &oidc_state_cookie_value(&cookie))
        .expect("signed state payload");
    assert_eq!(payload["state"], query["state"]);
    assert_eq!(payload["nonce"], query["nonce"]);
}

#[tokio::test]
async fn custom_oidc_cookie_names_are_used_for_login_callback_and_logout() {
    /*
     * Custom state and session cookie names must replace the defaults throughout the entire
     * login lifecycle. Login sets the custom state, callback clears it and sets the custom
     * session, and logout expires both custom names without leaking default-name cookies.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.session_cookie_name = "vault.sso-session".to_string();
    auth.oidc_state_cookie_name = "vault.sso-state".to_string();
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let login = app
        .clone()
        .oneshot(request(Method::GET, "/login?rd=/Project"))
        .await
        .expect("login");
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let login_cookies = all_set_cookies(&login);
    assert!(
        login_cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault.sso-state="))
    );
    assert!(
        !login_cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_oidc_state="))
    );

    let callback = app
        .clone()
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&callback), "/Project");
    let session_cookie = set_cookie_value(&callback, "vault.sso-session");
    let session = verify_session_payload(&auth, &session_cookie).expect("session payload");
    assert_eq!(session["uid"], 1);
    let callback_cookies = all_set_cookies(&callback);
    assert!(
        callback_cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault.sso-state=; Max-Age=0"))
    );
    assert!(
        !callback_cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_session="))
    );

    let logout = app
        .oneshot(request_with_headers(
            Method::POST,
            "/logout?rd=/Project",
            &[
                ("Origin", "http://vault.example.com"),
                ("Sec-Fetch-Site", "same-origin"),
            ],
        ))
        .await
        .expect("logout");
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    let logout_cookies = all_set_cookies(&logout);
    assert!(
        logout_cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault.sso-session=; Max-Age=0"))
    );
    assert!(
        logout_cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault.sso-state=; Max-Age=0"))
    );
}

#[tokio::test]
async fn oidc_login_derives_redirect_uri_from_forwarded_host_and_proto() {
    /*
     * With no configured public callback, login derives one from the trusted first forwarded
     * protocol and host values. Internal host values later in the proxy lists do not replace
     * the externally visible HTTPS origin.
     */
    let auth = oidc_auth_without_configured_redirect("https://idp.example.com/auth");
    let (state, _temp_dir) = test_state(auth).await;
    let app = http::router(state);

    let response = app
        .oneshot(request_with_headers(
            Method::GET,
            "/login",
            &[
                ("Host", "vault.internal:8000"),
                ("X-Forwarded-Proto", "https, http"),
                (
                    "X-Forwarded-Host",
                    "share.metal.gadstudios.io, vault.internal:8000",
                ),
            ],
        ))
        .await
        .expect("login");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let query = query_pairs(location(&response));
    assert_eq!(
        query["redirect_uri"],
        "https%3A%2F%2Fshare.metal.gadstudios.io%2Fauth%2Fcallback"
    );
}

#[tokio::test]
async fn oidc_state_cookie_secure_flag_follows_public_url_forwarded_proto_and_overrides() {
    /*
     * Automatic Secure handling for the transient state cookie recognizes either an HTTPS public
     * URL or trusted forwarded HTTPS. Explicit true and false settings override that
     * inference in both directions.
     */
    let mut https_public_url = oidc_auth("https://idp.example.com/auth");
    https_public_url.public_url = "https://vault.example.com".to_string();
    let cookie = login_state_cookie(https_public_url, &[]).await;
    assert!(cookie.contains("Secure"));

    let forwarded_https = oidc_auth("https://idp.example.com/auth");
    let cookie = login_state_cookie(forwarded_https, &[("X-Forwarded-Proto", "https, http")]).await;
    assert!(cookie.contains("Secure"));

    let mut explicit_false = oidc_auth("https://idp.example.com/auth");
    explicit_false.public_url = "https://vault.example.com".to_string();
    explicit_false.session_cookie_secure = "false".to_string();
    let cookie = login_state_cookie(explicit_false, &[("X-Forwarded-Proto", "https")]).await;
    assert!(!cookie.contains("Secure"));

    let mut explicit_true = oidc_auth("https://idp.example.com/auth");
    explicit_true.session_cookie_secure = "true".to_string();
    let cookie = login_state_cookie(explicit_true, &[]).await;
    assert!(cookie.contains("Secure"));
}

#[tokio::test]
async fn oidc_session_cookie_secure_flag_follows_public_url_forwarded_proto_and_overrides() {
    /*
     * The long-lived session cookie uses the same transport-policy inputs as login state after a
     * successful callback. HTTPS public or forwarded origins enable Secure automatically,
     * while explicit configuration remains authoritative.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
    )
    .await;

    let mut https_public_url = oidc_provider_auth(&provider.issuer);
    https_public_url.public_url = "https://vault.example.com".to_string();
    let cookie = callback_session_cookie(https_public_url, &[]).await;
    assert!(cookie.contains("Secure"));

    let forwarded_https = oidc_provider_auth(&provider.issuer);
    let cookie =
        callback_session_cookie(forwarded_https, &[("X-Forwarded-Proto", "https, http")]).await;
    assert!(cookie.contains("Secure"));

    let mut explicit_false = oidc_provider_auth(&provider.issuer);
    explicit_false.public_url = "https://vault.example.com".to_string();
    explicit_false.session_cookie_secure = "false".to_string();
    let cookie = callback_session_cookie(explicit_false, &[("X-Forwarded-Proto", "https")]).await;
    assert!(!cookie.contains("Secure"));

    let mut explicit_true = oidc_provider_auth(&provider.issuer);
    explicit_true.session_cookie_secure = "true".to_string();
    let cookie = callback_session_cookie(explicit_true, &[]).await;
    assert!(cookie.contains("Secure"));
}

#[tokio::test]
async fn oidc_login_rejects_insecure_nonlocal_authorization_endpoint() {
    /*
     * Redirecting credentials to a cleartext authorization endpoint on a nonlocal host is
     * forbidden. Login reports the provider configuration failure instead of issuing an
     * unsafe redirect.
     */
    let (state, _temp_dir) = test_state(oidc_auth("http://idp.example.com/auth")).await;
    let app = http::router(state);

    let response = app
        .oneshot(request(Method::GET, "/login"))
        .await
        .expect("login");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC authorization endpoint must use HTTPS"
    );
}

#[tokio::test]
async fn oidc_login_rejects_discovery_without_authorization_endpoint() {
    /*
     * Provider discovery returns token and verification metadata but omits the endpoint needed
     * to begin authorization. Login fails with the specific missing-endpoint diagnostic
     * rather than building a malformed redirect.
     */
    let provider = start_mock_provider_with_discovery(
        json!({}),
        |_issuer| json!({}),
        |issuer| {
            Some(json!({
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
                "userinfo_endpoint": format!("{issuer}/userinfo"),
            }))
        },
    )
    .await;
    let (state, _temp_dir) = test_state(oidc_provider_auth(&provider.issuer)).await;
    let app = http::router(state);

    let response = app
        .oneshot(request(Method::GET, "/login"))
        .await
        .expect("login");

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC authorization endpoint is missing"
    );
}

#[tokio::test]
async fn oidc_discovery_is_cached_for_configured_ttl() {
    /*
     * Two login attempts occur within a one-hour provider metadata lifetime.
     * Both redirects succeed while discovery is fetched only once and served from the shared
     * cache thereafter.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.oidc_discovery_ttl_seconds = 3600;
    let (state, _temp_dir) = test_state(auth).await;
    let app = http::router(state);

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(request(Method::GET, "/login"))
            .await
            .expect("login");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    assert_eq!(
        *provider
            .discovery_requests
            .lock()
            .expect("discovery requests"),
        1
    );
}

#[tokio::test]
async fn oidc_discovery_zero_ttl_refetches_provider_metadata() {
    /*
     * A zero discovery lifetime deliberately disables metadata reuse.
     * Consecutive logins both succeed and each performs its own discovery request.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.oidc_discovery_ttl_seconds = 0;
    let (state, _temp_dir) = test_state(auth).await;
    let app = http::router(state);

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(request(Method::GET, "/login"))
            .await
            .expect("login");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    assert_eq!(
        *provider
            .discovery_requests
            .lock()
            .expect("discovery requests"),
        2
    );
}

#[tokio::test]
async fn logout_deletes_session_and_state_cookies_and_uses_safe_redirects() {
    /*
     * A same-origin POST logout expires both session and in-progress OIDC state, disables
     * response caching, and preserves a valid local return path. An absolute external
     * destination is replaced with the root while cookie deletion and no-store headers still
     * apply.
     */
    let (state, _temp_dir) = test_state(oidc_auth("https://idp.example.com/auth")).await;
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(same_origin_post("/logout?rd=/Project"))
        .await
        .expect("logout");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/Project");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, max-age=0"
    );
    let cookies = all_set_cookies(&response);
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_session=; Max-Age=0"))
    );
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_oidc_state=; Max-Age=0"))
    );

    let unsafe_redirect = app
        .oneshot(request_with_headers(
            Method::POST,
            "/logout?rd=https://evil.example.com",
            &[("Sec-Fetch-Site", "same-origin")],
        ))
        .await
        .expect("unsafe logout");
    assert_eq!(unsafe_redirect.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&unsafe_redirect), "/");
    assert_eq!(
        unsafe_redirect.headers()[header::CACHE_CONTROL],
        "no-store, max-age=0",
    );
}

#[tokio::test]
async fn logout_preserves_safe_redirects_and_rejects_encoded_external_forms() {
    /*
     * Logout retains a local destination containing a query and fragment.
     * Multiple encoded open-redirect and response-splitting forms all fall back to root while
     * still clearing both cookies safely.
     */
    let (state, _temp_dir) = test_state(oidc_auth("https://idp.example.com/auth")).await;
    let app = http::router(state);

    let safe = app
        .clone()
        .oneshot(same_origin_post(
            "/logout?rd=%2FProject%2FPlan%3Ftab%3Dactivity%23history",
        ))
        .await
        .expect("safe logout redirect");
    assert_eq!(safe.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&safe), "/Project/Plan?tab=activity#history");

    for encoded in [
        "%2F%2Fevil.example.com",
        "%2F%2Fuser%3Apassword%40evil.example.com",
        "%2F%5Cevil.example.com",
        "%2F%255cevil.example.com",
        "%2F%25255cevil.example.com",
        "%2F%252fevil.example.com",
        "%2F%25252fevil.example.com",
        "%2F%250d%250aLocation%3A%252f%252fevil.example.com",
    ] {
        let response = app
            .clone()
            .oneshot(same_origin_post(&format!("/logout?rd={encoded}")))
            .await
            .expect("unsafe logout redirect");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{encoded}");
        assert_eq!(location(&response), "/", "{encoded}");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, max-age=0",
        );
        assert_eq!(all_set_cookies(&response).len(), 2);
    }
}

#[tokio::test]
async fn logout_requires_post_and_unambiguous_same_origin_provenance_without_deleting_cookies() {
    /*
     * Logout is state-changing, so GET is disallowed and POST requires unambiguous same-origin
     * evidence from Origin or fetch metadata. Missing, null, cross-site, contradictory, or
     * duplicate provenance is forbidden without redirecting or expiring any cookies.
     */
    let (state, _temp_dir) = test_state(oidc_auth("https://idp.example.com/auth")).await;
    let app = http::router(state);

    let get_response = app
        .clone()
        .oneshot(request(Method::GET, "/logout?rd=/Project"))
        .await
        .expect("logout get");
    assert_eq!(get_response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(all_set_cookies(&get_response).is_empty());
    assert!(get_response.headers().get(header::LOCATION).is_none());

    let rejected_headers = [
        vec![],
        vec![("Origin", "null")],
        vec![("Origin", "https://evil.example.com")],
        vec![("Sec-Fetch-Site", "cross-site")],
        vec![
            ("Origin", "http://vault.example.com"),
            ("Sec-Fetch-Site", "same-site"),
        ],
        vec![
            ("Origin", "https://evil.example.com"),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        vec![("Sec-Fetch-Site", "none")],
        vec![
            ("Origin", "http://vault.example.com"),
            ("Origin", "http://vault.example.com"),
        ],
    ];
    for headers in rejected_headers {
        let response = app
            .clone()
            .oneshot(request_with_headers(Method::POST, "/logout", &headers))
            .await
            .expect("rejected logout");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, max-age=0"
        );
        assert!(all_set_cookies(&response).is_empty());
        assert!(response.headers().get(header::LOCATION).is_none());
        assert_eq!(
            response_json(response).await["detail"],
            "Same-origin request provenance required",
        );
    }
}

#[tokio::test]
async fn logout_matches_normalized_public_and_trusted_effective_request_origins() {
    /*
     * Same-origin comparison normalizes host casing, default HTTPS ports, and irrelevant
     * public-URL path components. When no public URL is set, it also accepts an Origin
     * matching the trusted forwarded effective request scheme.
     */
    let mut public_auth = oidc_auth("https://idp.example.com/auth");
    public_auth.public_url = "https://VAULT.example.com:443/base".to_string();
    let (state, _temp_dir) = test_state(public_auth).await;
    let public_response = http::router(state)
        .oneshot(request_with_headers(
            Method::POST,
            "/logout",
            &[("Origin", "https://vault.example.com")],
        ))
        .await
        .expect("public origin logout");
    assert_eq!(public_response.status(), StatusCode::SEE_OTHER);

    let (state, _temp_dir) = test_state(oidc_auth("https://idp.example.com/auth")).await;
    let effective_response = http::router(state)
        .oneshot(request_with_headers(
            Method::POST,
            "/logout",
            &[
                ("Origin", "https://vault.example.com:443"),
                ("X-Forwarded-Proto", "https"),
            ],
        ))
        .await
        .expect("effective origin logout");
    assert_eq!(effective_response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn oidc_callback_preserves_safe_redirects_and_rejects_encoded_external_forms() {
    /*
     * Callback uses the return destination protected inside signed state, preserving a
     * legitimate local query and fragment. Even a validly signed state cannot turn
     * ambiguous, external, backslash, multiply encoded, or injection-shaped destinations into an
     * open redirect.
     */
    let provider = start_mock_provider("nonce-123", json!({"sub": "kevin"})).await;
    let auth = oidc_provider_auth(&provider.issuer);
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let safe_cookie = signed_state_cookie(
        &auth,
        "safe-state",
        "nonce-123",
        "/Project/Plan?tab=activity#history",
    );
    let safe = app
        .clone()
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=safe-state",
            &auth,
            &safe_cookie,
        ))
        .await
        .expect("safe callback redirect");
    assert_eq!(safe.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&safe), "/Project/Plan?tab=activity#history");

    for (index, redirect) in [
        "//evil.example.com",
        "//user:password@evil.example.com",
        "https://evil.example.com",
        "/\\evil.example.com",
        "/%5cevil.example.com",
        "/%255cevil.example.com",
        "/%2fevil.example.com",
        "/%252fevil.example.com",
        "/%250d%250aLocation:%252f%252fevil.example.com",
    ]
    .into_iter()
    .enumerate()
    {
        let state_value = format!("unsafe-state-{index}");
        let state_cookie = signed_state_cookie(&auth, &state_value, "nonce-123", redirect);
        let response = app
            .clone()
            .oneshot(callback_request(
                &format!("/auth/callback?code=auth-code&state={state_value}"),
                &auth,
                &state_cookie,
            ))
            .await
            .expect("unsafe callback redirect");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{redirect:?}");
        assert_eq!(location(&response), "/", "{redirect:?}");
    }
}

#[tokio::test]
async fn oidc_callback_verifies_provider_token_syncs_user_and_sets_session_cookie() {
    /*
     * A complete authorization-code callback exchanges with HTTP Basic client authentication,
     * validates the ID token and nonce, and reconciles the userinfo profile and groups.
     * It persists login metadata, issues a signed session, clears transient state, and returns
     * to the protected local destination.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
            "email_verified": true,
            "name": "Kevin Chien",
            "groups": ["vault-admin", "artists"],
        }),
    )
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let db = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/Project");
    let session_cookie = set_cookie_value(&response, &auth.session_cookie_name);
    let session = verify_session_payload(&auth, &session_cookie).expect("session payload");
    assert_eq!(session["uid"], 1);
    let cookies = all_set_cookies(&response);
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("vault_oidc_state=; Max-Age=0"))
    );

    let row = sqlx::query(
        "SELECT issuer, subject, email, name, last_login_at FROM vault_users WHERE id = 1",
    )
    .fetch_one(&db)
    .await
    .expect("user row");
    assert_eq!(row.get::<String, _>("issuer"), provider.issuer);
    assert_eq!(row.get::<String, _>("subject"), "kevin");
    assert_eq!(row.get::<String, _>("email"), "kevin@example.com");
    assert_eq!(row.get::<String, _>("name"), "Kevin Chien");
    assert!(row.get::<Option<String>, _>("last_login_at").is_some());

    let groups: Vec<String> = sqlx::query_scalar(
        r"
        SELECT vault_groups.name
        FROM vault_groups
        JOIN vault_group_memberships ON vault_group_memberships.group_id = vault_groups.id
        WHERE vault_group_memberships.user_id = 1
        ORDER BY vault_groups.name
        ",
    )
    .fetch_all(&db)
    .await
    .expect("groups");
    assert_eq!(groups, vec!["artists", "vault-admin"]);

    let token_requests = provider.token_requests.lock().expect("token requests");
    assert_eq!(token_requests.len(), 1);
    assert!(
        token_requests[0]
            .authorization
            .as_deref()
            .is_some_and(|value| value.starts_with("Basic "))
    );
    assert!(token_requests[0].body.contains("code=auth-code"));
    assert!(token_requests[0].body.contains("client_id=vault-client"));
}

#[tokio::test]
async fn oidc_callback_rejects_non_boolean_email_verification_as_an_admin_credential() {
    /*
     * A bootstrap-admin email appears in token claims with verification missing, false, or
     * represented by non-boolean values. None of those forms is trusted as an email
     * identity, so no email is stored and no administrative privilege is granted.
     */
    let profiles = [
        (
            "missing",
            json!({"email": "owner@example.com", "groups": ["artists"]}),
        ),
        (
            "false",
            json!({
                "email": "owner@example.com",
                "email_verified": false,
                "groups": ["artists"]
            }),
        ),
        (
            "string",
            json!({
                "email": "owner@example.com",
                "email_verified": "true",
                "groups": ["artists"]
            }),
        ),
        (
            "number",
            json!({
                "email": "owner@example.com",
                "email_verified": 1,
                "groups": ["artists"]
            }),
        ),
    ];

    for (label, profile) in profiles {
        let provider =
            start_mock_provider_with_token_response(json!({"sub": "kevin"}), move |issuer| {
                json!({
                    "id_token": signed_id_token_with_profile(issuer, "nonce-123", &profile),
                    "access_token": "access-token",
                })
            })
            .await;
        let mut auth = oidc_provider_auth(&provider.issuer);
        auth.bootstrap_admin_emails = ["owner@example.com".to_string()].into_iter().collect();
        let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/");
        let (state, _temp_dir) = test_state(auth.clone()).await;
        let db = state.db.clone();
        let app = http::router(state);

        let callback = app
            .clone()
            .oneshot(callback_request(
                "/auth/callback?code=auth-code&state=state-123",
                &auth,
                &state_cookie,
            ))
            .await
            .expect("callback");
        assert_eq!(callback.status(), StatusCode::SEE_OTHER, "{label}");
        let session_cookie = set_cookie_value(&callback, &auth.session_cookie_name);
        let bootstrap = app
            .oneshot(session_request("/api/bootstrap", &auth, &session_cookie))
            .await
            .expect("bootstrap");
        let bootstrap_json = response_json(bootstrap).await;
        let email: Option<String> =
            sqlx::query_scalar("SELECT email FROM vault_users WHERE subject = 'kevin'")
                .fetch_one(&db)
                .await
                .expect("stored email");

        assert_eq!(email, None, "{label}");
        assert_eq!(bootstrap_json["user"]["is_admin"], false, "{label}");
    }
}

#[tokio::test]
async fn oidc_callback_does_not_splice_or_override_verified_email_claims() {
    /*
     * An unverified userinfo email cannot override a separately verified ID-token email or
     * confer bootstrap administration. Conversely, an email from one claim source cannot
     * borrow a verification flag from another source; that cross-source combination stores no
     * email.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "owner@example.com",
            "email_verified": false,
            "groups": ["artists"],
        }),
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.bootstrap_admin_emails = ["owner@example.com".to_string()].into_iter().collect();
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let db = state.db.clone();
    let app = http::router(state);

    let callback = app
        .clone()
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let session_cookie = set_cookie_value(&callback, &auth.session_cookie_name);
    let bootstrap = app
        .oneshot(session_request("/api/bootstrap", &auth, &session_cookie))
        .await
        .expect("bootstrap");
    let bootstrap_json = response_json(bootstrap).await;
    let email: String = sqlx::query_scalar("SELECT email FROM vault_users WHERE subject = 'kevin'")
        .fetch_one(&db)
        .await
        .expect("stored email");

    assert_eq!(email, "claims@example.com");
    assert_eq!(bootstrap_json["user"]["is_admin"], false);

    let cross_source_provider = start_mock_provider_with_token_response(
        json!({"sub": "kevin", "email_verified": true}),
        |issuer| {
            json!({
                "id_token": signed_id_token_with_profile(
                    issuer,
                    "nonce-456",
                    &json!({"email": "owner@example.com", "groups": ["artists"]}),
                ),
                "access_token": "access-token",
            })
        },
    )
    .await;
    let cross_auth = oidc_provider_auth(&cross_source_provider.issuer);
    let cross_state_cookie = signed_state_cookie(&cross_auth, "state-456", "nonce-456", "/");
    let (cross_state, _cross_temp_dir) = test_state(cross_auth.clone()).await;
    let cross_db = cross_state.db.clone();
    let cross_response = http::router(cross_state)
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-456",
            &cross_auth,
            &cross_state_cookie,
        ))
        .await
        .expect("cross-source callback");
    let cross_email: Option<String> =
        sqlx::query_scalar("SELECT email FROM vault_users WHERE subject = 'kevin'")
            .fetch_one(&cross_db)
            .await
            .expect("cross-source email");

    assert_eq!(cross_response.status(), StatusCode::SEE_OTHER);
    assert_eq!(cross_email, None);
}

#[tokio::test]
async fn oidc_bootstrap_admin_subject_grants_admin_without_persisting_an_admin_flag() {
    /*
     * An exactly configured OIDC subject receives effective bootstrap administration and can
     * open the admin directory. This configuration-derived authority does not write a
     * permanent admin bit into the user's database row.
     */
    let provider =
        start_mock_provider("nonce-123", json!({"sub": "kevin", "groups": ["artists"]})).await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.oidc_bootstrap_admin_subjects = ["kevin".to_string()].into_iter().collect();
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let db = state.db.clone();
    let app = http::router(state);

    let callback = app
        .clone()
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let session_cookie = set_cookie_value(&callback, &auth.session_cookie_name);
    let directory = app
        .oneshot(session_request(
            "/api/admin/directory",
            &auth,
            &session_cookie,
        ))
        .await
        .expect("admin directory");
    let stored_admin: i64 =
        sqlx::query_scalar("SELECT is_admin FROM vault_users WHERE subject = 'kevin'")
            .fetch_one(&db)
            .await
            .expect("stored admin flag");

    assert_eq!(directory.status(), StatusCode::OK);
    assert_eq!(stored_admin, 0);
}

#[tokio::test]
async fn oidc_bootstrap_subject_preserves_whitespace_as_part_of_the_opaque_identifier() {
    /*
     * OIDC subjects are opaque identifiers, so surrounding whitespace is preserved rather than
     * normalized away. The subject `" kevin "` therefore remains distinct from configured
     * bootstrap subject `"kevin"` and receives no admin access.
     */
    let provider = start_mock_provider_with_token_response(
        json!({"sub": " kevin ", "groups": ["artists"]}),
        |issuer| {
            json!({
                "id_token": signed_id_token_with_profile(
                    issuer,
                    "nonce-123",
                    &json!({"sub": " kevin ", "groups": ["artists"]}),
                ),
                "access_token": "access-token",
            })
        },
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.oidc_bootstrap_admin_subjects = ["kevin".to_string()].into_iter().collect();
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let callback = app
        .clone()
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let session_cookie = set_cookie_value(&callback, &auth.session_cookie_name);
    let bootstrap = app
        .oneshot(session_request("/api/bootstrap", &auth, &session_cookie))
        .await
        .expect("bootstrap");
    let bootstrap_json = response_json(bootstrap).await;

    assert_eq!(bootstrap_json["user"]["subject"], " kevin ");
    assert_eq!(bootstrap_json["user"]["is_admin"], false);
}

#[tokio::test]
async fn oidc_callback_rejects_whitespace_distinct_id_token_issuer() {
    /*
     * Issuer comparison is exact because whitespace changes the identity-provider identifier
     * rather than being harmless formatting. A correctly signed token with a padded issuer
     * is rejected during ID-token validation.
     */
    let provider = start_mock_provider_with_token_response(json!({"sub": "kevin"}), |issuer| {
        json!({
            "id_token": signed_id_token_with_profile(
                issuer,
                "nonce-123",
                &json!({"iss": format!(" {issuer} ")}),
            ),
            "access_token": "access-token",
        })
    })
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/");
    let (state, _temp_dir) = test_state(auth.clone()).await;

    let response = http::router(state)
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC ID token validation failed"
    );
}

#[tokio::test]
async fn oidc_callback_rejects_state_mismatch_before_provider_exchange() {
    /*
     * The callback state differs from the value bound into its signed cookie.
     * Validation fails locally before attempting discovery or token exchange against the
     * deliberately unreachable provider.
     */
    let auth = oidc_provider_auth("http://127.0.0.1:9");
    let state_cookie = signed_state_cookie(&auth, "expected-state", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=wrong-state",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC state validation failed"
    );
}

#[tokio::test]
async fn oidc_callback_rejects_provider_error_before_provider_exchange() {
    /*
     * An authorization error returned by the provider is surfaced as a failed login immediately.
     * The callback does not attempt a code exchange when the provider has already denied access.
     */
    let auth = oidc_provider_auth("http://127.0.0.1:9");
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?error=access_denied&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC login failed: access_denied"
    );
}

#[tokio::test]
async fn oidc_callback_rejects_missing_code_or_state_before_provider_exchange() {
    /*
     * Both the authorization code and matching state are mandatory callback inputs.
     * Omitting either fails local state validation without contacting the unreachable provider.
     */
    let auth = oidc_provider_auth("http://127.0.0.1:9");
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    for uri in [
        "/auth/callback?state=state-123",
        "/auth/callback?code=auth-code",
    ] {
        let response = app
            .clone()
            .oneshot(callback_request(uri, &auth, &state_cookie))
            .await
            .expect("callback");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response_json(response).await["detail"],
            "OIDC state validation failed"
        );
    }
}

#[tokio::test]
async fn oidc_callback_rejects_token_response_without_id_token() {
    /*
     * The provider accepts the code but returns only an access token.
     * Because authenticated identity requires a verifiable ID token, the callback rejects the
     * response after exactly one exchange.
     */
    let provider = start_mock_provider_with_token_response(
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
        |_issuer| json!({"access_token": "access-token"}),
    )
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC provider did not return an ID token"
    );
    let token_requests = provider.token_requests.lock().expect("token requests");
    assert_eq!(token_requests.len(), 1);
}

#[tokio::test]
async fn oidc_callback_rejects_discovery_without_token_endpoint_before_token_exchange() {
    /*
     * Discovery supplies authorization and verification URLs but no token endpoint.
     * Callback reports that configuration error before sending the authorization code anywhere.
     */
    let provider = start_mock_provider_with_discovery(
        json!({}),
        |_issuer| json!({}),
        |issuer| {
            Some(json!({
                "authorization_endpoint": format!("{issuer}/authorize"),
                "jwks_uri": format!("{issuer}/jwks"),
                "userinfo_endpoint": format!("{issuer}/userinfo"),
            }))
        },
    )
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC token endpoint is missing"
    );
    let token_requests = provider.token_requests.lock().expect("token requests");
    assert!(token_requests.is_empty());
}

#[tokio::test]
async fn oidc_callback_rejects_userinfo_subject_mismatch() {
    /*
     * The signed ID token identifies one subject while userinfo returns a different account.
     * Callback rejects the profile rather than merging claims across two identities.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "different-user",
            "email": "kevin@example.com",
        }),
    )
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC userinfo subject mismatch"
    );
}

#[tokio::test]
async fn oidc_callback_rejects_non_string_userinfo_subject() {
    /*
     * Userinfo returns a numeric subject even though OpenID subject identifiers must be strings
     * matching the token. The malformed value is treated as an identity mismatch and
     * authentication stops.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": 123,
            "email": "kevin@example.com",
            "email_verified": true,
        }),
    )
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;

    let response = http::router(state)
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC userinfo subject mismatch"
    );
}

#[tokio::test]
async fn oidc_callback_rejects_whitespace_distinct_userinfo_subject() {
    /*
     * Userinfo pads the otherwise familiar subject with whitespace.
     * Exact opaque-identifier comparison prevents normalization from conflating it with the ID
     * token's account.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": " kevin ",
            "email": "kevin@example.com",
            "email_verified": true,
        }),
    )
    .await;
    let auth = oidc_provider_auth(&provider.issuer);
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;

    let response = http::router(state)
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(response).await["detail"],
        "OIDC userinfo subject mismatch"
    );
}

#[tokio::test]
async fn oidc_callback_client_auth_none_does_not_send_client_secret() {
    /*
     * A public-client configuration performs the code exchange without HTTP authorization or a
     * client secret form field. The callback still includes the public client identifier and
     * completes successfully.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.oidc_client_auth = "none".to_string();
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let token_requests = provider.token_requests.lock().expect("token requests");
    assert_eq!(token_requests.len(), 1);
    assert_eq!(token_requests[0].authorization, None);
    assert!(!token_requests[0].body.contains("client_secret"));
}

#[tokio::test]
async fn oidc_callback_client_secret_post_sends_secret_in_form_without_basic_auth() {
    /*
     * The client_secret_post mode puts both client id and secret into the token request form.
     * It deliberately omits HTTP Basic authorization and still completes the callback.
     */
    let provider = start_mock_provider(
        "nonce-123",
        json!({
            "sub": "kevin",
            "email": "kevin@example.com",
        }),
    )
    .await;
    let mut auth = oidc_provider_auth(&provider.issuer);
    auth.oidc_client_auth = "client_secret_post".to_string();
    let state_cookie = signed_state_cookie(&auth, "state-123", "nonce-123", "/Project");
    let (state, _temp_dir) = test_state(auth.clone()).await;
    let app = http::router(state);

    let response = app
        .oneshot(callback_request(
            "/auth/callback?code=auth-code&state=state-123",
            &auth,
            &state_cookie,
        ))
        .await
        .expect("callback");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let token_requests = provider.token_requests.lock().expect("token requests");
    assert_eq!(token_requests.len(), 1);
    assert_eq!(token_requests[0].authorization, None);
    assert!(
        token_requests[0]
            .body
            .contains("client_secret=vault-secret")
    );
    assert!(token_requests[0].body.contains("client_id=vault-client"));
}
