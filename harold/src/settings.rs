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
    /// All chat.db handle IDs associated with your Apple ID (phone number, emails).
    #[serde(default)]
    pub handle_ids: Vec<i64>,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct AgentProviderSettings {
    pub id: String,
    pub display_name: String,
    pub command_contains: Vec<String>,
    #[serde(default)]
    pub busy_all: Vec<String>,
    #[serde(default)]
    pub idle_all: Vec<String>,
    #[serde(default)]
    pub summary_line_prefixes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum AgentSettings {
    Named(Vec<AgentProviderSettings>),
    Legacy { command_contains: Vec<String> },
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self::Legacy {
            command_contains: vec!["claude".to_string(), "codex".to_string()],
        }
    }
}

impl AgentSettings {
    pub(crate) fn matches_command(&self, command: &str) -> bool {
        let command = command.trim().to_lowercase();
        let contains = |fragment: &str| command.contains(&fragment.trim().to_lowercase());
        match self {
            Self::Named(providers) => providers
                .iter()
                .flat_map(|provider| &provider.command_contains)
                .any(|fragment| !fragment.trim().is_empty() && contains(fragment)),
            Self::Legacy { command_contains } => command_contains
                .iter()
                .any(|fragment| !fragment.trim().is_empty() && contains(fragment)),
        }
    }

    pub(crate) fn validate(&self, monitor: &AgentMonitorSettings) -> Vec<String> {
        let mut errors = Vec::new();
        if monitor.inventory_interval_ms == 0 {
            errors.push("agent_monitor.inventory_interval_ms must be greater than zero".into());
        }
        if monitor.screen_interval_ms == 0 {
            errors.push("agent_monitor.screen_interval_ms must be greater than zero".into());
        }

        match self {
            Self::Legacy { command_contains } => validate_fragments(
                "agents.command_contains",
                command_contains,
                true,
                &mut errors,
            ),
            Self::Named(providers) => {
                let mut ids = std::collections::HashSet::new();
                for provider in providers {
                    if !valid_identifier(&provider.id) {
                        errors.push(format!(
                            "agents.id must match [a-z0-9][a-z0-9._-]{{0,63}}: {}",
                            provider.id
                        ));
                    } else if !ids.insert(provider.id.as_str()) {
                        errors.push(format!("duplicate agents.id: {}", provider.id));
                    }
                    if provider.id == "unknown" {
                        errors.push("reserved provider id must not be configured: unknown".into());
                    }
                    if provider.display_name.trim().is_empty() {
                        errors.push(format!(
                            "agents.display_name must not be empty for {}",
                            provider.id
                        ));
                    }
                    validate_fragments(
                        "agents.command_contains",
                        &provider.command_contains,
                        true,
                        &mut errors,
                    );
                    validate_fragments("agents.busy_all", &provider.busy_all, false, &mut errors);
                    validate_fragments("agents.idle_all", &provider.idle_all, false, &mut errors);
                    validate_fragments(
                        "agents.summary_line_prefixes",
                        &provider.summary_line_prefixes,
                        false,
                        &mut errors,
                    );
                }
            }
        }
        errors
    }
}

fn valid_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
}

