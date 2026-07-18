use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing_subscriber::EnvFilter;
use vault_server::assets;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::documents;
use vault_server::http::{self, AppState};
use vault_server::reconciliation;
use vault_server::state_events::{compact_state_events, notify_state_event_committed};
use vault_server::storage::{configured_blob_storage, sweep_legacy_s3_stage_files};
use vault_server::transfers;

use vault_server::db;

const SERVER_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_EVENT_COMPACTION_INTERVAL: Duration = Duration::from_mins(1);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::from_env();
    let auth = AuthSettings::from_env();
    auth.validate_runtime_config()?;
    let db = db::connect(&config.db_path()).await?;
    let storage = configured_blob_storage(&config).await?;
    storage.ensure().await?;
    spawn_legacy_s3_stage_sweep(&config);
    let transfers_path = config.transfers_path();
    let bind_addr = config.bind_addr();
    tokio::fs::create_dir_all(&transfers_path).await?;
    assets::validate_static_assets(&config.static_dir).await?;
    let listener = TcpListener::bind(bind_addr).await?;
    let state = AppState::new(config, auth, db, storage);
    state
        .preview_execution
        .start(state.db.clone(), state.storage.clone(), 2)
        .await?;
    let document_sweep = documents::sweep_expired_documents(&state.db, 250).await?;
    transfers::cleanup_upload_session_resources(
        &state.upload_hash_coordinator,
        &transfers_path,
        &document_sweep.terminated_uploads,
    )
    .await;
    if document_sweep.has_state_changes() {
        notify_state_event_committed();
    }
    transfers::sweep_expired_transfers(
        &state.db,
        &state.storage,
        &transfers_path,
        &state.upload_hash_coordinator,
        &state.transfer_maintenance,
    )
    .await?;
    transfers::recover_interrupted_transfers_with_export_runtime(
        &state.db,
        &state.storage,
        &transfers_path,
        true,
        &state.export_execution,
    )
    .await?;
    sweep_local_multipart_parts(&state).await;
    compact_state_events(&state.db).await?;
    spawn_state_event_compactor(state.db.clone());
    spawn_ttl_sweeper(state.clone(), transfers_path.clone());

    let export_execution = state.export_execution.clone();
    let preview_execution = state.preview_execution.clone();
    let app = http::network_router(state);

    tracing::info!(%bind_addr, "vault rust server listening");
    let (http_shutdown_tx, http_shutdown_rx) = oneshot::channel();
    let (shutdown_observed_tx, shutdown_observed_rx) = oneshot::channel();
    let signal_execution = export_execution.clone();
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        signal_execution.request_dispatcher_shutdown();
        let _ = http_shutdown_tx.send(());
        let _ = shutdown_observed_tx.send(());
    });
    let server_result = {
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = http_shutdown_rx.await;
        })
        .into_future();
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => {
                signal_task.abort();
                export_execution.request_dispatcher_shutdown();
                result
            }
            _ = shutdown_observed_rx => {
                if let Ok(result) =
                    tokio::time::timeout(SERVER_DRAIN_TIMEOUT, &mut server).await
                {
                    result
                } else {
                    tracing::warn!(
                        timeout_seconds = SERVER_DRAIN_TIMEOUT.as_secs(),
                        "HTTP connections did not drain before the shutdown deadline"
                    );
                    Ok(())
                }
            }
        }
    };
    export_execution.shutdown_dispatcher().await;
    preview_execution.shutdown().await;
    server_result?;
    Ok(())
}

fn spawn_state_event_compactor(db: db::DbPool) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(STATE_EVENT_COMPACTION_INTERVAL).await;
            if let Err(error) = compact_state_events(&db).await {
                tracing::error!(%error, "state event compaction failed");
            }
        }
    });
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(?error, "failed to listen for Ctrl-C");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(?error, "failed to listen for Ctrl-C");
    }
    tracing::info!("shutdown requested; draining active export workers");
}

fn spawn_ttl_sweeper(state: AppState, transfers_path: PathBuf) {
    let interval_seconds = state.config.ttl_sweep_interval_seconds.max(10);
    tokio::spawn(async move {
        let interval = Duration::from_secs(u64::try_from(interval_seconds).unwrap_or(10));
        loop {
            tokio::time::sleep(interval).await;
            match documents::sweep_expired_documents(&state.db, 250).await {
                Ok(result) => {
                    transfers::cleanup_upload_session_resources(
                        &state.upload_hash_coordinator,
                        &transfers_path,
                        &result.terminated_uploads,
                    )
                    .await;
                    if result.has_state_changes() {
                        notify_state_event_committed();
                    }
                }
                Err(error) => tracing::error!(%error, "document TTL sweep failed"),
            }
            if let Err(error) = transfers::sweep_expired_transfers(
                &state.db,
                &state.storage,
                &transfers_path,
                &state.upload_hash_coordinator,
                &state.transfer_maintenance,
            )
            .await
            {
                tracing::error!(%error, "transfer TTL sweep failed");
            }
            sweep_local_multipart_parts(&state).await;
        }
    });
}

async fn sweep_local_multipart_parts(state: &AppState) {
    if !state
        .config
        .storage_backend
        .trim()
        .eq_ignore_ascii_case("local")
    {
        return;
    }
    if let Err(error) = reconciliation::sweep_unreferenced_multipart_parts(
        &state.db,
        &state.local_storage_maintenance,
    )
    .await
    {
        tracing::warn!(?error, "multipart part garbage collection failed");
    }
}

fn spawn_legacy_s3_stage_sweep(config: &Config) {
    if !matches!(
        config.storage_backend.trim().to_ascii_lowercase().as_str(),
        "s3" | "r2"
    ) {
        return;
    }
    let minimum_age = Duration::from_secs(
        u64::try_from(config.transfer_session_ttl_seconds)
            .unwrap_or_default()
            .max(7 * 24 * 60 * 60),
    );
    let temp_dir = std::env::temp_dir();
    tokio::spawn(async move {
        match sweep_legacy_s3_stage_files(&temp_dir, minimum_age, 4_096).await {
            Ok(deleted) if !deleted.is_empty() => tracing::info!(
                count = deleted.len(),
                "removed aged legacy S3 upload stage files"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(?error, "legacy S3 upload stage cleanup failed"),
        }
    });
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
