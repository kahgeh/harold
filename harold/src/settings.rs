use std::net::{AddrParseError, IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

use config::{Config, ConfigError, File, FileFormat};
use serde::Deserialize;
use tracing::warn;

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

pub(crate) fn expand_tilde(path: &str) -> String {
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
    pub fn addr(&self) -> Result<SocketAddr, AddrParseError> {
        Ok(SocketAddr::new(parse_host(&self.host)?, self.port))
    }
}

/// A host as an address: `localhost` is `127.0.0.1`, anything else must be an IP address
/// (IPv6 without brackets).
fn parse_host(host: &str) -> Result<IpAddr, AddrParseError> {
    if host == "localhost" {
        return Ok(Ipv4Addr::LOCALHOST.into());
    }
    host.parse()
}

/// Whether `host` is an address on this machine only: `localhost` or any loopback IP.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    parse_host(host).is_ok_and(|ip| ip.is_loopback())
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

/// The `[watch]` table: the model every watch runs, and the user's optional budgets. A
/// budget is enforced only when set; an absent one is never treated as 0.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct WatchSettings {
    pub model: String,
    /// Every watch together, per local day.
    pub daily_budget_usd: Option<f64>,
    /// A run's limit when `start` gives no `--run-budget`.
    pub run_budget_usd: Option<f64>,
    /// The most `--run-budget` may ask.
    pub max_run_budget_usd: Option<f64>,
    /// Folders a watch skill's real path may lie in, besides the watch's own
    /// `.claude/skills/`. Absolute paths only.
    pub skill_folders: Vec<String>,
}

impl Default for WatchSettings {
    fn default() -> Self {
        Self {
            model: "haiku".into(),
            daily_budget_usd: None,
            run_budget_usd: None,
            max_run_budget_usd: None,
            skill_folders: Vec::new(),
        }
    }
}

impl WatchSettings {
    pub(crate) fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.model.is_empty()
            || self.model.len() > 128
            || !self.model.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte))
        {
            errors.push(
                "watch.model must be a model name of at most 128 ASCII token characters".into(),
            );
        }
        for (name, value) in [
            ("daily_budget_usd", self.daily_budget_usd),
            ("run_budget_usd", self.run_budget_usd),
            ("max_run_budget_usd", self.max_run_budget_usd),
        ] {
            if value.is_some_and(|value| !(value.is_finite() && value > 0.0)) {
                errors.push(format!("watch.{name} must be above 0 when set"));
            }
        }
        // `~/` is not expanded, as for timed prompts' `folder`. A folder that does not
        // exist is not an error: one Mac can list an organisation repository it has not
        // cloned.
        if self.skill_folders.iter().any(|folder| {
            !folder.starts_with('/') || folder.len() > 4_096 || folder.chars().any(char::is_control)
        }) {
            errors
                .push("watch.skill_folders entries must be absolute paths starting with /".into());
        }
        if let (Some(run), Some(max)) = (self.run_budget_usd, self.max_run_budget_usd)
            && run > max
        {
            errors
                .push("watch.run_budget_usd must be no more than watch.max_run_budget_usd".into());
        }
        errors
    }
}

/// The `[call_answerer]` table: the small model that answers each turn of a call. Its
/// program and environment are the activity summary's (`cli_path`, `environment`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct CallAnswererSettings {
    pub enabled: bool,
    pub model: String,
    pub timeout_seconds: u64,
    /// `--max-budget-usd` for each run.
    pub run_budget_usd: f64,
}

impl Default for CallAnswererSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "haiku".into(),
            timeout_seconds: 15,
            run_budget_usd: 0.02,
        }
    }
}

impl CallAnswererSettings {
    pub(crate) fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if !valid_model_name(&self.model) {
            errors.push(
                "call_answerer.model must be a model name of at most 128 ASCII token characters"
                    .into(),
            );
        }
        if !(1..=25).contains(&self.timeout_seconds) {
            errors.push("call_answerer.timeout_seconds must be from 1 to 25, under the 30 seconds the call waits for a run".into());
        }
        if !(self.run_budget_usd.is_finite() && self.run_budget_usd > 0.0) {
            errors.push("call_answerer.run_budget_usd must be above 0".into());
        }
        errors
    }
}

/// Whether `model` is a model name: at most 128 ASCII token characters.
fn valid_model_name(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model.as_bytes()[0].is_ascii_alphanumeric()
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte))
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ScreenAdapter {
    #[default]
    GenericV1,
    CodexV1,
    ClaudeV1,
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
        if monitor.inventory_timeout_ms == 0 {
            errors.push("agent_monitor.inventory_timeout_ms must be greater than zero".into());
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
    pub inventory_timeout_ms: u64,
    pub screen_interval_ms: u64,
    pub hook_grace_ms: u64,
    /// How long an adapter's report stands against the screen reading.
    pub report_grace_ms: u64,
}

impl Default for AgentMonitorSettings {
    fn default() -> Self {
        Self {
            inventory_interval_ms: 1_000,
            inventory_timeout_ms: 3_000,
            screen_interval_ms: 500,
            hook_grace_ms: 2_000,
            report_grace_ms: 30_000,
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

/// Where Harold sends its telemetry and takes alerts. `default.toml` sets the collector's
/// address, so export is on unless `export` is false. With no `alert_listen` no alert
/// address is opened, and with no `pod_registry` no pod is read.
#[derive(Debug, Clone, Deserialize)]
pub struct TelemetrySettings {
    /// Whether Harold sends to the collector. False is export off: no pipeline is built,
    /// though `otlp_endpoint` is still checked.
    #[serde(default = "export_on")]
    pub export: bool,
    /// The OpenTelemetry collector's gRPC address, such as `http://localhost:4317`.
    pub otlp_endpoint: Option<String>,
    /// The address and port Harold takes alerts on, such as `127.0.0.1:4320`.
    pub alert_listen: Option<String>,
    /// The path of the chief's `products.toml`. Set it on one Mac only, or merges are
    /// counted twice.
    pub pod_registry: Option<String>,
}

fn export_on() -> bool {
    true
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            export: true,
            otlp_endpoint: None,
            alert_listen: None,
            pod_registry: None,
        }
    }
}

impl TelemetrySettings {
    /// The alert address. `Settings::validate` reports one that does not parse.
    pub fn alert_addr(&self) -> Option<SocketAddr> {
        self.alert_listen.as_deref()?.parse().ok()
    }

    /// The pod registry's path, with a leading `~/` expanded.
    pub fn pod_registry_path(&self) -> Option<std::path::PathBuf> {
        let path = self.pod_registry.as_deref()?;
        Some(expand_tilde(path.trim()).into())
    }
}

/// The Python and arguments that run the voice worker for calls with the chief.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VoiceInputSettings {
    /// The Python the voice worker runs with.
    pub command: String,
    /// The worker's arguments. `--model` names the Whisper model.
    #[serde(default)]
    pub args: Vec<String>,
}

impl VoiceInputSettings {
    /// The value after `--model` in `args`.
    pub(crate) fn whisper_model(&self) -> Option<&str> {
        let at = self.args.iter().position(|arg| arg == "--model")?;
        self.args.get(at + 1).map(String::as_str)
    }
}

fn voice_input_errors(voice_input: Option<&VoiceInputSettings>) -> Vec<String> {
    let Some(voice_input) = voice_input else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    if voice_input.command.trim().is_empty() {
        errors.push("voice_input.command must not be empty".into());
    }
    if voice_input.whisper_model().is_none() {
        errors.push("voice_input.args must name the Whisper model with --model".into());
    }
    errors
}

fn telemetry_errors(telemetry: &TelemetrySettings) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(endpoint) = &telemetry.otlp_endpoint {
        // Only plain http:// is accepted: no TLS support is built, so an https://
        // collector could only fail at export.
        let has_scheme = endpoint.starts_with("http://");
        if !has_scheme || tonic::transport::Endpoint::from_shared(endpoint.clone()).is_err() {
            errors.push(
                "telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317"
                    .into(),
            );
        }
    }
    if let Some(listen) = &telemetry.alert_listen
        && listen.parse::<SocketAddr>().is_err()
    {
        errors.push(
            "telemetry.alert_listen must be an address and port such as 127.0.0.1:4320".into(),
        );
    }
    if let Some(registry) = &telemetry.pod_registry
        && registry.trim().is_empty()
    {
        errors.push("telemetry.pod_registry must be the path of the chief's products.toml".into());
    }
    errors
}

/// The home-screen app Harold serves to the phone through `tailscale serve`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WebAppSettings {
    /// The loopback address and port Harold listens on, such as `127.0.0.1:4330`.
    pub listen: String,
    /// The Tailscale login allowed to use the app.
    pub allowed_login: String,
    /// The name on the home screen and in the app's title; "Chief" when left out.
    pub app_name: Option<String>,
    /// The path of the app's PNG icon, with a leading `~/` expanded; no icon when left out.
    pub icon: Option<String>,
}

/// The longest `app_name`, in characters.
const APP_NAME_MAX: usize = 30;
const ICON_MIN_SIDE: u32 = 180;
const ICON_MAX_SIDE: u32 = 1024;
const ICON_MAX_BYTES: usize = 1024 * 1024;
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The app's icon, read once at start.
#[derive(Debug, Clone)]
pub(crate) struct WebAppIcon {
    pub bytes: Vec<u8>,
    /// The icon's width and height in pixels, which are equal.
    pub side: u32,
}

/// A PNG's side when it is a square of `ICON_MIN_SIDE` to `ICON_MAX_SIDE` pixels, read from
/// the signature and the IHDR chunk's width and height.
fn square_png_side(bytes: &[u8]) -> Option<u32> {
    if bytes.len() > ICON_MAX_BYTES || bytes.len() < 24 || bytes[..8] != PNG_SIGNATURE {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width == height && (ICON_MIN_SIDE..=ICON_MAX_SIDE).contains(&width)).then_some(width)
}

