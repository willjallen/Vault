use std::net::Ipv4Addr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::Config;

#[derive(Debug, Parser)]
#[command(author, version = crate::version::app_version(), about)]
pub struct ApplicationCli {
    #[command(flatten)]
    pub config: Config,

    #[command(subcommand)]
    pub command: Option<ApplicationCommand>,
}

impl ApplicationCli {
    #[must_use]
    pub fn from_env() -> Self {
        let arguments = std::env::args_os().collect::<Vec<_>>();
        if integrity_subcommand_requested(&arguments) {
            return IntegrityApplicationCli::parse_from(arguments).into_application_cli();
        }
        let mut cli = Self::parse();
        cli.config = cli.config.normalized();
        cli
    }
}

#[doc(hidden)]
#[must_use]
pub fn integrity_subcommand_requested(arguments: &[std::ffi::OsString]) -> bool {
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if argument == "--" {
            return arguments
                .get(index + 1)
                .is_some_and(|argument| argument == "integrity-check");
        }
        if argument.starts_with('-') {
            // Every non-help/version top-level option currently takes one
            // value. Treating unknown options the same way leaves diagnostics
            // to the complete clap parser without mistaking their values for
            // a subcommand.
            if !argument.contains('=')
                && !matches!(argument.as_ref(), "-h" | "--help" | "-V" | "--version")
            {
                index += 1;
            }
            index += 1;
            continue;
        }
        return argument == "integrity-check";
    }
    false
}

/// Integrity mode deliberately parses only the settings needed to locate and
/// read durable data. Invalid listener, worker, compression, or UI settings
/// must not prevent an offline audit from producing its report.
#[derive(Debug, Parser)]
#[command(author, version = crate::version::app_version(), about)]
struct IntegrityApplicationCli {
    #[command(flatten)]
    config: IntegrityConfig,

    #[command(subcommand)]
    command: ApplicationCommand,
}

impl IntegrityApplicationCli {
    fn into_application_cli(self) -> ApplicationCli {
        ApplicationCli {
            config: self.config.into_config(),
            command: Some(self.command),
        }
    }
}

#[derive(Debug, Args)]
struct IntegrityConfig {
    #[arg(long, env = "VAULT_DATA_DIR", default_value = "/data")]
    data_dir: PathBuf,

    #[arg(long, env = "VAULT_DB_PATH")]
    db_path: Option<PathBuf>,

    #[arg(long, env = "VAULT_OBJECTS_PATH")]
    objects_path: Option<PathBuf>,

    #[arg(long, env = "VAULT_TRANSFERS_PATH")]
    transfers_path: Option<PathBuf>,

    #[arg(long, env = "VAULT_STORAGE_BACKEND", default_value = "local")]
    storage_backend: String,

    #[arg(long, env = "VAULT_STORAGE_PREFIX", default_value = "objects")]
    storage_prefix: String,
}

impl IntegrityConfig {
    fn into_config(self) -> Config {
        Config {
            host: Ipv4Addr::UNSPECIFIED.into(),
            port: 8000,
            data_dir: self.data_dir,
            db_path: self.db_path,
            objects_path: self.objects_path,
            transfers_path: self.transfers_path,
            static_dir: PathBuf::from("vault/client"),
            storage_backend: self.storage_backend,
            storage_prefix: self.storage_prefix,
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
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ApplicationCommand {
    /// Audit database and stored content without starting the server or making repairs.
    IntegrityCheck(IntegrityCheckArgs),
}

#[derive(Debug, Clone, Args)]
pub struct IntegrityCheckArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    #[arg(
        long,
        default_value_t = 100,
        value_parser = parse_finding_cap
    )]
    pub max_findings_per_code: usize,
}

fn parse_finding_cap(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| "finding cap must be an integer".to_string())?;
    if (1..=10_000).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("finding cap must be between 1 and 10000".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}
