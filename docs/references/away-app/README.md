# Away app: the chief's thread on the phone

The away app puts one conversation with the chief of pods on the iPhone's home screen. When the chief sends you a message while you are away, Harold adds it to the app's thread and the phone shows an alert on the lock screen. You open the app, type a reply, and Harold types it into the chief's pane, marked `📱`. Every reply goes to the chief, so Harold never has to guess which pane it is for, as it does with an iMessage reply.

Harold serves the app itself, from this Mac, on a loopback address (`127.0.0.1`, reachable only from the Mac). Tailscale, the private network that joins your Mac and your iPhone, carries it to the phone. iMessage keeps working alongside it, as the fallback.

The chief is the agent running in the folder named by `[notify] away_messages_only_from`.

## Rolling back to an earlier Harold

This build adds a migration, `002_web_app`, to Harold's state database: two tables, one for the thread and one for the phones that turned on alerts. A Harold built before this one finds a migration it does not know, refuses the store, and stops at start with `incompatible state schema`.

To roll back, move the store aside first:

1. `haroldctl stop`.
2. Move the store folder: for the installed service, `mv ~/bin/harold/data/store ~/bin/harold/data/store-with-web-app` (with another `--prefix`, `<prefix>/harold/data/store`).
3. Install the earlier build. It starts with a new, empty store.

The moved folder keeps everything recorded until then. Moved back, it opens in this build or any later one.

## What is built

The app is built in two stages:

- **Stage 1a, built:** the thread, alerts and typed replies, described below.
- **Stage 1b, not built yet:** calls with the chief ([Calls](#calls-stage-1b)).

Both are built and tested on the Mac only. Publishing the app to the phone waits until stage 0, a throwaway test on a real iPhone, has passed and you have said so.

## Setting it up

Setting up takes five steps: Tailscale on both devices, `tailscale serve`, the `[web_app]` section, the home-screen icon, and alerts.

### Tailscale on the Mac and the iPhone

Install Tailscale on this Mac (PasM4MBP) and on the iPhone, and sign both in to the same tailnet, the private network Tailscale makes from the devices signed in to your account. Only devices in that tailnet can reach the app.

### Publishing the app with `tailscale serve`

Harold listens on `127.0.0.1:4330`. `tailscale serve` takes HTTPS requests from the tailnet at `https://pasm4mbp.<tailnet>.ts.net/`, with a certificate Tailscale obtains, and passes them to that address:

```sh
tailscale serve --bg http://127.0.0.1:4330
```

`tailscale serve status` shows what is published. Never use `tailscale funnel` for the app: Funnel opens a service to the whole internet.

For each request it passes on, Tailscale Serve adds a `Tailscale-User-Login` header naming the person signed in on the device that sent it. Harold answers `403` to every request whose header is missing or differs from `allowed_login`, on every route, and stores and logs nothing for it.

### The `[web_app]` section

Add the section to Harold's configuration:

```toml
[web_app]
listen = "127.0.0.1:4330"
allowed_login = "<your Tailscale login>"
```

| Key | Value |
|---|---|
| `listen` | The address and port Harold listens on. It must be a loopback address, such as `127.0.0.1:4330`. |
| `allowed_login` | Your Tailscale login, exactly as Tailscale Serve sends it in `Tailscale-User-Login`. |

The section also needs `[notify] away_messages_only_from` set, because that folder decides which agent is the chief.

- **Absent:** Harold serves nothing, listens on no port, and every message reaches the phone as before.
- **Invalid:** a `listen` that is not a loopback address and port, an empty `allowed_login`, or no `away_messages_only_from` stops Harold at start, and fails `--check-config`, with an error naming the key.
- **Cannot start:** when Harold cannot listen on the address or read its key, it logs `web app unavailable` with the address or the key's path, and runs without the app. Messages then reach the phone through iMessage.

### Adding the app to the home screen

On the iPhone, open `https://pasm4mbp.<tailnet>.ts.net/` in Safari, tap the Share button, and choose **Add to Home Screen**. The icon is called **Chief**. Open the app from that icon from now on: iOS delivers alerts only to a web app opened from the home screen, so a Safari tab does not show **Turn on alerts** at all.

### Turning on alerts

The thread page shows **Turn on alerts** at the top until this phone has turned them on:

1. Tap **Turn on alerts**. iOS asks for permission to show notifications; allow it.
2. The phone's push service, Apple's server that delivers alerts to the phone, gives the app an address for this phone and two keys. The app sends all three to Harold, which stores them.
3. The button disappears.

If you refuse permission, or Harold cannot store the address, the button stays, and you can tap it again.

Harold creates its own key pair the first time it starts with `[web_app]` set ([The key](#the-key)). The app subscribes with that key's public half, and Harold signs each push with the private half, so the push service knows the push comes from the same server the phone subscribed to.

## The thread

The thread is one page, with the oldest message at the top. It holds the last 30 days of messages; Harold deletes older ones at start and every hour. While the page is open, each new message appears as soon as Harold stores it.

A reply you type goes to the chief. It can be up to 1,000 characters; Harold refuses a blank one. Harold stores the reply first, then types `📱 <reply>` and Enter into the chief's pane. Under each reply, the thread shows what became of it:

| Under the reply | What happened |
|---|---|
| `typed` | Harold typed the reply into the chief's idle pane. |
| `waiting: chief busy` | The chief was working. Harold typed the reply into its pane anyway, and the chief reads it when its turn ends. |
| `not sent: no chief` | No agent runs in the chief's folder, or more than one does. Harold typed the reply nowhere. |
| `not sent: messaging paused` | Messaging is paused. Harold typed the reply nowhere. |

While Harold cannot list the tmux panes, it does not know which pane is the chief's, so it holds the reply and tries again.

## What reaches the phone, and by which route

Once a phone has turned on alerts, the chief's messages go to the app instead of iMessage. Every other agent's messages, and your iMessage replies, work as before:

| Message | Route |
|---|---|
| From the chief's pane (turn-end updates, `send-message-to-away-device`, spoken lines), with alerts turned on and no call open | **The app only, as text.** Harold puts the message in the thread and sends a push. A spoken line arrives as its text. No voice note is made. |
| A spoken line from the chief while a call is open | **The call.** It plays on the phone at once, and its text is added to the thread. (Stage 1b, not built yet.) |
| From the chief, when no phone has turned alerts on, or the push service refuses the push or cannot be reached | **iMessage only, once.** It is sent as today, starting `(app unreachable)`. A push address the service reports gone (`404` or `410`) is deleted. |
| From any other pane | **iMessage as today.** The chief-only rule still decides what is left out. |
| The user's iMessage replies | **As today**, routed by tag or the latest notifier. Nothing in iMessage changes. |

- **Once and once only:** a push the service accepted counts as delivered, and the message does not go to iMessage too. A push it refused goes to iMessage, so nothing is lost.
- **The alert:** it shows `Chief` and the first 120 characters of the message. The whole message waits in the thread.

The route applies only to messages that would reach the phone today. At the desk, with the screen unlocked, the chief's spoken lines are still said aloud. While Harold cannot list the tmux panes, it cannot tell which pane is the chief's, so the chief's messages go to iMessage as before.

## Calls (stage 1b)

Calls are not built yet. When they are, you tap **📞 Call the chief** and talk with the chief as in a phone call: Harold hears when you stop talking, types what you said into the chief's pane, plays the chief's spoken answer, and listens again. The desk's calls work the same way; the [voice input reference](../voice-input/README.md) describes them.

iOS limits what a home-screen web app can do during a call. Stage 0 checks each of these on a real iPhone:

| Limit | What the app does |
|---|---|
| A home-screen web app records only while it is open and in front, so the screen must stay on. | The app keeps the screen on for the length of a call, and lets it sleep again when the call ends. |
| The microphone stops when the phone locks or the app leaves the front. | The app ends the call, and on return shows `Call ended: the app was closed or the phone locked`. |
| Sound pauses while the app is in the background. | The call ends the same way. The chief's answer still reaches the thread as text, with an alert. |

Nothing records or plays while the phone is locked or the app is in the background.

## Logs and privacy

### What `harold.log` holds

Harold logs no message text, no reply, no push address and no key. The app adds these lines, each fixed text with only the fields listed:

| Line | Fields |
|---|---|
| `app push sent` | `outcome` (`accepted`) |
| `app push refused` | `outcome` (`gone`, `refused` or `unreachable`) and `reason` (`status_<code>`, `timeout`, `connect`, `send`, `build` or `client`) |
| `app reply stored` | `chars`, the reply's length in characters |

Four other lines report failures, and none carries content:

- `web app unavailable` names the listen address or the key's path, never the login or the key;
- `web app stopped` carries the server's error;
- `web app thread not pruned` carries no field;
- `app reply state not recorded` carries the reply's event id and the store's error.

A request refused by the login check writes no line at all.

The same lines reach Harold's exported telemetry. A turn started by a reply typed in the app counts as `started_by = phone`, as a phone reply through iMessage does.

A test runs the whole path, from a typed reply to a push, with the text `APP-CANARY-7731`, and checks that neither the text nor the push address, the phone's keys or Harold's public key appears in any `harold.log` line, exported log record or exported span.

### What Harold stores

- **The thread:** the chief's messages and your replies, with their times and states, in the state database. Harold deletes rows older than 30 days.
- **The phones' push subscriptions:** each phone's push address and its two keys, in the state database, until the push service reports the address gone.
- **The event stream:** each reply and each subscription is first stored as an event, as every reply and message is. The event stream keeps every event, with no retention ([The store](../operation/README.md#the-store)), so the 30 days apply to the thread's rows, not to the events.

### The key

Harold's key pair lives in `web-push/vapid.pem`, beside the store folder: `~/bin/harold/data/web-push/vapid.pem` for the installed service, or `<store path>/../web-push/vapid.pem` for a directly run binary. Harold creates the file with mode `0600` in a folder with mode `0700`, so only you can read it, and sets both modes again each time it reads the key. Phones subscribe with this key's public half, so keep the file.

### What the push service sees

Each push travels through the phone's push service, so that service sees some of it:

- **The alert's text:** encrypted for the phone, so the push service cannot read it.
- **Your login:** every push carries a signed token naming a contact for the sender, as the push protocol (VAPID) requires. Harold names `mailto:<allowed_login>`, so the phone's push service receives your Tailscale login with every push.
- **The address:** the push service issued it, so it knows which phone the push is for.

## Dependencies: OpenSSL inside Harold

The `web-push` crate, which encrypts and signs each push, brings OpenSSL in through its encryption crate, `ece`. Harold builds that OpenSSL from source, with the `openssl` crate's `vendored` feature, and links it into the `harold` binary. No OpenSSL library is needed on the Mac at run time.

- **The version:** openssl-src 300.6.1+3.6.3, which bundles OpenSSL 3.6.3.
- **The build target:** `.cargo/config.toml` sets `MACOSX_DEPLOYMENT_TARGET = "15.0"`, the same as the release build, so local builds and the release compile OpenSSL for the same macOS version.
- **The release guard:** the release build fails if `otool -L` lists `libssl` or `libcrypto` for the binary, which would mean the build fell back to a system OpenSSL.
- **The known CVEs:** OpenSSL 3.6.4 and 3.6.5 fix 24 CVEs in 3.6.3, in DTLS, QUIC, CMS, CMP, TLS and SM2, among others. Harold uses OpenSSL only through `ece`, for P-256 key agreement (ECDH), AES-128-GCM encryption and random bytes, and never for TLS, which keeps those CVEs out of reach. Pushes travel over HTTPS through `reqwest`, which uses macOS's own TLS. `cargo audit` does not report these CVEs, so the gap is tracked by hand until a patched openssl-src is released.
- **`rsa`:** `rsa` 0.9.10 is in `Cargo.lock`, through `jwt-simple`, with RUSTSEC-2023-0071, a timing attack on RSA. Harold signs its pushes with ES256 (P-256 ECDSA) only and never calls `rsa`, so the advisory cannot be reached. `cargo audit` still reports it.