fn validate_fragments(
    field: &str,
    fragments: &[String],
    require_one: bool,
    errors: &mut Vec<String>,
) {
    if require_one && fragments.is_empty() {
        errors.push(format!("{field} requires at least one fragment"));
    }
    if fragments.iter().any(|fragment| fragment.trim().is_empty()) {
        errors.push(format!("{field} must not contain empty fragments"));
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct AgentMonitorSettings {
    pub inventory_interval_ms: u64,
    pub screen_interval_ms: u64,
    pub hook_grace_ms: u64,
}

impl Default for AgentMonitorSettings {
    fn default() -> Self {
        Self {
            inventory_interval_ms: 1_000,
            screen_interval_ms: 500,
            hook_grace_ms: 2_000,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TtsSettings {
    pub command: String,
    pub voice: Option<String>,
    pub args: Option<Vec<String>>,
    pub fallback_command: Option<String>,
    pub fallback_voice: Option<String>,
    pub fallback_args: Option<Vec<String>>,
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
    pub skip_if_pane_active: bool,
    pub away_channel: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct TelegramSettings {
    pub bot_token: Option<String>,
    pub chat_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub grpc: GrpcSettings,
    pub imessage: ImessageSettings,
    pub chat_db: ChatDbSettings,
    pub ai: AiSettings,
    #[serde(default)]
    pub(crate) agents: AgentSettings,
    #[serde(default)]
    pub(crate) agent_monitor: AgentMonitorSettings,
    pub tts: TtsSettings,
    pub log: LogSettings,
    pub store: StoreSettings,
    pub notify: NotifySettings,
    #[serde(default)]
    pub telegram: TelegramSettings,
}

impl Settings {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        errors.extend(self.agents.validate(&self.agent_monitor));
        match self.notify.away_channel.as_str() {
            "imessage" => {
                if self.imessage.recipient.is_none() {
                    errors.push("imessage.recipient is required".into());
                }
                if self.imessage.handle_ids.is_empty() {
                    errors.push("imessage.handle_ids requires at least one handle ID".into());
                }
            }
            "telegram" => {
                if self.telegram.bot_token.is_none() {
                    errors.push(
                        "telegram.bot_token is required when away_channel = \"telegram\"".into(),
                    );
                }
                if self.telegram.chat_id.is_none() {
                    errors.push(
                        "telegram.chat_id is required when away_channel = \"telegram\"".into(),
                    );
                }
            }
            other => {
                errors.push(format!(
                    "notify.away_channel must be \"imessage\" or \"telegram\", got \"{other}\""
                ));
            }
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
        if matches!(settings.agents, AgentSettings::Legacy { .. }) {
            warn!(
                "legacy [agents].command_contains configuration is deprecated; migrate to [[agents]]"
            );
        }
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
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
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

#[cfg(test)]
mod tests {
    use config::{Config, File, FileFormat};
    use serde::Deserialize;

    use super::{AgentMonitorSettings, AgentProviderSettings, AgentSettings};

    #[derive(Debug, Deserialize)]
    struct AgentConfigFixture {
        agents: AgentSettings,
        #[serde(default)]
        agent_monitor: AgentMonitorSettings,
    }

    fn parse_agent_config(sources: &[&str]) -> AgentConfigFixture {
        let mut builder = Config::builder();
        for source in sources {
            builder = builder.add_source(File::from_str(source, FileFormat::Toml));
        }
        builder
            .build()
            .and_then(Config::try_deserialize)
            .expect("agent settings fixture should deserialize")
    }

    fn provider(id: &str) -> AgentProviderSettings {
        AgentProviderSettings {
            id: id.to_string(),
            display_name: "Provider".to_string(),
            command_contains: vec!["agent".to_string()],
            busy_all: vec!["Working".to_string()],
            idle_all: vec!["Ready".to_string()],
            summary_line_prefixes: vec![">".to_string()],
        }
    }

    #[test]
    fn named_and_legacy_agent_settings_both_deserialize() {
        let named = parse_agent_config(&[r#"
            [[agents]]
            id = "codex"
            display_name = "Codex"
            command_contains = ["codex"]
            busy_all = ["Working"]
            idle_all = ["Ask Codex"]
            summary_line_prefixes = ["›"]

            [[agents]]
            id = "claude"
            display_name = "Claude"
            command_contains = ["claude"]
        "#]);
        assert_eq!(named.agent_monitor, AgentMonitorSettings::default());
        let AgentSettings::Named(providers) = named.agents else {
            panic!("expected named providers");
        };
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id, "codex");
        assert_eq!(providers[1].id, "claude");

        let legacy = parse_agent_config(&[r#"
            [agents]
            command_contains = ["claude", "codex"]
        "#]);
        let AgentSettings::Legacy { command_contains } = legacy.agents else {
            panic!("expected legacy settings");
        };
        assert_eq!(command_contains, ["claude", "codex"]);
    }

    #[test]
    fn legacy_local_table_replaces_named_default_array() {
        let settings = parse_agent_config(&[
            r#"
                [[agents]]
                id = "codex"
                display_name = "Codex"
                command_contains = ["codex"]
            "#,
            r#"
                [agents]
                command_contains = ["future-agent"]
            "#,
        ]);

        let AgentSettings::Legacy { command_contains } = settings.agents else {
            panic!("expected local legacy table to replace the named defaults");
        };
        assert_eq!(command_contains, ["future-agent"]);
    }

    #[test]
    fn agent_settings_validation_rejects_unsafe_or_ambiguous_configuration() {
        let monitor = AgentMonitorSettings::default();
        let invalid_ids = [
            "Codex".to_string(),
            "-codex".to_string(),
            "co dex".to_string(),
            "a".repeat(65),
        ];
        for id in invalid_ids {
            let errors = AgentSettings::Named(vec![provider(&id)]).validate(&monitor);
            assert!(errors.iter().any(|error| error.contains("agents.id")));
        }

        let errors =
            AgentSettings::Named(vec![provider("codex"), provider("codex")]).validate(&monitor);
        assert!(errors.iter().any(|error| error.contains("duplicate")));

        let errors = AgentSettings::Named(vec![provider("unknown")]).validate(&monitor);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("reserved provider id"))
        );

        let mut invalid = provider("codex");
        invalid.display_name = "  ".to_string();
        invalid.command_contains = vec![" ".to_string()];
        invalid.busy_all = vec!["".to_string()];
        invalid.idle_all = vec![" ".to_string()];
        invalid.summary_line_prefixes = vec!["\t".to_string()];
        let errors = AgentSettings::Named(vec![invalid]).validate(&monitor);
        assert!(errors.iter().any(|error| error.contains("display_name")));
        assert!(
            errors
                .iter()
                .any(|error| error.contains("command_contains"))
        );
        assert!(errors.iter().any(|error| error.contains("busy_all")));
        assert!(errors.iter().any(|error| error.contains("idle_all")));
        assert!(
            errors
                .iter()
                .any(|error| error.contains("summary_line_prefixes"))
        );
    }

    #[test]
    fn default_named_providers_have_verified_screen_contracts() {
        let defaults = parse_agent_config(&[include_str!("../config/default.toml")]);
        let AgentSettings::Named(providers) = defaults.agents else {
            panic!("expected named default providers");
        };

        for provider in &providers {
            assert!(
                !provider.busy_all.is_empty(),
                "{} needs a verified busy clause",
                provider.id
            );
            assert!(
                !provider.idle_all.is_empty(),
                "{} needs a verified idle clause",
                provider.id
            );
        }

        for provider_id in ["codex", "claude"] {
            let provider = providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .expect("default provider should exist");
            assert!(
                !provider.summary_line_prefixes.is_empty(),
                "{provider_id} needs a verified safe summary prefix"
            );
        }

        let opencode = providers
            .iter()
            .find(|provider| provider.id == "opencode")
            .expect("OpenCode default should exist");
        assert!(
            opencode.summary_line_prefixes.is_empty(),
            "OpenCode's prompt and user-message rows share the same visible prefix"
        );
    }

    #[test]
    fn monitor_polling_intervals_must_be_non_zero() {
        let agents = AgentSettings::Named(vec![provider("codex")]);
        let zero_inventory = AgentMonitorSettings {
            inventory_interval_ms: 0,
            ..AgentMonitorSettings::default()
        };
        assert!(
            agents
                .validate(&zero_inventory)
                .iter()
                .any(|error| error.contains("inventory_interval_ms"))
        );

        let zero_screen = AgentMonitorSettings {
            screen_interval_ms: 0,
            hook_grace_ms: 0,
            ..AgentMonitorSettings::default()
        };
        let errors = agents.validate(&zero_screen);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("screen_interval_ms"))
        );
        assert!(!errors.iter().any(|error| error.contains("hook_grace_ms")));
    }

    #[test]
    fn legacy_matcher_rejects_empty_fragments() {
        let errors = AgentSettings::Legacy {
            command_contains: vec!["codex".to_string(), " ".to_string()],
        }
        .validate(&AgentMonitorSettings::default());

        assert!(
            errors
                .iter()
                .any(|error| error.contains("command_contains"))
        );
    }
}
