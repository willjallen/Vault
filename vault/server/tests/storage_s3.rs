use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, delete, get, head, put};
use futures_util::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use vault_server::blob_lifecycle::{begin_blob_publication, collect_unreferenced_blobs};
use vault_server::db;
use vault_server::storage::{
    BlobReadRange, BlobStorageBackend, BlobWriteKind, S3_UPLOAD_STAGE_FILENAME,
    S3CompatibleBlobStorage, S3StorageSettings, STORAGE_CHUNK_SIZE, StorageError,
    remove_s3_upload_stage_file, sweep_legacy_s3_stage_files,
};

type ObjectMap = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[derive(Clone, Default)]
struct BlockedPutState {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[derive(Clone)]
struct ReadinessMockState {
    head_status: StatusCode,
    head_calls: Arc<AtomicUsize>,
    mutation_calls: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum InventoryMockScenario {
    Paginated,
    PrefixBoundary,
    CyclicToken,
    OutOfPrefixKey,
    MissingTruncationFlag,
    MissingStableIdentity,
    EmptyStable,
    NonEmptyStable,
    ChangesBetweenPasses,
    ListingDenied,
}

#[derive(Clone)]
struct InventoryMockState {
    scenario: InventoryMockScenario,
    queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

#[tokio::test]
async fn s3_compatible_storage_puts_reads_ranges_and_deletes_objects() {
    /*
     * The S3-compatible backend must round-trip a content-addressed payload with correct backend
     * metadata. Buffered ranges and bounded streaming return the requested bytes, and
     * deletion makes the key unreadable.
     */
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");

    let content = b"hello remote storage";
    let digest = sha256_hex(content);
    let stored = storage.put_bytes(content).await.expect("put bytes");

    assert_eq!(stored.backend, "s3");
    assert_eq!(stored.bucket, "vault-test");
    assert_eq!(stored.hash_algo, "sha256");
    assert_eq!(stored.digest, digest);
    assert_eq!(stored.object_key, format!("objects/sha256/{digest}"));
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("read bytes"),
        content,
    );
    assert_eq!(
        storage
            .read_range(&stored.object_key, 6, 11)
            .await
            .expect("read range"),
        b"remote",
    );
    let mut stream = storage
        .stream_range(
            &stored.object_key,
            BlobReadRange {
                expected_size: content.len() as u64,
                offset: 6,
                length: 6,
            },
        )
        .await
        .expect("range stream");
    let mut streamed = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("range stream chunk");
        assert!(chunk.len() <= STORAGE_CHUNK_SIZE);
        streamed.extend_from_slice(&chunk);
    }
    assert_eq!(streamed, b"remote");

    storage
        .delete_object(&stored.object_key)
        .await
        .expect("delete object");
    assert!(matches!(
        storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound),
    ));
}

#[tokio::test]
async fn s3_object_inventory_collects_and_sorts_every_page() {
    /*
     * Remote inventory follows the provider's continuation token through a second page and
     * returns a deterministic key order. The mock also records the exact prefix and token sent
     * on each request so pagination cannot silently restart from the first page.
     */
    let (endpoint_url, state) = start_s3_inventory_mock(InventoryMockScenario::Paginated).await;
    let storage = s3_inventory_storage(&endpoint_url, "tenant-a").await;

    let inventory = storage
        .inventory_objects()
        .await
        .expect("paginated object inventory");

    assert_eq!(inventory.len(), 2);
    assert_eq!(inventory[0].object_key, "tenant-a/sha256/aaa");
    assert_eq!(inventory[0].size_bytes, 3);
    assert_eq!(inventory[0].etag.as_deref(), Some("\"etag-a\""));
    assert_eq!(inventory[1].object_key, "tenant-a/sha256/zzz");
    assert_eq!(inventory[1].size_bytes, 9);
    assert_eq!(inventory[1].etag.as_deref(), Some("\"etag-z\""));

    let queries = state.queries.lock().await;
    assert_eq!(queries.len(), 2);
    assert_eq!(
        queries[0].get("prefix").map(String::as_str),
        Some("tenant-a/")
    );
    assert!(!queries[0].contains_key("continuation-token"));
    assert_eq!(
        queries[1].get("continuation-token").map(String::as_str),
        Some("page-two"),
    );
    assert_eq!(
        queries[1].get("prefix").map(String::as_str),
        Some("tenant-a/")
    );
}

#[tokio::test]
async fn s3_object_inventory_uses_a_prefix_path_boundary() {
    /*
     * The configured `objects` namespace must be listed as `objects/`. The mock applies the
     * supplied prefix to a catalog containing an adjacent `objectscape` key, proving that the
     * inventory cannot absorb another namespace which merely shares its leading characters.
     */
    let (endpoint_url, state) =
        start_s3_inventory_mock(InventoryMockScenario::PrefixBoundary).await;
    let storage = s3_inventory_storage(&endpoint_url, "objects").await;

    let inventory = storage
        .inventory_objects()
        .await
        .expect("boundary-safe object inventory");

    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].object_key, "objects/sha256/inside");
    let queries = state.queries.lock().await;
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0].get("prefix").map(String::as_str),
        Some("objects/")
    );
}

