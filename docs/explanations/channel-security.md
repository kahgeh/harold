# Away Channel Security: iMessage

Harold texts the phone and reads its replies by iMessage when the screen is locked. This page describes the security properties of that channel.

## iMessage

**Transport security:** End-to-end encrypted (E2EE) between Apple devices. Apple cannot read message content in transit or at rest on their servers.

**Authentication:** Tied to Apple ID. No bot tokens or API keys to manage or leak.

**Attack surface in Harold:**

- Inbound messages are filtered by `handle_id` — only messages from conversations matching configured handle IDs are processed. An arbitrary external sender's messages are never read.
- Messages are relayed to tmux panes via `tmux send-keys -l` (literal mode) with ANSI/control character stripping, preventing terminal escape injection.
- Outgoing messages pass through `sanitise_for_applescript` + backslash/quote escaping before entering AppleScript.

**Limitations:**

- macOS only — requires the Messages app and `osascript`.
- Relies on `sqlite3` CLI queries against `~/Library/Messages/chat.db`, which Apple could change without notice.
- Duplicate detection depends on reading outgoing message history from the same database.

## The web app: paired devices

The web app's login check lets in every device signed in to the user's Tailscale account. A second check admits only a **paired device**: one that holds a device secret.

- **The secret.** A paired device keeps 32 random bytes as the cookie `harold_device` (`HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=34560000`, 400 days). Harold keeps only the SHA-256 of each secret in `web-app/devices.json` (mode 0600, folder 0700), with a name and times, so reading Harold's files does not give a secret away. A request without a known cookie gets `401`, and a page request gets the pair screen.
- **Pairing.** The user types a pairing code, a fixed code they chose, at least 6 characters, kept as `HAROLD_PAIRING_CODE` in `config/pairing.env` (mode 0600, outside the repository, carried across installs). Harold compares it in constant time, ignoring spaces, hyphens and case. The code is never logged, shown or sent; the pairing file is read at each attempt, and Harold refuses to pair if it is missing, empty, short or readable by others.
- **The lock.** A fixed code can be guessed over time, so five wrong tries in all lock pairing for good (`web-app/pairing-lock.json`, with a salted fingerprint of the code, surviving restarts), until the code is changed or `harold app pair --unlock`. A 6-digit code leaves a guesser about 1 chance in 200,000 before the lock; a longer code with letters is stronger.
- **What it protects against.** A device that was never paired, even on the user's Tailscale account; other programs and accounts on the Mac that reach `127.0.0.1:4330` without the secret.
- **What it does not.** Someone holding the paired phone unlocked; someone who copies the cookie off the phone (unpair it); anyone who can run commands as the user, who can read the pairing file and pair their own device, as they can already type into the chief's pane through tmux; the network, which Tailscale still carries and encrypts; and a short or easy code.

See [ADR 0013](../adr/0013-the-web-app-admits-only-paired-devices-by-a-device-secret-cookie-whose-fingerprint-harold-keeps.md) and the [web app reference](../references/away-app/README.md#pairing-a-phone).

## Handle filtering

Harold's inbound message filtering (handle IDs) ensures that only messages from the expected conversation are processed and relayed to agents.
