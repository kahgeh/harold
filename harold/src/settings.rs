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

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub(crate) struct ActivitySummarySettings {
    pub enabled: bool,
    pub cli_path: String,
    pub model: String,
    pub effort: String,
    pub timeout_ms: u64,
    pub max_concurrent: usize,
    pub max_pending: usize,
    pub max_instruction_chars: usize,
    pub max_reply_chars: usize,
    pub max_output_bytes: usize,
    #[serde(skip)]
    pub environment: ActivitySummaryEnvironment,
}

#[derive(Clone)]
pub(crate) struct ActivitySummaryEnvironment(pub Vec<(std::ffi::OsString, std::ffi::OsString)>);

impl Default for ActivitySummaryEnvironment {
    fn default() -> Self {
        // Keep authentication and executable discovery, without inheriting agent
        // hooks, provider redirects, debug logging, or parent tmux identity.
        const ALLOWED: &[&str] = &[
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ];
        Self(
            ALLOWED
                .iter()
                .filter_map(|key| std::env::var_os(key).map(|value| ((*key).into(), value)))
                .collect(),
        )
    }
}

impl std::fmt::Debug for ActivitySummaryEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ActivitySummaryEnvironment([redacted])")
    }
}

impl Default for ActivitySummarySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            cli_path: "claude".into(),
            model: "sonnet".into(),
            effort: "low".into(),
            timeout_ms: 15_000,
            max_concurrent: 2,
            max_pending: 64,
            max_instruction_chars: 4_000,
            max_reply_chars: 8_000,
            max_output_bytes: 65_536,
            environment: ActivitySummaryEnvironment::default(),
        }
    }
}

impl ActivitySummarySettings {
    pub(crate) fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (name, value, maximum) in [
            ("timeout_ms", self.timeout_ms, 120_000),
            ("max_concurrent", self.max_concurrent as u64, 16),
            ("max_pending", self.max_pending as u64, 4_096),
            (
                "max_instruction_chars",
                self.max_instruction_chars as u64,
                32_000,
            ),
            ("max_reply_chars", self.max_reply_chars as u64, 32_000),
            ("max_output_bytes", self.max_output_bytes as u64, 1_048_576),
        ] {
            if !(1..=maximum).contains(&value) {
                errors.push(format!(
                    "activity_summary.{name} must be between 1 and {maximum}"
                ));
            }
        }
        if self.cli_path.trim().is_empty()
            || self.cli_path.len() > 4_096
            || self.cli_path.chars().any(char::is_control)
        {
            errors.push("activity_summary.cli_path must be a nonempty executable path of at most 4096 bytes without controls".into());
        }
        if self.model.is_empty()
            || self.model.len() > 128
            || !self.model.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte))
        {
            errors.push(
                "activity_summary.model must be a model name of at most 128 ASCII token characters"
                    .into(),
            );
        }
        if !matches!(
            self.effort.as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            errors.push("activity_summary.effort must be low, medium, high, xhigh, or max".into());
        }
        errors
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ScreenAdapter {
    #[default]
    GenericV1,
    CodexV1,
}

