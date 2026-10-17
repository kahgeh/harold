use std::net::SocketAddr;
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

/// Where Harold sends its telemetry and takes alerts. Each key is optional: with none
/// set, nothing is exported, no alert address is opened and no pod is read.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TelemetrySettings {
    /// The OpenTelemetry collector's gRPC address, such as `http://localhost:4317`.
    pub otlp_endpoint: Option<String>,
    /// The address and port Harold takes alerts on, such as `127.0.0.1:4320`.
    pub alert_listen: Option<String>,
    /// The path of the chief's `products.toml`. Set it on one Mac only, or merges are
    /// counted twice.
    pub pod_registry: Option<String>,
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
    /// The folder messages for the away device come from while an agent runs in it:
    /// turn-end updates and the messages agents ask Harold to send. An absolute path,
    /// compared as text with a pane's working directory.
    #[serde(default)]
    pub away_messages_only_from: Option<String>,
    /// The name `away_messages_only_from` had while it covered turn-end updates only.
    /// Read only so that `validate` can report it.
    #[serde(default)]
    away_updates_only_from: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TelegramSettings {
    pub bot_token: Option<String>,
    pub chat_id: Option<i64>,
}

/// Which side this Mac is on when the away channel lets only one Mac read replies.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PeerRole {
    /// Reads the channel and forwards each reply to the Mac that owns it. A hub with no
    /// members is a single Mac.
    #[default]
    Hub,
    /// Does not read the channel. Reports its notifications to the hub and takes the
    /// replies the hub forwards.
    Member,
}

/// A member, as the hub knows it.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct PeerMember {
    /// The member's `machine_name`.
    pub name: String,
    /// Where the member's Harold listens, for example `http://laptop.tailnet:50060`.
    pub url: String,
    pub secret: String,
}

/// The hub, as a member knows it.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct PeerHub {
    pub url: String,
    pub secret: String,
}

// Written by hand so a secret is never printed with the settings.
impl std::fmt::Debug for PeerMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerMember")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("secret", &"[redacted]")
            .finish()
    }
}