#[tokio::test]
async fn s3_object_inventory_rejects_a_cyclic_continuation_token() {
    /*
     * A broken provider repeats the same continuation token while claiming both responses are
     * truncated. Inventory must stop with a remote error after the repeat instead of looping or
     * repeatedly appending the same objects.
     */
    let (endpoint_url, state) = start_s3_inventory_mock(InventoryMockScenario::CyclicToken).await;
    let storage = s3_inventory_storage(&endpoint_url, "objects").await;

    let error = storage
        .inventory_objects()
        .await
        .expect_err("cyclic continuation token must fail");

    match error {
        StorageError::Remote(message) => {
            assert!(message.contains("cycled a continuation token"));
        }
        other => panic!("expected remote inventory error, got {other:?}"),
    }
    let queries = state.queries.lock().await;
    assert_eq!(queries.len(), 2);
    assert!(!queries[0].contains_key("continuation-token"));
    assert_eq!(
        queries[1].get("continuation-token").map(String::as_str),
        Some("repeat-me"),
    );
}

#[tokio::test]
async fn s3_object_inventory_rejects_a_provider_key_outside_the_requested_prefix() {
    /*
     * Even with the correct `objects/` request, a faulty provider returns an adjacent
     * `objectscape` key. Inventory must reject the response instead of trusting remote prefix
     * filtering and treating the foreign key as managed Vault data.
     */
    let (endpoint_url, state) =
        start_s3_inventory_mock(InventoryMockScenario::OutOfPrefixKey).await;
    let storage = s3_inventory_storage(&endpoint_url, "objects").await;

    let error = storage
        .inventory_objects()
        .await
        .expect_err("out-of-prefix provider key must fail");

    assert!(matches!(error, StorageError::Remote(_)));
    let queries = state.queries.lock().await;
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0].get("prefix").map(String::as_str),
        Some("objects/")
    );
}

#[tokio::test]
async fn integrity_check_completes_two_stable_remote_inventory_passes() {
    let (endpoint_url, state) = start_s3_inventory_mock(InventoryMockScenario::EmptyStable).await;

    let (status, report) = run_s3_integrity_check(&endpoint_url).await;

    assert_eq!(status, Some(0), "{report:#}");
    assert_eq!(report["result"], "pass");
    assert_eq!(state.queries.lock().await.len(), 2);
}

#[tokio::test]
async fn integrity_check_accepts_a_stable_nonempty_remote_inventory_snapshot() {
    /*
     * A noncanonical key deliberately produces a warning without requiring a GET. More
     * importantly, its unchanged ETag-backed identity must compare equal across both listing
     * passes; an empty-only stability test cannot detect entry-fingerprint asymmetry.
     */
    let (endpoint_url, state) =
        start_s3_inventory_mock(InventoryMockScenario::NonEmptyStable).await;

    let (status, report) = run_s3_integrity_check(&endpoint_url).await;

    assert_eq!(status, Some(1), "{report:#}");
    assert_eq!(report["result"], "warnings");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .all(|finding| finding["code"] != "storage.remote_inventory_changed")
    );
    assert_eq!(state.queries.lock().await.len(), 2);
}

#[tokio::test]
async fn integrity_check_marks_remote_inventory_changes_incomplete() {
    let (endpoint_url, state) =
        start_s3_inventory_mock(InventoryMockScenario::ChangesBetweenPasses).await;

    let (status, report) = run_s3_integrity_check(&endpoint_url).await;

    assert_eq!(status, Some(2), "{report:#}");
    assert_eq!(report["result"], "incomplete");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|finding| finding["code"] == "storage.remote_inventory_changed")
    );
    assert_eq!(state.queries.lock().await.len(), 2);
}

#[tokio::test]
async fn integrity_check_marks_denied_remote_listing_incomplete() {
    let (endpoint_url, state) = start_s3_inventory_mock(InventoryMockScenario::ListingDenied).await;

    let (status, report) = run_s3_integrity_check(&endpoint_url).await;

    assert_eq!(status, Some(2), "{report:#}");
    assert_eq!(report["result"], "incomplete");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|finding| finding["code"] == "storage.remote_inventory_unavailable")
    );
    assert_eq!(state.queries.lock().await.len(), 1);
}

#[tokio::test]
async fn s3_object_inventory_requires_an_explicit_truncation_flag() {
    /*
     * A response which omits `IsTruncated` leaves pagination completeness unknowable.
     * Inventory must fail closed rather than interpreting the absent field as the final page and
     * reporting a potentially incomplete object set.
     */
    let (endpoint_url, state) =
        start_s3_inventory_mock(InventoryMockScenario::MissingTruncationFlag).await;
    let storage = s3_inventory_storage(&endpoint_url, "objects").await;

    let error = storage
        .inventory_objects()
        .await
        .expect_err("missing truncation flag must fail");

    assert!(matches!(error, StorageError::Remote(_)));
    let queries = state.queries.lock().await;
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0].get("prefix").map(String::as_str),
        Some("objects/")
    );
}

#[tokio::test]
async fn s3_object_inventory_requires_a_stable_object_identity() {
    let (endpoint_url, state) =
        start_s3_inventory_mock(InventoryMockScenario::MissingStableIdentity).await;
    let storage = s3_inventory_storage(&endpoint_url, "objects").await;

    let error = storage
        .inventory_objects()
        .await
        .expect_err("object without ETag or modification time must fail");

    assert!(matches!(error, StorageError::Remote(_)));
    assert_eq!(state.queries.lock().await.len(), 1);
}

#[tokio::test]
async fn s3_compatible_storage_overwrites_existing_digest_key_with_new_bytes() {
    /*
     * The mock bucket starts with incorrect data at the key derived from the correct payload's
     * digest. Uploading that payload must repair the remote object rather than accepting the
     * occupied key as a valid deduplicated copy.
     */
    let (endpoint_url, objects) = start_s3_mock_with_objects().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let content = b"correct remote bytes";
    let digest = sha256_hex(content);
    let object_key = format!("objects/sha256/{digest}");
    objects
        .lock()
        .await
        .insert(object_key.clone(), b"wrong remote bytes".to_vec());

    let stored = storage.put_bytes(content).await.expect("put bytes");

    assert_eq!(stored.object_key, object_key);
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("read repaired remote"),
        content,
    );
}

