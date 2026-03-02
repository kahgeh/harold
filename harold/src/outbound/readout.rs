use std::process::Command;

use tracing::{info, warn};

use crate::outbound::imessage::send_imessage;
use crate::settings::get_settings;
use crate::store::ReadoutRequested;
use crate::tmux;
use crate::util::ai_cli_env;

pub fn handle_readout(req: &ReadoutRequested) {
    let cfg = get_settings();
    let Some(cli) = cfg.ai.cli_path.as_deref() else {
        warn!("readout: cli_path not configured");
        send_imessage("File readout unavailable (CLI not configured)");
        return;
    };
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        return;
    };
    let Some(cwd) = tmux::pane_cwd(&req.pane_id) else {
        send_imessage("Can't determine working directory for that pane");
        return;
    };

    let safe_msg = req
        .user_message
        .replace("</message>", "")
        .replace("</prompt>", "");
    let safe_asst = req
        .last_assistant_message
        .replace("</message>", "")
        .replace("</prompt>", "");
    let safe_prompt = req
        .last_user_prompt
        .replace("</message>", "")
        .replace("</prompt>", "");

    let tts_script = "~/.claude/hooks/utils/tts/mlx_tts.py";

    let prompt = format!(
        "You are a file readout assistant. The user is away and communicating via iMessage.\n\n\
         USER MESSAGE:\n{safe_msg}\n\n\
         LAST ASSISTANT MESSAGE:\n{safe_asst}\n\n\
         LAST USER PROMPT:\n{safe_prompt}\n\n\
         WORKING DIRECTORY: {cwd}\n\
         IMESSAGE RECIPIENT: {recipient}\n\n\
         Steps:\n\
         1. Use `find {cwd} -name <filename> -type f -maxdepth 8` to locate the file\n\
         2. If multiple matches, pick the most relevant given conversation context\n\
         3. Read the file with `head -c 2000` (cap at 2000 chars)\n\
         4. Summarise the content in plain spoken language (no code, no markdown)\n\
         5. Generate audio: `uv run {tts_script} --output /tmp/harold_readout.wav \"<summary>\"`\n\
         6. Send: `osascript -e 'tell application \"Messages\" to send POSIX file \"/tmp/harold_readout.wav\" to buddy \"{recipient}\"'`\n\
         7. Clean up: `rm -f /tmp/harold_readout.wav`\n\
         8. Reply with a short confirmation like \"Sent audio summary of <filename>\""
    );

    info!("starting readout agent");
    let out = Command::new(cli)
        .args([
            "-p",
            &prompt,
            "--model",
            "sonnet",
            "--max-turns",
            "5",
            "--allowedTools",
            "Bash(find:*),Bash(cat:*),Bash(head:*),Bash(uv run:*),Bash(osascript:*),Bash(rm:*)",
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .env_remove("CLAUDECODE")
        .envs(ai_cli_env())
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let response = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !response.is_empty() {
                send_imessage(&response);
            }
            info!("readout completed");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            warn!(
                status = %o.status,
                stderr = %stderr.chars().take(200).collect::<String>(),
                "readout agent failed"
            );
            send_imessage("Couldn't complete the file readout");
        }
        Err(e) => {
            warn!(error = %e, "readout: failed to spawn CLI");
            send_imessage("Couldn't complete the file readout");
        }
    }
}
