---
name: send-message-to-away-device
description: Use when the user's instructions ask you to message them, text them, or send something to their phone or away device.
---

# Send a message to the user's away device

Harold is the service on this Mac that messages the user's phone. This skill gives you one command that sends text you write to that phone, tagged with the tmux pane or Rex pane you run in.

## When to send

Send a message only when the user's instructions call for it: they asked you to message or text them, or to send something to their phone, now or when a named thing happens. Do not send on your own initiative.

## The command

```
{{HAROLD_BIN}} send-message-to-away-device --src-id "${TMUX_PANE:-$REX_BLOCK}" 'Pod 3 is blocked on a merge conflict in store.rs'
```

- `--src-id "${TMUX_PANE:-$REX_BLOCK}"` names your pane. Pass it exactly as written. `$TMUX_PANE` is set in a tmux pane, and `$REX_BLOCK` is set in a Rex pane. Both are empty outside a multiplexer, and the command then fails.
- The phone shows `🤖 [<machine>/<session>:<window>.<pane>] <your text>`.
- Exit status 0 means Harold has stored the message for sending. It does not mean the phone has it.

## How to write the text

- Plain text in one paragraph, at most 1,000 characters. Harold turns line breaks and tabs into spaces.
- Pass the text in single quotes. Write a single quote inside it as `'\''`: `'it'\''s done'`.
- Do not start the text with `--`.
- The tag names your pane, not your role. When the pane label would not tell the user who is speaking, open the text with your role: `Chief of pods: pod 3 is blocked on a merge conflict in store.rs`.

## What a reply looks like

The user's reply arrives in your pane as a new prompt that starts with `📱`, for example `📱 rebase it onto main`.

## What to do on failure

When the command exits 1 it prints one line on standard error, such as `messaging is paused` or `no agent is running in pane %47`. Report that line in your output. Do not retry in a loop.