impl WebAppSettings {
    /// The name under the icon.
    pub(crate) fn name(&self) -> &str {
        self.app_name.as_deref().unwrap_or("Chief")
    }

    /// Reads and checks the icon file. Ok(None) when no icon is set; Err is the message
    /// Harold refuses to start with. The bytes are never part of a message.
    pub(crate) fn load_icon(&self) -> Result<Option<WebAppIcon>, String> {
        let Some(icon) = &self.icon else {
            return Ok(None);
        };
        let path = expand_tilde(icon);
        let unreadable = || format!("web_app.icon cannot be read: {path}");
        if !std::path::Path::new(&path).is_absolute() {
            return Err(unreadable());
        }
        let bytes = std::fs::read(&path).map_err(|_| unreadable())?;
        let side = square_png_side(&bytes).ok_or_else(|| {
            "web_app.icon must be a square PNG of 180 to 1024 pixels a side, at most 1 MB"
                .to_owned()
        })?;
        Ok(Some(WebAppIcon { bytes, side }))
    }

    /// The listen address. `Settings::validate` reports one that does not parse.
    pub(crate) fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen.parse().ok()
    }
}

fn web_app_errors(web_app: Option<&WebAppSettings>, chief_folder: Option<&str>) -> Vec<String> {
    let Some(web_app) = web_app else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    if !web_app
        .listen_addr()
        .is_some_and(|addr| addr.ip().is_loopback())
    {
        errors.push(
            "web_app.listen must be a loopback address and port such as 127.0.0.1:4330".into(),
        );
    }
    if web_app.allowed_login.trim().is_empty() {
        errors.push("web_app.allowed_login must be your Tailscale login".into());
    }
    if chief_folder.is_none() {
        errors.push("web_app needs notify.chief, the chief's folder".into());
    }
    if let Some(name) = &web_app.app_name {
        let length = name.chars().count();
        if length == 0 || length > APP_NAME_MAX || name.chars().any(char::is_control) {
            errors.push("web_app.app_name must be 1 to 30 characters on one line".into());
        }
    }
    if let Err(error) = web_app.load_icon() {
        errors.push(error);
    }
    errors
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
#[serde(deny_unknown_fields)]
pub struct NotifySettings {
    pub skip_if_session_active: bool,
    pub skip_if_pane_active: bool,
    /// `notify.away_channel`, read so that `away_channel = "imessage"` still loads. Harold
    /// no longer reads it (`away_channel_still_set` warns), and any other value is refused
    /// before this is read (`removed_setting_messages`).
    #[serde(default, rename = "away_channel")]
    _away_channel: Option<String>,
    /// Which pane is the chief, matched by folder as one entry (see `same_folder`).
    #[serde(default)]
    pub chief: Option<String>,
    /// The tmux session name of the chief. Among the agents in the chief's folder, the
    /// chief is the first agent pane of the session with this exact name. Unset keeps
    /// the longest-running agent's session.
    #[serde(default)]
    pub chief_session: Option<String>,
    /// Who may reach the phone by iMessage: folder entries. Empty means nobody and
    /// `["*"]` means everyone.
    #[serde(default)]
    pub imessage: Vec<String>,
    /// Who may reach the phone through the web app.
    #[serde(default)]
    pub web_app: Vec<String>,
    /// Who may be spoken aloud on the Mac.
    #[serde(default)]
    pub machine_audio_output: Vec<String>,
    /// The key `chief` and the channel lists replace. Read so that its folder can stand in
    /// for them (`apply_old_key`).
    #[serde(default)]
    pub away_messages_only_from: Option<String>,
    /// The name `away_messages_only_from` had while it covered turn-end updates only.
    /// Read only so that `validate` can report it.
    #[serde(default)]
    away_updates_only_from: Option<String>,
    /// Whether `apply_old_key` used `away_messages_only_from`; `main` warns once logging is up.
    #[serde(skip)]
    pub old_key_applied: bool,
}

/// One way Harold reaches the user. Each has a list in `[notify]` under its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    Imessage,
    WebApp,
    MachineAudioOutput,
}

impl Channel {
    /// The channel's key in `[notify]`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Imessage => "imessage",
            Self::WebApp => "web_app",
            Self::MachineAudioOutput => "machine_audio_output",
        }
    }
}

/// The entry that lets every pane through a channel list.
pub(crate) const EVERYONE: &str = "*";

impl NotifySettings {
    /// The folders whose agents may use `channel`.
    pub(crate) fn list(&self, channel: Channel) -> &[String] {
        match channel {
            Channel::Imessage => &self.imessage,
            Channel::WebApp => &self.web_app,
            Channel::MachineAudioOutput => &self.machine_audio_output,
        }
    }
}

/// Reads `away_messages_only_from` as `chief` and as each channel list that the local
/// layers (`local_keys`, the `[notify]` keys they set) do not set. Returns whether the old
/// key was set, so the caller can warn once.
pub(crate) fn apply_old_key(notify: &mut NotifySettings, local_keys: &[String]) -> bool {
    let Some(folder) = notify.away_messages_only_from.clone() else {
        return false;
    };
    let set = |key: &str| local_keys.iter().any(|local| local == key);
    if !set("chief") {
        notify.chief = Some(folder.clone());
    }
    for (key, list) in [
        ("imessage", &mut notify.imessage),
        ("web_app", &mut notify.web_app),
        ("machine_audio_output", &mut notify.machine_audio_output),
    ] {
        if !set(key) {
            *list = vec![folder.clone()];
        }
    }
    true
}

/// What is wrong with one entry of `chief` or a channel list, naming `key` and the entry.
/// `everyone_ok` lets `"*"` through, as in the lists.
fn entry_error(key: &str, entry: &str, everyone_ok: bool) -> Option<String> {
    if everyone_ok && entry == EVERYONE {
        return None;
    }
    if entry == EVERYONE {
        return Some(format!("notify.{key} must name a folder, not \"{entry}\""));
    }
    let mut components = entry.split('/').filter(|part| !part.is_empty()).peekable();
    let problem = if components.peek().is_none() {
        "must not be empty or only slashes"
    } else if components.any(|part| part == ".." || part == ".") {
        "must not hold \"..\" or \".\" as a folder"
    } else {
        return None;
    };
    Some(format!("notify.{key} entry \"{entry}\" {problem}"))
}

fn notify_entry_errors(notify: &NotifySettings) -> Vec<String> {
    let mut errors: Vec<String> = notify
        .chief
        .iter()
        .filter_map(|entry| entry_error("chief", entry, false))
        .collect();
    if let Some(session) = &notify.chief_session
        && (session.is_empty() || session.contains([':', '.']))
    {
        errors.push(
            "notify.chief_session must be a tmux session name, not empty and with no ':' or '.'"
                .into(),
        );
    }
    for channel in [
        Channel::Imessage,
        Channel::WebApp,
        Channel::MachineAudioOutput,
    ] {
        errors.extend(
            notify
                .list(channel)
                .iter()
                .filter_map(|entry| entry_error(channel.key(), entry, true)),
        );
    }
    errors
}

/// Whether `name` can identify a Mac in a reply tag. `/` and the brackets delimit the tag
/// (`[machine/pane]`). `:` is in every pane label (`session:window.pane`), so a name
/// without one is never mistaken for a label.
pub(crate) fn is_valid_machine_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '[' | ']' | ':'))
}

fn machine_name_error(name: &str) -> Option<String> {
    if is_valid_machine_name(name) {
        return None;
    }
    Some(format!(
        "machine_name must not be empty or contain whitespace, '/', '[', ']' or ':', got \"{name}\""
    ))
}

fn away_messages_only_from_error(folder: Option<&str>) -> Option<String> {
    let folder = folder?;
    if folder.starts_with('/') {
        return None;
    }
    Some(format!(
        "notify.away_messages_only_from must be an absolute path starting with \"/\", got \"{folder}\""
    ))
}

/// The error for a configuration that still has the key's old name, whatever its value
/// and whether or not the new key is there as well.
fn away_updates_only_from_error(old_key: Option<&str>) -> Option<String> {
    old_key?;
    Some(
        "notify.away_updates_only_from is now notify.away_messages_only_from; rename the key"
            .to_string(),
    )
}

/// What is wrong with the `machines` list, one error for each problem. An empty list is
/// valid: with `machines` empty multi-Mac routing is off. Harold keeps no latest notifier
/// and recognises no machine name but its own.
fn machines_errors(machine_name: &str, machines: &[String]) -> Vec<String> {
    let mut errors = machines
        .iter()
        .filter(|name| !is_valid_machine_name(name))
        .map(|name| {
            format!(
                "machines must contain only machine names (not empty, no whitespace, '/', '[', ']' or ':'), got \"{name}\""
            )
        })
        .collect::<Vec<_>>();
    let includes_this_mac = machines
        .iter()
        .any(|name| name.to_lowercase() == machine_name.to_lowercase());
    if !machines.is_empty() && !includes_this_mac {
        errors.push(format!(
            "machines must include this Mac's machine_name \"{machine_name}\", got {machines:?}"
        ));
    }
    errors
}

fn machine_name_from_hostname(hostname: &str) -> String {
    hostname.split_whitespace().collect::<Vec<_>>().join("-")
}

/// The Mac's short hostname. Empty when it cannot be read, which `validate` rejects, so
/// Harold never sends notifications under a made-up name.
fn default_machine_name() -> String {
    let out = std::process::Command::new("hostname").arg("-s").output();
    match out {
        Ok(out) if out.status.success() => {
            machine_name_from_hostname(&String::from_utf8_lossy(&out.stdout))
        }
        _ => {
            warn!("could not read the hostname; set machine_name in the configuration");
            String::new()
        }
    }
}

