use std::path::{Path, PathBuf};

use clap::Parser;
use vault_server::config::Config;
use vault_server::version::app_version;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn root_file(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).unwrap_or_else(|error| {
        panic!("read {path}: {error}");
    })
}

fn release_image() -> String {
    format!("ghcr.io/willjallen/vault:v{}", app_version())
}

#[test]
fn rust_config_uses_single_data_dir_defaults() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let config = Config::try_parse_from([
        "vault-server",
        "--data-dir",
        data_dir.path().to_str().expect("data dir"),
    ])
    .expect("config");

    assert_eq!(config.data_dir, data_dir.path());
    assert_eq!(config.db_path(), data_dir.path().join("vault.db"));
    assert_eq!(config.objects_path(), data_dir.path().join("objects"));
    assert_eq!(config.transfers_path(), data_dir.path().join("transfers"));
    assert_eq!(config.static_dir, PathBuf::from("vault/client"));
    assert_eq!(config.site_name, "Vault");
    assert_eq!(config.max_upload_bytes, 5 * 1024 * 1024 * 1024);
    assert_eq!(config.transfer_chunk_bytes, 32 * 1024 * 1024);
    assert_eq!(config.transfer_session_ttl_seconds, 86_400);
    assert_eq!(config.export_ttl_seconds, 86_400);
    assert_eq!(config.export_workers, 1);
    assert_eq!(
        config.export_zip_compression_threshold_bytes,
        3 * 1024 * 1024 * 1024
    );
    assert_eq!(config.export_zip_compresslevel, 1);
    assert_eq!(config.ttl_sweep_interval_seconds, 60);
    assert_eq!(config.gzip_minimum_size, 1024);
    assert_eq!(config.gzip_compresslevel, 6);
}

#[test]
fn explicit_runtime_paths_and_site_name_override_data_dir() {
    let base = tempfile::tempdir().expect("tempdir");
    let data_dir = base.path().join("data");
    let db_path = base.path().join("metadata").join("vault.db");
    let objects_path = base.path().join("blobs");
    let transfers_path = base.path().join("scratch");
    let static_dir = base.path().join("static");
    let config = Config::try_parse_from([
        "vault-server",
        "--host",
        "127.0.0.1",
        "--port",
        "9001",
        "--data-dir",
        data_dir.to_str().expect("data dir"),
        "--db-path",
        db_path.to_str().expect("db path"),
        "--objects-path",
        objects_path.to_str().expect("objects path"),
        "--transfers-path",
        transfers_path.to_str().expect("transfers path"),
        "--static-dir",
        static_dir.to_str().expect("static dir"),
        "--storage-backend",
        "s3",
        "--storage-prefix",
        "studio",
        "--site-name",
        "Studio Vault",
        "--max-upload-bytes",
        "1234",
        "--transfer-chunk-bytes",
        "5678",
        "--transfer-session-ttl-seconds",
        "90",
        "--export-ttl-seconds",
        "120",
        "--export-workers",
        "3",
        "--export-zip-compression-threshold-bytes",
        "456",
        "--export-zip-compresslevel",
        "7",
        "--ttl-sweep-interval-seconds",
        "45",
        "--gzip-minimum-size",
        "2048",
        "--gzip-compresslevel",
        "8",
    ])
    .expect("config");

    assert_eq!(config.bind_addr().to_string(), "127.0.0.1:9001");
    assert_eq!(config.data_dir, data_dir);
    assert_eq!(config.db_path(), db_path);
    assert_eq!(config.objects_path(), objects_path);
    assert_eq!(config.transfers_path(), transfers_path);
    assert_eq!(config.static_dir, static_dir);
    assert_eq!(config.storage_backend, "s3");
    assert_eq!(config.storage_prefix, "studio");
    assert_eq!(config.site_name, "Studio Vault");
    assert_eq!(config.max_upload_bytes, 1234);
    assert_eq!(config.transfer_chunk_bytes, 5678);
    assert_eq!(config.transfer_session_ttl_seconds, 90);
    assert_eq!(config.export_ttl_seconds, 120);
    assert_eq!(config.export_workers, 3);
    assert_eq!(config.export_zip_compression_threshold_bytes, 456);
    assert_eq!(config.export_zip_compresslevel, 7);
    assert_eq!(config.ttl_sweep_interval_seconds, 45);
    assert_eq!(config.gzip_minimum_size, 2048);
    assert_eq!(config.gzip_compresslevel, 8);
}

