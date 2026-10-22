# Web app: the chief's thread on the phone

The web app puts one conversation with the chief of pods on the iPhone's home screen. When the chief sends you a message while you are away, Harold adds it to the app's thread and the phone shows a push on the lock screen. You open the app, type a reply, and Harold types it into the chief's pane, marked `📱`. Every reply goes to the chief, so Harold never has to guess which pane it is for, as it does with an iMessage reply.

Harold serves the app itself, from this Mac, on a loopback address (`127.0.0.1`, reachable only from the Mac). Tailscale, the private network that joins your Mac and your iPhone, carries it to the phone. iMessage keeps working alongside it, as the fallback.

The chief is the first agent pane of the chief's tmux session among the agents running in a folder `[notify] chief` matches.

## Rolling back to an earlier Harold

This build adds a migration, `002_web_app`, to Harold's state database: two tables, one for the thread and one for the phones that turned on push. A Harold built before this one finds a migration it does not know, refuses the store, and stops at start with `incompatible state schema`.

To roll back, move the store aside first:

1. `haroldctl stop`.
2. Move the store folder: for the installed service, `mv ~/bin/harold/data/store ~/bin/harold/data/store-with-web-app` (with another `--prefix`, `<prefix>/harold/data/store`).
3. Install the earlier build. It starts with a new, empty store.

The moved folder keeps everything recorded until then. Moved back, it opens in this build or any later one.

## What is built

The app is built in two stages:

- **Stage 1a, built:** the thread, push and typed replies, described below.
- **Stage 1b, built:** calls with the chief ([Calls](#calls-stage-1b)).

Both stages are built. Both are tested on the Mac before the phone: publishing the app to the phone waits until stage 0, a throwaway test on a real iPhone, has passed and you have said so.

## Setting it up

Setting up takes five steps: Tailscale on both devices, `tailscale serve`, the `[web_app]` section, the home-screen icon, and push.

### Tailscale on the Mac and the iPhone

Install Tailscale on this Mac (PasM4MBP) and on the iPhone, and sign both in to the same tailnet, the private network Tailscale makes from the devices signed in to your account. Only devices in that tailnet can reach the app.

Before `tailscale serve` will serve HTTPS, the tailnet needs two settings turned on in the Tailscale admin console:

- **MagicDNS**, which gives the Mac its name in the tailnet, `pasm4mbp.<tailnet>.ts.net`;
- **HTTPS certificates**, which let Tailscale obtain a certificate for that name.

The first run of `tailscale serve` offers a link to turn on whichever is off. The `tailscale` command must also be on the `PATH`: the Mac app can install its command-line tool from its settings.

### Publishing the app with `tailscale serve`

Harold listens on `127.0.0.1:4330`. `tailscale serve` takes HTTPS requests from the tailnet at `https://pasm4mbp.<tailnet>.ts.net/`, with a certificate Tailscale obtains, and passes them to that address:

```sh
tailscale serve --bg http://127.0.0.1:4330
```

`tailscale serve status` shows what is published. Never use `tailscale funnel` for the app: Funnel opens a service to the whole internet.

For each request it passes on, Tailscale Serve adds a `Tailscale-User-Login` header naming the person signed in on the device that sent it. Harold answers `403` to every request whose header is missing or differs from `allowed_login`, on every route, and stores and logs nothing for it.

Harold also accepts only a Host ending in `.ts.net`, such as the Mac's tailnet name, or the loopback address it listens on. Stage 0 must confirm that `tailscale serve` passes the `.ts.net` Host through to Harold. If it passes some other name instead, Harold answers `403` to every request, and the app is blocked entirely.

### What the app refuses

The login header comes from the device, not from the page, so every page open in the phone's browser carries it, as it would a cookie. Two more checks keep other pages out, and each refusal is a `403` with nothing stored or logged:

- **Posts from another page:** a reply or a push sign-up must come from the app's own page. The browser must say the request is from the same site (`Sec-Fetch-Site: same-origin`, or else an `Origin` that matches the Host), and the request must carry a header another site's page cannot send without asking first: `Datastar-Request: true`, which the page's reply form sends, or `Content-Type: application/json`, which the push sign-up sends. Harold never grants that asking, so another site's page cannot post at all.
- **Another Host:** a page can point its own name at `127.0.0.1` and reach Harold's listener. Harold refuses every request whose Host is not the tailnet name or the loopback address, so such a page gets nothing.

These checks protect against web pages and other devices, not against software on the Mac. Any program on the Mac can still reach `127.0.0.1:4330` and send a correct Host, Origin and login. The Mac's own processes are trusted, as they already are with tmux, where any of them can type into the chief's pane.

### The `[web_app]` section

Add the section to Harold's configuration:

```toml
[web_app]
listen = "127.0.0.1:4330"
allowed_login = "<your Tailscale login>"
# app_name = "Chief M4"
# icon = "~/Dev/p/personal-chief/machines/pasm4mbp/icon.png"
```

| Key | Value |
|---|---|
| `listen` | The address and port Harold listens on. It must be a loopback address, such as `127.0.0.1:4330`. |
| `allowed_login` | Your Tailscale login, exactly as Tailscale Serve sends it in `Tailscale-User-Login`. |
| `app_name` | Optional. The name under the home-screen icon and in the app's title, 1 to 30 characters on one line. Left out, it is `Chief`. iOS shows about 12 characters, so a short name such as `Chief M4` reads best. |
| `icon` | Optional. The path of a square PNG, 180 to 1024 pixels a side and at most 1 MB. A leading `~/` is the home folder; otherwise the path must be absolute. Left out, the app has no icon of its own. Harold reads the file once, at start, so a change needs a restart. It does not resize the image; iOS scales a larger square down itself. |

Set both in each Mac's own `local.toml`, so the two Macs' apps differ on the home screen. The file is served at `/static/icon.png` behind the login check, and the manifest and thread page take the name and icon from these keys.

The section also needs `[notify] chief` (the default `"chief-of-pods"` will do), because that entry decides which agent is the chief. The chief reaches the app only when its folder is on `[notify] web_app`.

- **Absent:** Harold serves nothing, listens on no port, and every message reaches the phone as before.
- **Invalid:** a `listen` that is not a loopback address and port, an empty `allowed_login`, no `notify.chief`, an `app_name` that is empty, over 30 characters or holds a control character, or an `icon` that cannot be read, is not a square PNG of 180 to 1024 pixels a side or is over 1 MB stops Harold at start with an error naming the key. `--check-config` fails too, but prints only `invalid configuration`; start Harold to see which key is wrong.
- **Cannot start:** when Harold cannot listen on the address or read its key, it logs `web app unavailable` with the address or the key's path, and runs without the app. Messages then reach the phone through iMessage.

### Adding the app to the home screen

On the iPhone, open `https://pasm4mbp.<tailnet>.ts.net/` in Safari, tap the Share button, and choose **Add to Home Screen**. The name under the icon is `app_name`, or **Chief** when none is set, and the icon shows the `icon` file when one is set. iOS saves the name and icon when you add the app and does not read them again. To see a changed name or icon: delete the app from the home screen, add it again from Safari, and tap **Turn on alerts** again, because deleting the app also deletes its sign-up for pushes. Open the app from that icon from now on: iOS delivers pushes only to a web app opened from the home screen, so a Safari tab does not show **Turn on alerts** at all.

### Turning on push

The thread page shows **Turn on alerts** at the top until this phone has turned them on:

1. Tap **Turn on alerts**. iOS asks for permission to show notifications; allow it.
2. The phone's push service, Apple's server that delivers pushes to the phone, gives the app an address for this phone and two keys. The app sends all three to Harold, which stores them.
3. The button disappears.

If you refuse permission, or Harold cannot store the address, the button stays, and you can tap it again.

Each time the app opens, it sends the phone's address to Harold again, and hides the button only when Harold stores it. If Harold cannot store it, the button stays, and a tap signs the phone up afresh. A sign-up made with another key, such as one from before Harold's key was replaced, is dropped, and the button shows.

Harold creates its own key pair the first time it starts with `[web_app]` set ([The key](#the-key)). The app subscribes with that key's public half, and Harold signs each push with the private half, so the push service knows the push comes from the same server the phone subscribed to.

## The thread

The app has two pages side by side: the thread page and the todo page. Swipe left from the thread page to reach the todo page and right to come back, or tap **Todos** in the thread page's header and **‹ <app name>** in the todo page's. Two page dots sit above the reply box (at the bottom of the todo page, which has no reply box); the dark one is the page you are on.

The thread page's header holds the app's name, **Todos** with the todo count in orange (no number when nothing waits), the **Turn on alerts** button until push is on, and **Call**.

The thread is the oldest message at the top. It holds the last 30 days of messages; Harold deletes older ones at start and every hour. The page opens with the newest 50 messages and loads older ones, 50 at a time, as you scroll up; "Loading earlier messages…" shows at the top while more remain. While the page is open, each new message appears as soon as Harold stores it. After Harold restarts, the page picks the thread up again by itself within seconds, and keeps the older messages it had loaded. While Harold can't be reached, it tries less often, at least every 30 s.

- **Sender labels.** A small label at the bottom right of each message says who wrote it: `chief`, or for your reply the mark and `me`.
- **Older messages.** Older messages don't count as new: they don't move you to the bottom, show **New message ↓** or add to the todo count. The message you were reading stays where it was. Each time "Loading earlier messages…" comes into view, 50 more load. When the phone comes back from the background, the page keeps the older messages it loaded.
- **New messages.** A new message scrolls into view when you are within 80 px of the bottom. Scrolled further up, the page stays put and a **New message ↓** button appears above the reply box; tapping it scrolls to the newest message, and it goes once you are at the bottom. Sending a reply scrolls to the bottom.
- **Links.** An `http://` or `https://` address in a message is a link that opens in the phone's browser. A trailing `.`, `,`, `;`, `:`, `!`, `?`, `)`, `]`, `'` or `"` stays outside the link.
- **Responding dots.** After your reply is typed, three grey dots show on the chief's side while the chief's pane is busy. They pulse in turn, and stay still when the phone asks for reduced motion. They go when the chief's next message appears, or when its turn ends with no message.

A reply you type goes to the chief. It can be up to 1,000 characters; Harold refuses a blank one. **Send** is disabled while a reply is on its way, so a double tap sends it once. Harold stores the reply first, then types `📱 <reply>` and Enter into the chief's pane. Next to `me`, the thread shows what became of the reply:

| Under the reply | What happened |
|---|---|
| ✓ `me` (the stored mark) | Harold has stored the reply and not yet typed it. |
| ✓✓ `me` (the typed mark) | Harold typed the reply into the chief's idle pane. |
| ✓✓ `me · waiting: chief busy` | The chief was working. Harold typed the reply into its pane anyway. The label goes when the chief's turn ends, and the row then reads ✓✓ `me`. |
| ✓ `me · not sent: no chief` | No agent runs in the chief's folder. Harold typed the reply nowhere. |
| ✓ `me · not sent: messaging paused` | Messaging is paused. Harold typed the reply nowhere. |

While Harold cannot list the tmux panes, it does not know which pane is the chief's, so it holds the reply and tries again.

## The todos

The todo page lists the chief's todos as cards in the question tool's order: the waiting questions and instructions first, then the questions you set aside with **Later**. Its header shows a small speech-bubble icon and a number when chief messages arrived while you were on the todo page; the number goes when you return to the thread page.

- **A question** shows its id, product and intro, a link to the spec when it has one, and each decision with its options. `suggested` marks the suggested option, and a decision starts on your saved pick, else the suggested option. A free option shows a text box while picked. **Send** is enabled once every decision has a pick and every free pick has text. **Later** sets the question aside.
- **Instructions** show a title and numbered steps. Ticking a step stores the tick at once. **Done** and **Ignore** close them.
- **Answers reach the chief at once**, as when you press `send` in the dash. **Revise…** and **Reject…** sit in a row under **Send** and **Later**. Each opens a note box in the card (`What should change?` or `Why reject it?`); the card's **Send** then reads **Send revise** or **Send reject**, **Later** gives its place to **Cancel**, and the picks made so far go with the note. A blank note is never sent, and the note takes up to 1,999 characters (the box shows the count from 1,800). When Harold refuses the answer, the card shows its notice and keeps the note box open with the text in it. Once Harold has taken any answer from the page (**Send**, **Revise**, **Reject**, **Done** or **Ignore**), a sent line at the top of the page reads, for example, `Q53: revise sent to the chief.` until the next post from that page or until you leave the page. The line belongs to the page that sent the answer.
- **When a post is refused**, the card shows a notice and is redrawn from the next list: `<id> changed. Look again before you send.`, `<id> was already answered or withdrawn.`, or `Harold can't send this right now. Try again.` When Harold cannot reach the tool to save your picks, the last one reads `Harold can't save your picks right now` instead. Once the tool takes a later save of that card's picks, the next redraw drops the notice, and **Send** still sends the picks the card shows, saved or not. A todo that is no longer listed has no card left to show its notice on.
- With no todo the page reads `No todos for you.`; when Harold cannot read them and has no list yet, it reads `Harold can't read the todos right now.` and the todo count is empty. A failed read later keeps the last list.

The web app reads the todos through the question tool's gRPC service on Harold's own address (`[grpc]`), every 2 seconds while a page is open and at once after each post ([ADR 0007](../../adr/0007-the-web-app-reaches-todos-through-the-question-service.md)). The posts are `POST /todos/answer`, `/todos/picks`, `/todos/later`, `/todos/tick`, `/todos/done`, `/todos/ignore`, `/todos/revise` and `/todos/reject`, each behind the login and same-origin checks like every route. The phone saves a question's picks with `/todos/picks` each time you select an option and after half a second of typing; a save the tool took answers `204` and does not redraw your page, and every open page shows the saved picks at its next todo read.

## What reaches the phone, and by which route

With `[web_app]` set, the chief's messages always go into the app's thread. Once a phone has turned on push, they reach the phone as a push instead of through iMessage; until then, each also goes to iMessage, starting `(app unreachable)`. Every other agent's messages, and your iMessage replies, work as before:

| Message | Route |
|---|---|
| From the chief's pane (turn-end updates, `send-message-to-away-device`, spoken lines), with push turned on and no call open | **The app only, as text.** Harold puts the message in the thread and sends a push, unless the app is open on screen: then the thread shows it, with no push and no iMessage. A spoken line arrives as its text. No voice note is made. |
| A spoken line from the chief while the phone's call is open | **The phone's call.** It plays on the phone at once, and its text is added to the thread, with no push and no iMessage. |
| From the chief, when no phone has turned push on, or the push service refuses the push or cannot be reached | **iMessage only, once.** It is sent as today, starting `(app unreachable)`. A push address the service reports gone (`404` or `410`) is deleted. |
| From any other pane | **iMessage as today.** The away channel's list (`notify.imessage`) decides what is left out. |
| The user's iMessage replies | **As today**, routed by tag or the latest notifier. Nothing in iMessage changes. |

- **Once and once only:** a push the service accepted counts as delivered, and the message does not go to iMessage too. A push it refused goes to iMessage, so nothing is lost.
- **The push:** it shows `Chief` and the first 120 characters of the message. The whole message waits in the thread.
- **Skipped while the app is open:** a page that is open and visible holds a `GET /updates` stream. While one is open, Harold sends no push to any subscription (Safari and the home-screen app keep separate ones) and the message does not go to iMessage, since the page shows it. A page that is hidden or locked tells Harold with `POST /page/hidden` (`204`, behind the same login, host and origin checks), and a page shown again opens a new stream, which counts it visible again. With the app closed, hidden or locked, the message is pushed as before.

The route applies only to messages that would reach the phone today. At the desk, with the screen unlocked, the chief's spoken lines are still said aloud. While Harold cannot list the tmux panes, it cannot tell which pane is the chief's, so the chief's messages go to iMessage as before.

## Calls (stage 1b)

You tap **📞 Call**, and talk with the chief as in a phone call. Harold hears when you stop talking, has the [call answerer](../voice-input/call-answerer.md) answer at once and pass real work to the chief, plays the answer on the phone, and listens again. When the call answerer fails, Harold types what you said into the chief's pane, marked `🎙`, and plays the chief's spoken answer. The desk's calls work the same way; the [voice input reference](../voice-input/README.md) describes them. Call mode uses the `[voice_input]` section for transcription. Without it, the **Call** button is disabled and reads `Voice is not set up on the Mac`, and `POST /call` answers `409` with the same line.

Starting a call on the phone ends a call open at the desk first, so the chief does not get turns from the desk's microphone too. Nothing stops the dash from starting a desk call while the phone's call is open.

Tapping **Call** on a newer screen, such as the app opened again, takes over the phone's call: the same call goes on with the newer screen, and the older screen's call ends there when Harold refuses its next piece of audio. Whatever the older screen was saying when the call was taken over is dropped, not typed.

### The routes

Every call route passes the login check and the checks on posts ([What the app refuses](#what-the-app-refuses)). The page's script uses them; nothing else needs to.

| Route | Does |
|---|---|
| `POST /call` | Starts the call, or takes over the open one. `200` once it listens, with a `text/plain` body holding the call's id; a takeover answers a new id, and the older id is refused from then on. Otherwise `409` with a line saying why. |
| `POST /call/audio?call=<id>&at=<ms>` | A quarter-second piece of audio for the call `id`, as `POST /call` answered it: 16 kHz mono, little-endian `i16`, 4,000 samples (8,000 bytes), recorded `at` milliseconds from the call's start: on the phone, just before it sent `POST /call`; on the Mac, when that request arrived. `204` once taken. `409` when no call is open, or the piece carries no id or another call's id; `400` for an odd number of bytes or a missing `at`, `413` for more than one second of audio, `503` when too many pieces wait. A refused piece does not end the call. The app posts the pieces one after another, in the order they were recorded, since Harold hears them in the order they arrive. A `409` for a piece of the open call means the call ended on the Mac without the phone hearing of it, or a newer screen took it over, so the app ends the call on the phone too. |
| `POST /call/talk-now` | Stops what plays and listens at once. |
| `DELETE /call` | Ends the call, and answers `204` once it has ended and cleaned up. |
| `GET /call/answer/<id>` | The WAV of an answer or phrase Harold offers. `404` once it is no longer offered. |
| `POST /call/played?id=<id>` | The phone reports that it has played the sound, or said the line, `id`. |

`POST /call/played` is not in the spec's route list. It is added because Harold has to know when a sound has ended before it listens again, and the phone is the only one that knows. A late or unknown report changes nothing and answers `204`.

### Not hearing the chief

While an answer, a phrase or the tick plays, and for 0.3 seconds after, the phone sends no audio. Two guards keep the chief's voice out of the next turn:

- **The app:** it stops sending from the moment a sound is offered, and starts again 0.3 seconds after the sound ends. The browser's echo cancellation helps too.
- **The core on the Mac:** it ignores any piece recorded, by the time the app stamped it, while a sound played or within 0.3 seconds after. A piece delayed on the network is judged by when it was recorded, not when it arrived.

The phone does no detection of its own. The core's rule ends a turn after 1.2 seconds of silence. To cut an answer short, tap **Talk now**.

### What iOS limits

iOS limits what a home-screen web app can do during a call. Stage 0 checks each of these on a real iPhone:

| Limit | What the app does |
|---|---|
| A home-screen web app records only while it is open and in front, so the screen must stay on. | The app keeps the screen on for the length of a call, and lets it sleep again when the call ends. |
| The microphone stops when the phone locks or the app leaves the front. | The app ends the call, and on return shows `Call ended: the app was closed or the phone locked`. |
| Sound pauses while the app is in the background. | The call ends the same way. The chief's answer still reaches the thread as text, with a push. |

Nothing records or plays while the phone is locked or the app is in the background. If the phone vanishes without ending the call, Harold ends it 15 seconds after the last piece of audio, while nothing plays.

### When something gets in the way

| Case | What you hear |
|---|---|
| The chief's pane is busy | `The chief is busy. I'll pass it on when it's free.` Harold types the turn when the pane is free, up to 2 minutes, and listens again meanwhile. |
| The chief takes longer than 3 seconds | A soft tick every 3 seconds, from 3 seconds after your turn is typed. At 90 seconds, `No answer yet. It will be in the thread.` |
| The chief's turn ends without `harold speak` | The chief's turn-end update, made into audio. |
| Nothing was heard, or the transcript is empty | `Sorry, I didn't catch that.` Harold listens again. |
| No chief is running when you tap **Call** | Nothing. The screen shows `There's no chief running.` for a moment, then the thread; the line is shown, not heard. No call starts. |
| You say `bye`, `bye bye`, `goodbye` or `bye chief` at the end of a turn | `Bye.` The call ends. Anything before those words is typed to the chief; a turn of only those words types nothing. |

Harold's own phrases are made into audio on the Mac. A line that has no audio, such as one only the `[tts]` command could say, is said by the phone's own voice. When no call starts, Harold plays and says nothing on the phone: `POST /call` answers `409` at once with the line, and the screen shows it.

### Privacy in a call

- **Transcripts** are not stored as events or in the thread, and no log line holds one. The screen shows the last one while the call lasts.
- **Audio** of your turns is held on the Mac only until the turn is transcribed.
- **Answer audio** is deleted once the phone has played it, or after 2 minutes. A sound the phone fetched counts as played after its length plus 10 seconds, if no report comes.
- **Logs:** a call writes `app call started` and `app call ended` with a `reason`, and the turns write the desk's lines. No line holds a transcript, audio or the text of a line.
- **The chief's lines:** a spoken line played in a call is added to the thread as text, so you can read it after. It sends no push and no iMessage.

## Echo test

A hidden page that measures how much of Harold's voice the phone's mic picks up while the phone plays it, with the call's own mic settings (`echoCancellation: true`, one channel) and the call's own recorder and player. Its result decides whether Harold can let you cut in on a call.

**Opening it.** Nothing in the app links to `/echo-test`. The chief sends its address in the thread or on an instructions todo; a tap on a link to the app's own address opens inside the installed app (`pages.js`), where every other link opens in Safari.

**Running it.** Pick a setup (held to your ear, on the table on speaker, or headphones) and a run, then tap **Start**. A run is 1 s quiet, 15 s of Harold's voice, then 1 s quiet. Run each setup twice: once **silent** (say nothing) and once **talking** over Harold. The page shows each run's verdict and ends with a summary of every run done.

**What it sends.** One number for every 20 ms of what the mic heard (the root mean square of 320 samples, as the speech detector computes it), at most 1,000 of them, and no sound. The page says so before **Start**.

| Route | What it does |
|---|---|
| `GET /echo-test` | The page. |
| `GET /echo-test/sample` | 15 s of speech as `audio/wav`, made at the first request with macOS `say` (`--file-format=WAVE --data-format=LEI16@24000`) and kept in memory. `503` when `say` fails. |
| `POST /echo-test/runs` | One run as JSON: `setup`, `kind`, `frames`, `playing_from` and `playing_to` (frame numbers), `echo_cancellation`, `standalone`, `user_agent`. Answers `{ saved, verdict, line, ... }`. `400` for a body over 64 KB, an unknown `setup` or `kind`, more than 1,000 frames or a playback window outside them; `500` when the file cannot be written. Same-origin only, like every post. |

**Where the runs go.** Each run, with its verdict, is written to `echo-test/<UTC time>-<setup>-<kind>.json` beside the store's folder, where `web-push/` lives. Harold logs one `echo test run` line for it.

**Reading a verdict.** The background is the mean of the first 25 frames, at least 30.

- **Silent run:** passes when at most 5% of the frames during playback are louder than 3 times the background, the bar at which the speech detector hears speech. The line gives that share; the file gives the median and 95th-percentile echo level.
- **Talk run:** its bar is 2 times the 95th-percentile echo level of the newest silent run with the same setup in the last hour. It passes when 10 of 15 frames in a row (0.3 s) during playback are over that bar, which means your voice stood out from the echo. With no such silent run the verdict is `run_silent_first`. Harold keeps the silent runs in memory, so a restart asks for a silent run again.

## Logs and privacy

### What `harold.log` holds

Harold logs no message text, no reply, no push address and no key. The app adds these lines, each fixed text with only the fields listed:

| Line | Fields |
|---|---|
| `app push sent` | `outcome` (`accepted`) |
| `app push skipped` | `reason` (`page_visible`) and `pages`, the open streams: a page was open and visible, so no subscription was pushed to |
| `app push refused` | `outcome` (`gone`, `refused` or `unreachable`) and `reason` (`status_<code>`, `timeout`, `connect`, `send`, `build` or `client`) |
| `app message not pushed` | `reason` (`no_subscriptions`, `subscriptions_unread` or `none_accepted`) and `outcome`: `away` with `channel` (`imessage`) when the away channel took the message as `(app unreachable)`, or `held` when the pane is not on the away channel's list |
| `app page served` | `rows`, `bytes` and `ms` of the thread page |
| `app page connected` | `rows` and `bytes` of its first patch |
| `app thread older served` | `rows`, `bytes` and `ms` of one load of older messages |
| `app thread older not read` | `reason` (`store_unread`), logged as a warning; the `#older` line stays and asks again |
| `app page not connected` | `reason` (`store_unread` or `render_failed`) |
| `app page disconnected` | `reason` (`page_gone`, `shutdown` or `store_stopped`) and `seconds`, how long the page was connected. Harold notices a closed page at the next patch or keep-alive, within about 15 seconds |
| `app reply state` | `event_id` and `state` (`typed`, `waiting_chief_busy`, `not_sent_no_chief` or `not_sent_paused`), logged before the state is written |
| `app reply stored` | `chars`, the reply's length in characters |
| `app todo answered` | `kind` (`answer`, `free_text`, `picks`, `later`, `tick`, `done`, `ignore`, `revise` or `reject`) and `result` (`sent`, `version_changed`, `not_waiting` or `unavailable`). The line never carries the todo's id; the question tool's own lines in `harold.log` do |
| `app todos not read` | `reason` (`unavailable` or `timeout`), logged once when reads start failing |
| `app todos read again` | none, logged once when reads recover |
| `app call started` | none |
| `call audio refused` | `reason` (`full`, `malformed` or `too_large`), a warning logged once per reason per call for a piece of audio the open call refused |
| `echo test run` | `setup` (`ear`, `speaker` or `headphones`), `kind` (`silent` or `talk`), `verdict` (`pass`, `fail` or `run_silent_first`), `share_over_bar` (silent runs: the share of playback frames over the bar) or `voice_found` (talk runs with a bar), `echo_cancellation` and `standalone` (yes or no) |
| `echo test run not saved` | `reason` (`write_failed` or `bad_request`), logged as a warning |
| `echo test sample not made` | `reason` (`say_failed`), logged as a warning |
| `app call ended` | `reason`, one of the desk's `call ended` reasons ([Log lines](../voice-input/README.md#log-lines)); `microphone_failed` when the phone stopped posting audio |

Five other lines report failures, and none carries content:

- `app push subscriptions not read` carries the store's error; the message is not pushed and `app message not pushed` follows with `subscriptions_unread`;
- `web app unavailable` names the listen address or the key's path, never the login or the key;
- `web app stopped` carries the server's error;
- `web app thread not pruned` carries no field;
- `app reply state not recorded` carries the reply's event id and the store's error.

A request refused by the login check, or by the checks on posts and the Host, writes no line at all.

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

- **The push's text:** encrypted for the phone, so the push service cannot read it.
- **Your login:** every push carries a signed token naming a contact for the sender, as the push protocol (VAPID) requires. Harold names `mailto:<allowed_login>`, so the phone's push service receives your Tailscale login with every push.
- **The address:** the push service issued it, so it knows which phone the push is for.

## Dependencies: OpenSSL inside Harold

The `web-push` crate, which encrypts and signs each push, brings OpenSSL in through its encryption crate, `ece`. Harold builds that OpenSSL from source, with the `openssl` crate's `vendored` feature, and links it into the `harold` binary. No OpenSSL library is needed on the Mac at run time.

- **The version:** openssl-src 300.6.1+3.6.3, which bundles OpenSSL 3.6.3.
- **The build target:** `.cargo/config.toml` sets `MACOSX_DEPLOYMENT_TARGET = "15.0"`, the same as the release build, so local builds and the release compile OpenSSL for the same macOS version.
- **The release guard:** the release build fails if `otool -L` lists `libssl` or `libcrypto` for the binary, which would mean the build fell back to a system OpenSSL.
- **The known CVEs:** OpenSSL 3.6.4 and 3.6.5 fix 24 CVEs in 3.6.3, in DTLS, QUIC, CMS, CMP, TLS and SM2, among others. Harold uses OpenSSL only through `ece`, for P-256 key agreement (ECDH), AES-128-GCM encryption and random bytes, and never for TLS, which keeps those CVEs out of reach. Pushes travel over HTTPS through `reqwest`, which uses macOS's own TLS. `cargo audit` does not report these CVEs, so the gap is tracked by hand until a patched openssl-src is released.
- **`rsa`:** `rsa` 0.9.10 is in `Cargo.lock`, through `jwt-simple`, with RUSTSEC-2023-0071, a timing attack on RSA. Harold signs its pushes with ES256 (P-256 ECDSA) only and never calls `rsa`, so the advisory cannot be reached. `cargo audit` still reports it.