fn default_give_up_after_minutes() -> i64 {
    120
}

/// One `[[timed_prompts]]` entry: a text Harold types into the pane of the agent running
/// in `folder` at `at` each day. Every key defaults to empty when the entry is read, so
/// `validate` reports a missing key by the entry's position like any other invalid value.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct TimedPromptSettings {
    /// Names the prompt in the log and in the stored record.
    #[serde(default)]
    pub name: String,
    /// Local time of day, `HH:MM`.
    #[serde(default)]
    pub at: String,
    /// An absolute path, compared as text with a pane's working directory.
    #[serde(default)]
    pub folder: String,
    /// What is typed, followed by Enter.
    #[serde(default)]
    pub text: String,
    /// How long after `at` the prompt stays due.
    #[serde(default = "default_give_up_after_minutes")]
    pub give_up_after_minutes: i64,
}

/// The minute of the day a 24-hour `HH:MM` names, from 0 for `00:00` to 1439 for `23:59`.
pub(crate) fn minute_of_day(at: &str) -> Option<u32> {
    let bytes = at.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let two_digits = |pair: &[u8]| {
        pair.iter()
            .all(u8::is_ascii_digit)
            .then(|| u32::from(pair[0] - b'0') * 10 + u32::from(pair[1] - b'0'))
    };
    let hours = two_digits(&bytes[..2])?;
    let minutes = two_digits(&bytes[3..])?;
    (hours <= 23 && minutes <= 59).then_some(hours * 60 + minutes)
}

/// What is wrong with the `timed_prompts` list, one error for each invalid value. Each
/// error starts with the entry's position and the key.
fn timed_prompts_errors(prompts: &[TimedPromptSettings]) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, prompt) in prompts.iter().enumerate() {
        let name = &prompt.name;
        let name_is_valid = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        let earlier = prompts[..index]
            .iter()
            .position(|earlier| earlier.name == *name);
        if !name_is_valid {
            errors.push(format!(
                "timed_prompts[{index}].name must be 1 to 64 characters from letters, digits, \"-\" and \"_\", got \"{name}\""
            ));
        } else if let Some(earlier) = earlier {
            errors.push(format!(
                "timed_prompts[{index}].name \"{name}\" is already used by timed_prompts[{earlier}]"
            ));
        }
        if minute_of_day(&prompt.at).is_none() {
            errors.push(format!(
                "timed_prompts[{index}].at must be a time of day \"HH:MM\" from 00:00 to 23:59, got \"{}\"",
                prompt.at
            ));
        }
        if !prompt.folder.starts_with('/') {
            errors.push(format!(
                "timed_prompts[{index}].folder must be an absolute path starting with \"/\", got \"{}\"",
                prompt.folder
            ));
        }
        // The error gives the length, not the text: the text never goes to a log.
        let length = prompt.text.chars().count();
        if !(1..=2_000).contains(&length) || prompt.text.chars().any(char::is_control) {
            errors.push(format!(
                "timed_prompts[{index}].text must be 1 to 2000 characters on one line with no control characters, got {length} characters"
            ));
        }
        if !(1..=1439).contains(&prompt.give_up_after_minutes) {
            errors.push(format!(
                "timed_prompts[{index}].give_up_after_minutes must be from 1 to 1439, got {}",
                prompt.give_up_after_minutes
            ));
        }
    }
    errors
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    /// Names this Mac in every message Harold sends and in reply tags.
    #[serde(default = "default_machine_name")]
    pub machine_name: String,
    /// The machine names of all the user's Macs, the same list on every Mac. When it is
    /// not empty, only these names are machine names.
    #[serde(default)]
    pub machines: Vec<String>,
    pub grpc: GrpcSettings,
    pub imessage: ImessageSettings,
    pub chat_db: ChatDbSettings,
    pub ai: AiSettings,
    #[serde(default)]
    pub(crate) activity_summary: ActivitySummarySettings,
    #[serde(default)]
    pub(crate) watch: WatchSettings,
    #[serde(default)]
    pub(crate) call_answerer: CallAnswererSettings,
    #[serde(default)]
    pub(crate) agents: AgentSettings,
    #[serde(default)]
    pub(crate) agent_monitor: AgentMonitorSettings,
    pub tts: TtsSettings,
    pub log: LogSettings,
    pub store: StoreSettings,
    pub notify: NotifySettings,
    /// Texts Harold types into an agent's pane at a set time each day.
    #[serde(default)]
    pub(crate) timed_prompts: Vec<TimedPromptSettings>,
    /// The collector, alert address and pod registry. Off when the section is absent.
    #[serde(default)]
    pub telemetry: TelemetrySettings,
    /// Calls with the chief at the desk. Off when the section is absent.
    #[serde(default)]
    pub(crate) voice_input: Option<VoiceInputSettings>,
    /// The home-screen app. Off when the section is absent.
    #[serde(default)]
    pub(crate) web_app: Option<WebAppSettings>,
}

impl Settings {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        errors.extend(machine_name_error(&self.machine_name));
        errors.extend(machines_errors(&self.machine_name, &self.machines));
        errors.extend(self.agents.validate(&self.agent_monitor));
        errors.extend(self.activity_summary.validate());
        errors.extend(self.watch.validate());
        errors.extend(self.call_answerer.validate());
        errors.extend(away_messages_only_from_error(
            self.notify.away_messages_only_from.as_deref(),
        ));
        errors.extend(away_updates_only_from_error(
            self.notify.away_updates_only_from.as_deref(),
        ));
        errors.extend(notify_entry_errors(&self.notify));
        errors.extend(timed_prompts_errors(&self.timed_prompts));
        errors.extend(telemetry_errors(&self.telemetry));
        errors.extend(voice_input_errors(self.voice_input.as_ref()));
        errors.extend(web_app_errors(
            self.web_app.as_ref(),
            self.notify.chief.as_deref(),
        ));
        if self.imessage.recipient.is_none() {
            errors.push("imessage.recipient is required".into());
        }
        if self.imessage.handle_ids.is_empty() {
            errors.push("imessage.handle_ids requires at least one handle ID".into());
        }
        errors
    }

    pub fn load() -> Result<Arc<Self>, ConfigError> {
        let config = merged_config()?;
        let mut settings = config.try_deserialize::<Settings>()?;
        let local = local_config()?;
        let local_keys = local
            .get_table("notify")
            .map(|table| table.into_keys().collect::<Vec<_>>())
            .unwrap_or_default();
        settings.notify.old_key_applied = apply_old_key(&mut settings.notify, &local_keys);
        Ok(Arc::new(settings))
    }

    /// Loads only `default.toml`, so unit tests never see a developer's `local.toml`
    /// or `HAROLD_*` environment overrides.
    #[cfg(test)]
    pub fn load_defaults(config_dir: &str) -> Result<Arc<Self>, ConfigError> {
        let config = Config::builder()
            .add_source(File::new(
                &format!("{config_dir}/default"),
                FileFormat::Toml,
            ))
            .build()?;

        let settings = config.try_deserialize::<Settings>()?;
        Ok(Arc::new(settings))
    }
}

const TELEGRAM_REMOVED: &str = "[telegram] is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables.";
const PEERS_REMOVED: &str = "[peers] is no longer read: two Macs share replies through iMessage, with no hub or member. Remove it from config/local.toml or the HAROLD__ environment variables.";
const NOTIFY_TELEGRAM_REMOVED: &str = "notify.telegram is no longer read: the lists are imessage, web_app and machine_audio_output. Remove it from config/local.toml or the HAROLD__ environment variables.";
const AWAY_CHANNEL_REMOVED: &str = "notify.away_channel is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables.";
const GRPC_HOST_NOT_LOOPBACK: &str = "grpc.host must be a loopback address (127.0.0.0/8, ::1 or localhost): Harold no longer serves other Macs.";

/// The config directory and environment name `load` reads from.
fn config_location() -> (String, String) {
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
    (config_dir, env)
}

/// The `default` file, the local file and the `HAROLD__` environment, laid over each other.
fn merged_config() -> Result<Config, ConfigError> {
    let (config_dir, env) = config_location();
    Config::builder()
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
        .build()
}

/// The local file and the `HAROLD__` environment alone: what the operator set.
fn local_config() -> Result<Config, ConfigError> {
    let (config_dir, env) = config_location();
    Config::builder()
        .add_source(File::new(&format!("{config_dir}/{env}"), FileFormat::Toml).required(false))
        .add_source(
            config::Environment::with_prefix("HAROLD")
                .separator("__")
                .try_parsing(true),
        )
        .build()
}

fn has_key(config: &Config, key: &str) -> bool {
    config.get::<config::Value>(key).is_ok()
}

/// The fixed sentences for each setting the config names that Harold no longer reads. Key
/// presence only: the one value read is `grpc.host`, for the loopback test, and no sentence
/// holds a value. A config that cannot be built returns nothing; `Settings::load` refuses it.
pub(crate) fn removed_setting_messages() -> Vec<String> {
    match (local_config(), merged_config()) {
        (Ok(local), Ok(merged)) => removed_setting_messages_from(&local, &merged),
        _ => Vec::new(),
    }
}

/// Whether the local config still sets `notify.away_channel = "imessage"`, which is ignored.
pub(crate) fn away_channel_still_set() -> bool {
    local_config().is_ok_and(|local| away_channel_is_imessage(&local))
}

fn away_channel_is_imessage(local: &Config) -> bool {
    local
        .get_string("notify.away_channel")
        .is_ok_and(|value| value == "imessage")
}

