use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Config {
    #[arg(long, env = "VAULT_HOST", default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    pub host: IpAddr,

    #[arg(long, env = "VAULT_PORT", default_value_t = 8000)]
    pub port: u16,

    #[arg(long, env = "VAULT_DATA_DIR", default_value = "/data")]
    pub data_dir: PathBuf,

    #[arg(long, env = "VAULT_DB_PATH")]
    pub db_path: Option<PathBuf>,

    #[arg(long, env = "VAULT_OBJECTS_PATH")]
    pub objects_path: Option<PathBuf>,

    #[arg(long, env = "VAULT_TRANSFERS_PATH")]
    pub transfers_path: Option<PathBuf>,

    #[arg(long, env = "VAULT_STATIC_DIR", default_value = "vault/client")]
    pub static_dir: PathBuf,

    #[arg(long, env = "VAULT_STORAGE_BACKEND", default_value = "local")]
    pub storage_backend: String,

    #[arg(long, env = "VAULT_STORAGE_PREFIX", default_value = "objects")]
    pub storage_prefix: String,

    #[arg(long, env = "VAULT_SITE_NAME", default_value = "Vault")]
    pub site_name: String,

    #[arg(long, env = "VAULT_MAX_UPLOAD_BYTES", default_value_t = 5 * 1024 * 1024 * 1024)]
    pub max_upload_bytes: i64,

    #[arg(long, env = "VAULT_TRANSFER_CHUNK_BYTES", default_value_t = 32 * 1024 * 1024)]
    pub transfer_chunk_bytes: i64,

    #[arg(
        long,
        env = "VAULT_TRANSFER_SESSION_TTL_SECONDS",
        default_value_t = 86_400
    )]
    pub transfer_session_ttl_seconds: i64,

    #[arg(long, env = "VAULT_EXPORT_TTL_SECONDS", default_value_t = 86_400)]
    pub export_ttl_seconds: i64,

    #[arg(long, env = "VAULT_EXPORT_WORKERS", default_value_t = 1)]
    pub export_workers: i64,

    #[arg(
        long,
        env = "VAULT_EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES",
        default_value_t = 3 * 1024 * 1024 * 1024
    )]
    pub export_zip_compression_threshold_bytes: i64,

    #[arg(long, env = "VAULT_EXPORT_ZIP_COMPRESSLEVEL", default_value_t = 1)]
    pub export_zip_compresslevel: i64,

    #[arg(long, env = "VAULT_TTL_SWEEP_INTERVAL_SECONDS", default_value_t = 60)]
    pub ttl_sweep_interval_seconds: i64,

    #[arg(long, env = "VAULT_GZIP_MINIMUM_SIZE", default_value_t = 1024)]
    pub gzip_minimum_size: i64,

    #[arg(long, env = "VAULT_GZIP_COMPRESSLEVEL", default_value_t = 6)]
    pub gzip_compresslevel: i64,
}

impl Config {
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse().normalized()
    }

    #[must_use]
    pub fn normalized(mut self) -> Self {
        // Keep operational bounds aligned with the Python runtime config so bad
        // environment values cannot disable uploads, sweeps, or export workers.
        self.max_upload_bytes = self.max_upload_bytes.max(1);
        self.transfer_chunk_bytes = self.transfer_chunk_bytes.max(1);
        self.transfer_session_ttl_seconds = self.transfer_session_ttl_seconds.max(60);
        self.export_ttl_seconds = self.export_ttl_seconds.max(60);
        self.export_workers = self.export_workers.max(1);
        self.export_zip_compression_threshold_bytes =
            self.export_zip_compression_threshold_bytes.max(0);
        self.export_zip_compresslevel = self.export_zip_compresslevel.clamp(1, 9);
        self.ttl_sweep_interval_seconds = self.ttl_sweep_interval_seconds.max(10);
        self.gzip_minimum_size = self.gzip_minimum_size.max(0);
        self.gzip_compresslevel = self.gzip_compresslevel.clamp(1, 9);
        self
    }

    #[must_use]
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join("vault.db"))
    }

    #[must_use]
    pub fn objects_path(&self) -> PathBuf {
        self.objects_path_with_env(|name| std::env::var_os(name))
    }

    #[must_use]
    pub fn objects_path_with_env<F>(&self, env_var: F) -> PathBuf
    where
        F: Fn(&str) -> Option<OsString>,
    {
        if let Some(path) = self.objects_path.clone() {
            return path;
        }
        legacy_objects_path(env_var).unwrap_or_else(|| self.data_dir.join("objects"))
    }

    #[must_use]
    pub fn transfers_path(&self) -> PathBuf {
        self.transfers_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join("transfers"))
    }
}

fn legacy_objects_path<F>(env_var: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    ["VAULT_LOCAL_OBJECTS_PATH", "VAULT_FILES_PATH"]
        .into_iter()
        .find_map(|name| {
            env_var(name)
                .filter(|value| !value.as_os_str().is_empty())
                .map(PathBuf::from)
        })
}
