use std::process::Command;

use tracing::{info, warn};

use crate::settings::{TtsSettings, get_settings};
use crate::store::TurnCompleted;

fn run_local_model(system_prompt: &str, prompt: &str, max_tokens: u32) -> Option<String> {
    let cfg = get_settings();
    let model = cfg.ai.local_model.as_deref()?;
    let model_dir = cfg.ai.local_model_dir.as_deref()?;

    let out = Command::new("uv")
        .args([
            "run",
            "mlx_lm.generate",
            "--model",
            model,
            "--system-prompt",
            system_prompt,
            "--prompt",
            prompt,
            "--max-tokens",
            &max_tokens.to_string(),
        ])
        .current_dir(model_dir)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let output = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if output.is_empty() {
        return None;
    }

    // Extract content between ========== markers (mlx_lm output format)
    if output.contains("==========") {
        let mut in_content = false;
        let mut lines: Vec<&str> = Vec::new();
        for line in output.lines() {
            if line.trim() == "==========" {
                if in_content {
                    break;
                }
                in_content = true;
            } else if in_content {
                lines.push(line.trim());
            }
        }
        let text = lines
            .join(" ")
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        // Strip <think>...</think> blocks (reasoning models)
        let text = regex_strip_think(&text);
        if text.is_empty() { None } else { Some(text) }
    } else {
        let text = output.trim_matches('"').trim_matches('\'');
        let text = regex_strip_think(text);
        if text.is_empty() {
            None
        } else {
            Some(text.chars().take(200).collect())
        }
    }
}

pub(crate) fn regex_strip_think(text: &str) -> String {
    // Remove <think>...</think> blocks emitted by reasoning models like Qwen3.
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        if let Some(end) = rest.find("</think>") {
            rest = rest[end + "</think>".len()..].trim_start();
        } else {
            rest = "";
            break;
        }
    }
    result.push_str(rest);
    result.trim().to_string()
}

fn build_short_summary(turn: &TurnCompleted) -> String {
    let system_prompt = "You are a notification assistant. Given a user's last request, \
         write ONLY a brief 3-8 word summary of what was completed. \
         Do not include any thinking, explanations, or extra text. \
         Output format: Just the summary message.";
    let prompt = format!(
        "User's last request: {}\n\nWrite a 3-8 word summary of what was done:",
        turn.last_user_prompt.chars().take(500).collect::<String>(),
    );
    run_local_model(system_prompt, &prompt, 20).unwrap_or_else(|| "Work complete".into())
}

fn run_tts_command(
    command: &str,
    args: Option<&Vec<String>>,
    voice: Option<&String>,
    message: &str,
) -> std::io::Result<bool> {
    let mut cmd = Command::new(command);
    if let Some(extra_args) = args {
        cmd.args(extra_args);
    }
    if let Some(voice) = voice {
        cmd.args(["-v", voice]);
    }

    let status = cmd.arg(message).status()?;
    Ok(status.success())
}

fn notify_with_fallback(tts: &TtsSettings, message: &str) -> bool {
    match run_tts_command(&tts.command, tts.args.as_ref(), tts.voice.as_ref(), message) {
        Ok(true) => {
            info!("TTS notification sent");
            return true;
        }
        Ok(false) => warn!(command = %tts.command, "TTS command failed"),
        Err(e) => warn!(error = %e, command = %tts.command, "TTS command failed to start"),
    }

    let Some(fallback_command) = &tts.fallback_command else {
        return false;
    };

    match run_tts_command(
        fallback_command,
        tts.fallback_args.as_ref(),
        tts.fallback_voice.as_ref(),
        message,
    ) {
        Ok(true) => {
            info!("fallback TTS notification sent");
            true
        }
        Ok(false) => {
            warn!(command = %fallback_command, "fallback TTS command failed");
            false
        }
        Err(e) => {
            warn!(
                error = %e,
                command = %fallback_command,
                "fallback TTS command failed to start"
            );
            false
        }
    }
}

pub fn notify_at_desk(turn: &TurnCompleted, _trace_id: &str) {
    let summary = build_short_summary(turn);
    let message = format!(
        "{} on {} and waiting for further instructions",
        summary, turn.main_context
    );
    let tts = &get_settings().tts;
    notify_with_fallback(tts, &message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_with_fallback_uses_fallback_when_primary_fails() {
        let tts = TtsSettings {
            command: "/usr/bin/false".into(),
            voice: None,
            args: None,
            fallback_command: Some("/usr/bin/true".into()),
            fallback_voice: None,
            fallback_args: None,
        };

        assert!(notify_with_fallback(&tts, "test message"));
    }

    #[test]
    fn notify_with_fallback_fails_without_fallback() {
        let tts = TtsSettings {
            command: "/usr/bin/false".into(),
            voice: None,
            args: None,
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
        };

        assert!(!notify_with_fallback(&tts, "test message"));
    }
}