#[test]
fn runtime_numeric_values_normalize_to_python_compatible_bounds() {
    let config = Config::try_parse_from([
        "vault-server",
        "--max-upload-bytes",
        "0",
        "--transfer-chunk-bytes",
        "0",
        "--transfer-session-ttl-seconds",
        "1",
        "--export-ttl-seconds",
        "1",
        "--export-workers",
        "0",
        "--export-zip-compression-threshold-bytes=-10",
        "--export-zip-compresslevel",
        "99",
        "--ttl-sweep-interval-seconds",
        "1",
        "--gzip-minimum-size=-1",
        "--gzip-compresslevel",
        "99",
    ])
    .expect("config")
    .normalized();

    assert_eq!(config.max_upload_bytes, 1);
    assert_eq!(config.transfer_chunk_bytes, 1);
    assert_eq!(config.transfer_session_ttl_seconds, 60);
    assert_eq!(config.export_ttl_seconds, 60);
    assert_eq!(config.export_workers, 1);
    assert_eq!(config.export_zip_compression_threshold_bytes, 0);
    assert_eq!(config.export_zip_compresslevel, 9);
    assert_eq!(config.ttl_sweep_interval_seconds, 10);
    assert_eq!(config.gzip_minimum_size, 0);
    assert_eq!(config.gzip_compresslevel, 9);
}

#[test]
fn legacy_object_path_env_fallback_matches_python_runtime_compatibility() {
    let base = tempfile::tempdir().expect("tempdir");
    let data_dir = base.path().join("data");
    let legacy_objects_path = base.path().join("legacy-objects");
    let legacy_files_path = base.path().join("legacy-files");
    let config = Config::try_parse_from([
        "vault-server",
        "--data-dir",
        data_dir.to_str().expect("data dir"),
    ])
    .expect("config");

    let objects_path = config.objects_path_with_env(|name| match name {
        "VAULT_LOCAL_OBJECTS_PATH" => Some(legacy_objects_path.clone().into_os_string()),
        "VAULT_FILES_PATH" => Some(legacy_files_path.clone().into_os_string()),
        _ => None,
    });
    let files_path = config.objects_path_with_env(|name| match name {
        "VAULT_FILES_PATH" => Some(legacy_files_path.clone().into_os_string()),
        _ => None,
    });
    let default_path = config.objects_path_with_env(|_| None);

    assert_eq!(objects_path, legacy_objects_path);
    assert_eq!(files_path, legacy_files_path);
    assert_eq!(default_path, data_dir.join("objects"));
}

#[test]
fn explicit_object_path_overrides_legacy_object_path_env_fallbacks() {
    let base = tempfile::tempdir().expect("tempdir");
    let data_dir = base.path().join("data");
    let explicit_objects_path = base.path().join("explicit-objects");
    let legacy_objects_path = base.path().join("legacy-objects");
    let config = Config::try_parse_from([
        "vault-server",
        "--data-dir",
        data_dir.to_str().expect("data dir"),
        "--objects-path",
        explicit_objects_path.to_str().expect("objects path"),
    ])
    .expect("config");

    let objects_path = config.objects_path_with_env(|name| match name {
        "VAULT_LOCAL_OBJECTS_PATH" => Some(legacy_objects_path.clone().into_os_string()),
        _ => None,
    });

    assert_eq!(objects_path, explicit_objects_path);
}

#[test]
fn app_version_comes_from_version_file() {
    let expected = std::fs::read_to_string(repo_root().join("VERSION")).expect("VERSION");

    assert_eq!(app_version(), expected.trim());
}

#[test]
fn dockerfile_runs_rust_server_with_single_data_volume_contract() {
    let dockerfile = root_file("Dockerfile");

    assert!(dockerfile.contains("FROM node:22-slim AS assets"));
    assert!(dockerfile.contains("RUN npm ci"));
    assert!(dockerfile.contains("RUN npm run build:assets"));
    assert!(dockerfile.contains("FROM rust:1.95-slim-bookworm AS rust-builder"));
    assert!(dockerfile.contains("RUN cargo build --release -p vault-server"));
    assert!(dockerfile.contains("FROM debian:bookworm-slim"));
    assert!(dockerfile.contains("/build/target/release/vault-server /app/vault-server"));
    assert!(dockerfile.contains("COPY --from=assets --chown=vault:vault /build/vault/client/dist"));
    assert!(dockerfile.contains("VAULT_DATA_DIR=/data"));
    assert!(dockerfile.contains("VAULT_DB_PATH=/data/vault.db"));
    assert!(dockerfile.contains("VAULT_OBJECTS_PATH=/data/objects"));
    assert!(dockerfile.contains("VAULT_STATIC_DIR=/app/vault/client"));
    assert!(dockerfile.contains("VAULT_DOCKER_RUNTIME=1"));
    assert!(dockerfile.contains("VOLUME [\"/data\"]"));
    assert!(dockerfile.contains("EXPOSE 8000"));
    assert!(dockerfile.contains("USER vault"));
    assert!(dockerfile.contains("HEALTHCHECK"));
    assert!(dockerfile.contains("curl -fsS --max-time 2 http://127.0.0.1:8000/health"));
    assert!(dockerfile.contains("CMD [\"/app/vault-server\"]"));
    assert!(!dockerfile.contains("uvicorn"));
    assert!(!dockerfile.contains("python:3.11"));
    assert!(!dockerfile.contains("VAULT_VERSION"));
    assert!(!dockerfile.contains("/vault-metadata"));
    assert!(!dockerfile.contains("/vault-objects"));
}

