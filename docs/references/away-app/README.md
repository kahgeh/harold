# Web app: the chief's thread on the phone

The web app puts one conversation with the chief of pods on the iPhone's home screen. When the chief sends you a message while you are away, Harold adds it to the app's thread and the phone shows a push on the lock screen. You open the app, type a reply, and Harold types it into the chief's pane, marked `📱`. Every reply goes to the chief, so Harold never has to guess which pane it is for, as it does with an iMessage reply.

Harold serves the app itself, from this Mac, on a loopback address (`127.0.0.1`, reachable only from the Mac). Tailscale, the private network that joins your Mac and your iPhone, carries it to the phone. iMessage keeps working alongside it, as the fallback.

The chief is the first agent pane of the chief's tmux session among the agents running in a folder `[notify] chief` matches.

The web app works in every build. Its calls need the `calls` feature, and its iMessage fallback needs the `imessage` feature.

## Rolling back to an earlier Harold

This build adds migrations to Harold's state database: `002_web_app` (two tables, one for the thread and one for the phones that turned on push) and `003_app_photo` (a `photo` column on the thread's rows and a table of the photos uploaded). A Harold built before this one finds a migration it does not know, refuses the store, and stops at start with `incompatible state schema`.

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

Install Tailscale on this Mac (laptop-mac) and on the iPhone, and sign both in to the same tailnet, the private network Tailscale makes from the devices signed in to your account. Only devices in that tailnet can reach the app.

Before `tailscale serve` will serve HTTPS, the tailnet needs two settings turned on in the Tailscale admin console:

- **MagicDNS**, which gives the Mac its name in the tailnet, `laptop-mac.<tailnet>.ts.net`;
- **HTTPS certificates**, which let Tailscale obtain a certificate for that name.

The first run of `tailscale serve` offers a link to turn on whichever is off. The `tailscale` command must also be on the `PATH`: the Mac app can install its command-line tool from its settings.

### Publishing the app with `tailscale serve`

Harold listens on `127.0.0.1:4330`. `tailscale serve` takes HTTPS requests from the tailnet at `https://laptop-mac.<tailnet>.ts.net/`, with a certificate Tailscale obtains, and passes them to that address:

```sh
tailscale serve --bg http://127.0.0.1:4330
```

`tailscale serve status` shows what is published. Never use `tailscale funnel` for the app: Funnel opens a service to the whole internet.

For each request it passes on, Tailscale Serve adds a `Tailscale-User-Login` header naming the person signed in on the device that sent it. Harold answers `403` to every request whose header is missing or differs from `allowed_login`, on every route, and stores and logs nothing for it.

Harold also accepts only a Host ending in `.ts.net`, such as the Mac's tailnet name, or the loopback address it listens on. Stage 0 must confirm that `tailscale serve` passes the `.ts.net` Host through to Harold. If it passes some other name instead, Harold answers `403` to every request, and the app is blocked entirely.

### What the app refuses

The login header comes from the device, not from the page, so every page open in the phone's browser carries it, as it would a cookie. Two more checks keep other pages out, and each refusal is a `403` with nothing stored or logged:

- **Posts from another page:** every post and delete must come from the app's own page, and goes through Datastar. The browser must say the request is from the same site (`Sec-Fetch-Site: same-origin`, or else an `Origin` that matches the Host), and the request must carry `Datastar-Request: true`, which Datastar's `@post` and `@delete` send and another site's page cannot send without asking first. Two routes keep their own transport, and each is let in by its content type on its own route only: `POST /call/audio` with `application/octet-stream`, and `POST /telemetry/traces` with `application/json`. These are the two exceptions the user approved, and only the user approves another ([ADR 0018](../../adr/0018-every-web-app-post-goes-through-datastar-and-only-the-user-approves-an-exception.md)). Harold never grants that asking, so another site's page cannot post at all. A post from the app's own origin that lacks the header, on any other route, gets `403` and logs `app post refused` with its `route` and `reason` `not_datastar`.
- **Another Host:** a page can point its own name at `127.0.0.1` and reach Harold's listener. Harold refuses every request whose Host is not the tailnet name or the loopback address, so such a page gets nothing.

**What each request sends.** Datastar's `@get` puts every signal on the page in the URL, and `@post` puts them in the body, unless the call names the ones to send. A script that posts sets its signals with Datastar's `mergePatch`, starts the request with Datastar's `post` action, and sets `retry: 'never'`, `retryMaxCount: 0` and `openWhenHidden: true`, so a failed post is sent once and a post sent as the page hides is not sent again on return. A `401` is a plain `401`, which Datastar announces as an `error` event, and the page reloads to the pair screen. Each call in the app's templates sets `filterSignals` to the signals its handler reads: `/updates` and `/thread/older` send `oldest`, and `POST /replies` sends `reply` and `photo`. The todo posts send their own form fields. A reply still being typed therefore leaves the phone only when you tap **Send**, and never lands in a URL that a proxy, a log or the browser might record. A test fails if a template adds a request without the filter.