#[tokio::test]
async fn s3_full_object_stream_is_bounded_across_multiple_mebibytes() {
    /*
     * A remote object larger than two internal chunks is streamed to completion.
     * Every emitted frame stays within the memory bound, while their total exactly matches the
     * source size.
     */
    let (endpoint_url, objects) = start_s3_mock_with_objects().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let size = STORAGE_CHUNK_SIZE * 2 + 17;
    let object_key = "objects/large-stream".to_string();
    objects
        .lock()
        .await
        .insert(object_key.clone(), vec![b'z'; size]);

    let mut stream = storage
        .stream_range(
            &object_key,
            BlobReadRange {
                expected_size: size as u64,
                offset: 0,
                length: size as u64,
            },
        )
        .await
        .expect("full object stream");
    let mut streamed = 0_usize;
    let mut chunks = 0_usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("full object chunk");
        assert!(!chunk.is_empty());
        assert!(chunk.len() <= STORAGE_CHUNK_SIZE);
        streamed += chunk.len();
        chunks += 1;
    }

    assert_eq!(streamed, size);
    assert!(chunks >= 3);
}

#[tokio::test]
async fn s3_range_stream_rejects_a_provider_that_ignores_the_range() {
    /*
     * The fake provider ignores a range request and responds with the full object and a normal
     * success status. Storage must recognize the response contract violation before exposing
     * those unintended bytes.
     */
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    let app = Router::new().route(
        "/{bucket}/{*key}",
        get(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, 6)
                .body(Body::from("abcdef"))
                .expect("ignored range response")
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("ignored range mock");
    });
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url(addr)),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");

    let result = storage
        .stream_range(
            "object",
            BlobReadRange {
                expected_size: 6,
                offset: 2,
                length: 2,
            },
        )
        .await;

    assert!(matches!(result, Err(StorageError::ContentMismatch)));
}

#[tokio::test]
async fn s3_compatible_storage_rejects_missing_bucket_configuration() {
    /*
     * Constructing an R2 backend with an empty bucket must fail during configuration validation.
     * The diagnostic identifies the Vault bucket setting the operator needs to supply.
     */
    let error = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "r2".to_string(),
        bucket: String::new(),
        region: "auto".to_string(),
        endpoint_url: Some("http://127.0.0.1:1".to_string()),
        allow_insecure_local_http: true,
        access_key_id: Some("access".to_string()),
        secret_access_key: Some("secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect_err("missing bucket error");

    assert!(matches!(error, StorageError::Configuration(_)));
    assert!(error.to_string().contains("VAULT_R2_BUCKET"));
}

fn endpoint_policy_settings(
    endpoint_url: &str,
    allow_insecure_local_http: bool,
) -> S3StorageSettings {
    S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url.to_string()),
        allow_insecure_local_http,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    }
}

