use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use config::{Config, ConfigError, File, FileFormat};
use serde::Deserialize;
use tracing::warn;

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => {
                warn!("HOME env var not set; cannot expand tilde in path: {path}");
                path.to_string()
            }
        }
    } else {
        path.to_string()
    }
}

#[derive(Debug, Deserialize)]
pub struct GrpcSettings {
    pub host: String,
    pub port: u16,
}

impl GrpcSettings {
    pub fn addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.host, self.port).parse()
    }
}

#[derive(Debug, Deserialize)]
pub struct ImessageSettings {
    pub recipient: Option<String>,
    pub handle_id: Option<i64>,
    pub extra_handle_ids: Option<Vec<i64>>,
}

#[derive(Debug, Deserialize)]
pub struct ChatDbSettings {
    pub path: String,
}

impl ChatDbSettings {
    pub fn resolved_path(&self) -> String {
        expand_tilde(&self.path)
    }
}

#[derive(Debug, Deserialize)]
pub struct AiSettings {
    pub cli_path: Option<String>,
    pub local_model: Option<String>,
    pub local_model_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TtsSettings {
    pub command: String,
    pub voice: Option<String>,
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct LogSettings {
    pub level: String,
}

#[derive(Debug, Deserialize)]
pub struct StoreSettings {
    pub path: String,
}

impl StoreSettings {
    pub fn resolved_path(&self) -> String {
        expand_tilde(&self.path)
    }
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub grpc: GrpcSettings,
    pub imessage: ImessageSettings,
    pub chat_db: ChatDbSettings,
    pub ai: AiSettings,
    pub tts: TtsSettings,
    pub log: LogSettings,
    pub store: StoreSettings,
}

impl Settings {
    pub fn load() -> Result<Arc<Self>, ConfigError> {
        let env = std::env::var("HAROLD_ENV").unwrap_or_else(|_| "local".into());
        let config_dir = std::env::var("HAROLD_CONFIG_DIR").unwrap_or_else(|_| "config".into());

        let config = Config::builder()
            .add_source(File::new(
                &format!("{config_dir}/default"),
                FileFormat::Toml,
            ))
            .add_source(File::new(&format!("{config_dir}/{env}"), FileFormat::Toml).required(false))
            .add_source(
                config::Environment::with_prefix("HAROLD")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        let settings = config.try_deserialize::<Settings>()?;
        Ok(Arc::new(settings))
    }
}

pub fn get_settings() -> &'static Arc<Settings> {
    SETTINGS.get().expect("settings not initialised")
}

pub fn init_settings(settings: Arc<Settings>) {
    SETTINGS
        .set(settings)
        .expect("init_settings called more than once");
}
