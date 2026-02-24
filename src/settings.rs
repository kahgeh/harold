use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use config::{Config, ConfigError, File, FileFormat};
use serde::Deserialize;

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

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
    #[expect(dead_code, reason = "used in Step 7")]
    pub recipient: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatDbSettings {
    #[expect(dead_code, reason = "used in Step 8")]
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct AiSettings {
    #[expect(dead_code, reason = "used in Steps 6-7")]
    pub cli_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TtsSettings {
    #[expect(dead_code, reason = "used in Step 6")]
    pub command: String,
}

#[derive(Debug, Deserialize)]
pub struct LogSettings {
    pub level: String,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub grpc: GrpcSettings,
    #[expect(dead_code, reason = "used in Steps 7-8")]
    pub imessage: ImessageSettings,
    #[expect(dead_code, reason = "used in Step 8")]
    pub chat_db: ChatDbSettings,
    #[expect(dead_code, reason = "used in Steps 6-7")]
    pub ai: AiSettings,
    #[expect(dead_code, reason = "used in Step 6")]
    pub tts: TtsSettings,
    pub log: LogSettings,
}

impl Settings {
    pub fn load() -> Result<Arc<Self>, ConfigError> {
        let env = std::env::var("HAROLD_ENV").unwrap_or_else(|_| "local".into());

        let config_dir = std::env::var("HAROLD_CONFIG_DIR")
            .unwrap_or_else(|_| "config".into());

        let config = Config::builder()
            .add_source(File::new(&format!("{config_dir}/default"), FileFormat::Toml))
            .add_source(
                File::new(&format!("{config_dir}/{env}"), FileFormat::Toml).required(false),
            )
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