fn removed_setting_messages_from(local: &Config, merged: &Config) -> Vec<String> {
    let mut messages = Vec::new();
    if has_key(local, "telegram") {
        messages.push(TELEGRAM_REMOVED.to_owned());
    }
    if has_key(local, "peers") {
        messages.push(PEERS_REMOVED.to_owned());
    }
    if has_key(local, "notify.telegram") {
        messages.push(NOTIFY_TELEGRAM_REMOVED.to_owned());
    }
    if has_key(local, "notify.away_channel") && !away_channel_is_imessage(local) {
        messages.push(AWAY_CHANNEL_REMOVED.to_owned());
    }
    // An unparseable host is left to the validation that refuses it today.
    if let Ok(host) = merged.get_string("grpc.host")
        && parse_host(&host).is_ok()
        && !is_loopback_host(&host)
    {
        messages.push(GRPC_HOST_NOT_LOOPBACK.to_owned());
    }
    messages
}

pub fn get_settings() -> &'static Arc<Settings> {
    SETTINGS.get().expect("settings not initialised")
}

#[cfg(test)]
pub fn init_settings_for_test() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let s = Settings::load_defaults(&format!("{manifest_dir}/config"))
            .expect("failed to load settings for test");
        let _ = SETTINGS.set(s);
    });
}

pub fn init_settings(settings: Arc<Settings>) {
    SETTINGS
        .set(settings)
        .expect("init_settings called more than once");
}

/// The shipped default configuration with `overrides` (TOML) laid over it: a `Settings` a
/// test builds itself, so code that takes `&Settings` is tested without the process-wide one.
#[cfg(test)]
pub(crate) fn settings_with(overrides: &str) -> Settings {
    use config::{Config, File, FileFormat};
    let default = format!("{}/config/default", env!("CARGO_MANIFEST_DIR"));
    Config::builder()
        .add_source(File::new(&default, FileFormat::Toml))
        .add_source(File::from_str(overrides, FileFormat::Toml))
        .build()
        .and_then(Config::try_deserialize)
        .expect("default configuration with overrides should deserialize")
}

#[cfg(test)]
mod tests {
    use config::{Config, File, FileFormat};
    use serde::Deserialize;

    use super::{
        AgentMonitorSettings, AgentProviderSettings, AgentSettings, Settings,
        is_valid_machine_name, machine_name_error, machine_name_from_hostname,
    };

    fn grpc(host: &str) -> super::GrpcSettings {
        super::GrpcSettings {
            host: host.to_owned(),
            port: 50051,
        }
    }

    #[test]
    fn grpc_addr_accepts_loopback_forms() {
        for (host, expected) in [
            ("127.0.0.1", "127.0.0.1:50051"),
            ("127.0.0.2", "127.0.0.2:50051"),
            ("::1", "[::1]:50051"),
            ("localhost", "127.0.0.1:50051"),
        ] {
            assert_eq!(grpc(host).addr().unwrap().to_string(), expected, "{host}");
        }
        assert!(grpc("invalid host").addr().is_err());
    }

    fn built(text: &str) -> Config {
        Config::builder()
            .add_source(File::from_str(text, FileFormat::Toml))
            .build()
            .unwrap()
    }

    fn removed(local: &str) -> Vec<String> {
        let default = include_str!("../config/default.toml");
        let merged = Config::builder()
            .add_source(File::from_str(default, FileFormat::Toml))
            .add_source(File::from_str(local, FileFormat::Toml))
            .build()
            .unwrap();
        super::removed_setting_messages_from(&built(local), &merged)
    }

    #[test]
    fn removed_setting_messages_are_the_spec_sentences() {
        assert_eq!(
            removed("[telegram]\nbot_token = 'x'\n"),
            [super::TELEGRAM_REMOVED]
        );
        assert_eq!(
            super::TELEGRAM_REMOVED,
            "[telegram] is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables."
        );
        assert_eq!(
            removed("[peers.hub]\nsecret = 'x'\n"),
            [
                "[peers] is no longer read: two Macs share replies through iMessage, with no hub or member. Remove it from config/local.toml or the HAROLD__ environment variables."
            ]
        );
        assert_eq!(
            removed("[notify]\ntelegram = ['x']\n"),
            [
                "notify.telegram is no longer read: the lists are imessage, web_app and machine_audio_output. Remove it from config/local.toml or the HAROLD__ environment variables."
            ]
        );
        assert_eq!(
            removed("[notify]\naway_channel = 'telegram'\n"),
            [
                "notify.away_channel is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables."
            ]
        );
        assert_eq!(
            removed("[grpc]\nhost = '0.0.0.0'\n"),
            [
                "grpc.host must be a loopback address (127.0.0.0/8, ::1 or localhost): Harold no longer serves other Macs."
            ]
        );
    }

    #[test]
    fn removed_setting_messages_cover_the_edge_cases() {
        assert!(removed("").is_empty());
        assert_eq!(removed("[telegram]\n").len(), 1);
        assert_eq!(removed("[peers]\n").len(), 1);
        for host in ["127.0.0.1", "127.0.0.2", "::1", "localhost"] {
            assert!(
                removed(&format!("[grpc]\nhost = '{host}'\n")).is_empty(),
                "{host}"
            );
        }
        for host in ["::", "100.64.0.1"] {
            assert_eq!(
                removed(&format!("[grpc]\nhost = '{host}'\n")).len(),
                1,
                "{host}"
            );
        }
        assert!(removed("[grpc]\nhost = 'invalid host'\n").is_empty());
        assert!(removed("[notify]\naway_channel = 'imessage'\n").is_empty());
        assert!(super::away_channel_is_imessage(&built(
            "[notify]\naway_channel = 'imessage'\n"
        )));
        assert!(!super::away_channel_is_imessage(&built("")));
        assert_eq!(
            removed("[telegram]\n[peers]\n[grpc]\nhost = '0.0.0.0'\n"),
            [
                super::TELEGRAM_REMOVED,
                super::PEERS_REMOVED,
                super::GRPC_HOST_NOT_LOOPBACK
            ]
        );
    }

    #[test]
    fn removed_setting_messages_see_the_environment() {
        let env = std::collections::HashMap::from([(
            "HAROLD__TELEGRAM__BOT_TOKEN".to_owned(),
            "x".to_owned(),
        )]);
        let local = Config::builder()
            .add_source(
                config::Environment::with_prefix("HAROLD")
                    .separator("__")
                    .source(Some(env)),
            )
            .build()
            .unwrap();
        assert_eq!(
            super::removed_setting_messages_from(&local, &local),
            [super::TELEGRAM_REMOVED]
        );
    }

    #[test]
    fn removed_setting_messages_never_hold_a_value() {
        let secret = "review-sentinel-secret";
        let local = format!(
            "[telegram]\nbot_token = '{secret}'\n[peers.hub]\nsecret = '{secret}'\n[notify]\ntelegram = ['{secret}']\naway_channel = '{secret}'\n[grpc]\nhost = '{secret}'\n"
        );
        let messages = removed(&local);
        assert_eq!(messages.len(), 4);
        assert!(messages.iter().all(|m| !m.contains(secret)));
        let host = removed("[grpc]\nhost = '100.64.0.1'\n");
        assert!(host.iter().all(|m| !m.contains("100.64.0.1")));
    }

    #[test]
    fn is_loopback_host_is_true_only_for_loopback_hosts() {
        for host in ["127.0.0.1", "127.0.0.2", "::1", "localhost"] {
            assert!(super::is_loopback_host(host), "{host}");
        }
        for host in ["0.0.0.0", "::", "100.64.0.1", "invalid host"] {
            assert!(!super::is_loopback_host(host), "{host}");
        }
    }

    #[derive(Debug, Deserialize)]
    struct WatchFixture {
        #[serde(default)]
        watch: super::WatchSettings,
    }

    fn parse_watch(source: &str) -> super::WatchSettings {
        Config::builder()
            .add_source(File::from_str(source, FileFormat::Toml))
            .build()
            .and_then(Config::try_deserialize::<WatchFixture>)
            .expect("watch fixture should deserialize")
            .watch
    }

    #[test]
    fn watch_settings_ship_haiku_and_no_budget() {
        let shipped = parse_watch(include_str!("../config/default.toml"));
        assert_eq!(shipped.model, "haiku");
        assert_eq!(shipped.daily_budget_usd, None);
        assert_eq!(shipped.run_budget_usd, None);
        assert_eq!(shipped.max_run_budget_usd, None);
        assert!(shipped.validate().is_empty());
        assert_eq!(super::WatchSettings::default(), shipped);
    }

    #[test]
    fn watch_settings_name_the_key_that_is_out_of_range() {
        for key in ["daily_budget_usd", "run_budget_usd", "max_run_budget_usd"] {
            for bad in ["0", "-1.0"] {
                let settings = parse_watch(&format!("[watch]\n{key} = {bad}\n"));
                let errors = settings.validate();
                assert_eq!(errors.len(), 1, "{key} = {bad}: {errors:?}");
                assert!(errors[0].contains(&format!("watch.{key}")), "{errors:?}");
            }
            let ok = parse_watch(&format!("[watch]\n{key} = 0.5\n"));
            assert!(ok.validate().is_empty(), "{key}");
        }
        let nan = super::WatchSettings {
            daily_budget_usd: Some(f64::NAN),
            ..Default::default()
        };
        assert_eq!(nan.validate().len(), 1);
    }