#[tokio::test]
async fn s3_readiness_uses_non_mutating_head_bucket_and_maps_failure() {
    /*
     * Readiness probes a successful and a forbidden bucket using HEAD only.
     * It reports remote failure accurately and never performs a write or other mutating request
     * as part of health checking.
     */
    for (head_status, should_succeed) in [(StatusCode::OK, true), (StatusCode::FORBIDDEN, false)] {
        let (endpoint_url, state) = start_s3_readiness_mock(head_status).await;
        let storage =
            S3CompatibleBlobStorage::from_settings(endpoint_policy_settings(&endpoint_url, true))
                .await
                .expect("S3 readiness storage");

        let result = storage.readiness_check().await;

        assert_eq!(result.is_ok(), should_succeed, "{result:?}");
        assert!(
            should_succeed || matches!(&result, Err(StorageError::Remote(_))),
            "{result:?}"
        );
        assert!(state.head_calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(state.mutation_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn s3_endpoint_policy_accepts_https_and_explicit_loopback_http() {
    /*
     * Secure endpoints are accepted normally, while cleartext HTTP is allowed only when
     * explicitly enabled for recognized loopback spellings. The accepted set covers
     * localhost subdomains plus IPv4, IPv6, and mapped-loopback numeric forms.
     */
    for (endpoint_url, allow_insecure_local_http) in [
        ("https://s3.example.test", false),
        ("http://localhost:9000", true),
        ("http://minio.localhost:9000", true),
        ("http://127.42.1.9:9000", true),
        ("http://2130706433:9000", true),
        ("http://0177.0.0.1:9000", true),
        ("http://0x7f000001:9000", true),
        ("http://[::1]:9000", true),
        ("http://[::ffff:127.42.1.9]:9000", true),
    ] {
        let result = S3CompatibleBlobStorage::from_settings(endpoint_policy_settings(
            endpoint_url,
            allow_insecure_local_http,
        ))
        .await;

        assert!(result.is_ok(), "{endpoint_url}: {result:?}");
    }
}

#[tokio::test]
async fn s3_endpoint_policy_rejects_http_without_explicit_opt_in() {
    /*
     * Even a loopback endpoint must use HTTPS unless the insecure-local option is deliberately
     * enabled. Validation rejects the default cleartext configuration with an actionable
     * protocol error.
     */
    let error = S3CompatibleBlobStorage::from_settings(endpoint_policy_settings(
        "http://127.0.0.1:9000",
        false,
    ))
    .await
    .expect_err("insecure endpoint must be rejected by default");

    assert!(matches!(error, StorageError::Configuration(_)));
    assert!(error.to_string().contains("must use HTTPS"));
}

#[tokio::test]
async fn s3_endpoint_policy_rejects_non_loopback_http_even_with_opt_in() {
    /*
     * The local-development HTTP exception must not extend to public, private-network,
     * link-local, carrier-grade NAT, unspecified, or deceptive hostnames. Each candidate is
     * rejected without echoing the potentially sensitive endpoint in the error.
     */
    for endpoint_url in [
        "http://example.test:9000",
        "http://8.8.8.8:9000",
        "http://localhost.evil:9000",
        "http://localhost.:9000",
        "http://10.0.0.1:9000",
        "http://172.16.0.1:9000",
        "http://192.168.0.1:9000",
        "http://169.254.1.1:9000",
        "http://[fe80::1]:9000",
        "http://[::ffff:10.0.0.1]:9000",
        "http://100.64.0.1:9000",
        "http://0.0.0.0:9000",
        "http://[::]:9000",
        "http://minio.local:9000",
        "http://minio:9000",
    ] {
        let error =
            S3CompatibleBlobStorage::from_settings(endpoint_policy_settings(endpoint_url, true))
                .await
                .expect_err("non-loopback HTTP endpoint must be rejected");

        assert!(
            matches!(error, StorageError::Configuration(_)),
            "{endpoint_url}: {error:?}"
        );
        assert!(
            error.to_string().contains("only for loopback hosts"),
            "{endpoint_url}: {error}"
        );
        assert!(!error.to_string().contains(endpoint_url));
    }
}

#[tokio::test]
async fn s3_endpoint_policy_rejects_non_http_and_malformed_urls() {
    /*
     * Endpoint parsing rejects unsupported schemes, malformed hosts, embedded credentials, and
     * query or fragment tricks. Errors remain specific enough to diagnose configuration
     * while redacting the supplied URL.
     */
    for (endpoint_url, message) in [
        ("ftp://localhost/bucket", "must use HTTP or HTTPS"),
        ("not a URL", "URL is invalid"),
        ("https://[::1", "URL is invalid"),
        ("http://evil@localhost:9000", "URL is invalid"),
        ("http://localhost@evil:9000", "URL is invalid"),
        ("http://user:pass@localhost:9000", "URL is invalid"),
        ("http://localhost:9000?bucket=other", "URL is invalid"),
        ("http://localhost:9000#other", "URL is invalid"),
    ] {
        let error =
            S3CompatibleBlobStorage::from_settings(endpoint_policy_settings(endpoint_url, true))
                .await
                .expect_err("invalid endpoint must be rejected");

        assert!(
            matches!(error, StorageError::Configuration(_)),
            "{endpoint_url}: {error:?}"
        );
        assert!(
            error.to_string().contains(message),
            "{endpoint_url}: {error}"
        );
        assert!(!error.to_string().contains(endpoint_url));
    }
}

#[test]
fn s3_storage_settings_treat_unknown_insecure_http_flag_as_disabled() {
    /*
     * An unrecognized value for the insecure-local HTTP environment flag must fail closed.
     * Settings therefore leave cleartext loopback access disabled instead of guessing operator
     * intent.
     */
    let settings = S3StorageSettings::s3_from_env_with("objects", |name| {
        (name == "VAULT_S3_ALLOW_INSECURE_LOCAL_HTTP").then(|| "sometimes".to_string())
    });

    assert!(!settings.allow_insecure_local_http);
}

#[test]
fn s3_storage_settings_use_vault_env_with_aws_credential_fallbacks() {
    /*
     * Generic S3 settings take Vault-specific bucket, region, endpoint, prefix, and transport
     * values from the environment. Standard AWS credential variables remain valid fallbacks,
     * including the optional session token.
     */
    let env = HashMap::from([
        ("VAULT_S3_BUCKET", "vault-prod"),
        ("VAULT_S3_REGION", "us-west-2"),
        ("VAULT_S3_ENDPOINT_URL", "https://s3.example.test"),
        ("VAULT_S3_ALLOW_INSECURE_LOCAL_HTTP", "1"),
        ("AWS_ACCESS_KEY_ID", "aws-access"),
        ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
        ("AWS_SESSION_TOKEN", "aws-session"),
    ]);

    let settings = S3StorageSettings::s3_from_env_with("tenant-a", |name| {
        env.get(name).map(|value| (*value).to_string())
    });

    assert_eq!(settings.name, "s3");
    assert_eq!(settings.bucket, "vault-prod");
    assert_eq!(settings.region, "us-west-2");
    assert_eq!(
        settings.endpoint_url.as_deref(),
        Some("https://s3.example.test")
    );
    assert!(settings.allow_insecure_local_http);
    assert_eq!(settings.access_key_id.as_deref(), Some("aws-access"));
    assert_eq!(settings.secret_access_key.as_deref(), Some("aws-secret"));
    assert_eq!(settings.session_token.as_deref(), Some("aws-session"));
    assert_eq!(settings.prefix, "tenant-a");
}

#[test]
fn r2_storage_settings_derive_endpoint_from_account_id() {
    /*
     * R2 configuration builds Cloudflare's endpoint from the account identifier and uses the
     * provider's automatic region. It also selects the R2 credential variables and preserves
     * the configured object prefix.
     */
    let env = HashMap::from([
        ("VAULT_R2_BUCKET", "vault-r2"),
        ("VAULT_R2_ACCOUNT_ID", "acct123"),
        ("VAULT_R2_ACCESS_KEY_ID", "r2-access"),
        ("VAULT_R2_SECRET_ACCESS_KEY", "r2-secret"),
        ("VAULT_R2_ALLOW_INSECURE_LOCAL_HTTP", "true"),
    ]);

    let settings = S3StorageSettings::r2_from_env_with("objects", |name| {
        env.get(name).map(|value| (*value).to_string())
    });

    assert_eq!(settings.name, "r2");
    assert_eq!(settings.bucket, "vault-r2");
    assert_eq!(settings.region, "auto");
    assert_eq!(
        settings.endpoint_url.as_deref(),
        Some("https://acct123.r2.cloudflarestorage.com"),
    );
    assert!(settings.allow_insecure_local_http);
    assert_eq!(settings.access_key_id.as_deref(), Some("r2-access"));
    assert_eq!(settings.secret_access_key.as_deref(), Some("r2-secret"));
    assert_eq!(settings.session_token, None);
    assert_eq!(settings.prefix, "objects");
}

#[tokio::test]
async fn s3_compatible_storage_promotes_part_files_as_content_addressed_object() {
    /*
     * Two local upload parts are staged, concatenated, verified against the expected digest, and
     * uploaded as one R2 object. Returned metadata and remote bytes match the combined
     * payload, and the temporary staging file is removed.
     */
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "r2".to_string(),
        bucket: "vault-parts".to_string(),
        region: "auto".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "tenant-a".to_string(),
    })
    .await
    .expect("r2 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let first = temp_dir.path().join("00000001.part");
    let second = temp_dir.path().join("00000002.part");
    tokio::fs::write(&first, b"hello ")
        .await
        .expect("first part");
    tokio::fs::write(&second, b"world")
        .await
        .expect("second part");
    let combined = b"hello world";
    let digest = sha256_hex(combined);

    let stored = storage
        .put_part_files_in_staging(&[first, second], Some(&digest), temp_dir.path())
        .await
        .expect("put part files");

    assert_eq!(stored.backend, "r2");
    assert_eq!(stored.bucket, "vault-parts");
    assert_eq!(stored.digest, digest);
    assert_eq!(stored.size_bytes, combined.len() as u64);
    assert_eq!(stored.object_key, format!("tenant-a/sha256/{digest}"));
    assert!(!temp_dir.path().join(S3_UPLOAD_STAGE_FILENAME).exists());
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("uploaded object"),
        combined,
    );
}

#[tokio::test]
async fn s3_compatible_storage_rejects_part_file_checksum_mismatch_without_uploading() {
    /*
     * The expected digest intentionally describes different bytes than the staged part.
     * Verification fails before remote publication and cleans the local staging file, leaving no
     * object under the actual digest either.
     */
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-parts".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part = temp_dir.path().join("00000001.part");
    tokio::fs::write(&part, b"actual bytes")
        .await
        .expect("part");
    let actual_digest = sha256_hex(b"actual bytes");
    let wrong_digest = sha256_hex(b"different bytes");

    let error = storage
        .put_part_files_in_staging(&[part], Some(&wrong_digest), temp_dir.path())
        .await
        .expect_err("checksum mismatch");

    assert!(matches!(error, StorageError::ChecksumMismatch));
    assert!(!temp_dir.path().join(S3_UPLOAD_STAGE_FILENAME).exists());
    assert!(matches!(
        storage
            .read_bytes(&format!("objects/sha256/{actual_digest}"))
            .await,
        Err(StorageError::NotFound),
    ));
}

#[tokio::test]
async fn s3_staged_part_upload_supports_empty_objects() {
    /*
     * An upload with no part files still represents a legitimate zero-byte object.
     * It is published under the empty-content digest, reads back empty, and leaves no staging
     * artifact.
     */
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-parts".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let digest = sha256_hex(&[]);

    let stored = storage
        .put_part_files_in_staging(&[], Some(&digest), temp_dir.path())
        .await
        .expect("empty staged upload");

    assert_eq!(stored.digest, digest);
    assert_eq!(stored.size_bytes, 0);
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("empty object"),
        b"",
    );
    assert!(!temp_dir.path().join(S3_UPLOAD_STAGE_FILENAME).exists());
}

#[tokio::test]
async fn s3_part_staging_is_session_local_and_cancel_safe() {
    /*
     * While a remote PUT is blocked, the assembled payload must exist only inside that upload
     * session and contain the exact combined bytes. Cancelling the task removes the
     * session-local staging file through its cleanup guard.
     */
    let (endpoint_url, blocked_put) = start_blocked_s3_put_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-parts".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let session_dir = temp_dir.path().join("transfers/uploads/session");
    tokio::fs::create_dir_all(&session_dir)
        .await
        .expect("session dir");
    let first = session_dir.join("00000001.part");
    let second = session_dir.join("00000002.part");
    tokio::fs::write(&first, b"hello ")
        .await
        .expect("first part");
    tokio::fs::write(&second, b"world")
        .await
        .expect("second part");
    let digest = sha256_hex(b"hello world");
    let task_session_dir = session_dir.clone();
    let upload = tokio::spawn(async move {
        storage
            .put_part_files_in_staging(&[first, second], Some(&digest), &task_session_dir)
            .await
    });

    timeout(Duration::from_secs(5), blocked_put.entered.notified())
        .await
        .expect("S3 PUT started");
    let stage_path = session_dir.join(S3_UPLOAD_STAGE_FILENAME);
    assert_eq!(
        tokio::fs::read(&stage_path).await.expect("stage bytes"),
        b"hello world",
    );

    upload.abort();
    let error = upload.await.expect_err("cancelled upload task");
    assert!(error.is_cancelled());
    assert!(!stage_path.exists());
    blocked_put.release.notify_waiters();
}

#[cfg(unix)]
#[tokio::test]
async fn s3_stage_cleanup_refuses_symlinks() {
    /*
     * A staging filename is replaced with a symlink to an outside file before cleanup.
     * Cleanup rejects the unsafe path and preserves both the link and its external target.
     */
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let session_dir = temp_dir.path().join("session");
    tokio::fs::create_dir_all(&session_dir)
        .await
        .expect("session dir");
    let outside = temp_dir.path().join("outside");
    tokio::fs::write(&outside, b"outside")
        .await
        .expect("outside file");
    let stage_path = session_dir.join(S3_UPLOAD_STAGE_FILENAME);
    symlink(&outside, &stage_path).expect("stage symlink");

    let error = remove_s3_upload_stage_file(&session_dir)
        .await
        .expect_err("symlink must be refused");

    assert!(matches!(error, StorageError::InvalidStoragePath));
    assert!(
        tokio::fs::symlink_metadata(&stage_path)
            .await
            .expect("stage metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        tokio::fs::read(&outside).await.expect("outside bytes"),
        b"outside",
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_s3_stage_sweep_is_aged_bounded_and_symlink_safe() {
    /*
     * Legacy cleanup must honor its work limit and age threshold while matching only the exact
     * temporary-file naming scheme. It deletes the old regular file but preserves fresh
     * files, lookalikes, directories, symlinks, outside targets, and an unsafe root.
     */
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let legacy_name = |digit: char| format!("vault-s3-upload-{}.tmp", digit.to_string().repeat(32));
    let old_name = legacy_name('0');
    let fresh_name = legacy_name('1');
    let symlink_name = legacy_name('2');
    let directory_name = legacy_name('3');
    let near_miss_name = format!("vault-s3-upload-{}.tmp", "A".repeat(32));
    let old_path = temp_dir.path().join(&old_name);
    tokio::fs::write(&old_path, b"old stage")
        .await
        .expect("old stage");
    tokio::fs::write(temp_dir.path().join(&fresh_name), b"fresh stage")
        .await
        .expect("fresh stage");
    tokio::fs::create_dir(temp_dir.path().join(&directory_name))
        .await
        .expect("lookalike directory");
    tokio::fs::write(temp_dir.path().join(&near_miss_name), b"near miss")
        .await
        .expect("near-miss stage");
    let outside = temp_dir.path().join("outside");
    tokio::fs::write(&outside, b"outside")
        .await
        .expect("outside file");
    symlink(&outside, temp_dir.path().join(&symlink_name)).expect("legacy stage symlink");
    std::fs::File::open(&old_path)
        .expect("old stage handle")
        .set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_hours(2)),
        )
        .expect("old stage mtime");

    assert!(
        sweep_legacy_s3_stage_files(temp_dir.path(), Duration::ZERO, 0)
            .await
            .expect("zero-work sweep")
            .is_empty()
    );
    assert!(old_path.is_file());
    assert!(matches!(
        sweep_legacy_s3_stage_files(std::path::Path::new("/."), Duration::ZERO, 1,).await,
        Err(StorageError::InvalidStoragePath)
    ));

    let deleted = sweep_legacy_s3_stage_files(temp_dir.path(), Duration::from_hours(1), 128)
        .await
        .expect("legacy stage sweep");

    assert_eq!(deleted, vec![old_name]);
    assert!(temp_dir.path().join(fresh_name).is_file());
    assert!(temp_dir.path().join(directory_name).is_dir());
    assert!(temp_dir.path().join(near_miss_name).is_file());
    assert!(
        tokio::fs::symlink_metadata(temp_dir.path().join(symlink_name))
            .await
            .expect("legacy symlink metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        tokio::fs::read(outside).await.expect("outside bytes"),
        b"outside"
    );
}

#[tokio::test]
async fn blob_lifecycle_garbage_collection_deletes_s3_object_and_metadata() {
    /*
     * An S3 object is published with blob metadata but deliberately receives no live reference.
     * Garbage collection must report and remove both the remote payload and its database row
     * without recording a failure.
     */
    let (endpoint_url, objects) = start_s3_mock_with_objects().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-gc".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp_dir.path().join("vault.db"))
        .await
        .expect("database");
    let content = b"remote garbage collection";
    let digest = sha256_hex(content);
    let publication = begin_blob_publication(
        &pool,
        &storage,
        "sha256",
        &digest,
        content.len() as u64,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("publication lease");
    let stored = publication
        .run_storage(storage.put_bytes(content))
        .await
        .expect("put object");
    let mut transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("metadata transaction");
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, &stored)
        .await
        .expect("prepare metadata");
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish metadata");
    transaction.commit().await.expect("commit metadata");
    drop(publication);

    let result = collect_unreferenced_blobs(&pool, &storage)
        .await
        .expect("garbage collection");

    assert_eq!(result.deleted_blob_ids, vec![blob_id]);
    assert_eq!(result.deleted_objects, vec![stored.object_key.clone()]);
    assert!(result.failures.is_empty());
    assert!(!objects.lock().await.contains_key(&stored.object_key));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blobs WHERE id = ?")
            .bind(blob_id)
            .fetch_one(&pool)
            .await
            .expect("blob count"),
        0,
    );
}

async fn start_s3_mock() -> String {
    start_s3_mock_with_objects().await.0
}

async fn s3_inventory_storage(endpoint_url: &str, prefix: &str) -> S3CompatibleBlobStorage {
    S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url.to_string()),
        allow_insecure_local_http: true,
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: prefix.to_string(),
    })
    .await
    .expect("S3 inventory storage")
}