#[test]
fn production_compose_uses_release_image_single_data_volume_and_hardened_defaults() {
    let compose = root_file("docker-compose.yml");

    assert!(compose.contains(&release_image()));
    assert!(compose.contains("${VAULT_BIND_ADDRESS:-127.0.0.1}:${VAULT_PORT:-8000}:8000"));
    assert!(compose.contains("- vault-data:/data"));
    assert!(compose.contains("vault-data:"));
    assert_eq!(compose.matches(":/data").count(), 1);
    assert!(compose.contains("VAULT_SITE_NAME: ${VAULT_SITE_NAME:-Vault}"));
    assert!(compose.contains("VAULT_DEV_MODE: ${VAULT_DEV_MODE:-0}"));
    assert!(compose.contains("VAULT_DOCKER_RUNTIME: ${VAULT_DOCKER_RUNTIME:-1}"));
    assert!(compose.contains("VAULT_AUTH_MODE: ${VAULT_AUTH_MODE:-headers}"));
    assert!(compose.contains("VAULT_REQUIRE_SESSION_SECRET: ${VAULT_REQUIRE_SESSION_SECRET:-}"));
    assert!(compose.contains("VAULT_SESSION_SECRET: ${VAULT_SESSION_SECRET:-}"));
    assert!(
        compose.contains("VAULT_SESSION_COOKIE_NAME: ${VAULT_SESSION_COOKIE_NAME:-vault_session}")
    );
    assert!(compose.contains("VAULT_SESSION_COOKIE_SECURE: ${VAULT_SESSION_COOKIE_SECURE:-auto}"));
    assert!(
        compose
            .contains("VAULT_TTL_SWEEP_INTERVAL_SECONDS: ${VAULT_TTL_SWEEP_INTERVAL_SECONDS:-60}")
    );
    assert!(compose.contains("VAULT_MAX_UPLOAD_BYTES: ${VAULT_MAX_UPLOAD_BYTES:-5368709120}"));
    assert!(
        compose.contains("VAULT_TRANSFER_CHUNK_BYTES: ${VAULT_TRANSFER_CHUNK_BYTES:-33554432}")
    );
    assert!(compose.contains(
        "VAULT_TRANSFER_SESSION_TTL_SECONDS: ${VAULT_TRANSFER_SESSION_TTL_SECONDS:-86400}"
    ));
    assert!(compose.contains("VAULT_TRANSFERS_PATH: ${VAULT_TRANSFERS_PATH:-/data/transfers}"));
    assert!(compose.contains("VAULT_EXPORT_TTL_SECONDS: ${VAULT_EXPORT_TTL_SECONDS:-86400}"));
    assert!(compose.contains("VAULT_EXPORT_WORKERS: ${VAULT_EXPORT_WORKERS:-1}"));
    assert!(
        compose.contains(
            "VAULT_EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES: ${VAULT_EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES:-3221225472}",
        )
    );
    assert!(
        compose.contains("VAULT_EXPORT_ZIP_COMPRESSLEVEL: ${VAULT_EXPORT_ZIP_COMPRESSLEVEL:-1}")
    );
    assert!(compose.contains("VAULT_GZIP_MINIMUM_SIZE: ${VAULT_GZIP_MINIMUM_SIZE:-1024}"));
    assert!(compose.contains("VAULT_GZIP_COMPRESSLEVEL: ${VAULT_GZIP_COMPRESSLEVEL:-6}"));
    assert!(compose.contains("VAULT_CONTENT_SECURITY_POLICY: ${VAULT_CONTENT_SECURITY_POLICY:-}"));
    assert!(compose.contains("VAULT_HSTS_PRELOAD: ${VAULT_HSTS_PRELOAD:-0}"));
    assert!(compose.contains("VAULT_OIDC_ISSUER: ${VAULT_OIDC_ISSUER:-}"));
    assert!(compose.contains("VAULT_OIDC_CLIENT_ID: ${VAULT_OIDC_CLIENT_ID:-}"));
    assert!(compose.contains("VAULT_OIDC_CLIENT_SECRET: ${VAULT_OIDC_CLIENT_SECRET:-}"));
    assert!(
        compose.contains("VAULT_OIDC_CLIENT_AUTH: ${VAULT_OIDC_CLIENT_AUTH:-client_secret_basic}")
    );
    assert!(compose.contains(
        "VAULT_OIDC_STATE_COOKIE_NAME: ${VAULT_OIDC_STATE_COOKIE_NAME:-vault_oidc_state}"
    ));
    assert!(
        compose
            .contains("VAULT_OIDC_AUTHORIZATION_ENDPOINT: ${VAULT_OIDC_AUTHORIZATION_ENDPOINT:-}")
    );
    assert!(
        compose.contains("VAULT_OIDC_ALLOW_INSECURE_HTTP: ${VAULT_OIDC_ALLOW_INSECURE_HTTP:-0}")
    );
    assert!(compose.contains("VAULT_OIDC_NONCE_BYTES: ${VAULT_OIDC_NONCE_BYTES:-24}"));
    assert!(
        compose.contains(
            "VAULT_OIDC_DISCOVERY_TTL_SECONDS: ${VAULT_OIDC_DISCOVERY_TTL_SECONDS:-3600}"
        )
    );
    assert!(
        compose.contains("VAULT_OIDC_HTTP_TIMEOUT_SECONDS: ${VAULT_OIDC_HTTP_TIMEOUT_SECONDS:-8}")
    );
    assert!(compose.contains("VAULT_S3_BUCKET: ${VAULT_S3_BUCKET:-}"));
    assert!(compose.contains("VAULT_R2_BUCKET: ${VAULT_R2_BUCKET:-}"));
    assert!(!compose.contains("VAULT_DEV_AUTH"));
    assert!(!compose.contains("dev-insecure-session-secret"));
    assert!(!compose.contains("FORWARDED_ALLOW_IPS"));
    assert!(!compose.contains("/vault-metadata"));
    assert!(!compose.contains("/vault-objects"));
    assert!(!compose.contains("0.0.0.0:8000:8000"));
}