    #[test]
    fn watch_skill_folders_are_absolute_paths_and_optional() {
        assert!(parse_watch("[watch]\n").skill_folders.is_empty());
        let listed = parse_watch(
            "[watch]\nskill_folders = [\"/Users/x/chief-of-pods/skills/watches\", \"/Users/x/org/skills/watches\"]\n",
        );
        assert_eq!(listed.skill_folders.len(), 2);
        assert!(listed.validate().is_empty());
        // A folder that is not there is not an error.
        let missing = parse_watch("[watch]\nskill_folders = [\"/no/such/folder\"]\n");
        assert!(missing.validate().is_empty());
        for bad in ["~/Dev/p/chief-of-pods/skills/watches", "skills/watches", ""] {
            let settings = parse_watch(&format!("[watch]\nskill_folders = [\"{bad}\"]\n"));
            let errors = settings.validate();
            assert_eq!(errors.len(), 1, "{bad:?}: {errors:?}");
            assert!(errors[0].contains("watch.skill_folders"), "{errors:?}");
        }
    }

    #[test]
    fn watch_run_budget_may_not_pass_the_maximum() {
        let over = parse_watch("[watch]\nrun_budget_usd = 0.2\nmax_run_budget_usd = 0.1\n");
        let errors = over.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("watch.run_budget_usd"));
        let equal = parse_watch("[watch]\nrun_budget_usd = 0.1\nmax_run_budget_usd = 0.1\n");
        assert!(equal.validate().is_empty());
    }

    #[derive(Debug, Deserialize)]
    struct CallAnswererFixture {
        #[serde(default)]
        call_answerer: super::CallAnswererSettings,
    }

    fn parse_call_answerer(source: &str) -> super::CallAnswererSettings {
        Config::builder()
            .add_source(File::from_str(source, FileFormat::Toml))
            .build()
            .and_then(Config::try_deserialize::<CallAnswererFixture>)
            .expect("call answerer fixture should deserialize")
            .call_answerer
    }

    #[test]
    fn call_answerer_settings_ship_haiku_on_with_a_two_cent_budget() {
        let shipped = parse_call_answerer(include_str!("../config/default.toml"));
        assert!(shipped.enabled);
        assert_eq!(shipped.model, "haiku");
        assert_eq!(shipped.timeout_seconds, 15);
        assert_eq!(shipped.run_budget_usd, 0.02);
        assert!(shipped.validate().is_empty());
        assert_eq!(super::CallAnswererSettings::default(), shipped);
        assert_eq!(parse_call_answerer(""), shipped);
    }

    #[test]
    fn call_answerer_settings_name_the_key_that_is_out_of_range() {
        for (key, bad) in [
            ("timeout_seconds", "0"),
            ("timeout_seconds", "26"),
            ("run_budget_usd", "0"),
            ("run_budget_usd", "-0.5"),
            ("model", "\"\""),
            ("model", "\"-haiku\""),
            ("model", "\"hai ku\""),
        ] {
            let settings = parse_call_answerer(&format!("[call_answerer]\n{key} = {bad}\n"));
            let errors = settings.validate();
            assert_eq!(errors.len(), 1, "{key} = {bad}: {errors:?}");
            assert!(
                errors[0].contains(&format!("call_answerer.{key}")),
                "{errors:?}"
            );
        }
        let nan = super::CallAnswererSettings {
            run_budget_usd: f64::NAN,
            ..Default::default()
        };
        assert_eq!(nan.validate().len(), 1);
        let off = parse_call_answerer("[call_answerer]\nenabled = false\n");
        assert!(!off.enabled);
        assert!(off.validate().is_empty());
    }

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
        assert_eq!(
            serde_json::from_str::<super::ScreenAdapter>(r#""claude-v1""#).unwrap(),
            super::ScreenAdapter::ClaudeV1
        );
        assert_eq!(providers[0].screen_adapter, super::ScreenAdapter::CodexV1);
        assert_eq!(providers[1].screen_adapter, super::ScreenAdapter::ClaudeV1);
        assert!(
            providers[2..]
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
    fn report_grace_defaults_to_30_seconds_and_accepts_any_value() {
        let shipped = parse_agent_config(&[include_str!("../config/default.toml")]);
        assert_eq!(shipped.agent_monitor.report_grace_ms, 30_000);
        assert_eq!(AgentMonitorSettings::default().report_grace_ms, 30_000);
        assert!(include_str!("../config/default.toml").contains("report_grace_ms = 30000"));

        let configured = parse_agent_config(&["agents = []\n[agent_monitor]\nreport_grace_ms = 0"]);
        assert_eq!(configured.agent_monitor.report_grace_ms, 0);
        assert!(
            AgentSettings::default()
                .validate(&configured.agent_monitor)
                .is_empty()
        );
    }

    #[test]
    fn inventory_timeout_is_configurable_and_must_be_positive() {
        let defaults = parse_agent_config(&[include_str!("../config/default.toml")]);
        assert_eq!(defaults.agent_monitor.inventory_timeout_ms, 3000);
        assert_eq!(AgentMonitorSettings::default().inventory_timeout_ms, 3000);
        let configured =
            parse_agent_config(&["agents = []\n[agent_monitor]\ninventory_timeout_ms = 5000"]);
        assert_eq!(configured.agent_monitor.inventory_timeout_ms, 5000);
        let agents = AgentSettings::default();
        assert!(agents.validate(&configured.agent_monitor).is_empty());
        let zero = parse_agent_config(&["agents = []\n[agent_monitor]\ninventory_timeout_ms = 0"]);
        assert!(
            agents
                .validate(&zero.agent_monitor)
                .iter()
                .any(|error| error.contains("inventory_timeout_ms"))
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

    #[test]
    fn machine_name_default_replaces_whitespace_in_the_hostname() {
        assert_eq!(machine_name_from_hostname("studio\n"), "studio");
        assert_eq!(
            machine_name_from_hostname("Kahs MacBook  Pro\n"),
            "Kahs-MacBook-Pro"
        );
        assert_eq!(machine_name_from_hostname(""), "");
    }

    #[test]
    fn machine_name_rejects_characters_that_delimit_a_tag_or_a_pane_label() {
        for name in ["studio", "Kahs-MacBook-Pro", "mac.local", "m1"] {
            assert!(is_valid_machine_name(name), "{name:?}");
        }
        for name in [
            "",
            "my mac",
            "a/b",
            "[studio",
            "studio]",
            "harold:0.1",
            "a\tb",
        ] {
            assert!(!is_valid_machine_name(name), "{name:?}");
        }
    }

    #[test]
    fn invalid_machine_name_is_a_configuration_error_that_names_the_setting() {
        assert_eq!(machine_name_error("studio"), None);
        let error = machine_name_error("my mac").expect("an error");
        assert!(error.starts_with("machine_name "), "{error}");
        assert!(error.contains("\"my mac\""), "{error}");
        assert!(machine_name_error("").is_some());
    }

    #[test]
    fn default_configuration_has_a_valid_machine_name() {
        crate::settings::init_settings_for_test();
        let name = &crate::settings::get_settings().machine_name;
        assert!(is_valid_machine_name(name), "{name:?}");
    }

    /// The shipped default configuration with `overrides` laid over it, as `Settings::load`
    /// reads it: the old key is applied against the keys `overrides` sets.
    fn default_settings_with(overrides: &str) -> super::Settings {
        let (settings, _) = default_settings_and_warning(overrides);
        settings
    }

    /// `default_settings_with`, and whether the old key was set (the start warns then).
    fn default_settings_and_warning(overrides: &str) -> (super::Settings, bool) {
        let default = format!("{}/config/default", env!("CARGO_MANIFEST_DIR"));
        let mut settings: super::Settings = Config::builder()
            .add_source(File::new(&default, FileFormat::Toml))
            .add_source(File::from_str(overrides, FileFormat::Toml))
            .build()
            .and_then(Config::try_deserialize)
            .expect("default configuration with overrides should deserialize");
        let local_keys: Vec<String> = Config::builder()
            .add_source(File::from_str(overrides, FileFormat::Toml))
            .build()
            .and_then(|local| local.get_table("notify"))
            .map(|table| table.into_keys().collect())
            .unwrap_or_default();
        let warned = super::apply_old_key(&mut settings.notify, &local_keys);
        settings.notify.old_key_applied = warned;
        (settings, warned)
    }

    /// The errors `validate()` reports about the message folder, under its new key or
    /// its old one, for the default configuration with `overrides` laid over it.
    fn message_folder_errors(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("notify.away_"))
            .collect()
    }

    const OLD_KEY_ERROR: &str =
        "notify.away_updates_only_from is now notify.away_messages_only_from; rename the key";

    #[test]
    fn message_folder_is_optional_and_read_as_written() {
        assert_eq!(
            default_settings_with("").notify.away_messages_only_from,
            None
        );
        let settings =
            default_settings_with("[notify]\naway_messages_only_from = \"/work/chief-of-pods/\"");
        assert_eq!(
            settings.notify.away_messages_only_from.as_deref(),
            Some("/work/chief-of-pods/")
        );
    }

    #[test]
    fn validate_accepts_no_message_folder_or_an_absolute_one() {
        assert!(message_folder_errors("").is_empty());
        for folder in ["/work/chief-of-pods", "/work/chief-of-pods/", "/"] {
            let overrides = format!("[notify]\naway_messages_only_from = \"{folder}\"");
            assert!(message_folder_errors(&overrides).is_empty(), "{folder:?}");
        }
    }

    #[test]
    fn validate_reports_a_message_folder_that_is_not_an_absolute_path() {
        for folder in [
            "",
            "chief-of-pods",
            "~/chief-of-pods",
            "./chief",
            " /work",
            "  ",
        ] {
            let overrides = format!("[notify]\naway_messages_only_from = \"{folder}\"");
            assert_eq!(
                message_folder_errors(&overrides),
                [format!(
                    "notify.away_messages_only_from must be an absolute path starting with \"/\", got \"{folder}\""
                )],
                "{folder:?}"
            );
        }
    }

    #[test]
    fn validate_reports_the_old_key_with_and_without_the_new_one() {
        for overrides in [
            "[notify]\naway_updates_only_from = \"/work/chief-of-pods\"",
            "[notify]\naway_updates_only_from = \"\"",
            "[notify]\naway_updates_only_from = \"/work/chief-of-pods\"\naway_messages_only_from = \"/work/chief-of-pods\"",
        ] {
            assert_eq!(
                message_folder_errors(overrides),
                [OLD_KEY_ERROR],
                "{overrides}"
            );
        }
    }

    #[test]
    fn validate_reports_the_old_key_and_an_invalid_new_key_together() {
        let overrides = "[notify]\naway_updates_only_from = \"/work/chief-of-pods\"\naway_messages_only_from = \"chief-of-pods\"";
        assert_eq!(
            message_folder_errors(overrides),
            [
                "notify.away_messages_only_from must be an absolute path starting with \"/\", got \"chief-of-pods\"",
                OLD_KEY_ERROR,
            ]
        );
    }

    const LIST_KEYS: [&str; 3] = ["imessage", "web_app", "machine_audio_output"];

    fn notify_errors(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("notify.") && !error.starts_with("notify.away_"))
            .collect()
    }

    #[test]
    fn defaults_carry_the_four_keys() {
        let settings = default_settings_with("");
        assert_eq!(settings.notify.chief.as_deref(), Some("chief-of-pods"));
        for channel in [
            super::Channel::Imessage,
            super::Channel::WebApp,
            super::Channel::MachineAudioOutput,
        ] {
            assert_eq!(settings.notify.list(channel), ["chief-of-pods"]);
        }
        assert!(notify_errors("").is_empty());
    }

    #[test]
    fn bad_entries_stop_the_start_naming_key_and_entry() {
        for key in ["chief", "imessage", "web_app", "machine_audio_output"] {
            for entry in ["", "/", "//", "a/../b", "..", ".", "a/./b"] {
                let value = if key == "chief" {
                    format!("\"{entry}\"")
                } else {
                    format!("[\"{entry}\"]")
                };
                let errors = notify_errors(&format!("[notify]\n{key} = {value}"));
                assert!(
                    errors.iter().any(|e| e.contains(&format!("notify.{key}"))
                        && e.contains(&format!("\"{entry}\""))),
                    "{key} {entry:?}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn chief_session_accepts_chief_and_chief_of_pods() {
        for name in ["chief", "chief-of-pods"] {
            let errors = notify_errors(&format!("[notify]\nchief_session = \"{name}\""));
            assert!(errors.is_empty(), "{name}: {errors:?}");
        }
    }

    #[test]
    fn chief_session_rejects_empty_colon_and_dot() {
        for bad in ["", "a:b", "a.b"] {
            let errors = notify_errors(&format!("[notify]\nchief_session = \"{bad}\""));
            assert_eq!(
                errors,
                [
                    "notify.chief_session must be a tmux session name, not empty and with no ':' or '.'"
                ],
                "{bad:?}"
            );
        }
    }

    #[test]
    fn chief_session_unset_is_none() {
        let settings = default_settings_with("");
        assert_eq!(settings.notify.chief_session, None);
    }

    #[test]
    fn star_is_valid_in_lists_and_refused_in_chief() {
        for key in LIST_KEYS {
            assert!(
                notify_errors(&format!("[notify]\n{key} = [\"*\"]")).is_empty(),
                "{key}"
            );
            assert!(
                notify_errors(&format!("[notify]\n{key} = []")).is_empty(),
                "{key}"
            );
        }
        let errors = notify_errors("[notify]\nchief = \"*\"");
        assert!(
            errors.iter().any(|e| e.contains("notify.chief")),
            "{errors:?}"
        );
    }

    #[test]
    fn unknown_notify_key_stops_the_start_and_known_ones_load() {
        let default = format!("{}/config/default", env!("CARGO_MANIFEST_DIR"));
        let result: Result<super::Settings, _> = Config::builder()
            .add_source(File::new(&default, FileFormat::Toml))
            .add_source(File::from_str(
                "[notify]\nimesage = [\"x\"]",
                FileFormat::Toml,
            ))
            .build()
            .and_then(Config::try_deserialize);
        let error = result.expect_err("unknown key").to_string();
        assert!(error.contains("imesage"), "{error}");

        let known = "[notify]\nskip_if_session_active = false\nskip_if_pane_active = true\naway_channel = \"imessage\"\naway_messages_only_from = \"/a\"\nchief = \"/a\"\nimessage = [\"/a\"]\nweb_app = [\"/a\"]\nmachine_audio_output = [\"/a\"]";
        default_settings_with(known);
    }

    #[test]
    fn old_key_alone_fills_chief_and_all_lists_and_warns() {
        let (settings, warned) =
            default_settings_and_warning("[notify]\naway_messages_only_from = \"/old\"");
        assert!(warned);
        assert_eq!(settings.notify.chief.as_deref(), Some("/old"));
        for key in [
            super::Channel::Imessage,
            super::Channel::WebApp,
            super::Channel::MachineAudioOutput,
        ] {
            assert_eq!(settings.notify.list(key), ["/old"]);
        }
    }

    #[test]
    fn new_keys_win_over_old_key_and_an_empty_list_is_not_refilled() {
        let (settings, warned) = default_settings_and_warning(
            "[notify]\naway_messages_only_from = \"/old\"\nimessage = [\"b\"]\nweb_app = []\nchief = \"c\"",
        );
        assert!(warned);
        assert_eq!(settings.notify.imessage, ["b"]);
        assert!(settings.notify.web_app.is_empty());
        assert_eq!(settings.notify.chief.as_deref(), Some("c"));
        assert_eq!(settings.notify.machine_audio_output, ["/old"]);
    }

    #[test]
    fn no_old_key_changes_nothing_and_does_not_warn() {
        let (settings, warned) = default_settings_and_warning("");
        assert!(!warned);
        assert_eq!(settings.notify.imessage, ["chief-of-pods"]);
    }

    #[test]
    fn channel_keys() {
        assert_eq!(super::Channel::WebApp.key(), "web_app");
        assert_eq!(
            super::Channel::MachineAudioOutput.key(),
            "machine_audio_output"
        );
    }

    #[test]
    fn old_key_does_not_set_the_message_folder() {
        let settings =
            default_settings_with("[notify]\naway_updates_only_from = \"/work/chief-of-pods\"");
        assert_eq!(settings.notify.away_messages_only_from, None);
    }

    #[test]
    fn validate_always_requires_the_imessage_recipient_and_handles() {
        let errors = default_settings_with("").validate();
        assert!(errors.contains(&"imessage.recipient is required".to_owned()));
        assert!(errors.contains(&"imessage.handle_ids requires at least one handle ID".to_owned()));

        let set = default_settings_with("[imessage]\nrecipient = \"a@b.c\"\nhandle_ids = [1]");
        assert!(
            !set.validate()
                .iter()
                .any(|error| error.starts_with("imessage."))
        );
    }

    #[test]
    fn validate_reports_an_invalid_or_empty_machine_name() {
        let machine_name_errors = |name: &str| {
            default_settings_with(&format!("machine_name = \"{name}\""))
                .validate()
                .into_iter()
                .filter(|error| error.starts_with("machine_name "))
                .collect::<Vec<_>>()
        };

        assert!(machine_name_errors("studio").is_empty());
        for name in ["", "my mac", "a/b", "[a]", "harold:0.1"] {
            assert_eq!(machine_name_errors(name).len(), 1, "{name:?}");
        }
    }

    /// The errors `validate()` reports about `machines` for the default configuration
    /// with `overrides` laid over it.
    fn machines_errors_with(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("machines "))
            .collect()
    }

    #[test]
    fn machines_defaults_to_an_empty_list_which_is_valid() {
        let settings = default_settings_with("machine_name = \"studio\"");

        assert!(settings.machines.is_empty());
        assert!(machines_errors_with("machine_name = \"studio\"").is_empty());
    }

    #[test]
    fn machines_that_include_this_mac_are_valid() {
        for machines in ["[\"studio\", \"laptop\"]", "[\"Studio\", \"laptop\"]"] {
            let errors =
                machines_errors_with(&format!("machine_name = \"studio\"\nmachines = {machines}"));
            assert!(errors.is_empty(), "{machines}: {errors:?}");
        }
    }

    #[test]
    fn validate_reports_machines_that_leave_out_this_mac() {
        let errors = machines_errors_with("machine_name = \"studio\"\nmachines = [\"laptop\"]");

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("\"studio\""), "{errors:?}");
    }

    #[test]
    fn validate_reports_each_entry_of_machines_that_is_not_a_machine_name() {
        let errors = machines_errors_with(
            "machine_name = \"studio\"\nmachines = [\"studio\", \"my mac\", \"a/b\"]",
        );

        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("\"my mac\""), "{errors:?}");
        assert!(errors[1].contains("\"a/b\""), "{errors:?}");
    }

    /// The errors `validate()` reports about `[telemetry]`, for the default
    /// configuration with `overrides` laid over it.
    fn telemetry_validate_errors(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("telemetry."))
            .collect()
    }

    #[test]
    fn telemetry_exports_to_the_local_collector_by_default() {
        let settings = default_settings_with("");
        assert_eq!(
            settings.telemetry.otlp_endpoint.as_deref(),
            Some("http://localhost:4317")
        );
        assert!(settings.telemetry.export);
        assert!(settings.telemetry.alert_listen.is_none());
        assert!(settings.telemetry.pod_registry.is_none());
        assert_eq!(settings.telemetry.alert_addr(), None);
        assert_eq!(settings.telemetry.pod_registry_path(), None);
        assert!(telemetry_validate_errors("").is_empty());
    }

    #[test]
    fn export_off_has_no_error_and_keeps_the_endpoint_check() {
        assert!(telemetry_validate_errors("[telemetry]\nexport = false\n").is_empty());
        assert_eq!(
            telemetry_validate_errors(
                "[telemetry]\nexport = false\notlp_endpoint = \"localhost:4317\"\n"
            ),
            ["telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317"]
        );
    }

    #[test]
    fn export_true_is_the_default() {
        let settings = default_settings_with("[telemetry]\nexport = true\n");
        assert!(settings.telemetry.export);
        assert!(
            !default_settings_with("[telemetry]\nexport = false\n")
                .telemetry
                .export
        );
    }

    #[test]
    fn voice_input_is_off_when_absent() {
        assert!(default_settings_with("").voice_input.is_none());
    }

    #[test]
    fn whisper_model_is_the_value_after_dash_dash_model() {
        let settings = super::VoiceInputSettings {
            command: "python".into(),
            args: [
                "-m",
                "mlx_audio.stt.generate",
                "--model",
                "mlx-community/whisper-large-v3-turbo",
            ]
            .map(String::from)
            .to_vec(),
        };
        assert_eq!(
            settings.whisper_model(),
            Some("mlx-community/whisper-large-v3-turbo")
        );
        let without_value = super::VoiceInputSettings {
            command: "python".into(),
            args: vec!["--model".into()],
        };
        assert_eq!(without_value.whisper_model(), None);
    }

    #[test]
    fn validate_reports_voice_input_without_a_command_or_a_model() {
        let errors: Vec<String> = default_settings_with("[voice_input]\ncommand = \"\"\n")
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("voice_input."))
            .collect();
        assert_eq!(
            errors,
            [
                "voice_input.command must not be empty",
                "voice_input.args must name the Whisper model with --model",
            ]
        );
    }

    #[test]
    fn a_malformed_telemetry_value_names_its_key() {
        for (key, value, expected) in [
            (
                "otlp_endpoint",
                "localhost:4317",
                "telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317",
            ),
            (
                "otlp_endpoint",
                "",
                "telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317",
            ),
            (
                "otlp_endpoint",
                "http://",
                "telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317",
            ),
            // No TLS support is built, so an https:// collector could only fail at export.
            (
                "otlp_endpoint",
                "https://collector.example:4317",
                "telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317",
            ),
            (
                "alert_listen",
                "4320",
                "telemetry.alert_listen must be an address and port such as 127.0.0.1:4320",
            ),
            (
                "alert_listen",
                "localhost:4320",
                "telemetry.alert_listen must be an address and port such as 127.0.0.1:4320",
            ),
            (
                "pod_registry",
                "",
                "telemetry.pod_registry must be the path of the chief's products.toml",
            ),
            (
                "pod_registry",
                "   ",
                "telemetry.pod_registry must be the path of the chief's products.toml",
            ),
        ] {
            let errors = telemetry_validate_errors(&format!("[telemetry]\n{key} = \"{value}\"\n"));
            assert_eq!(errors, vec![expected.to_string()], "{key}={value:?}");
        }
    }

    #[test]
    fn well_formed_telemetry_values_pass() {
        let overrides = "[telemetry]\notlp_endpoint = \"http://localhost:4317\"\nalert_listen = \"127.0.0.1:4320\"\npod_registry = \"~/products.toml\"\n";
        let settings = default_settings_with(overrides);
        assert!(telemetry_validate_errors(overrides).is_empty());
        assert_eq!(
            settings.telemetry.alert_addr(),
            Some("127.0.0.1:4320".parse().unwrap())
        );
        let path = settings.telemetry.pod_registry_path().unwrap();
        assert!(!path.starts_with("~"), "{path:?}");
        assert!(path.ends_with("products.toml"), "{path:?}");
    }

    #[test]
    fn telemetry_errors_lists_every_malformed_key() {
        let telemetry = super::TelemetrySettings {
            export: true,
            otlp_endpoint: Some("localhost:4317".into()),
            alert_listen: Some("4320".into()),
            pod_registry: Some(String::new()),
        };
        let errors = super::telemetry_errors(&telemetry);
        let keys: Vec<&str> = errors
            .iter()
            .map(|error| error.split_whitespace().next().unwrap_or_default())
            .collect();
        assert_eq!(
            keys,
            [
                "telemetry.otlp_endpoint",
                "telemetry.alert_listen",
                "telemetry.pod_registry"
            ],
            "{errors:?}"
        );
        assert!(super::telemetry_errors(&super::TelemetrySettings::default()).is_empty());
    }

    /// The errors `validate()` reports about timed prompts for the default
    /// configuration with `overrides` laid over it.
    fn timed_prompt_errors(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("timed_prompts["))
            .collect()
    }

    /// One valid entry as TOML, with `key = value` replaced by `replacement`.
    fn digest_entry_with(key: &str, replacement: &str) -> String {
        let entry = [
            ("name", "name = \"digest\""),
            ("at", "at = \"09:00\""),
            ("folder", "folder = \"/work/chief-of-pods\""),
            ("text", "text = \"digest\""),
        ]
        .map(|(name, line)| if name == key { replacement } else { line })
        .join("\n");
        format!("[[timed_prompts]]\n{entry}\n")
    }

    #[test]
    fn timed_prompts_default_to_an_empty_list_which_is_valid() {
        assert!(default_settings_with("").timed_prompts.is_empty());
        assert!(timed_prompt_errors("").is_empty());
    }

    #[test]
    fn timed_prompt_entry_is_read_as_written_with_120_minutes_by_default() {
        let settings = default_settings_with(&digest_entry_with("", ""));

        assert_eq!(
            settings.timed_prompts,
            [super::TimedPromptSettings {
                name: "digest".into(),
                at: "09:00".into(),
                folder: "/work/chief-of-pods".into(),
                text: "digest".into(),
                give_up_after_minutes: 120,
            }]
        );
        assert!(timed_prompt_errors(&digest_entry_with("", "")).is_empty());

        let own = format!("{}give_up_after_minutes = 30", digest_entry_with("", ""));
        assert_eq!(
            default_settings_with(&own).timed_prompts[0].give_up_after_minutes,
            30
        );
    }

    #[test]
    fn minute_of_day_reads_only_a_24_hour_hh_mm() {
        assert_eq!(super::minute_of_day("00:00"), Some(0));
        assert_eq!(super::minute_of_day("09:00"), Some(540));
        assert_eq!(super::minute_of_day("23:59"), Some(1439));
        for at in [
            "", "9am", "9:00", "24:00", "09:60", "09-00", "09:00:00", " 09:00", "0९:00",
        ] {
            assert_eq!(super::minute_of_day(at), None, "{at:?}");
        }
    }

    #[test]
    fn validate_reports_each_invalid_timed_prompt_value_by_position_and_key() {
        let long_name = "n".repeat(65);
        let long_text = "t".repeat(2_001);
        let cases = [
            ("name", "name = \"\"".to_string(), "timed_prompts[0].name must be 1 to 64 characters from letters, digits, \"-\" and \"_\", got \"\"".to_string()),
            ("name", "name = \"daily digest\"".to_string(), "timed_prompts[0].name must be 1 to 64 characters from letters, digits, \"-\" and \"_\", got \"daily digest\"".to_string()),
            ("name", format!("name = \"{long_name}\""), format!("timed_prompts[0].name must be 1 to 64 characters from letters, digits, \"-\" and \"_\", got \"{long_name}\"")),
            ("at", "at = \"9am\"".to_string(), "timed_prompts[0].at must be a time of day \"HH:MM\" from 00:00 to 23:59, got \"9am\"".to_string()),
            ("at", "at = \"24:00\"".to_string(), "timed_prompts[0].at must be a time of day \"HH:MM\" from 00:00 to 23:59, got \"24:00\"".to_string()),
            ("folder", "folder = \"chief-of-pods\"".to_string(), "timed_prompts[0].folder must be an absolute path starting with \"/\", got \"chief-of-pods\"".to_string()),
            ("folder", "folder = \"~/chief-of-pods\"".to_string(), "timed_prompts[0].folder must be an absolute path starting with \"/\", got \"~/chief-of-pods\"".to_string()),
            ("text", "text = \"\"".to_string(), "timed_prompts[0].text must be 1 to 2000 characters on one line with no control characters, got 0 characters".to_string()),
            ("text", format!("text = \"{long_text}\""), "timed_prompts[0].text must be 1 to 2000 characters on one line with no control characters, got 2001 characters".to_string()),
            ("text", "text = \"two\\nlines\"".to_string(), "timed_prompts[0].text must be 1 to 2000 characters on one line with no control characters, got 9 characters".to_string()),
            ("text", "text = \"bell\\u0007\"".to_string(), "timed_prompts[0].text must be 1 to 2000 characters on one line with no control characters, got 5 characters".to_string()),
        ];
        for (key, replacement, expected) in cases {
            assert_eq!(
                timed_prompt_errors(&digest_entry_with(key, &replacement)),
                [expected],
                "{replacement}"
            );
        }
    }

    #[test]
    fn validate_accepts_the_longest_name_and_text_and_a_trailing_slash() {
        let overrides = digest_entry_with("name", &format!("name = \"{}\"", "n".repeat(64)));
        assert!(timed_prompt_errors(&overrides).is_empty());
        let overrides = digest_entry_with("name", "name = \"Daily_digest-2\"");
        assert!(timed_prompt_errors(&overrides).is_empty());
        let overrides = digest_entry_with("text", &format!("text = \"{}\"", "t".repeat(2_000)));
        assert!(timed_prompt_errors(&overrides).is_empty());
        let overrides = digest_entry_with("folder", "folder = \"/work/chief-of-pods/\"");
        assert!(timed_prompt_errors(&overrides).is_empty());
    }

    #[test]
    fn validate_reports_give_up_after_minutes_outside_1_to_1439() {
        for minutes in [0, -1, 1440] {
            let overrides = format!(
                "{}give_up_after_minutes = {minutes}",
                digest_entry_with("", "")
            );
            assert_eq!(
                timed_prompt_errors(&overrides),
                [format!(
                    "timed_prompts[0].give_up_after_minutes must be from 1 to 1439, got {minutes}"
                )]
            );
        }
        for minutes in [1, 1439] {
            let overrides = format!(
                "{}give_up_after_minutes = {minutes}",
                digest_entry_with("", "")
            );
            assert!(timed_prompt_errors(&overrides).is_empty(), "{minutes}");
        }
    }

    #[test]
    fn validate_reports_two_timed_prompts_with_one_name() {
        let overrides = format!(
            "{}{}",
            digest_entry_with("", ""),
            digest_entry_with("at", "at = \"17:00\"")
        );

        assert_eq!(
            timed_prompt_errors(&overrides),
            ["timed_prompts[1].name \"digest\" is already used by timed_prompts[0]"]
        );
    }

    #[test]
    fn validate_reports_a_missing_key_by_position_and_key() {
        let second_without_text = format!(
            "{}{}",
            digest_entry_with("", ""),
            digest_entry_with("text", "").replace("name = \"digest\"", "name = \"evening\"")
        );

        assert_eq!(
            timed_prompt_errors(&second_without_text),
            [
                "timed_prompts[1].text must be 1 to 2000 characters on one line with no control characters, got 0 characters"
            ]
        );
    }

    fn web_app_validate_errors(overrides: &str) -> Vec<String> {
        crate::settings::settings_with(overrides).validate()
    }

    const WEB_APP_FOLDER: &str = "[notify]\nchief = \"/work/chief-of-pods\"\n";

    #[test]
    fn web_app_is_off_when_the_section_is_absent() {
        let settings = crate::settings::settings_with("");
        assert!(settings.web_app.is_none());
        assert!(
            !settings
                .validate()
                .iter()
                .any(|error| error.starts_with("web_app"))
        );
    }

    #[test]
    fn validate_reports_a_listen_address_that_is_not_loopback() {
        let message = "web_app.listen must be a loopback address and port such as 127.0.0.1:4330";
        for listen in ["0.0.0.0:4330", "100.64.0.1:4330", "not-an-address"] {
            let errors = web_app_validate_errors(&format!(
                "{WEB_APP_FOLDER}[web_app]\nlisten = \"{listen}\"\nallowed_login = \"a@b.c\""
            ));
            assert!(errors.iter().any(|e| e == message), "{listen}: {errors:?}");
        }
        for listen in ["127.0.0.1:4330", "[::1]:4330"] {
            let errors = web_app_validate_errors(&format!(
                "{WEB_APP_FOLDER}[web_app]\nlisten = \"{listen}\"\nallowed_login = \"a@b.c\""
            ));
            assert!(!errors.iter().any(|e| e == message), "{listen}: {errors:?}");
        }
    }

    #[test]
    fn validate_reports_an_empty_allowed_login() {
        let message = "web_app.allowed_login must be your Tailscale login";
        for login in ["", "  "] {
            let errors = web_app_validate_errors(&format!(
                "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"{login}\""
            ));
            assert!(errors.iter().any(|e| e == message), "{login:?}: {errors:?}");
        }
    }

    #[test]
    fn validate_reports_web_app_without_the_chiefs_folder() {
        let message = "web_app needs notify.chief, the chief's folder";
        let mut settings = crate::settings::settings_with(
            "[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"",
        );
        settings.notify.chief = None;
        let without = settings.validate();
        assert!(without.iter().any(|e| e == message), "{without:?}");
        let with = web_app_validate_errors(&format!(
            "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\""
        ));
        assert!(!with.iter().any(|e| e == message), "{with:?}");
    }

    /// A PNG header of the given size: the signature, an IHDR chunk, and `padding` extra bytes.
    fn png(width: u32, height: u32, padding: usize) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        bytes.extend(b"IHDR");
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes.resize(bytes.len() + padding, 0);
        bytes
    }

    const ICON_MESSAGE: &str =
        "web_app.icon must be a square PNG of 180 to 1024 pixels a side, at most 1 MB";
    const NAME_MESSAGE: &str = "web_app.app_name must be 1 to 30 characters on one line";

    fn icon_errors(file: &str, contents: &[u8]) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("harold-icon-{}-{file}", std::process::id()));
        let _guard = TempDirGuard(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("icon.png");
        std::fs::write(&path, contents).unwrap();
        web_app_validate_errors(&format!(
            "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"\nicon = \"{}\"",
            path.display()
        ))
    }

    #[test]
    fn validate_refuses_an_icon_that_is_not_a_square_png_of_180_to_1024() {
        let big = png(512, 512, 2 * 1024 * 1024);
        let cases: [(&str, Vec<u8>); 6] = [
            (
                "jpeg",
                vec![
                    0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0,
                    1, 0, 0, 0, 0, 0, 0,
                ],
            ),
            ("small", png(100, 100, 0)),
            ("wide", png(200, 100, 0)),
            ("huge", png(2048, 2048, 0)),
            ("heavy", big),
            ("short", vec![0x89, b'P', b'N', b'G']),
        ];
        for (name, bytes) in cases {
            let errors = icon_errors(name, &bytes);
            assert!(
                errors.iter().any(|e| e == ICON_MESSAGE),
                "{name}: {errors:?}"
            );
        }
        for (name, bytes) in [("min", png(180, 180, 0)), ("max", png(1024, 1024, 100))] {
            let errors = icon_errors(name, &bytes);
            assert!(
                !errors.iter().any(|e| e.starts_with("web_app.icon")),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn validate_refuses_an_icon_that_cannot_be_read() {
        for path in ["/no/such/icon.png", "relative/icon.png"] {
            let errors = web_app_validate_errors(&format!(
                "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"\nicon = \"{path}\""
            ));
            let message = format!("web_app.icon cannot be read: {path}");
            assert!(errors.contains(&message), "{path}: {errors:?}");
        }
    }

    #[test]
    fn a_tilde_icon_path_is_expanded() {
        let home = std::env::var("HOME").expect("HOME is set");
        let errors = web_app_validate_errors(&format!(
            "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"\nicon = \"~/no-such-icon.png\""
        ));
        let message = format!("web_app.icon cannot be read: {home}/no-such-icon.png");
        assert!(errors.contains(&message), "{errors:?}");
    }

    #[test]
    fn validate_refuses_an_app_name_that_is_empty_long_or_multiline() {
        for name in ["", &"x".repeat(31), "two\\nlines"] {
            let errors = web_app_validate_errors(&format!(
                "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"\napp_name = \"{name}\""
            ));
            assert!(
                errors.iter().any(|e| e == NAME_MESSAGE),
                "{name:?}: {errors:?}"
            );
        }
        for name in ["Chief M4", &"é".repeat(30)] {
            let errors = web_app_validate_errors(&format!(
                "{WEB_APP_FOLDER}[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"\napp_name = \"{name}\""
            ));
            assert!(
                !errors.iter().any(|e| e == NAME_MESSAGE),
                "{name:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn without_the_keys_the_app_is_called_chief_and_has_no_icon() {
        let settings = crate::settings::settings_with(
            "[web_app]\nlisten = \"127.0.0.1:4330\"\nallowed_login = \"a@b.c\"",
        );
        let web_app = settings.web_app.unwrap();
        assert_eq!(web_app.name(), "Chief");
        assert!(web_app.load_icon().unwrap().is_none());
    }

    /// Removes the temp dir when dropped, even if an assertion fails.
    struct TempDirGuard(std::path::PathBuf);

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Clears the config env vars when dropped, even if an assertion fails.
    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: only load_defaults_ignores_local_toml_but_load_reads_it touches these vars.
            unsafe {
                std::env::remove_var("HAROLD_CONFIG_DIR");
                std::env::remove_var("HAROLD_ENV");
            }
        }
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("harold-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn load_defaults_ignores_local_toml_but_load_reads_it() {
        let dir = unique_temp_dir("load-defaults");
        let _dir_guard = TempDirGuard(dir.clone());
        let _env_guard = EnvGuard;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        std::fs::copy(
            format!("{manifest_dir}/config/default.toml"),
            dir.join("default.toml"),
        )
        .expect("copy default.toml");
        std::fs::write(
            dir.join("local.toml"),
            "[ai]\nlocal_model = \"test-model\"\nlocal_model_dir = \"/nonexistent\"",
        )
        .expect("write local.toml");
        let dir_str = dir.to_str().expect("utf-8 temp dir");

        let defaults = Settings::load_defaults(dir_str).expect("load_defaults");
        assert_eq!(defaults.ai.local_model, None);

        // SAFETY: this is the only test that touches these vars.
        unsafe {
            std::env::set_var("HAROLD_CONFIG_DIR", dir_str);
            std::env::set_var("HAROLD_ENV", "local");
        }
        let loaded = Settings::load().expect("load");
        assert_eq!(loaded.ai.local_model.as_deref(), Some("test-model"));
    }

    #[test]
    fn load_defaults_errors_when_default_toml_missing() {
        let dir = unique_temp_dir("load-defaults-missing");
        let _guard = TempDirGuard(dir.clone());
        let dir_str = dir.to_str().expect("utf-8 temp dir");
        assert!(Settings::load_defaults(dir_str).is_err());
    }
}