async fn run_s3_integrity_check(endpoint_url: &str) -> (Option<i32>, Value) {
    let data_dir = tempfile::tempdir().expect("temporary remote Vault");
    let pool = db::connect(&data_dir.path().join("vault.db"))
        .await
        .expect("initialize remote Vault database");
    pool.close().await;
    std::fs::create_dir(data_dir.path().join("transfers")).expect("create transfer root");
    let output = Command::new(env!("CARGO_BIN_EXE_vault-server"))
        .args([
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temporary path"),
            "--storage-backend",
            "s3",
            "integrity-check",
            "--format",
            "json",
        ])
        .env("VAULT_S3_BUCKET", "vault-test")
        .env("VAULT_STORAGE_PREFIX", "objects")
        .env("VAULT_S3_REGION", "us-east-1")
        .env("VAULT_S3_ENDPOINT_URL", endpoint_url)
        .env("VAULT_S3_ALLOW_INSECURE_LOCAL_HTTP", "true")
        .env("VAULT_S3_ACCESS_KEY_ID", "test-access")
        .env("VAULT_S3_SECRET_ACCESS_KEY", "test-secret")
        .env_remove("VAULT_DB_PATH")
        .env_remove("VAULT_OBJECTS_PATH")
        .env_remove("VAULT_TRANSFERS_PATH")
        .output()
        .await
        .expect("run remote integrity check");
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "remote integrity output was not JSON: {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code(), report)
}