#[test]
fn development_compose_is_the_only_compose_file_that_enables_dev_auth() {
    let compose = root_file("docker-compose.yml");
    let dev_compose = root_file("docker-compose.dev.yml");

    assert!(!compose.contains("VAULT_DEV_AUTH"));
    assert!(dev_compose.contains("build:"));
    assert!(dev_compose.contains("VAULT_AUTH_MODE: dev"));
    assert!(dev_compose.contains("VAULT_SITE_NAME: ${VAULT_SITE_NAME:-Vault}"));
    assert!(dev_compose.contains("VAULT_DEV_MODE: \"1\""));
    assert!(
        dev_compose
            .contains("VAULT_TTL_SWEEP_INTERVAL_SECONDS: ${VAULT_TTL_SWEEP_INTERVAL_SECONDS:-60}")
    );
    assert!(dev_compose.contains("VAULT_DEV_AUTH: \"1\""));
    assert!(dev_compose.contains("dev-insecure-session-secret-change-me"));
    assert!(!dev_compose.contains("VAULT_VERSION"));
}

#[test]
fn generated_static_assets_are_ignored_build_output() {
    let dockerignore = root_file(".dockerignore");
    let gitignore = root_file(".gitignore");

    assert!(dockerignore.contains("vault/client/dist/"));
    assert!(dockerignore.contains("target/"));
    assert!(gitignore.contains("vault/client/dist/"));
}

#[test]
fn semver_tag_workflow_builds_and_publishes_versioned_ghcr_image() {
    let workflow = root_file(".github/workflows/docker-image.yml");

    assert!(workflow.contains("      - \"v*.*.*\""));
    assert!(workflow.contains("      - \"[0-9]*.[0-9]*.[0-9]*\""));
    assert!(workflow.contains("Validate semantic version tag"));
    assert!(workflow.contains("ghcr.io/${GITHUB_REPOSITORY,,}"));
    assert!(workflow.contains("docker/login-action@v3"));
    assert!(workflow.contains("docker/metadata-action@v5"));
    assert!(workflow.contains("docker/build-push-action@v6"));
    assert!(workflow.contains("push: true"));
    assert!(workflow.contains("type=semver,pattern={{version}}"));
    assert!(!workflow.contains("VAULT_VERSION"));
    assert!(!workflow.contains("type=semver,pattern={{major}}.{{minor}}"));
    assert!(!workflow.contains("type=semver,pattern={{major}}"));
    assert!(!workflow.contains("type=raw,value=latest"));
}
