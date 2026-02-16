use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(name = "openxet-server", about = "OpenXet CAS server")]
pub struct Cli {
    /// Path to the configuration file
    #[arg(short, long, default_value = "openxet.toml")]
    pub config: PathBuf,

    /// Host to bind to (overrides config)
    #[arg(long)]
    pub host: Option<String>,

    /// Port to bind to (overrides config)
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Data directory (overrides config)
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub frontend_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub backend: String,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub secret: String,
    pub shard_key_ttl_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            frontend_dir: PathBuf::from("./web/dist"),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: "filesystem".to_string(),
            data_dir: PathBuf::from("./.data"),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            secret: "change-me-in-production".to_string(),
            shard_key_ttl_seconds: 3600,
        }
    }
}

impl AppConfig {
    /// Load configuration from file (if it exists) and apply CLI overrides.
    /// Priority (highest wins): env vars > CLI args > TOML file > defaults.
    pub fn load(cli: &Cli) -> anyhow::Result<Self> {
        let mut config = if cli.config.exists() {
            let contents = std::fs::read_to_string(&cli.config)?;
            toml::from_str::<AppConfig>(&contents)?
        } else {
            AppConfig::default()
        };

        // CLI overrides
        if let Some(host) = &cli.host {
            config.server.host = host.clone();
        }
        if let Some(port) = cli.port {
            config.server.port = port;
        }
        if let Some(data_dir) = &cli.data_dir {
            config.storage.data_dir = data_dir.clone();
        }

        // Environment variable overrides (highest priority)
        config.apply_env_overrides();

        Ok(config)
    }

    /// Apply environment variable overrides to the config.
    fn apply_env_overrides(&mut self) {
        if let Ok(host) = std::env::var("OPENXET_HOST") {
            self.server.host = host;
        }
        if let Ok(port) = std::env::var("OPENXET_PORT")
            && let Ok(port) = port.parse::<u16>()
        {
            self.server.port = port;
        }
        if let Ok(data_dir) = std::env::var("OPENXET_DATA_DIR") {
            self.storage.data_dir = PathBuf::from(data_dir);
        }
        if let Ok(frontend_dir) = std::env::var("OPENXET_FRONTEND_DIR") {
            self.server.frontend_dir = PathBuf::from(frontend_dir);
        }
        if let Ok(secret) = std::env::var("OPENXET_AUTH_SECRET") {
            self.auth.secret = secret;
        }
        if let Ok(ttl) = std::env::var("OPENXET_SHARD_KEY_TTL")
            && let Ok(ttl) = ttl.parse::<u64>()
        {
            self.auth.shard_key_ttl_seconds = ttl;
        }
    }

    /// Build the base URL for this server instance.
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.server.host, self.server.port)
    }

    /// Resolve the data directory path.
    pub fn data_dir(&self) -> &Path {
        &self.storage.data_dir
    }
}