async fn start_s3_inventory_mock(scenario: InventoryMockScenario) -> (String, InventoryMockState) {
    let state = InventoryMockState {
        scenario,
        queries: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/{bucket}", get(mock_list_objects))
        .route("/{bucket}/", get(mock_list_objects))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("S3 inventory mock");
    });
    (endpoint_url(addr), state)
}

async fn mock_list_objects(
    State(state): State<InventoryMockState>,
    Path(_bucket): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let request_number = {
        let mut queries = state.queries.lock().await;
        queries.push(query.clone());
        queries.len()
    };
    if matches!(state.scenario, InventoryMockScenario::ListingDenied) {
        return empty_response(StatusCode::FORBIDDEN);
    }
    let continuation_token = query.get("continuation-token").map(String::as_str);
    let body = match (state.scenario, continuation_token) {
        (InventoryMockScenario::Paginated, None) => PAGINATED_INVENTORY_FIRST_PAGE,
        (InventoryMockScenario::Paginated, Some("page-two")) => PAGINATED_INVENTORY_SECOND_PAGE,
        (InventoryMockScenario::PrefixBoundary, None) => {
            if query.get("prefix").map(String::as_str) == Some("objects/") {
                BOUNDARY_SAFE_INVENTORY
            } else {
                BOUNDARY_UNSAFE_INVENTORY
            }
        }
        (InventoryMockScenario::CyclicToken, None | Some("repeat-me")) => {
            CYCLIC_TOKEN_INVENTORY_PAGE
        }
        (InventoryMockScenario::OutOfPrefixKey, None) => OUT_OF_PREFIX_INVENTORY_PAGE,
        (InventoryMockScenario::MissingTruncationFlag, None) => {
            MISSING_TRUNCATION_FLAG_INVENTORY_PAGE
        }
        (InventoryMockScenario::MissingStableIdentity, None) => {
            MISSING_STABLE_IDENTITY_INVENTORY_PAGE
        }
        (InventoryMockScenario::EmptyStable, None) => EMPTY_INVENTORY_PAGE,
        (InventoryMockScenario::NonEmptyStable, None) => STABLE_NONEMPTY_INVENTORY_PAGE,
        (InventoryMockScenario::ChangesBetweenPasses, None) if request_number == 1 => {
            EMPTY_INVENTORY_PAGE
        }
        (InventoryMockScenario::ChangesBetweenPasses, None) => CHANGED_INVENTORY_PAGE,
        _ => return empty_response(StatusCode::BAD_REQUEST),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(body))
        .expect("list objects response")
}

