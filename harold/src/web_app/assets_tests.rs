use sha2::{Digest, Sha256};

#[test]
fn embedded_datastar_script_is_the_audited_one() {
    let digest = hex::encode(Sha256::digest(super::DATASTAR_JS));
    assert_eq!(digest, super::DATASTAR_SHA256);
}

#[test]
fn the_call_script_keeps_its_contract() {
    let script = include_str!("../../static/web_app/call.js");
    for wanted in [
        // Each piece carries the id POST /call answered.
        "/call/audio?call=",
        "&at=",
        "current.id = id",
        "application/octet-stream",
        "echoCancellation: true",
        "wakeLock",
        "/call/played?id=",
        "start_ms=",
        "/call/talk-now",
        // Every call post but the audio goes through Datastar, once, and stays open as the
        // page hides.
        "actions[options.action || \"post\"]",
        "openWhenHidden: true",
        "retry: \"never\"",
        "retryMaxCount: 0",
        r#"send("DELETE", "/call").catch"#,
        "Call ended: the app was closed or the phone locked",
        "prefers-reduced-motion",
        r#"audioWorklet.addModule("/static/call-worklet.js")"#,
        // A piece Harold refuses with no call open ends the call on the phone too.
        "answer.status === 409",
        // Pieces are posted one after another, in the order they were recorded.
        "current.posts = current.posts.then(",
        // The context is resumed after the microphone starts, and on each state change.
        "await ctx.resume()",
        "ctx.onstatechange",
        // The check is posted 3 s after the call opens; no piece made ends the call.
        "const CHECK_MS = 3000;",
        "setTimeout(() => check(current), CHECK_MS)",
        "/call/check",
        r#"problem: "no_sound""#,
        "/call?why=no_sound",
        "The microphone gave no sound. Tap Call to try again.",
        // A thrown error is reported with the step and the error's name.
        "step,",
        "error_name: name",
        "processorOptions: { report: true }",
        // A call that ended for a missing speech model shows why, until the next Call or End.
        r#"getAttribute("data-harold-notice")"#,
        "showNotice(why)",
        r#""data-harold-notice","#,
    ] {
        assert!(script.contains(wanted), "call.js lacks {wanted}");
    }
    for absent in ["eval(", "new Function", "innerHTML", "http://", "https://"] {
        assert!(!script.contains(absent), "call.js holds {absent}");
    }
}

#[test]
fn the_call_script_checks_for_an_open_call_before_it_records_what_the_screen_names() {
    let script = include_str!("../../static/web_app/call.js");
    let sync = &script[script.find("function sync() {").expect("call.js has sync")..];
    let open_check = sync
        .find("if (!current || !current.open) {")
        .expect("sync checks for an open call");
    let recorded = sync
        .find("seen.sound = sound;")
        .expect("sync records the sound");
    assert!(
        open_check < recorded,
        "sync records the sound before it checks that the call is open"
    );
}

#[test]
fn the_worklet_posts_its_empty_block_count_once_a_second_when_asked() {
    let worklet = include_str!("../../static/web_app/call-worklet.js");
    for wanted in [
        "this.port.postMessage({ empty: this.emptyBlocks });",
        "const REPORT_EVERY_S = 1;",
        "this.emptyBlocks += 1;",
        "options.processorOptions.report",
    ] {
        assert!(worklet.contains(wanted), "call-worklet.js lacks {wanted}");
    }
}

#[test]
fn the_responding_dots_pulse_unless_motion_is_reduced() {
    let css = include_str!("../../static/web_app/app.css");
    for wanted in [
        "@keyframes dot-rise",
        "animation: dot-rise 1.2s ease-in-out infinite both;",
        ".dots i:nth-child(2) {\n  animation-delay: 0.2s;",
        ".dots i:nth-child(3) {\n  animation-delay: 0.4s;",
        "@media (prefers-reduced-motion: reduce) {\n  .dots i {\n    animation: none;",
    ] {
        assert!(css.contains(wanted), "app.css lacks {wanted}");
    }
}

#[test]
fn revise_and_reject_read_as_plain_words_beside_send() {
    let css = include_str!("../../static/web_app/app.css");
    let start = css
        .find(".card-more button {")
        .expect("app.css lacks the .card-more button rule");
    let rule = &css[start..start + css[start..].find('}').expect("rule is closed")];
    for wanted in [
        "background: transparent;",
        "border: 0;",
        "color: var(--text-2);",
    ] {
        assert!(rule.contains(wanted), ".card-more button lacks {wanted}");
    }
    // The tap height stays the shared 48px.
    assert!(
        !rule.contains("min-height"),
        ".card-more button sets min-height"
    );
}

#[test]
fn pages_js_keeps_older_rows_out_of_the_count_and_keeps_the_place() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        // A record that adds message rows ahead of a message row is older rows.
        "function olderRecord(record)",
        "record.nextSibling",
        r#"classList.contains("message")"#,
        // The place is kept by the height the older rows add, before the next paint.
        "thread.scrollHeight - previousHeight",
        "thread.scrollTop += added",
        // Only a row at the end is counted and followed.
        "unseen += 1",
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
    // An older record is set aside before any row is counted or followed.
    let observer = &script[script
        .find("new MutationObserver((records)")
        .expect("observer")..];
    let older = observer
        .find("olderRecord(record)")
        .expect("the observer checks it");
    assert!(
        older < observer.find("onNewRow").unwrap(),
        "the check comes first"
    );
    assert!(observer.contains("keepPlace()"));
}

