use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use vault_server::assets;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::documents;
use vault_server::http::{self, AppState};
use vault_server::state_events::notify_state_event_committed;
use vault_server::storage::configured_blob_storage;
use vault_server::transfers;

use vault_server::db;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::from_env();
    let auth = AuthSettings::from_env();
    auth.validate_runtime_config()?;
    let db = db::connect(&config.db_path()).await?;
    let storage = configured_blob_storage(&config).await?;
    storage.ensure().await?;
    let transfers_path = config.transfers_path();
    let bind_addr = config.bind_addr();
    tokio::fs::create_dir_all(&transfers_path).await?;
    assets::validate_static_assets(&config.static_dir).await?;
    let state = AppState::new(config, auth, db, storage);
    let document_sweep = documents::sweep_expired_documents(&state.db, 250).await?;
    if document_sweep.has_state_changes() {
        notify_state_event_committed();
    }
    transfers::sweep_expired_transfers(&state.db, &state.storage, &transfers_path).await?;
    transfers::recover_interrupted_transfers_with_export_runtime(
        &state.db,
        &state.storage,
        &transfers_path,
        true,
        &state.export_execution,
    )
    .await?;
    spawn_ttl_sweeper(state.clone(), transfers_path.clone());

    let app = http::router(state);

    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "vault rust server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn spawn_ttl_sweeper(state: AppState, transfers_path: PathBuf) {
    let interval_seconds = state.config.ttl_sweep_interval_seconds.max(10);
    tokio::spawn(async move {
        let interval = Duration::from_secs(u64::try_from(interval_seconds).unwrap_or(10));
        loop {
            tokio::time::sleep(interval).await;
            match documents::sweep_expired_documents(&state.db, 250).await {
                Ok(result) => {
                    if result.has_state_changes() {
                        notify_state_event_committed();
                    }
                }
                Err(error) => tracing::error!(%error, "document TTL sweep failed"),
            }
            if let Err(error) =
                transfers::sweep_expired_transfers(&state.db, &state.storage, &transfers_path).await
            {
                tracing::error!(%error, "transfer TTL sweep failed");
            }
        }
    });
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
