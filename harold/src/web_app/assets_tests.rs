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
        "current.id = text",
        "application/octet-stream",
        "echoCancellation: true",
        "wakeLock",
        "/call/played?id=",
        "start_ms=",
        "/call/talk-now",
        "keepalive: true",
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
        "color-mix(in srgb, var(--surface-1) 72%, transparent) 0 var(--status-strip),",
        "var(--surface-1) var(--status-strip)",
    ] {
        assert!(top.contains(wanted), ".top lacks {wanted:?}");
    }
    let root = rule(":root");
    assert!(
        root.contains("--status-strip: max(8px, calc(env(safe-area-inset-top) - 12px));"),
        ":root lacks the --status-strip that starts under the phone's icons"
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
        "application/json",
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
        "application/json",
        "display-mode: standalone",
        "localStorage",
        "online",
        "offline",
    ] {
        assert!(script.contains(wanted), "speech-check.js lacks {wanted}");
    }
    // Words are read from a result only to be compared; the one post is the run object.
    assert_eq!(
        script.matches("fetch(").count(),
        3,
        "sentence twice, the run once"
    );
    assert!(script.contains("body: JSON.stringify(run)"));
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
        r#"post("/page/hidden")"#,
        "fetch(path, {",
        r#"method: "POST""#,
        "keepalive: true",
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
        r#"post("/page/seen", "#,
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
        "JSON.stringify({ beat: lastBeat })",
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
        // It uploads at once, with fetch, and reads the id back as text.
        r#"fetch("/replies/photo""#,
        r#""Content-Type": "application/octet-stream""#,
        "(await response.text()).trim()",
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
        // One check per page load, as JSON, to the one route.
        r#"fetch("/dictation/check""#,
        "JSON.stringify({ result })",
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
    assert_eq!(script.matches("fetch(").count(), 1);
}

#[test]
fn the_dictation_rules_are_in_the_stylesheet() {
    let css = include_str!("../../static/web_app/app.css");
    for wanted in [
        ".bottom input.interim {\n  color: var(--text-2);\n  border-color: var(--brand);",
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