**Devices that are not paired.** After the login check, Harold looks for the cookie `harold_device` and finds the device by the SHA-256 of its value in `web-app/devices.json`. A request without a known cookie gets `401` and nothing else, and `GET /` gets the pair screen instead of the thread. Only these files are served without the cookie, by exact path: `/static/app.css`, the scripts `datastar-1.0.2.js`, `alerts.js`, `call.js`, `call-worklet.js`, `dictation.js`, `echo-test.js`, `pages.js`, `photo.js` and `pair.js`, `/static/sw.js`, `/static/manifest.webmanifest` and `/static/icon.png`. Photos, the live stream, the todos, calls, pushes, the browser spans and every other route need the cookie. Harold logs `app device refused` (INFO) with a `reason` of `no_cookie` or `unknown_device`, once per reason per hour, never the cookie. See [Pairing a phone](#pairing-a-phone).

The Host, origin and login checks protect against web pages and other devices; the device check adds protection against other programs and accounts on the Mac that reach `127.0.0.1:4330` without the secret. None of them protects against a program that runs as you: it can read the pairing file and pair a device of its own, as it can already type into the chief's pane through tmux.

### Pairing a phone

A phone is paired once, inside the installed app. The pair screen has one field and a **Pair** button. You type the pairing code, which is a line `HAROLD_PAIRING_CODE=<your code>` in the pairing file, `~/bin/harold/config/pairing.env` (mode 0600, at least 6 letters or digits, outside the repository; the installer carries it across installs). Harold reads the file at each attempt, so a changed code takes effect at once.

`POST /pair` takes the page's `code` signal, `{"code": "..."}`, behind the same login, Host and origin checks as every route, and compares the code ignoring spaces, hyphens and letter case, in constant time:

| Answer | When |
|---|---|
| `200` and the cookie | The code is right. The answer is Datastar events that patch the `paired` signal to `true`, and the page reloads.  The cookie is `harold_device=<32 random bytes, hex>; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=34560000` (400 days). Harold stores its SHA-256 with a name such as `iPhone, paired 23 Oct`. |
| `200`, notice "That code didn't work" | The code is wrong (a try). The answer patches `#pair-notice`. A body that is not `{"code": "..."}` gets `400` and is not a try. |
| `200`, notice "Pairing is locked on the Mac" | Five wrong tries in all have locked pairing, even for the right code. |
| `200`, notice "Pairing isn't set up on the Mac" | The pairing file is missing, has no code, has a code under 6 characters, or can be read by others (mode other than 0600). Harold logs one WARN `app pairing not set up` with `reason` `missing`, `no_code`, `too_short` or `mode`. |

**The lock.** The count of wrong tries is kept in `web-app/pairing-lock.json` (mode 0600) with a salted fingerprint of the code, so a restart does not reset it, and under a lock, so ten posts at once get at most five tries. The fifth wrong try locks pairing for good (WARN `app pairing locked`, once); each wrong try logs WARN `app pairing code failed` with `tries`. Two things unlock it: changing the code in the file, or `harold app pair --unlock`. A successful pairing sets the count back to 0.

**Commands.**

| Command | What it does |
|---|---|
| `harold app pair` | Says whether pairing is set up and locked, and the wrong tries so far. It never shows the code. |
| `harold app pair --unlock` | Clears the lock and the count, by removing `web-app/pairing-lock.json` itself as you. It is not a call to the daemon, because any program on the Mac could make that call and reset the lock to guess without limit. |
| `harold app devices` | Lists the paired devices with when each was paired and last seen. |
| `harold app unpair <name>` | Removes a device. Its next request gets `401`, and the app reloads to the pair screen. |

**Last seen** is kept in memory and written to `devices.json` at most once an hour. When a request gets `401`, the app reloads (`pages.js` for the live stream, replies and todos; `call.js`, `photo.js`, `alerts.js` and `dictation.js` for their own requests).

Nothing in the page, the log, the commands' output or any message ever holds the code or a line of the pairing file.

### The `[web_app]` section

Add the section to Harold's configuration:

```toml
[web_app]
listen = "127.0.0.1:4330"
allowed_login = "<your Tailscale login>"
# app_name = "Chief M4"
# icon = "~/code/your-organisation/machines/<this Mac>/icon.png"
# pairing_file = "~/bin/harold/config/pairing.env"
# require_pairing = true
# specs_listen = "127.0.0.1:4331"
```

| Key | Value |
|---|---|
| `listen` | The address and port Harold listens on. It must be a loopback address, such as `127.0.0.1:4330`. |
| `allowed_login` | Your Tailscale login, exactly as Tailscale Serve sends it in `Tailscale-User-Login`. |
| `app_name` | Optional. The name under the home-screen icon and in the app's title, 1 to 30 characters on one line. Left out, it is `Chief`. iOS shows about 12 characters, so a short name such as `Chief M4` reads best. |
| `icon` | Optional. The path of a square PNG, 180 to 1024 pixels a side and at most 1 MB. A leading `~/` is the home folder; otherwise the path must be absolute. Left out, the app has no icon of its own. Harold reads the file once, at start, so a change needs a restart. It does not resize the image; iOS scales a larger square down itself. |
| `pairing_file` | Optional. The file holding `HAROLD_PAIRING_CODE=<code>`. A leading `~/` is the home folder. Left out, it is `~/bin/harold/config/pairing.env`. |
| `specs_listen` | Optional. The loopback address and port of the [specs listener](#specs). It must differ from `listen`. Left out, it is `127.0.0.1:4331`. The listener opens only when `[telemetry] pod_registry` is set. |
| `require_pairing` | Optional. `true` when left out: only a paired device may use the app. `false` switches the device check off, for a test Harold or for recovery, and Harold logs the WARN `web app allows any device` at every start-up. |

Set `app_name` and `icon` in each Mac's own `local.toml`, so the two Macs' apps differ on the home screen. The file is served at `/static/icon.png` behind the login check, and the manifest and thread page take the name and icon from these keys.

The section also needs `[notify] chief` (the default `"chief-of-pods"` will do), because that entry decides which agent is the chief. The chief reaches the app only when its folder is on `[notify] web_app`.

- **Absent:** Harold serves nothing, listens on no port, and every message reaches the phone as before.
- **Invalid:** a `listen` or a `specs_listen` that is not a loopback address and port, a `specs_listen` equal to `listen`, an empty `allowed_login`, no `notify.chief`, an `app_name` that is empty, over 30 characters or holds a control character, or an `icon` that cannot be read, is not a square PNG of 180 to 1024 pixels a side or is over 1 MB stops Harold at start with an error naming the key. `--check-config` fails too, but prints only `invalid configuration`; start Harold to see which key is wrong.
- **Cannot start:** when Harold cannot listen on the address or read its key, it logs `web app unavailable` with the address or the key's path, and runs without the app. Messages then reach the phone through iMessage.

### Adding the app to the home screen

On the iPhone, open `https://laptop-mac.<tailnet>.ts.net/` in Safari, tap the Share button, and choose **Add to Home Screen**. The name under the icon is `app_name`, or **Chief** when none is set, and the icon shows the `icon` file when one is set. iOS saves the name and icon when you add the app and does not read them again. To see a changed name or icon: delete the app from the home screen, add it again from Safari, and tap **Turn on alerts** again, because deleting the app also deletes its sign-up for pushes. Open the app from that icon from now on: iOS delivers pushes only to a web app opened from the home screen, so a Safari tab does not show **Turn on alerts** at all.

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

The thread page's header holds the app's name, **Todos** with the todo count in orange (no number when nothing waits), the **Turn on alerts** button until push is on, and **Call**, a round phone-icon button (greyed out, with `Voice is not set up on the Mac` as its label, when voice isn't set up).

The thread is the oldest message at the top. It holds the last 30 days of messages; Harold deletes older ones at start and every hour. The page opens with the newest 50 messages and loads older ones, 50 at a time, as you scroll up; "Loading earlier messages…" shows at the top while more remain. While the page is open, each new message appears as soon as Harold stores it. After Harold restarts, the page picks the thread up again by itself within seconds, and keeps the older messages it had loaded. While Harold can't be reached, it tries less often, at least every 30 s.

- **Where the thread opens.** Each device keeps its own read mark, the newest message that has been fully on screen while the app was in view, in that browser's storage under `harold-read-mark`; reading on the desk does not move the phone's. When the app loads, reloads or comes back to the front, it opens at the first message after the mark that is not your own, just under the header, with **New message ↓** ready if more arrive. With nothing new it opens at the newest message. If the mark is older than the 50 messages the page holds, it opens at the oldest of them. Sending a reply moves the mark to that reply. A private window that refuses storage opens at the newest message. Each opening is a `browser.thread_opened` span.
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

### Replies with a photo

The reply bar reads: text box, photo button, **Send**, which is an up-arrow icon in a round button. Tap the photo button and the phone's own chooser opens (Photo Library, Take Photo, Choose File). The chosen photo shows above the bar with its size and **✕** to take it off. **Send** works with a photo and no text.

- **The phone shrinks the photo first.** It draws it to a canvas no longer than 2,048 px on its longer side and sends JPEG at quality 0.85, which makes a photo or screenshot about 0.3 to 1 MB and turns an iPhone HEIC photo into JPEG. Every upload is JPEG, a photo no wider than 2,048 px included.
- **It uploads at once.** `photo.js` posts the shrunk photo to `POST /replies/photo` as soon as it is chosen, so **Send** has nothing to wait for; **Send** is disabled while the upload runs. A failed upload clears the strip and shows `The photo didn't reach Harold.` in it for 3 seconds. The page sends the photo through Datastar's `@post` as the base64 signal `_photo`: a signal that starts with `_` stays out of every request by default, only this post names it, and the page clears it when the post ends. The route answers with Datastar events that patch the `photo` signal to the photo's id.
- **The reply names the photo.** The page's `photo` signal holds the id, and `POST /replies` sends it with the words. A reply may have no words when it names an upload no reply has used; an id that names none gets `400`.
- **The chief gets a path.** The line typed into the chief's pane is `📱 <your words> 📷 <absolute path>`, or `📱 📷 <absolute path>` with no words, for example `/Users/<you>/bin/harold/data/app-photos/<id>.jpg`. Claude Code reads the file with its Read tool. iMessage replies are unchanged.
- **Your row shows it.** The photo sits above your words; tapping it opens it full size. A row whose photo Harold has deleted shows `photo removed`.
- **How long photos are kept.** As long as their thread row: 30 days. The hourly prune deletes the files with the rows, and deletes an upload no reply named after 1 hour (you tapped ✕ or left the reply).

| Route | Does |
|---|---|
| `POST /replies/photo` | The body is the `_photo` signal, `{"_photo": "<base64>"}`, up to 7 MB. The decoded photo is at most 5 MB (the limit Claude has for one image), and starts like a JPEG (`FF D8 FF`) or a PNG (`89 50 4E 47`); Harold never decodes the image itself. It is written to `app-photos/<uuid>.jpg` (or `.png`) beside the store's folder, the folder at mode 0700 and the file at 0600. `200`, Datastar events that patch the `photo` signal to the id. `413` when too large, `400` when not that signal, not base64, or not a JPEG or PNG, `503` when it cannot be written. |
| `GET /replies/photo/<id>` | The photo of upload `id`, with `Cache-Control: private, max-age=86400`. `404` for an id that is not a UUID, or whose photo was deleted. |

Both routes pass the login check and the checks on posts ([What the app refuses](#what-the-app-refuses)). The photo's content, its path and your words are in no log line.

### Dictating a reply

With nothing typed and no photo chosen, the reply bar's last button is a mic. Typing a letter or choosing a photo turns it back into **Send**. Tap the mic and a red dot with "Listening" shows above the bar; your words appear in the box as you speak, in grey until you stop. Tap the stop button, or leave 4 seconds of quiet, and the words stay in the box in the normal colour. Check them, fix them with the keyboard, and tap **Send**: the chief gets an ordinary reply, `📱 <the words>`.

- **The phone does the listening.** `dictation.js` uses Safari's speech recognition (`webkitSpeechRecognition`). Safari gives the microphone's sound to Apple's speech recognition, the service behind the keyboard's dictation key; Apple runs it on the phone or on its servers, depending on the phone and the language, and the page cannot see which. The sound never goes to Harold, and Harold transcribes nothing. The words reach Harold, then the chief and the model, only when you tap **Send**. iOS asks once for the microphone and for speech recognition, the first time you tap the mic.
- **It may not work in the installed app.** Earlier iOS versions failed in an app added to the home screen with `service-not-allowed`. The mic shows whenever the API exists. On `not-allowed` or `service-not-allowed` the strip says `Dictation isn't available in this app.` for 3 seconds and the mic stays. The keyboard's own dictation key is still there. `no-speech` and `aborted` go back to the mic quietly; `network` says `Dictation needs a connection.` and `audio-capture` says `The microphone gave no sound.`, each for 3 seconds.
- **It stops when you leave.** Hiding the app or starting a call aborts it; the words so far stay in the box.
- **A dictated reply is a typed reply.** `POST /replies`, `app reply stored`, the same row and states. No mark is added, because you checked the words before sending.

| Route | Does |
|---|---|
| `POST /dictation/check` | The page reports once per page load the first thing it knows: `missing` (neither API exists), `ok` (the first result), or the first error: `not_allowed`, `service_not_allowed`, `network`, `no_speech` or `audio_capture`. The body is the `result` signal, `{"result": "<word>"}`, at most 64 bytes, sent through Datastar with `openWhenHidden` and no retry. `204`; an unknown word, a longer body or bad JSON gets `400`. It passes the login check and the checks on posts ([What the app refuses](#what-the-app-refuses)). |

Whether dictation works in the installed app is answered from the log, over 30 days:

```
SELECT LogAttributes['result'] AS result, count() AS pages
FROM otel.otel_logs
WHERE ServiceName = 'harold' AND Body = 'app dictation check' AND Timestamp > now() - INTERVAL 30 DAY
GROUP BY result
```

Any `service_not_allowed` means the installed app cannot dictate, and the mic stays hidden there. No log line holds the words.

## The todos

The todo page lists the chief's todos as cards in the question tool's order. Its header shows a small speech-bubble icon and a number when chief messages arrived while you were on the todo page; the number goes when you return to the thread page.

- **A question** shows its id, product and intro, a link to the spec when it has one (**Open the spec**, to its [spec page](#specs), when the todo has a spec file in a product's specs folder; else the todo's `link`), and each decision with its options. `suggested` marks the suggested option, and a decision starts on your saved pick, else the suggested option. A free option shows a text box while picked. **Send** is enabled once every decision has a pick and every free pick has text. **Relegate to idea…** takes the question off your todos and asks the chief to keep it as an idea note (see below).
- **The checked line** sits above the cards and says when the chief last checked every todo against its spec: `Checked by the chief at 14:05` in the Mac's local time (`yesterday at 14:05` or `on 22 Oct at 14:05` for an earlier day). More than 2 hours after the check, with a todo waiting, it turns red and reads `Not checked since 14:05`. Before any check it reads `Not checked yet`, in red. It changes on its own at the next todo read, with no reload, and is left out when there is no todo.
- **A changed spec** shows the notice `Changed spec: the spec changed after this was asked. The chief will put it again.` on the card. Its **Send** is off, and **Revise…**, **Reject…** and **Relegate to idea…** are left out; on instructions **Done** and **Ignore** are off. The notice goes when the chief puts the todo again. If an answer is posted anyway, Harold refuses it (`spec_changed`) and the card shows the same notice.
- **Instructions** show a title and numbered steps. Ticking a step stores the tick at once. **Done** and **Ignore** close them.
- **Answers reach the chief at once**, as when you press `send` in the dash. **Relegate to idea…** sits beside **Send**, and **Revise…** and **Reject…** sit in a row under them. Each opens a note box in the card (`Anything to add to the idea? (optional)`, `What should change?` or `Why reject it?`); the card's **Send** then reads **Send relegate**, **Send revise** or **Send reject**, **Relegate to idea…** gives its place to **Cancel**, and the picks made so far go with the note. A blank note is never sent for a revise or a reject; **Send relegate** works with an empty note, and the question then leaves your todos at once. The note takes up to 1,999 characters (the box shows the count from 1,800). When Harold refuses the answer, the card shows its notice and keeps the note box open with the text in it. Once Harold has taken any answer from the page (**Send**, **Revise**, **Reject**, **Relegate**, **Done** or **Ignore**), a sent line at the top of the page reads, for example, `Q53: revise sent to the chief.` or `Q53: relegate sent to the chief.` until the next post from that page or until you leave the page. The line belongs to the page that sent the answer.
- **When a post is refused**, the card shows a notice and is redrawn from the next list: `<id> changed. Look again before you send.`, `<id> was already answered or withdrawn.`, or `Harold can't send this right now. Try again.` When Harold cannot reach the tool to save your picks, the last one reads `Harold can't save your picks right now` instead. Once the tool takes a later save of that card's picks, the next redraw drops the notice, and **Send** still sends the picks the card shows, saved or not. A todo that is no longer listed has no card left to show its notice on.
- With no todo the page reads `No todos for you.`; when Harold cannot read them and has no list yet, it reads `Harold can't read the todos right now.` and the todo count is empty. A failed read later keeps the last list.

The web app reads the todos through the question tool's gRPC service on Harold's own address (`[grpc]`), every 2 seconds while a page is open and at once after each post ([ADR 0007](../../adr/0007-the-web-app-reaches-todos-through-the-question-service.md)). The posts are `POST /todos/answer`, `/todos/picks`, `/todos/relegate`, `/todos/tick`, `/todos/done`, `/todos/ignore`, `/todos/revise` and `/todos/reject`, each behind the login and same-origin checks like every route. The phone saves a question's picks with `/todos/picks` each time you select an option and after half a second of typing; a save the tool took answers `204` and does not redraw your page, and every open page shows the saved picks at its next todo read.

## Notes

**Notes** means the vault notes, the organisation's Markdown notes that the [knowledge tool](../tools/knowledge.md) indexes. It never means backlog notes. The page exists only when `[knowledge] vault` is set; without it there is no Notes link and the routes answer `404`.

- **The link.** The thread's header shows **Notes** beside **Todos**. It opens the Notes page, and ‹ back comes back to the thread. The header row wraps: until push is on, **Turn on alerts** takes a second line, so the row fits at 320 px.
- **The page** is a search box, then **Changed lately**: the 10 vault notes changed most recently, newest first. Each row shows only the vault note's kind (such as `idea`, `reference` or `decision`) and its title. Nothing more is on the first screen: no description, no matching passage and no related list.
- **A search** runs as you pause typing (300 ms). The list becomes **Closest notes**, at most 10 rows, in the same form. A query under 2 characters keeps the recent list. When nothing matches the list reads `No notes match.` When the model could not load it reads `Search is resting: the model didn't load.` and when the tool cannot be reached, `Notes are not available right now.`
- **Tapping a row** opens the vault note, read-only: its kind, its title and its Markdown rendered, with raw HTML switched off (shown as the text it is), images left out, and links only to a web address, mail or a place in the page. Its `[[links]]` show as plain words, not links. There is no related list. A vault note that no longer exists answers `404`.

| Route | Answers |
|---|---|
| `GET /notes` | The page, with the 10 most recent |
| `GET /notes/search?q=` | A Datastar patch of the list (`#notes-list`) |
| `GET /notes/{name}` | The vault note `name`, a path under the vault without `.md`, such as `references/hyperdx`. A vault note named `search` is shadowed by the search route |

The routes sit behind the web app's login, device, host and origin checks like every other route. The web app reads the vault notes through the knowledge tool's gRPC service on Harold's own address, as it reads the todos ([ADR 0007](../../adr/0007-the-web-app-reaches-todos-through-the-question-service.md)). A search carries `source = "app"` on the tool's `harold.knowledge.search` span; the words typed ride in the request's URL only as far as Harold's own address, and no log line holds them.

## Specs

A todo's **Open the spec** link opens its spec page. A spec page links to the specs list, and the back arrow comes back to the thread. The pages show the spec files of the products in the registry that `[telemetry] pod_registry` names, through the same `Products` port as the question tool ([ADR 0015](../../adr/0015-the-question-tool-reads-the-spec-files-the-chief-names.md)). They show nothing the files do not hold.

- **The list** (`GET /specs`) shows each product's specs, newest first, with their titles. A machine with no registry reads `No specs on this machine`.
- **A spec page** (`GET /specs/{product}/{id}`) shows the product, the id and the first 12 characters of the file's SHA-256 on its top line, so you can compare it with the SPEC line, then the rendered spec. The id is lowercase letters, digits and `-`. A fenced `mermaid` block, or a raw `<pre class="mermaid">` block, is drawn as a diagram, with the theme `neutral` in light mode and `dark` in dark mode. Mermaid runs with `securityLevel: 'strict'`. A diagram that fails to draw shows its source. Raw HTML shows as text, and a link keeps its `href` only for `http:`, `https:` or a relative address.
- **Refusals.** An unknown product or id, an id with a dot or a `/`, a symlink out of the specs folder and a file over 1 MiB each answer `404` with a short page and no path. A registry that cannot be read answers `503`.
- **Share.** The **Share** button posts `/specs/{product}/{id}/share`, behind the login, device and same-origin checks. Harold computes the checksum from the file at that moment, and puts the reply `Share spec <product>/<id> sha <12>` in the thread, as if you typed it. The button then reads `Asked the chief to share it`. The chief has the PnEM publish that spec as a private artifact, and sends you the link. Nothing leaves the machines unless you press the button.
- **The content security policy.** A spec page carries the app's policy with `style-src 'self' 'unsafe-inline'`, because Mermaid writes a `<style>` into each diagram. Scripts stay `'self'`. Every other page keeps the app's policy.

### The specs listener

A second listener serves the browser on the Mac at `http://127.0.0.1:4331` (`[web_app] specs_listen`). The dash `o` opens a todo's spec page there. `tailscale serve` does not forward to it.

- It answers only `GET /specs`, `GET /specs/{product}/{id}` and the three files `/static/app.css`, `/static/specs.js` and the Mermaid bundle. Every other path, `POST /specs/{product}/{id}/share` included, answers `404`. It has no **Share** button.
- It has no login or pairing check, because a browser on the Mac sends no Tailscale login. It refuses a request whose `Host` is not its own loopback name (`127.0.0.1:4331` or `localhost:4331`) with `403`, which stops DNS rebinding.
- Any local user or process can read every spec through it, as it can read the files. [ADR 0002](../../adr/0002-tool-services-trust-local-callers.md) trusts local callers. It answers no `POST`, so no local process can make the chief publish a spec through it. See [ADR 0017](../../adr/0017-harold-serves-the-specs-and-a-spec-leaves-the-machines-only-when-the-user-shares-it.md).
- Harold opens it only when `pod_registry` is set, so a machine with no registry has no specs listener, and its `/specs` reads `No specs on this machine`. The todos on such a machine come from the chief on another Mac, so their links point at that Mac's pages.

Harold logs `app spec shown` (INFO) with `listener` (`web_app` or `specs`), `result` (`ok`, `not_found`, `refused` or `unreadable`) and `ms`, and adds `product` and `id` only when the product is in the registry and the id passes the id pattern. It logs `app spec shared` (INFO) with `product`, `id` and `sha` when a **Share** reply is stored. No log line holds a path or a spec's text.

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
- **Skipped while the app is open:** a page that is open and visible holds a `GET /updates` stream. While one is open, Harold sends no push to any subscription (Safari and the home-screen app keep separate ones) and the message does not go to iMessage, since the page shows it. A stream counts as on screen only if it was opened since the page's last hidden report and the page was heard from in the last 75 s. A page on screen says so with `POST /page/seen` (`204`; the `beat` signal, `{"beat": n}`, names the last beat it got) at load, when it becomes visible and every 30 s; a page that is hidden or locked tells Harold with `POST /page/hidden`, which covers only the streams open at that moment. Both posts go through Datastar with `openWhenHidden: true` and no retry. Datastar has no `keepalive`, so the phone can drop a hidden report before it leaves, which the 75 s rule below covers. The `app page disconnected` line has the field `hidden_reported`: `true` for one hidden report during the stream, `false` for none, and `repeated` for more than one. Both sit behind the same login, host and origin checks. A page that vanishes without a hidden report stops holding pushes back after 75 s. **A stream that stalls:** a stream can stop delivering without ending or failing, when the connection between the phone and Harold breaks silently and Harold's writes go into buffers. So every open stream sends the page a beat, the `_beat` signal, every 15 s, and a page that is visible and has had none for 45 s reports itself and opens the stream again (`harold-reopen`, with the `stalled` signal set to `1` for that one request), which brings back every row sent meanwhile. Harold's newest stream of the page that is more than 2 beats ahead of the beat the page reports is logged once (`app page stream stalled`) and no longer counts as on screen. With the app closed, hidden or locked, the message is pushed as before.

The route applies only to messages that would reach the phone today. At the desk, with the screen unlocked, the chief's spoken lines are still said aloud. While Harold cannot list the tmux panes, it cannot tell which pane is the chief's, so the chief's messages go to iMessage as before.

## What the app records

With telemetry export on (the default), the page records its page loads, requests and script errors, and when a row of the thread is drawn, as OpenTelemetry spans in the browser. It posts them to Harold's `POST /telemetry/traces`, which keeps only the fields it allows (paths without queries, no text you typed; a script error's message is cut to 200 characters) and forwards them to the local collector, where they are stored under `harold-web-app`. A Send, and a todo answer, then show as one trace from the tap, through the chief's turn, to the answer on the phone. The page downloads one extra script of about 72 KB, kept by the phone until its name changes. With export off none of this is served or kept. The [telemetry reference](../telemetry/README.md#browser-spans) has the details.

## Calls (stage 1b)

You tap the phone icon (**Call**) in the header, and talk with the chief as in a phone call. Harold hears when you stop talking, has the [call answerer](../voice-input/call-answerer.md) answer at once and pass real work to the chief, plays the answer on the phone, and listens again. When the call answerer fails, Harold types what you said into the chief's pane, marked `🎙`, and plays the chief's spoken answer. The desk's calls work the same way; the [voice input reference](../voice-input/README.md) describes them. **Connecting.** Harold loads its voice worker while a page of the app is on screen, and keeps it for 10 minutes after the last page leaves the screen and the last call ends (`[voice_input] keep_worker_minutes`). Every call starts like a phone call: the screen shows `Connecting…` with the hint `Wait for the tone to stop` (or `Getting the speech model: 45%` while a model downloads), and Harold plays the ringback, "ring-ring", for at least one whole 3-second cycle, and another whole cycle each time the call is not ready at the end of one. Harold uses the ringing to start the voice worker if it is not loaded and to warm the call answerer. Then a short pick-up sound plays and the screen shows `Listening`. A call takes the worker the page loaded, so most calls ring once. You are not heard while it rings. Harold says nothing before the call answerer's answer; if the answer takes more than three seconds a soft tick plays every three seconds. If a speech model cannot be fetched, the ringing stops, Harold says `The speech model isn't on the Mac yet.` in the phone's own voice, the call ends with `voice_failed`, and `#call-notice` names the model, the reason and the command to run on the Mac ([the voice input reference](../voice-input/README.md#speech-models)).

Call mode uses the `[voice_input]` section for transcription. Without it, the **Call** button is greyed out and its label reads `Voice is not set up on the Mac`, and `POST /call` answers with the same line in `_callWhy`.

Starting a call on the phone ends a call open at the desk first, so the chief does not get turns from the desk's microphone too. Nothing stops the dash from starting a desk call while the phone's call is open.

Tapping **Call** on a newer screen, such as the app opened again, takes over the phone's call: the same call goes on with the newer screen, and the older screen's call ends there when Harold refuses its next piece of audio. Whatever the older screen was saying when the call was taken over is dropped, not typed.

### The routes

Every call route passes the login check and the checks on posts ([What the app refuses](#what-the-app-refuses)). The page's script uses them; nothing else needs to.

| Route | Does |
|---|---|
| `POST /call` | Starts the call, or takes over the open one. `200` once the call has opened, listening or connecting, as Datastar events that patch the `_callId` signal to the call's id; a takeover answers a new id, and the older id is refused from then on. Otherwise `200` with the `_callWhy` signal patched to a line that gives the reason no call started. |
| `POST /call/audio?call=<id>&at=<ms>` | A quarter-second piece of audio for the call `id`, as `POST /call` answered it: 16 kHz mono, little-endian `i16`, 4,000 samples (8,000 bytes), recorded `at` milliseconds from the call's start: on the phone, just before it sent `POST /call`; on the Mac, when that request arrived. `204` once taken. `409` when no call is open, or the piece carries no id or another call's id; `400` for an odd number of bytes or a missing `at`, `413` for more than one second of audio, `503` when too many pieces wait. A refused piece does not end the call. The app posts the pieces one after another, in the order they were recorded, since Harold hears them in the order they arrive. A `409` for a piece of the open call means the call ended on the Mac without the phone hearing of it, or a newer screen took it over, so the app ends the call on the phone too. |
| `POST /call/talk-now` | Stops what plays and listens at once. |
| `DELETE /call` | Ends the call, and answers `204` once it has ended and cleaned up. With `?why=no_sound` the call ends as `microphone_failed` instead of `stopped`. |
| `POST /call/check` | The page's facts about the call's microphone, sent 3 seconds after the call opens, or when starting the call threw. JSON of known words and counts: `problem`, `step`, `error_name` (1 to 40 letters), `context_state`, `track_state`, `track_muted`, `pieces_made`, `pieces_posted`, `pieces_dropped` (`playing`, `echo_tail`, `before_start`, `not_open`), `posts_failed` and `empty_blocks` (counts 0 to 100,000). `204` once logged; `400`, with nothing logged, for any other word, field or count. |
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
| The microphone gives no sound: no audio piece has been made 3 seconds after the call opened. | The app ends the call and shows `The microphone gave no sound. Tap Call to try again.`; Harold logs `app call microphone failed` with what the page saw. |
| Sound pauses while the app is in the background. | The call ends the same way. The chief's answer still reaches the thread as text, with a push. |

Nothing records or plays while the phone is locked or the app is in the background. If the phone vanishes without ending the call, Harold ends it 15 seconds after the last piece of audio, while nothing plays.

### When something gets in the way

| Case | What you hear |
|---|---|
| The chief's pane is busy | `The chief is busy. I'll pass it on when it's free.` Harold types the turn when the pane is free, up to 2 minutes, and listens again meanwhile. |
| The chief takes longer than 3 seconds | A soft tick every 3 seconds, from 3 seconds after your turn is typed. At 90 seconds, `No answer yet. It will be in the thread.` |
| The chief's turn ends without `harold speak` | The chief's turn-end update, made into audio. |
| Nothing was heard, or the transcript is empty | `Sorry, I didn't catch that.` Harold listens again. |
| The call has just started, the voice worker is still loading, or a speech model is being fetched | The ringback, one whole 3-second cycle after another, and the screen says `Connecting…`. You are not heard until the pick-up sound has played. |
| A turn goes to the call answerer | The answer, with the tick every 3 seconds while it takes longer than that. |
| A speech model did not download | `The speech model isn't on the Mac yet.`, said by the phone; the call ends and the notice names the model, the reason and the command. |
| No chief is running when you tap **Call** | Nothing. The screen shows `There's no chief running.` for a moment, then the thread; the line is shown, not heard. No call starts. |
| You say `bye`, `bye bye`, `goodbye` or `bye chief` at the end of a turn | `Bye.` The call ends. Anything before those words is typed to the chief; a turn of only those words types nothing. |

Harold's own phrases are made into audio on the Mac. A line that has no audio, such as one only the `[tts]` command could say, is said by the phone's own voice. When no call starts, Harold plays and says nothing on the phone: `POST /call` answers at once with the line in `_callWhy`, and the screen shows it.

### Privacy in a call

- **Transcripts** are not stored as events or in the thread, and no log line holds one. The screen shows the last one while the call lasts.
- **Audio** of your turns is held on the Mac only until the turn is transcribed.
- **Answer audio** is deleted once the phone has played it, or after 2 minutes. A sound the phone fetched counts as played after its length plus 10 seconds, if no report comes.
- **Logs:** a call writes `app call started` and `app call ended` with a `reason`, and the turns write the desk's lines. No line holds a transcript, audio or the text of a line.
- **The chief's lines:** a spoken line played in a call is added to the thread as text, so you can read it after. It sends no push and no iMessage.

## Speech check

A throwaway page, run once on the iPhone in the installed app, that shows how Safari's speech recognition (`webkitSpeechRecognition`, as dictation and a phone call use it) behaves in a call-shaped use. It is Stage 0 of call-phone-speech, and Stage 1 removes it, with its route and its log line.

**Opening it.** Nothing links to `/speech-check`. The chief sends `/speech-check` in the thread; a tap on a link to the app's own address opens inside the installed app (`pages.js`).

**The checks.** The screen shows one step at a time and says plainly when to switch the network off and when back on. Each check records only a yes or no, a count, milliseconds or the recognizer's error name:

| # | The check | What you do | Fields (`check<n>_…`) |
|---|---|---|---|
| 0 | Recognition with the network off | Turn on airplane mode (or turn off Wi-Fi and cellular), tap Start and say "ten eleven twelve", then turn the network back on | `offline` (the phone said it was offline when the check began; a result without it says nothing about on-device recognition), `offline_at_start` (the same, read when listening starts), `started`, `results`, `first_result_ms`, `error` (such as `network`, `service-not-allowed`, `not-allowed`) |
| 1 | Recognition starts inside a tap | Tap Start and say "one two three" | `started`, `results`, `heard` (one of the words came in a result, compared on the phone), `first_result_ms`, `error` |
| 2 | Recognition starts 2.5 s after a tap, as in a call | Tap Start; the page fetches a file from Harold, waits 2.5 s, then starts recognition; say "four five six" | `started`, `results`, `heard`, `error` |
| 3 | Recognition restarts without a tap | Count aloud; the page ends recognition and starts it again 5 times, 2 s apart | `started` (how many of the 5), `heard` (how many of the 5 heard any word), `errors` (their names) |
| 4 | When results become final | Read three short sentences shown on screen, pausing 2 s after each | `results`, `heard` (how many of the 3 sentences had any word heard), `final_at_ms` (the ms from a sentence's showing to its first final result), `gap_ms` (the ms from the last interim result to that final one), each as three values with `-` where no final came; `error` |
| 5 | Listening while Harold's voice plays | Stay quiet while the page plays a 4 s sentence in Harold's voice (Kokoro, `check-sentence.wav`) through the call's own playback path | `started`, `kept_running`, `playback_ok`, `words_heard` (how many of the sentence's 9 words recognition heard, counted on the phone), `error` |
| 6 | Listening after Harold's voice | Right after check 5, say "seven eight nine" | `restarted` (recognition had stopped and the page started it again without a tap), `heard`, `error` |

**Passed.** For checks 1, 2 and 3, "passed" means started and heard: `started` alone does not pass a check. Each check closes its recognition, waits for its `onend` (at most 2 s), and waits 1 s more before the next check can start; every check makes a new recognition object.

**What it sends.** The page keeps the results in `localStorage` and posts them once, after check 6, to `POST /speech-check/runs`, again every 2 to 10 seconds until Harold answers `204`, because check 0 runs with the network off. It never keeps or sends a word you say: words are compared on the phone to produce counts and then thrown away.

| Route | What it does |
|---|---|
| `GET /speech-check` | The page. |
| `POST /speech-check/runs` | The run as the page's signals, sent through Datastar as one JSON object: `standalone` and the `check<n>_…` fields above, all optional. Only booleans, whole numbers (at most 600,000 ms, 5 restarts, 3 sentences, 20 words) and error names from the recognizer's list (plus `missing`, `start-failed` and `other`) are accepted. `204` once logged; `400` for any other field, type or name; `413` over 4 KB. Same-origin only, like every post. |

**Reading the results.** Search `harold.log` for `app speech check`. It is one line per run with every field above, for example `check0_error="network" check3_started=5 check4_final_at_ms="5100,5100,5100"`.

## Echo test

A hidden page that measures how much of Harold's voice the phone's mic picks up while the phone plays it, with the call's own mic settings (`echoCancellation: true`, one channel) and the call's own recorder and player. Its result decides whether Harold can let you cut in on a call.

**Opening it.** Nothing in the app links to `/echo-test`. The chief sends its address in the thread or on an instructions todo; a tap on a link to the app's own address opens inside the installed app (`pages.js`), where every other link opens in Safari.

**Running it.** Pick a setup (held to your ear, on the table on speaker, or headphones) and a run, then tap **Start**. A run is 1 s quiet, 15 s of Harold's voice, then 1 s quiet. Run each setup twice: once **silent** (say nothing) and once **talking** over Harold. The page shows each run's verdict and ends with a summary of every run done.

**What it sends.** One number for every 20 ms of what the mic heard (the root mean square of 320 samples, as the speech detector computes it), at most 1,000 of them, and no sound. The page says so before **Start**.

| Route | What it does |
|---|---|
| `GET /echo-test` | The page. |
| `GET /echo-test/sample` | 15 s of speech as `audio/wav`, made at the first request with macOS `say` (`--file-format=WAVE --data-format=LEI16@24000`) and kept in memory. `503` when `say` fails. |
| `POST /echo-test/runs` | One run as the page's signals, sent through Datastar as one JSON object: `setup`, `kind`, `frames`, `playing_from` and `playing_to` (frame numbers), `echo_cancellation`, `standalone`, `user_agent`. Answers `200` with Datastar events that patch `#echo-results` to the verdict's line (class `fail` for a failed run). `400` for a body over 64 KB, an unknown `setup` or `kind`, more than 1,000 frames or a playback window outside them; `500` when the file cannot be written. Same-origin only, like every post. |

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
| `app page connected` | `rows` and `bytes` of its first patch, and `reopened_after_stall=true` when the page opened the stream again because the last one stalled |
| `app page stream stalled` | WARN. `beats_behind`, how many beats the stream sent that the page did not report, and `seconds`, about how long the page had gone without one. Once per stream. Expected near 0 a day |
| `app thread older served` | `rows`, `bytes` and `ms` of one load of older messages |
| `app thread older not read` | `reason` (`store_unread`), logged as a warning; the `#older` line stays and asks again |
| `app page not connected` | `reason` (`store_unread` or `render_failed`) |
| `app page disconnected` | `reason` (`page_gone`, `shutdown` or `store_stopped`) and `seconds`, how long the page was connected, and `hidden_reported` (`true` for one `POST /page/hidden` during the stream, `false` for none, `repeated` for more than one). Harold notices a closed page at the next patch or keep-alive, within about 15 seconds |
| `app post refused` | WARN. A post from the app's own origin without `Datastar-Request: true`, on a route that needs it: `route` (the path without its query) and `reason` `not_datastar`. No body. Expected rate: 0. |
| `app reply state` | `event_id` and `state` (`typed`, `waiting_chief_busy`, `not_sent_no_chief` or `not_sent_paused`), logged before the state is written |
| `app reply stored` | `chars`, the reply's length in characters, and `photo_bytes`, the photo's size, when the reply carries one |
| `app dictation check` | `result` (`missing`, `ok`, `not_allowed`, `service_not_allowed`, `network`, `no_speech` or `audio_capture`), once per page load |
| `app speech check` | One per run of the [speech check](#speech-check): `standalone` and the `check<n>_…` fields, never a word |
| `app speech check not logged` | `reason` (`bad_request`), logged as a warning |
| `app photo stored` | `photo_bytes` and `kind` (`jpeg` or `png`) |
| `app photo not shrunk` | A warning, `photo_bytes`, logged beside `app photo stored` for an upload over 3 MB: the phone shrinks to about 0.3 to 1 MB, so it did not, for example a HEIC photo it could not decode |
| `app photo refused` | A warning with `reason` (`too_large`, `not_an_image`, `store_failed` or `unknown_photo`), and `photo_bytes` when known |
| `app photos pruned` | `kind` (`expired` for photos past their keep time, `unused` for uploads no reply named) and `count`, logged by the hourly prune when it deletes any. A row showing `photo removed` has an `expired` count at the hour it went; an upload that never arrived has no `app photo stored` line |
| `app todo answered` | `kind` (`answer`, `free_text`, `picks`, `relegate`, `tick`, `done`, `ignore`, `revise` or `reject`) and `result` (`sent`, `version_changed`, `not_waiting`, `spec_changed` or `unavailable`). The line never carries the todo's id; the question tool's own lines in `harold.log` do |
| `app todos not read` | `reason` (`unavailable` or `timeout`), logged once when reads start failing |
| `app todos read again` | none, logged once when reads recover |
| `app call started` | none |
| `app call check` | One per call, 3 seconds in, from the page's `POST /call/check`: `context_state`, `track_state`, `track_muted`, `pieces_made`, `pieces_posted`, `pieces_dropped` (`playing=n echo_tail=n before_start=n not_open=n`), `posts_failed` and `empty_blocks`. No free text. |
| `app call microphone failed` | A warning with the same fields plus `problem` (`no_sound`) or `step` (`microphone`, `context`, `worklet` or `call`) and `error_name`, the browser's error name |
| `call audio refused` | `reason` (`full`, `malformed` or `too_large`), a warning logged once per reason per call for a piece of audio the open call refused |
| `echo test run` | `setup` (`ear`, `speaker` or `headphones`), `kind` (`silent` or `talk`), `verdict` (`pass`, `fail` or `run_silent_first`), `share_over_bar` (silent runs: the share of playback frames over the bar) or `voice_found` (talk runs with a bar), `echo_cancellation` and `standalone` (yes or no) |
| `echo test run not saved` | `reason` (`write_failed` or `bad_request`), logged as a warning |
| `echo test sample not made` | `reason` (`say_failed`), logged as a warning |
| `app call ended` | `reason`, one of the desk's `call ended` reasons ([Log lines](../voice-input/README.md#log-lines)); `microphone_failed` when the phone stopped posting audio |

Six other lines report failures, and none carries content:

- `app push subscriptions not read` carries the store's error; the message is not pushed and `app message not pushed` follows with `subscriptions_unread`;
- `web app unavailable` names the listen address or the key's path, never the login or the key;
- `web app stopped` carries the server's error;
- `web app thread not pruned` and `web app photos not pruned` carry no field;
- `app reply state not recorded` carries the reply's event id and the store's error.

A request refused by the login check, or by the Host check, writes no line at all. A post from the app's own origin without `Datastar-Request: true`, on a route that needs it, writes WARN `app post refused` with `route` (the path without its query) and `reason` `not_datastar`, and no body. A post from another site writes no line.

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