const PAGINATED_INVENTORY_FIRST_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>tenant-a/</Prefix><KeyCount>1</KeyCount><MaxKeys>1</MaxKeys>
<IsTruncated>true</IsTruncated><NextContinuationToken>page-two</NextContinuationToken>
<Contents><Key>tenant-a/sha256/zzz</Key><ETag>&quot;etag-z&quot;</ETag><Size>9</Size></Contents>
</ListBucketResult>"#;

const PAGINATED_INVENTORY_SECOND_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>tenant-a/</Prefix><KeyCount>1</KeyCount><MaxKeys>1</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>tenant-a/sha256/aaa</Key><ETag>&quot;etag-a&quot;</ETag><Size>3</Size></Contents>
</ListBucketResult>"#;

const BOUNDARY_SAFE_INVENTORY: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>objects/sha256/inside</Key><ETag>&quot;inside&quot;</ETag><Size>6</Size></Contents>
</ListBucketResult>"#;

const BOUNDARY_UNSAFE_INVENTORY: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects</Prefix><KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>objects/sha256/inside</Key><ETag>&quot;inside&quot;</ETag><Size>6</Size></Contents>
<Contents><Key>objectscape/outside</Key><ETag>&quot;outside&quot;</ETag><Size>7</Size></Contents>
</ListBucketResult>"#;

const CYCLIC_TOKEN_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1</MaxKeys>
<IsTruncated>true</IsTruncated><NextContinuationToken>repeat-me</NextContinuationToken>
<Contents><Key>objects/sha256/repeated</Key><ETag>&quot;repeat&quot;</ETag><Size>8</Size></Contents>
</ListBucketResult>"#;