fn default_screen_history_lines() -> u16 {
    2000
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
    #[serde(default)]
    pub screen_adapter: ScreenAdapter,
    #[serde(default = "default_screen_history_lines")]
    pub screen_history_lines: u16,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub(crate) struct AgentSettings(pub Vec<AgentProviderSettings>);

impl AgentSettings {
    pub(crate) fn matches_command(&self, command: &str) -> bool {
        let command = command.trim().to_lowercase();
        let contains = |fragment: &str| command.contains(&fragment.trim().to_lowercase());
        self.0
            .iter()
            .flat_map(|provider| &provider.command_contains)
            .any(|fragment| !fragment.trim().is_empty() && contains(fragment))
    }

    pub(crate) fn validate(&self, monitor: &AgentMonitorSettings) -> Vec<String> {
        let mut errors = Vec::new();
        if monitor.inventory_interval_ms == 0 {
            errors.push("agent_monitor.inventory_interval_ms must be greater than zero".into());
        }
        if monitor.screen_interval_ms == 0 {
            errors.push("agent_monitor.screen_interval_ms must be greater than zero".into());
        }

        let mut ids = std::collections::HashSet::new();
        for provider in &self.0 {
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
            if !(1..=10_000).contains(&provider.screen_history_lines) {
                errors.push("agents.screen_history_lines must be between 1 and 10000".into());
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
    pub(crate) activity_summary: ActivitySummarySettings,
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
        errors.extend(self.activity_summary.validate());
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
            screen_adapter: crate::settings::ScreenAdapter::GenericV1,
            screen_history_lines: 2000,
        }
    }

    #[test]
    fn screen_adapter_defaults_and_names_are_explicit_and_validated() {
        let parsed = parse_agent_config(&[r#"[[agents]]
            id = "custom"
            display_name = "Custom"
            command_contains = ["custom"]
        "#]);
        let AgentSettings(providers) = parsed.agents;
        assert_eq!(providers[0].screen_adapter, super::ScreenAdapter::GenericV1);
        assert_eq!(providers[0].screen_history_lines, 2000);
        assert_eq!(
            serde_json::from_str::<super::ScreenAdapter>(r#""codex-v1""#).unwrap(),
            super::ScreenAdapter::CodexV1
        );
        assert!(serde_json::from_str::<super::ScreenAdapter>(r#""unknown-v1""#).is_err());
        let shipped = parse_agent_config(&[include_str!("../config/default.toml")]);
        let AgentSettings(providers) = shipped.agents;
        assert_eq!(providers[0].screen_adapter, super::ScreenAdapter::CodexV1);
        assert!(
            providers[1..]
                .iter()
                .all(|provider| provider.screen_adapter == super::ScreenAdapter::GenericV1)
        );
    }

    #[test]
    fn screen_history_rejects_zero_and_excessive_limits() {
        for limit in [0, 10001] {
            let text = format!(
                r#"[[agents]]
                id = "custom"
                display_name = "Custom"
                command_contains = ["custom"]
                screen_history_lines = {limit}
            "#
            );
            let parsed = parse_agent_config(&[&text]);
            assert!(!parsed.agents.validate(&parsed.agent_monitor).is_empty());
        }
    }

    #[test]
    fn agents_configuration_requires_a_provider_list() {
        let result = Config::builder()
            .add_source(File::from_str(
                "[agents]\ncommand_contains = [\"codex\"]",
                FileFormat::Toml,
            ))
            .build()
            .and_then(Config::try_deserialize::<AgentConfigFixture>);
        assert!(result.is_err());
    }

    #[test]
    fn named_agent_settings_deserialize_with_optional_defaults() {
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
        let AgentSettings(providers) = named.agents;
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id, "codex");
        assert_eq!(providers[1].id, "claude");

        assert!(providers[1].busy_all.is_empty());
        assert!(providers[1].idle_all.is_empty());
        assert!(providers[1].summary_line_prefixes.is_empty());
    }

    #[test]
    fn local_provider_list_replaces_default_list() {
        let settings = parse_agent_config(&[
            include_str!("../config/default.toml"),
            r#"
                [[agents]]
                id = "custom"
                display_name = "Custom"
                command_contains = ["custom-agent"]
            "#,
        ]);

        assert_eq!(settings.agents.0.len(), 1);
        assert_eq!(settings.agents.0[0].id, "custom");
        assert!(settings.agents.matches_command("/bin/CUSTOM-AGENT --run"));
        assert!(!settings.agents.matches_command("codex"));
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
            let errors = AgentSettings(vec![provider(&id)]).validate(&monitor);
            assert!(errors.iter().any(|error| error.contains("agents.id")));
        }

        let errors = AgentSettings(vec![provider("codex"), provider("codex")]).validate(&monitor);
        assert!(errors.iter().any(|error| error.contains("duplicate")));

        let errors = AgentSettings(vec![provider("unknown")]).validate(&monitor);
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
        let errors = AgentSettings(vec![invalid]).validate(&monitor);
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
        let AgentSettings(providers) = defaults.agents;

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
        let agents = AgentSettings(vec![provider("codex")]);
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
    fn command_matching_ignores_empty_fragments_and_normalizes_case() {
        let mut custom = provider("custom");
        custom.command_contains = vec![" ".into(), " Custom-Agent ".into()];
        let settings = AgentSettings(vec![custom]);
        assert!(settings.matches_command(" /opt/bin/CUSTOM-AGENT --run "));
        assert!(!settings.matches_command("unrelated-command"));
        assert!(!AgentSettings::default().matches_command("codex"));
    }
}