impl std::fmt::Debug for PeerHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerHub")
            .field("url", &self.url)
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct PeersSettings {
    pub role: PeerRole,
    /// The members, on the hub.
    pub members: Vec<PeerMember>,
    /// The hub, on a member.
    pub hub: Option<PeerHub>,
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

/// What is wrong with a peer's `url`. Calls between Macs are plain gRPC over HTTP.
fn peer_url_error(field: &str, url: &str) -> Option<String> {
    let has_host = url
        .strip_prefix("http://")
        .is_some_and(|host| !host.is_empty());
    if has_host {
        return None;
    }
    Some(format!(
        "{field} must be a plain HTTP address such as \"http://laptop.tailnet:50060\", got \"{url}\""
    ))
}

/// What is wrong with the `peers` settings, one error for each problem. The default, a hub
/// with no members, is valid: it is a single Mac.
fn peers_errors(machine_name: &str, machines: &[String], peers: &PeersSettings) -> Vec<String> {
    let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let mut errors = Vec::new();

    if peers.role == PeerRole::Member {
        if peers.hub.is_none() {
            errors.push("peers.hub is required when peers.role = \"member\"".into());
        }
        if !peers.members.is_empty() {
            errors.push("peers.members must be empty when peers.role = \"member\"".into());
            return errors;
        }
    }
    if let Some(hub) = &peers.hub {
        errors.extend(peer_url_error("peers.hub.url", &hub.url));
        if hub.secret.trim().is_empty() {
            errors.push("peers.hub.secret must not be empty".into());
        }
    }

    if !peers.members.is_empty() && machines.is_empty() {
        errors.push("peers.members requires machines to be set".into());
        return errors;
    }
    for (index, member) in peers.members.iter().enumerate() {
        let name = &member.name;
        if same(name, machine_name) {
            errors.push(format!(
                "peers.members must not include this Mac's machine_name \"{machine_name}\""
            ));
        } else if !machines.iter().any(|listed| same(listed, name)) {
            errors.push(format!(
                "peers.members name \"{name}\" must be in machines, got {machines:?}"
            ));
        }
        if peers.members[..index]
            .iter()
            .any(|earlier| same(&earlier.name, name))
        {
            errors.push(format!("peers.members lists \"{name}\" more than once"));
        }
        errors.extend(peer_url_error(
            &format!("peers.members url for \"{name}\""),
            &member.url,
        ));
        if member.secret.trim().is_empty() {
            errors.push(format!(
                "peers.members secret for \"{name}\" must not be empty"
            ));
        }
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
    pub(crate) agents: AgentSettings,
    #[serde(default)]
    pub(crate) agent_monitor: AgentMonitorSettings,
    pub tts: TtsSettings,
    pub log: LogSettings,
    pub store: StoreSettings,
    pub notify: NotifySettings,
    #[serde(default)]
    pub telegram: TelegramSettings,
    /// The other Macs, for an away channel only one Mac can read.
    #[serde(default)]
    pub(crate) peers: PeersSettings,
    /// Texts Harold types into an agent's pane at a set time each day.
    #[serde(default)]
    pub(crate) timed_prompts: Vec<TimedPromptSettings>,
    /// The collector, alert address and pod registry. Off when the section is absent.
    #[serde(default)]
    pub telemetry: TelemetrySettings,
    /// Calls with the chief at the desk. Off when the section is absent.
    #[serde(default)]
    pub(crate) voice_input: Option<VoiceInputSettings>,
}

impl Settings {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        errors.extend(machine_name_error(&self.machine_name));
        errors.extend(machines_errors(&self.machine_name, &self.machines));
        errors.extend(peers_errors(
            &self.machine_name,
            &self.machines,
            &self.peers,
        ));
        errors.extend(self.agents.validate(&self.agent_monitor));
        errors.extend(self.activity_summary.validate());
        errors.extend(away_messages_only_from_error(
            self.notify.away_messages_only_from.as_deref(),
        ));
        errors.extend(away_updates_only_from_error(
            self.notify.away_updates_only_from.as_deref(),
        ));
        errors.extend(timed_prompts_errors(&self.timed_prompts));
        errors.extend(telemetry_errors(&self.telemetry));
        errors.extend(voice_input_errors(self.voice_input.as_ref()));
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

    use super::{
        AgentMonitorSettings, AgentProviderSettings, AgentSettings, is_valid_machine_name,
        machine_name_error, machine_name_from_hostname,
    };

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

    /// The shipped default configuration with `overrides` laid over it.
    fn default_settings_with(overrides: &str) -> super::Settings {
        let default = format!("{}/config/default", env!("CARGO_MANIFEST_DIR"));
        Config::builder()
            .add_source(File::new(&default, FileFormat::Toml))
            .add_source(File::from_str(overrides, FileFormat::Toml))
            .build()
            .and_then(Config::try_deserialize)
            .expect("default configuration with overrides should deserialize")
    }

    /// The errors `validate()` reports about the message folder, under its new key or
    /// its old one, for the default configuration with `overrides` laid over it.
    fn message_folder_errors(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("notify.away_"))
            .filter(|error| !error.starts_with("notify.away_channel"))
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

    #[test]
    fn old_key_does_not_set_the_message_folder() {
        let settings =
            default_settings_with("[notify]\naway_updates_only_from = \"/work/chief-of-pods\"");
        assert_eq!(settings.notify.away_messages_only_from, None);
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
    fn telemetry_is_off_when_the_section_is_absent() {
        let settings = default_settings_with("");
        assert!(settings.telemetry.otlp_endpoint.is_none());
        assert!(settings.telemetry.alert_listen.is_none());
        assert!(settings.telemetry.pod_registry.is_none());
        assert_eq!(settings.telemetry.alert_addr(), None);
        assert_eq!(settings.telemetry.pod_registry_path(), None);
        assert!(telemetry_validate_errors("").is_empty());
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

    use super::{PeerHub, PeerMember, PeerRole};

    /// A hub named `studio` with one member `laptop`, as TOML laid over the defaults.
    const HUB: &str = r#"
        machine_name = "studio"
        machines = ["studio", "laptop"]
        [[peers.members]]
        name = "laptop"
        url = "http://laptop.tailnet:50060"
        secret = "s3cret"
    "#;

    /// A member named `laptop`, as TOML laid over the defaults.
    const MEMBER: &str = r#"
        machine_name = "laptop"
        machines = ["studio", "laptop"]
        [peers]
        role = "member"
        [peers.hub]
        url = "http://studio.tailnet:50060"
        secret = "s3cret"
    "#;

    /// The errors `validate()` reports about `peers` for the default configuration with
    /// `overrides` laid over it.
    fn peers_errors_with(overrides: &str) -> Vec<String> {
        default_settings_with(overrides)
            .validate()
            .into_iter()
            .filter(|error| error.starts_with("peers."))
            .collect()
    }

    #[test]
    fn peers_default_to_a_hub_with_no_members_which_is_valid() {
        let settings = default_settings_with("machine_name = \"studio\"");

        assert_eq!(settings.peers.role, PeerRole::Hub);
        assert!(settings.peers.members.is_empty());
        assert_eq!(settings.peers.hub, None);
        assert!(peers_errors_with("machine_name = \"studio\"").is_empty());
    }

    #[test]
    fn hub_with_a_listed_member_is_valid() {
        let settings = default_settings_with(HUB);

        assert_eq!(
            settings.peers.members,
            [PeerMember {
                name: "laptop".into(),
                url: "http://laptop.tailnet:50060".into(),
                secret: "s3cret".into(),
            }]
        );
        assert!(peers_errors_with(HUB).is_empty());
    }

    #[test]
    fn member_with_a_hub_is_valid() {
        let settings = default_settings_with(MEMBER);

        assert_eq!(settings.peers.role, PeerRole::Member);
        assert_eq!(
            settings.peers.hub,
            Some(PeerHub {
                url: "http://studio.tailnet:50060".into(),
                secret: "s3cret".into(),
            })
        );
        assert!(peers_errors_with(MEMBER).is_empty());
    }

    #[test]
    fn member_name_matches_machines_ignoring_case() {
        let errors = peers_errors_with(&HUB.replace("name = \"laptop\"", "name = \"Laptop\""));

        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn unknown_role_is_rejected_when_the_configuration_is_read() {
        let default = format!("{}/config/default", env!("CARGO_MANIFEST_DIR"));
        let result = Config::builder()
            .add_source(File::new(&default, FileFormat::Toml))
            .add_source(File::from_str("[peers]\nrole = \"boss\"", FileFormat::Toml))
            .build()
            .and_then(Config::try_deserialize::<super::Settings>);

        assert!(result.is_err());
    }

    #[test]
    fn validate_reports_a_member_without_a_hub() {
        let errors = peers_errors_with("machine_name = \"laptop\"\n[peers]\nrole = \"member\"");

        assert_eq!(
            errors,
            ["peers.hub is required when peers.role = \"member\""]
        );
    }

    #[test]
    fn validate_reports_a_member_that_lists_members() {
        let overrides = format!(
            "{MEMBER}\n[[peers.members]]\nname = \"studio\"\nurl = \"http://studio.tailnet:50060\"\nsecret = \"x\""
        );

        let errors = peers_errors_with(&overrides);

        assert_eq!(
            errors,
            ["peers.members must be empty when peers.role = \"member\""]
        );
    }

    #[test]
    fn validate_reports_members_when_machines_is_not_set() {
        let errors = peers_errors_with(&HUB.replace("machines = [\"studio\", \"laptop\"]", ""));

        assert_eq!(errors, ["peers.members requires machines to be set"]);
    }

    #[test]
    fn validate_reports_a_member_that_is_not_in_machines() {
        let errors = peers_errors_with(&HUB.replace("name = \"laptop\"", "name = \"mini\""));

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("peers.members name \"mini\" must be in machines"));
    }

    #[test]
    fn validate_reports_this_mac_listed_as_its_own_member() {
        let errors = peers_errors_with(&HUB.replace("name = \"laptop\"", "name = \"Studio\""));

        assert_eq!(
            errors,
            ["peers.members must not include this Mac's machine_name \"studio\""]
        );
    }

    #[test]
    fn validate_reports_a_member_listed_twice() {
        let overrides = format!(
            "{HUB}\n[[peers.members]]\nname = \"LAPTOP\"\nurl = \"http://other.tailnet:50060\"\nsecret = \"x\""
        );

        let errors = peers_errors_with(&overrides);

        assert_eq!(errors, ["peers.members lists \"LAPTOP\" more than once"]);
    }

    #[test]
    fn validate_reports_an_empty_member_secret() {
        for secret in ["", "  "] {
            let errors = peers_errors_with(
                &HUB.replace("secret = \"s3cret\"", &format!("secret = \"{secret}\"")),
            );

            assert_eq!(
                errors,
                ["peers.members secret for \"laptop\" must not be empty"],
                "{secret:?}"
            );
        }
    }

    #[test]
    fn validate_reports_an_empty_hub_secret() {
        let errors = peers_errors_with(&MEMBER.replace("secret = \"s3cret\"", "secret = \"\""));

        assert_eq!(errors, ["peers.hub.secret must not be empty"]);
    }

    #[test]
    fn validate_reports_a_peer_url_that_is_not_plain_http() {
        for url in [
            "",
            "http://",
            "https://laptop.tailnet:50060",
            "laptop.tailnet:50060",
        ] {
            let member = peers_errors_with(&HUB.replace("http://laptop.tailnet:50060", url));
            assert_eq!(member.len(), 1, "{url:?}: {member:?}");
            assert!(
                member[0]
                    .starts_with("peers.members url for \"laptop\" must be a plain HTTP address"),
                "{member:?}"
            );

            let hub = peers_errors_with(&MEMBER.replace("http://studio.tailnet:50060", url));
            assert_eq!(hub.len(), 1, "{url:?}: {hub:?}");
            assert!(
                hub[0].starts_with("peers.hub.url must be a plain HTTP address"),
                "{hub:?}"
            );
        }
    }

    #[test]
    fn peer_secrets_are_not_printed_by_debug() {
        let hub = format!("{:?}", default_settings_with(HUB));
        let member = format!("{:?}", default_settings_with(MEMBER));

        assert!(!hub.contains("s3cret"), "{hub}");
        assert!(!member.contains("s3cret"), "{member}");
        assert!(hub.contains("laptop.tailnet"), "{hub}");
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
}