const OUT_OF_PREFIX_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>objectscape/outside</Key><ETag>&quot;outside&quot;</ETag><Size>7</Size></Contents>
</ListBucketResult>"#;

const MISSING_TRUNCATION_FLAG_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<Contents><Key>objects/sha256/inside</Key><ETag>&quot;inside&quot;</ETag><Size>6</Size></Contents>
</ListBucketResult>"#;

const MISSING_STABLE_IDENTITY_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>objects/unidentified</Key><Size>6</Size></Contents>
</ListBucketResult>"#;

const EMPTY_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>0</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
</ListBucketResult>"#;

const STABLE_NONEMPTY_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>objects/noncanonical</Key><ETag>&quot;stable&quot;</ETag><Size>6</Size></Contents>
</ListBucketResult>"#;

const CHANGED_INVENTORY_PAGE: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>vault-test</Name><Prefix>objects/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>objects/sha256/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</Key><ETag>&quot;changed&quot;</ETag><Size>1</Size></Contents>
</ListBucketResult>"#;

async fn start_s3_readiness_mock(status: StatusCode) -> (String, ReadinessMockState) {
    let state = ReadinessMockState {
        head_status: status,
        head_calls: Arc::new(AtomicUsize::new(0)),
        mutation_calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/{bucket}", head(mock_head_bucket))
        .route("/{bucket}/", head(mock_head_bucket))
        .fallback(any(mock_unexpected_readiness_mutation))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("S3 readiness mock");
    });
    (endpoint_url(addr), state)
}

async fn start_s3_mock_with_objects() -> (String, ObjectMap) {
    let objects = ObjectMap::default();
    let app = Router::new()
        .route("/{bucket}/{*key}", head(mock_head_object))
        .route("/{bucket}/{*key}", put(mock_put_object))
        .route("/{bucket}/{*key}", get(mock_get_object))
        .route("/{bucket}/{*key}", delete(mock_delete_object))
        .with_state(objects.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("s3 mock");
    });
    (endpoint_url(addr), objects)
}

async fn start_blocked_s3_put_mock() -> (String, BlockedPutState) {
    let state = BlockedPutState::default();
    let app = Router::new()
        .route("/{bucket}/{*key}", put(mock_blocked_put))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("blocked S3 mock");
    });
    (endpoint_url(addr), state)
}

async fn mock_blocked_put(State(state): State<BlockedPutState>, _body: Body) -> StatusCode {
    state.entered.notify_one();
    state.release.notified().await;
    StatusCode::OK
}

async fn mock_head_bucket(
    State(state): State<ReadinessMockState>,
    Path(_bucket): Path<String>,
) -> StatusCode {
    state.head_calls.fetch_add(1, Ordering::SeqCst);
    state.head_status
}

async fn mock_unexpected_readiness_mutation(State(state): State<ReadinessMockState>) -> StatusCode {
    state.mutation_calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::METHOD_NOT_ALLOWED
}

fn endpoint_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

async fn mock_head_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
) -> StatusCode {
    if objects.lock().await.contains_key(&key) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn mock_put_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
    body: Body,
) -> Result<StatusCode, StatusCode> {
    let bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    objects
        .lock()
        .await
        .insert(key, decode_aws_chunked_body(&bytes));
    Ok(StatusCode::OK)
}

async fn mock_get_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(bytes) = objects.lock().await.get(&key).cloned() else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let Some(range) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_byte_range)
    else {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .expect("response");
    };
    if range.0 > range.1 || range.1 >= bytes.len() {
        return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    let total_size = bytes.len();
    let range_bytes = Bytes::copy_from_slice(&bytes[range.0..=range.1]);
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_LENGTH, range_bytes.len())
        .header(
            header::CONTENT_RANGE,
            format!("Bytes {:02}-{:02}/{total_size:03}", range.0, range.1),
        )
        .body(Body::from(range_bytes))
        .expect("response")
}

async fn mock_delete_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
) -> StatusCode {
    objects.lock().await.remove(&key);
    StatusCode::NO_CONTENT
}

fn parse_byte_range(raw: &str) -> Option<(usize, usize)> {
    let range = raw.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn empty_response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("response")
}

fn decode_aws_chunked_body(bytes: &[u8]) -> Vec<u8> {
    if !bytes
        .windows(b";chunk-signature=".len())
        .any(|window| window == b";chunk-signature=")
    {
        return bytes.to_vec();
    }
    let mut output = Vec::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let Some(line_end) = find_crlf(bytes, offset) else {
            return bytes.to_vec();
        };
        let line = &bytes[offset..line_end];
        let size_end = line
            .iter()
            .position(|byte| *byte == b';')
            .unwrap_or(line.len());
        let Ok(size_text) = std::str::from_utf8(&line[..size_end]) else {
            return bytes.to_vec();
        };
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return bytes.to_vec();
        };
        offset = line_end + 2;
        if size == 0 {
            break;
        }
        let data_end = offset.saturating_add(size);
        if data_end + 2 > bytes.len() {
            return bytes.to_vec();
        }
        output.extend_from_slice(&bytes[offset..data_end]);
        if &bytes[data_end..data_end + 2] != b"\r\n" {
            return bytes.to_vec();
        }
        offset = data_end + 2;
    }
    output
}

fn find_crlf(bytes: &[u8], start: usize) -> Option<usize> {
    bytes
        .get(start..)?
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|relative| start + relative)
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    lower_hex(&digest)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
