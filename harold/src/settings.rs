use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use config::{Config, ConfigError, File, FileFormat};
use serde::Deserialize;
use tracing::warn;

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

fn expand_tilde(path: &str) -> String {
    let Some(rest) = path.strip_prefix("~/") else {
        return path.to_string();
    };
    match std::env::var("HOME") {
        Ok(home) => format!("{home}/{rest}"),
        Err(_) => {
            warn!("HOME env var not set; cannot expand tilde in path: {path}");
            path.to_string()
        }
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
    // handle_id for your own Apple ID in chat.db (e.g. your Gmail or Apple ID email).
    // Messages sent from your phone appear in chat.db as is_from_me=1 on this handle.
    pub self_handle_id: Option<i64>,
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
pub struct NotifySettings {
    pub skip_if_session_active: bool,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            skip_if_session_active: true,
        }
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
    #[serde(default)]
    pub notify: NotifySettings,
}

impl Settings {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.imessage.recipient.is_none() {
            errors.push("imessage.recipient is required".into());
        }
        let has_inbound = self.imessage.handle_id.is_some_and(|id| id > 0)
            || self.imessage.self_handle_id.is_some_and(|id| id > 0);
        if !has_inbound {
            errors.push(
                "imessage.handle_id or imessage.self_handle_id is required to receive messages"
                    .into(),
            );
        }
        errors
    }

    pub fn load() -> Result<Arc<Self>, ConfigError> {
        let env = std::env::var("HAROLD_ENV").unwrap_or_else(|_| "local".into());
        let config_dir = std::env::var("HAROLD_CONFIG_DIR").unwrap_or_else(|_| {
            // Default to a config/ directory next to the running binary.
            std::env::current_exe()
                .ok()
                .and_then(|p| {
                    p.parent()
                        .map(|d| d.join("config").to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "config".into())
        });

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

#[cfg(test)]
pub fn init_settings_for_test() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        // SAFETY: called exactly once via OnceLock before any other thread reads this var.
        unsafe {
            std::env::set_var("HAROLD_CONFIG_DIR", format!("{manifest_dir}/config"));
        }
        let s = Settings::load().expect("failed to load settings for test");
        let _ = SETTINGS.set(s);
    });
}

pub fn init_settings(settings: Arc<Settings>) {
    SETTINGS
        .set(settings)
        .expect("init_settings called more than once");
}