#[test]
fn the_older_line_is_centred_in_the_second_text_colour() {
    let css = include_str!("../../static/web_app/app.css");
    let start = css.find(".older {").expect("app.css lacks the .older rule");
    let rule = &css[start..start + css[start..].find('}').expect("rule is closed")];
    for wanted in [
        "text-align: center;",
        "color: var(--text-2);",
        "font-size: 14px;",
    ] {
        assert!(rule.contains(wanted), ".older lacks {wanted}");
    }
}

#[test]
fn the_status_strip_is_blurred_and_the_header_row_and_notice_are_solid() {
    let css = include_str!("../../static/web_app/app.css");
    let rule = |selector: &str| {
        let start = css
            .find(&format!("\n{selector} {{"))
            .unwrap_or_else(|| panic!("app.css lacks the {selector} rule"));
        let body = &css[start..];
        &body[..body.find('}').expect("rule is closed")]
    };
    let top = rule(".top");
    for wanted in [
        "position: fixed;",
        "\n  backdrop-filter:",
        "\n  -webkit-backdrop-filter:",
        "background: linear-gradient(",
        "color-mix(in srgb, var(--surface-1) 72%, transparent) 0 var(--status-glass),",
        "var(--surface-1) var(--status-glass)",
    ] {
        assert!(top.contains(wanted), ".top lacks {wanted:?}");
    }
    let root = rule(":root");
    assert!(
        root.contains("--status-strip: max(8px, calc(env(safe-area-inset-top) - 12px));"),
        ":root lacks the --status-strip that starts under the phone's icons"
    );
    assert!(
        root.contains("--status-glass: max(0px, calc(env(safe-area-inset-top) - 12px));"),
        ":root lacks the --status-glass, the see-through height"
    );
    assert!(
        root.contains("--header-height: calc(var(--status-strip)"),
        "--header-height does not use --status-strip"
    );
    let notice = rule(".call-notice");
    for wanted in [
        "position: fixed;",
        "top: var(--header-height);",
        "background: var(--surface-3);",
    ] {
        assert!(notice.contains(wanted), ".call-notice lacks {wanted:?}");
    }
    assert!(
        !notice.contains("backdrop-filter"),
        ".call-notice must not blur"
    );
    for selector in [".page", "#todos"] {
        assert!(
            rule(selector).contains("var(--notice-height, 0px)"),
            "{selector} lacks the notice height"
        );
    }
    let call = include_str!("../../static/web_app/call.js");
    for wanted in ["ResizeObserver", "--notice-height"] {
        assert!(call.contains(wanted), "call.js lacks {wanted:?}");
    }
}

#[test]
fn the_echo_test_script_asks_for_the_calls_microphone_and_plays_the_way_the_call_does() {
    let call = include_str!("../../static/web_app/call.js");
    let script = include_str!("../../static/web_app/echo-test.js");
    // The same constraints, written the same way, in both.
    let constraints = "audio: { echoCancellation: true, channelCount: 1 }";
    assert!(
        call.contains(constraints),
        "call.js changed its constraints"
    );
    for wanted in [
        constraints,
        r#"audioWorklet.addModule("/static/call-worklet.js")"#,
        "decodeAudioData",
        "createBufferSource",
        "createAnalyser",
        "/echo-test/sample",
        "/echo-test/runs",
        "getSettings()",
        "display-mode: standalone",
        "actions[options.action || \"post\"]",
        "retry: \"never\"",
        "retryMaxCount: 0",
        "harold-call-recorder",
    ] {
        assert!(script.contains(wanted), "echo-test.js lacks {wanted}");
    }
    for absent in [
        "eval(",
        "new Function",
        "innerHTML",
        "http://",
        "https://",
        "MediaRecorder",
    ] {
        assert!(!script.contains(absent), "echo-test.js holds {absent}");
    }
}

#[test]
fn the_speech_check_script_posts_only_the_run_and_never_keeps_what_was_said() {
    let script = include_str!("../../static/web_app/speech-check.js");
    for wanted in [
        "webkitSpeechRecognition",
        "continuous = true",
        "interimResults = true",
        "navigator.language",
        "decodeAudioData",
        "createBufferSource",
        "createAnalyser",
        "/static/check-sentence.wav",
        "/speech-check/runs",
        "actions[options.action || \"post\"]",
        "retry: \"never\"",
        "retryMaxCount: 0",
        "display-mode: standalone",
        "localStorage",
        "online",
        "offline",
    ] {
        assert!(script.contains(wanted), "speech-check.js lacks {wanted}");
    }
    // Words are read from a result only to be compared; the one post is the run object,
    // sent through Datastar, so `fetch(` is only the sentence, twice.
    assert_eq!(script.matches("fetch(").count(), 2, "the sentence twice");
    assert!(script.contains("post(statusLine, RUNS_URL, run, RUN_SIGNALS)"));
    for absent in [
        "transcript:",
        ".transcript =",
        "run.transcript",
        "run.words",
        "eval(",
        "new Function",
        "innerHTML",
        "http://",
        "https://",
        "getUserMedia",
        "MediaRecorder",
    ] {
        assert!(!script.contains(absent), "speech-check.js holds {absent}");
    }
}

#[test]
fn datastar_posts_a_run_with_null_check_4_times_as_harold_reads_it() {
    let script = include_str!("../../static/web_app/speech-check.js");
    let signals = script
        .lines()
        .find_map(|line| line.trim().strip_prefix("const RUN_SIGNALS = "))
        .expect("speech-check.js lacks RUN_SIGNALS")
        .trim_end_matches(';')
        .to_string();
    let directory = std::env::temp_dir().join(format!(
        "harold-speech-check-datastar-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    // The page's own Datastar module, run with a stub document and a fetch that keeps the body.
    let module = directory.join("datastar.mjs");
    std::fs::write(
        &module,
        include_str!("../../static/web_app/datastar-1.0.2.js"),
    )
    .unwrap();
    let program = format!(
        r#"
globalThis.document = {{
  addEventListener() {{}}, removeEventListener() {{}}, dispatchEvent() {{ return true; }},
  createElement() {{ return {{}}; }}, querySelector() {{ return null; }},
  querySelectorAll() {{ return []; }}, getElementById() {{ return null; }},
  documentElement: {{}}, body: {{}}, head: {{}}, readyState: "complete",
  baseURI: "http://harold.test/speech-check",
}};
globalThis.window = globalThis;
globalThis.location = {{ href: "http://harold.test/speech-check", origin: "http://harold.test" }};
globalThis.MutationObserver = class {{ observe() {{}} disconnect() {{}} }};
let body;
globalThis.fetch = async (url, init) => {{ body = init.body; return new Response(null, {{ status: 204 }}); }};
const RUN_SIGNALS = {signals};
const {{ actions, mergePatch }} = await import({module:?});
mergePatch({{
  standalone: true, check0_offline: false, check0_error: "network",
  check3_started: 5, check3_errors: ["network"],
  check4_results: true, check4_heard: 2,
  check4_final_at_ms: [1500, null, 2100], check4_gap_ms: [400, null, 380],
  check6_heard: true, notes: "not a run signal",
}});
const el = {{ id: "status", isConnected: true, closest() {{ return null; }} }};
await actions.post(
  {{ el, evt: undefined, error: (name) => new Error(name), cleanups: new Map() }},
  "/speech-check/runs",
  {{ filterSignals: {{ include: RUN_SIGNALS }}, openWhenHidden: true, requestCancellation: "disabled", retry: "never", retryMaxCount: 0 }},
);
process.stdout.write(body);
process.exit(0);
"#,
        module = format!("file://{}", module.display()),
    );
    let output = std::process::Command::new("node")
        .args(["--input-type=module", "-e", &program])
        .output()
        .expect("the test needs `node` on the PATH, and it is missing");
    std::fs::remove_dir_all(&directory).unwrap();
    assert!(
        output.status.success(),
        "node failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let run = super::speech_check::parse_run(&output.stdout).unwrap_or_else(|refused| {
        panic!(
            "{refused:?} for {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(run.check4_final_at_ms, [Some(1500), None, Some(2100)]);
    assert_eq!(run.check4_gap_ms, [Some(400), None, Some(380)]);
    assert!(run.standalone && run.check6_heard);
}

#[test]
fn the_speech_check_counts_digits_as_heard_words() {
    let script = include_str!("../../static/web_app/speech-check.js");
    let code = script
        .split("// words-start")
        .nth(1)
        .and_then(|rest| rest.split("// words-end").next())
        .expect("speech-check.js lacks the words-start and words-end markers");
    let list = |name: &str| {
        let prefix = format!("const {name} = ");
        script
            .lines()
            .find_map(|line| line.trim().strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("speech-check.js lacks {name}"))
            .trim_end_matches(';')
            .to_string()
    };
    let program = format!(
        r#"
const CHECK1_WORDS = {};
const CHECK2_WORDS = {};
const NUMBER_WORDS = {};
{code}
const event = (transcript) => ({{ results: [[{{ transcript }}]] }});
const heard = (transcript, expected) => hasExpected(event(transcript), 0, expected);
const cases = [
  [CHECK1_WORDS, ["123", "1 2 3", "one two three", "One, two, three."], true],
  [CHECK2_WORDS, ["456", "4 5 6", "four five six", "Four, five, six."], true],
  [NUMBER_WORDS, ["789", "7 8 9", "seven eight nine", "Seven, eight, nine."], true],
  [CHECK1_WORDS, ["Hello there"], false],
];
const wrong = [];
for (const [expected, transcripts, want] of cases) {{
  for (const transcript of transcripts) {{
    if (heard(transcript, expected) !== want) wrong.push(transcript);
  }}
}}
if (wrong.length > 0) {{
  console.log("wrong: " + wrong.join(" | "));
  process.exit(1);
}}
"#,
        list("CHECK1_WORDS"),
        list("CHECK2_WORDS"),
        list("NUMBER_WORDS"),
    );
    let output = std::process::Command::new("node")
        .args(["-e", &program])
        .output()
        .expect("the test needs `node` on the PATH, and it is missing");
    assert!(
        output.status.success(),
        "node failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_reply_bar_follows_the_visual_viewport_when_the_keyboard_opens() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in ["visualViewport", "--viewport-height", "keyboard-open"] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
    let css = include_str!("../../static/web_app/app.css");
    assert!(
        css.contains("height: var(--viewport-height, 100%)"),
        "app.css lacks the body height from --viewport-height"
    );
    assert!(
        css.contains("body.keyboard-open .bottom"),
        "app.css lacks the bottom padding with the keyboard open"
    );
}

#[test]
fn pages_js_keeps_a_same_origin_link_in_the_app() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        r#"document.addEventListener("click""#,
        r#"closest("a[href]")"#,
        "link.origin === location.origin",
        "location.assign(link.href)",
        "event.preventDefault()",
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
    // The rule sits ahead of the thread's own setup, so any page that loads the script has it.
    assert!(
        script.find("link.origin === location.origin").unwrap()
            < script.find(r#"document.getElementById("pages")"#).unwrap()
    );
}

#[test]
fn pages_js_reopens_updates_only_when_the_stream_really_ended() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        r#"document.addEventListener("datastar-fetch""#,
        r#"detail.el.id !== "messages""#,
        r#"detail.type === "started""#,
        r#"detail.type === "finished""#,
        "setTimeout(",
        "harold-reopen",
        "REOPEN_AFTER_MS = 1000",
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
}

#[test]
fn pages_js_reports_hiding_with_a_request_that_outlives_the_page() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        r#"report("/page/hidden", {}, /^none$/)"#,
        // Through Datastar, open while the page hides, and sent once.
        "actions[options.action || \"post\"]",
        "openWhenHidden: true",
        "retry: \"never\"",
        "retryMaxCount: 0",
        r#"document.addEventListener("visibilitychange""#,
        r#"document.visibilityState === "hidden""#,
        r#"window.addEventListener("pagehide", reportHidden)"#,
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
}

#[test]
fn pages_js_beats_seen_while_the_page_is_visible() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        r#"report("/page/seen", { beat: lastBeat }, /^beat$/)"#,
        "PAGE_SEEN_EVERY = 30000",
        "setInterval(reportSeen, PAGE_SEEN_EVERY)",
        "clearInterval(",
        r#"document.visibilityState === "visible""#,
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
}

#[test]
fn pages_js_reopens_a_stream_that_has_had_no_beat_for_45_seconds_while_visible() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        "STALL_AFTER_MS = 45000",
        "STALL_CHECK_EVERY = 5000",
        "setInterval(checkStream, STALL_CHECK_EVERY)",
        "Date.now() - lastActivity < STALL_AFTER_MS",
        "STALL_GRACE_MS = 1000",
        r#"new CustomEvent("harold-reopen")"#,
        r#"setInput(stalledInput, "1")"#,
        "{ beat: lastBeat }",
        r#"document.getElementById("beat")"#,
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
}

#[test]
fn the_thread_binds_the_beat_and_the_reopen_sends_the_stalled_signal() {
    let thread = include_str!("../../templates/web_app/thread.html");
    assert!(thread.contains(r#"<input type="hidden" id="beat" data-bind:_beat>"#));
    assert!(thread.contains(r#"<input type="hidden" id="stalled" data-bind:stalled>"#));
    let messages = include_str!("../../templates/web_app/messages.html");
    assert!(messages.contains(
        "data-on:harold-reopen=\"@get('/updates', {retry: 'error', retryMaxCount: 1000, filterSignals: {include: /^(oldest|stalled)$/}})\""
    ));
}

#[test]
fn pages_js_copies_a_todo_note_before_any_element_listener_can_redraw_it() {
    let script = include_str!("../../static/web_app/pages.js");
    // The copy of the card is taken in the capture phase: a bubbling listener runs after the
    // textarea's own handler, whose redraw makes restore() write the old note back.
    for wanted in [
        r#"document.addEventListener("input", noteEdit, true);"#,
        r#"document.addEventListener("change", noteEdit, true);"#,
        r#"document.addEventListener("focusin", noteEdit, true);"#,
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
}

#[test]
fn photo_js_shrinks_to_2048_as_jpeg_uploads_before_send_and_clears_with_the_x() {
    let script = include_str!("../../static/web_app/photo.js");
    for wanted in [
        // The photo is drawn to a canvas no longer than 2,048 px, and sent as JPEG at 0.85.
        "const LONGEST_SIDE = 2048;",
        "const QUALITY = 0.85;",
        "Math.min(1, LONGEST_SIDE / longest)",
        // Decoded without a blob: address, which `img-src 'self'` would refuse.
        "createImageBitmap(file",
        r#"canvas.toBlob(resolve, "image/jpeg", QUALITY)"#,
        // It uploads at once, through Datastar, as the base64 `_photo` signal that only this
        // post sends and the script clears; Harold patches the `photo` signal to the id.
        r#"post(button, "/replies/photo", { _photo: photo }, /^_photo$/"#,
        "filterSignals: { include: /^_photo$/, exclude: /^$/ }",
        r#"mergePatch({ _photo: "" })"#,
        r#"getPath("photo")"#,
        // The id goes to the hidden input bound to the `photo` signal, with its input event.
        r#"idInput.dispatchEvent(new Event("input", { bubbles: true }))"#,
        // Send is disabled while the upload runs, and a submit then is stopped.
        "send.disabled = on;",
        "event.stopImmediatePropagation();",
        // The X clears the id and the strip.
        r#"remove.addEventListener("click", clear);"#,
        "setId(\"\");",
        // A failed upload says so in the strip for 3 s.
        "The photo didn't reach Harold.",
        "const FAILED_FOR_MS = 3000;",
    ] {
        assert!(script.contains(wanted), "photo.js lacks {wanted}");
    }
    for absent in [
        "eval(",
        "new Function",
        "innerHTML",
        "http://",
        "https://",
        "createObjectURL",
    ] {
        assert!(!script.contains(absent), "photo.js holds {absent}");
    }
}

#[test]
fn the_photo_rules_are_in_the_stylesheet() {
    let css = include_str!("../../static/web_app/app.css");
    for wanted in [
        ".icon-button {",
        ".photo-button {",
        ".quiet-button {",
        ".photo-chosen {",
        ".photo-chosen[hidden] {\n  display: none;",
        ".message .photo img {",
        "max-width: min(220px, 100%);",
    ] {
        assert!(css.contains(wanted), "app.css lacks {wanted}");
    }
}

#[test]
fn dictation_js_listens_into_the_box_and_tells_harold_once() {
    let script = include_str!("../../static/web_app/dictation.js");
    for wanted in [
        // The mic hides only when neither API exists; an old remembered hide is cleared.
        "window.SpeechRecognition || window.webkitSpeechRecognition",
        r#"report("missing");"#,
        r#"const STORAGE_KEY = "harold-dictation";"#,
        "localStorage.removeItem(STORAGE_KEY)",
        // The recognition starts with the phone's language, interim results and no end.
        "r.lang = navigator.language;",
        "r.interimResults = true;",
        "r.continuous = true;",
        // The box is the reply textarea.
        r#"form.querySelector("textarea")"#,
        // Each result goes into the box and fires its input event, grey until it ends.
        "box.value = words;",
        r#"box.dispatchEvent(new Event("input", { bubbles: true }))"#,
        r#"box.classList.toggle("interim", interim)"#,
        r#"box.classList.remove("interim")"#,
        // It stops by itself after 4 s without a result.
        "const QUIET_MS = 4000;",
        "quietTimer = setTimeout(",
        "r.onstart = () => {",
        "r.stop();",
        // It aborts on hiding, on a call, and on a submit.
        r#"document.visibilityState === "hidden""#,
        "r.abort();",
        r#"call.addEventListener("click", abort);"#,
        r#"form.addEventListener("submit", abort);"#,
        // The errors, and what the strip says for them.
        r#""not-allowed": "not_allowed""#,
        r#""service-not-allowed": "service_not_allowed""#,
        r#""audio-capture": "audio_capture""#,
        "Dictation isn't available in this app.",
        "Dictation needs a connection.",
        "The microphone gave no sound.",
        "const NOTICE_MS = 3000;",
        // One check per page load, through Datastar, to the one route, open while the page
        // hides.
        r#"post(null, "/dictation/check", { result }, /^result$/"#,
        "openWhenHidden: true",
        "retryMaxCount: 0",
        "if (reported) {",
        r#"report("ok");"#,
        // The signals are set through hidden inputs Datastar binds.
        r#"input.dispatchEvent(new Event("input", { bubbles: true }))"#,
    ] {
        assert!(script.contains(wanted), "dictation.js lacks {wanted}");
    }
    for absent in [
        "eval(",
        "new Function",
        "innerHTML",
        "http://",
        "https://",
        "getUserMedia",
        "MediaRecorder",
        "transcript:",
        // A refusal never hides the mic or is remembered.
        "REMEMBER_MS",
        "failedRecently",
        "setFlag(onInput, false)",
    ] {
        assert!(!script.contains(absent), "dictation.js holds {absent}");
    }
    // The words are never sent: the only post is the check, with the result word alone.
    assert_eq!(
        script.matches("post(").count(),
        2,
        "the helper and its one use"
    );
    assert!(!script.contains("fetch("));
}

#[test]
fn the_reply_box_is_3_lines_tall_and_scrolls_inside() {
    let css = include_str!("../../static/web_app/app.css");
    let rule = &css[css.find(".bottom textarea {").unwrap()..];
    let rule = &rule[..rule.find('}').unwrap()];
    assert!(rule.contains("height: calc(3 * 1.4em + 24px);"), "{rule}");
    assert!(!rule.contains("max-height"), "{rule}");
    assert!(rule.contains("overflow-y: auto;"), "{rule}");
    let page = include_str!("../../templates/web_app/thread.html");
    let tag = &page[page.find("<textarea").unwrap()..];
    let tag = &tag[..tag.find('>').unwrap()];
    assert!(tag.contains(r#"rows="3""#), "{tag}");
    assert!(!tag.contains("style.height"), "{tag}");
    assert!(
        tag.contains(r#"data-effect="$reply; if (el.selectionEnd === el.value.length) el.scrollTop = el.scrollHeight""#),
        "the scroll-only effect keeps the end of a dictated text in view: {tag}"
    );
}

#[test]
fn the_dictation_rules_are_in_the_stylesheet() {
    let css = include_str!("../../static/web_app/app.css");
    for wanted in [
        ".bottom textarea.interim {\n  color: var(--text-2);\n  border-color: var(--brand);",
        ".listening-note {",
        ".listening-note[hidden] {\n  display: none;",
        ".listening-note + .bottom {\n  border-top: 0;",
        ".listening-note .listen-dot {",
        "background: var(--end);",
    ] {
        assert!(css.contains(wanted), "app.css lacks {wanted}");
    }
}

#[test]
fn the_otel_bundle_is_named_after_its_bytes_and_built_from_the_locked_packages() {
    let hash = hex::encode(Sha256::digest(super::pages::OTEL_JS));
    assert_eq!(
        super::pages::OTEL_JS_NAME,
        format!("otel-2.12.0-{}.js", &hash[..8])
    );
    let bundle = std::str::from_utf8(super::pages::OTEL_JS).unwrap();
    assert!(
        bundle.starts_with("/* OpenTelemetry JS for the Harold web app"),
        "the bundle lacks its header"
    );
    let lock = include_bytes!("../../../web-otel/package-lock.json");
    let lock_sha = hex::encode(Sha256::digest(lock));
    assert!(
        bundle.contains(&format!("package-lock.json sha256 {lock_sha}")),
        "the lockfile changed since the bundle was built: run scripts/build_web_otel.sh"
    );
    for hook in ["require-in-the-middle", "import-in-the-middle"] {
        assert!(!bundle.contains(hook), "the bundle holds {hook}");
    }
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/web_app");
    let bundles: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("otel-") && name.ends_with(".js"))
        .collect();
    assert_eq!(bundles, [super::pages::OTEL_JS_NAME]);
}

#[test]
fn the_mermaid_bundle_is_named_after_its_bytes_and_built_from_the_locked_packages() {
    let hash = hex::encode(Sha256::digest(super::pages::MERMAID_JS));
    let package = include_str!("../../../web-mermaid/package.json");
    assert!(
        package.contains(r#""mermaid": "12.1.0""#),
        "web-mermaid/package.json does not pin mermaid 12.1.0"
    );
    assert_eq!(
        super::pages::MERMAID_JS_NAME,
        format!("mermaid-12.1.0-{}.js", &hash[..8])
    );
    let bundle = std::str::from_utf8(super::pages::MERMAID_JS).unwrap();
    assert!(
        bundle.starts_with("/* Mermaid for the Harold spec pages"),
        "the bundle lacks its header"
    );
    let lock = include_bytes!("../../../web-mermaid/package-lock.json");
    let lock_sha = hex::encode(Sha256::digest(lock));
    assert!(
        bundle.contains(&format!("package-lock.json sha256 {lock_sha}")),
        "the lockfile changed since the bundle was built: run scripts/build_web_mermaid.sh"
    );
    for module in ["child_process", "node:fs"] {
        assert!(!bundle.contains(module), "the bundle holds {module}");
    }
    assert!(
        !bundle.contains("org.eclipse.elk"),
        "the bundle holds elkjs code"
    );
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/web_app");
    let bundles: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("mermaid-") && name.ends_with(".js"))
        .collect();
    assert_eq!(bundles, [super::pages::MERMAID_JS_NAME]);
}

#[test]
fn the_specs_script_imports_the_mermaid_bundle_by_its_name() {
    let script = include_str!("../../static/web_app/specs.js");
    assert!(
        script.contains(&format!(
            r#"from "/static/{}""#,
            super::pages::MERMAID_JS_NAME
        )),
        "specs.js does not import the bundle by its name"
    );
    assert!(
        script.contains(r#"layout: "dagre""#),
        "specs.js does not start Mermaid with the dagre layout, and the bundle has no ELK"
    );
}

#[test]
fn the_telemetry_script_imports_the_bundle_skips_five_paths_and_cuts_urls_to_paths() {
    let script = include_str!("../../static/web_app/telemetry.js");
    assert!(
        script.contains(&format!(r#"from "/static/{}""#, super::pages::OTEL_JS_NAME)),
        "telemetry.js does not import the bundle by its name"
    );
    for path in [
        r"\/updates",
        r"\/call\/audio",
        r"\/page\/seen",
        r"\/page\/hidden",
        r"\/telemetry\/traces",
    ] {
        assert!(script.contains(path), "telemetry.js does not skip {path}");
    }
    for wanted in [
        "new URL(address, location.href).pathname",
        r#""harold-version""#,
        r#""browser.error""#,
        r#""browser.message_shown""#,
        r#"getAttribute("data-trace")"#,
        "requestAnimationFrame",
        "new BatchSpanProcessor({",
        "scheduledDelayMillis: SEND_EVERY_MS",
    ] {
        assert!(script.contains(wanted), "telemetry.js lacks {wanted}");
    }
    // The processor sends its batch when the page is hidden, unless told not to.
    for absent in [
        "eval(",
        "new Function",
        "innerHTML",
        "http://",
        "https://",
        "disableAutoFlushOnDocumentHide",
    ] {
        assert!(!script.contains(absent), "telemetry.js holds {absent}");
    }
}

#[test]
fn the_thread_page_loads_the_telemetry_script_before_datastar_and_the_deferred_scripts() {
    let page = include_str!("../../templates/web_app/thread.html");
    let at = |needle: &str| {
        page.find(needle)
            .unwrap_or_else(|| panic!("thread.html lacks {needle}"))
    };
    let telemetry = at(r#"<script type="module" src="/static/telemetry.js">"#);
    for later in [
        "/static/datastar-1.0.2.js",
        "/static/alerts.js",
        "/static/call.js",
        "/static/photo.js",
        "/static/dictation.js",
        "/static/pages.js",
    ] {
        assert!(telemetry < at(later), "telemetry.js loads after {later}");
    }
}

#[test]
fn pages_js_opens_the_thread_at_the_read_mark() {
    let script = include_str!("../../static/web_app/pages.js");
    for wanted in [
        r#""harold-read-mark""#,
        "new IntersectionObserver",
        "function openAtReadMark()",
        // The thread closure has its own listener for the page coming back.
        "function openWhenPatched()",
        r#"document.addEventListener("datastar-signal-patch", open)"#,
        "harold:thread-opened",
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
    // The load places the thread at the read mark, not with a bare scrollToBottom().
    let end = script
        .rfind("openAtReadMark();")
        .expect("the load opens at the read mark");
    assert!(
        !script[end..].contains("scrollToBottom();"),
        "the load ends with scrollToBottom()"
    );
    // Storage may be refused: the read and the write each sit in a try.
    let read = &script[script.find("function readMark()").unwrap()..];
    assert!(read.starts_with("function readMark() {\n    try {\n      return localStorage"));
    assert!(script.contains("try {\n      localStorage.setItem(READ_MARK"));
}

/// The calls in `source` that send a post or a delete without Datastar: a `fetch(` whose
/// method is not the literal `GET`, `navigator.sendBeacon(`, or an `XMLHttpRequest`. The
/// call page's raw audio post in `call.js` is the one approved exception: it is the `fetch`
/// that sends `application/octet-stream`.
fn unapproved_posts(file: &str, source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for at in source.match_indices("fetch(").map(|(at, _)| at) {
        // `datastar-fetch` names an event, and `x.fetch(` is some other object's call,
        // except on the global object.
        let head = &source[..at];
        let global = ["window.", "self.", "globalThis."]
            .iter()
            .any(|prefix| head.ends_with(prefix));
        let before = head.chars().next_back();
        let other = before.is_some_and(|c| c == '-' || c.is_alphanumeric() || c == '_')
            || (before == Some('.') && !global);
        if other {
            continue;
        }
        let arguments = call_arguments(&source[at + "fetch(".len()..]);
        // A second argument that is not an object literal hides its method.
        if second_argument(arguments).is_some_and(|second| !second.starts_with('{')) {
            found.push(format!("{file}: fetch({arguments}"));
            continue;
        }
        let Some(method) = arguments.find("method") else {
            continue;
        };
        let after = arguments[method + "method".len()..].trim_start();
        let literal_get = after
            .strip_prefix(':')
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('"'))
            .and_then(|rest| rest.split('"').next())
            .is_some_and(|word| word.eq_ignore_ascii_case("GET"));
        let audio = file == "call.js"
            && arguments.contains(r#""Content-Type": "application/octet-stream""#);
        if !literal_get && !audio {
            found.push(format!("{file}: fetch({arguments}"));
        }
    }
    for other in ["sendBeacon(", "XMLHttpRequest"] {
        if source.contains(other) {
            found.push(format!("{file}: {other}"));
        }
    }
    found
}

/// The text after the first top-level comma of `arguments`, trimmed; none for one argument.
fn second_argument(arguments: &str) -> Option<&str> {
    let mut depth = 0;
    for (at, c) in arguments.char_indices() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                let rest = arguments[at + 1..].trim();
                return (!rest.is_empty()).then_some(rest);
            }
            _ => {}
        }
    }
    None
}

/// The text of a call's arguments, from just after its `(` up to the matching `)`.
fn call_arguments(rest: &str) -> &str {
    let mut depth = 1;
    for (at, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..at];
                }
            }
            _ => {}
        }
    }
    rest
}

/// The page scripts: every `.js` in `static/web_app/` except the vendored bundles, by name
/// and source.
fn page_scripts() -> Vec<(String, String)> {
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/web_app");
    let mut scripts: Vec<_> = std::fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "js"))
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            let vendored = ["datastar-", "otel-", "mermaid-"]
                .iter()
                .any(|prefix| name.starts_with(prefix));
            (!vendored).then(|| (name, std::fs::read_to_string(&path).unwrap()))
        })
        .collect();
    scripts.sort();
    scripts
}

#[test]
fn no_page_script_posts_or_deletes_without_datastar_except_the_call_audio() {
    let scripts = page_scripts();
    // The scan reads the scripts it should: the call script and the pair script are there.
    for wanted in ["call.js", "pair.js", "pages.js", "photo.js", "telemetry.js"] {
        assert!(scripts.iter().any(|(name, _)| name == wanted), "{wanted}");
    }
    let found: Vec<_> = scripts
        .iter()
        .flat_map(|(name, source)| unapproved_posts(name, source))
        .collect();
    assert!(found.is_empty(), "posts outside Datastar: {found:#?}");
    // The one exception is still there, and is still a post.
    let call = &scripts
        .iter()
        .find(|(name, _)| name == "call.js")
        .unwrap()
        .1;
    assert!(call.contains(r#"fetch(path, {"#) && call.contains("/call/audio?call="));
}

#[test]
fn the_guard_fails_on_a_fetch_post_and_passes_on_the_call_audio_and_on_reads() {
    let post = r#"fetch("/x", { method: "POST", body: "{}" });"#;
    let delete = r#"fetch(path, { method: "DELETE", keepalive: true })"#;
    let shorthand = r#"fetch(path, { method, headers })"#;
    let read = r#"fetch(SENTENCE_URL, { cache: "no-store" }); fetch("/a", { method: "GET" });"#;
    let audio = r#"await fetch(path, {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: piece.samples,
      });"#;
    for source in [post, delete, shorthand] {
        for file in ["pair.js", "call.js", "any.js"] {
            assert_eq!(unapproved_posts(file, source).len(), 1, "{file} {source}");
        }
    }
    assert!(unapproved_posts("echo-test.js", read).is_empty());
    assert!(unapproved_posts("call.js", audio).is_empty());
    // The exception is the call page's, and only the audio post.
    assert_eq!(unapproved_posts("pair.js", audio).len(), 1);
    assert_eq!(
        unapproved_posts("call.js", &format!("{audio}{post}")).len(),
        1
    );
    for beacon in ["navigator.sendBeacon(url)", "new XMLHttpRequest()"] {
        assert_eq!(unapproved_posts("any.js", beacon).len(), 1, "{beacon}");
    }
    // The global object's `fetch` is the same call, and options in a variable hide a method.
    for sneaky in [
        r#"window.fetch("/x", { method: "POST" })"#,
        r#"self.fetch("/x", { method: "POST" })"#,
        r#"globalThis.fetch("/x", { method: "DELETE" })"#,
        "fetch(url, options)",
        "fetch(path, init())",
    ] {
        assert_eq!(unapproved_posts("any.js", sneaky).len(), 1, "{sneaky}");
    }
    assert!(
        unapproved_posts(
            "any.js",
            r#"fetch("/a"); fetch("/a", { cache: "no-store" })"#
        )
        .is_empty()
    );
    // An event name and a method of another object are no `fetch` call.
    assert!(
        unapproved_posts(
            "any.js",
            r#"x.fetch("/a", {method: "POST"}); "datastar-fetch(""#
        )
        .is_empty()
    );
}

#[test]
fn every_home_screen_page_runs_under_the_status_bar_and_the_light_strip_is_tinted() {
    let pages = [
        (
            "thread",
            include_str!("../../templates/web_app/thread.html"),
        ),
        ("notes", include_str!("../../templates/web_app/notes.html")),
        ("note", include_str!("../../templates/web_app/note.html")),
        (
            "specs_list",
            include_str!("../../templates/web_app/specs_list.html"),
        ),
        ("spec", include_str!("../../templates/web_app/spec.html")),
        (
            "spec_gone",
            include_str!("../../templates/web_app/spec_gone.html"),
        ),
        ("pair", include_str!("../../templates/web_app/pair.html")),
        (
            "echo_test",
            include_str!("../../templates/web_app/echo_test.html"),
        ),
        (
            "speech_check",
            include_str!("../../templates/web_app/speech_check.html"),
        ),
    ];
    for (name, page) in pages {
        assert!(
            page.contains(r#"<meta name="apple-mobile-web-app-capable" content="yes">"#),
            "{name}.html lacks the capable meta"
        );
        assert!(
            page.contains(
                r#"<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">"#
            ),
            "{name}.html lacks the black-translucent meta"
        );
    }
    let css = include_str!("../../static/web_app/app.css");
    let start = css
        .find("@media (prefers-color-scheme: light) {")
        .expect("app.css lacks the light-mode header tint");
    let tint = &css[start..];
    let tint = &tint[..tint.find("\n}\n").expect("tint is closed")];
    for wanted in [
        ".top {",
        "var(--status-tint) 0 var(--status-glass),",
        "var(--surface-1) var(--status-glass)",
    ] {
        assert!(
            tint.contains(wanted),
            "the light-mode tint lacks {wanted:?}"
        );
    }
}

#[test]
fn the_light_strip_covers_the_pages_without_a_header_with_the_same_tint() {
    let css = include_str!("../../static/web_app/app.css");
    let start = css
        .find("@media (prefers-color-scheme: light) {")
        .expect("app.css lacks the light-mode block");
    let block = &css[start..];
    let block = &block[..block.find("\n}\n").expect("block is closed")];
    let tint = "color-mix(in srgb, var(--surface-1), black 60%)";
    assert_eq!(css.matches(tint).count(), 1, "the tint is defined once");
    assert!(block.contains(&format!("--status-tint: {tint};")));
    let strip_start = block
        .find("body.pair-body::before,")
        .expect("the light block lacks the strip rule");
    let strip = &block[strip_start..];
    let strip = &strip[..strip.find('}').expect("strip rule is closed")];
    for wanted in [
        "body.echo-test::before,",
        "body.in-call::before {",
        "position: fixed;",
        "height: env(safe-area-inset-top);",
        "background: var(--status-tint);",
        "pointer-events: none;",
    ] {
        assert!(strip.contains(wanted), "the strip rule lacks {wanted:?}");
    }
    let top = &block[block.find("  .top {").expect("the light block lacks .top")..];
    assert!(
        top[..top.find('}').expect("the .top rule is closed")]
            .contains("var(--status-tint) 0 var(--status-glass)")
    );
}

/// The names a scope declares with `function` more than once. Declarations of one scope share
/// an indent and follow one run of lines with no shallower line between; a later declaration
/// silently replaces an earlier one.
fn repeated_functions(source: &str) -> Vec<String> {
    let mut seen: Vec<(usize, usize, String)> = Vec::new();
    let mut repeated = Vec::new();
    // The last line seen at each indent, by indent: the opener of the scope below it.
    let mut openers: Vec<usize> = Vec::new();
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        let indent = line.len() - trimmed.len();
        openers.truncate(indent);
        openers.resize(indent + 1, number);
        let declaration = trimmed.strip_prefix("async ").unwrap_or(trimmed);
        let Some(rest) = declaration.strip_prefix("function ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        let scope = openers[..indent].last().copied().unwrap_or(0);
        if seen.contains(&(indent, scope, name.clone())) {
            repeated.push(format!("{name} (line {})", number + 1));
        } else {
            seen.push((indent, scope, name));
        }
    }
    repeated
}

#[test]
fn no_page_script_declares_the_same_function_twice_in_one_scope() {
    let scripts = page_scripts();
    for wanted in ["call.js", "pages.js"] {
        assert!(scripts.iter().any(|(name, _)| name == wanted), "{wanted}");
    }
    let found: Vec<_> = scripts
        .iter()
        .flat_map(|(name, source)| {
            repeated_functions(source)
                .into_iter()
                .map(move |function| format!("{name}: {function}"))
        })
        .collect();
    assert!(found.is_empty(), "functions declared twice: {found:#?}");
    // The scan finds a repeat, and keeps a nested function of the same name apart.
    assert_eq!(
        repeated_functions("(() => {\n  function a() {}\n  function a() {}\n})();"),
        ["a (line 3)"]
    );
    assert!(
        repeated_functions("(() => {\n  function a() {\n    function b() {}\n  }\n  function c() {\n    function b() {}\n  }\n})();")
            .is_empty()
    );
}

#[test]
fn the_call_script_posts_its_signals_with_post_signals_and_reports_a_call_with_no_id() {
    let script = include_str!("../../static/web_app/call.js");
    assert!(script.contains("async function postSignals(element, url, signals, include"));
    assert!(script.contains("return postSignals(null, path, {}, /^none$/"));
    let check = script
        .split("function postCheck(body) {")
        .nth(1)
        .and_then(|rest| rest.split("\n  }\n").next())
        .expect("postCheck is there");
    assert!(check.contains("postSignals("), "{check}");
    // The audio piece poster keeps its name, and the Datastar poster is no longer `post`.
    assert!(script.contains("function post(current, piece)"));
    assert!(!script.contains("function post(element"));
    // A POST /call with no id sends the check before it ends the call.
    let no_id = script
        .split("if (status !== 200 || !id) {")
        .nth(1)
        .and_then(|rest| rest.split("\n      }\n").next())
        .expect("the no-id path is there");
    assert!(
        no_id.contains(r#"postCheck({ ...facts(current), step: "call" });"#),
        "{no_id}"
    );
    assert!(no_id.find("postCheck").unwrap() < no_id.find("end(current)").unwrap());
}
