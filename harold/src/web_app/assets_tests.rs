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
    ] {
        assert!(script.contains(wanted), "call.js lacks {wanted}");
    }
    for absent in ["eval(", "new Function", "innerHTML", "http://", "https://"] {
        assert!(!script.contains(absent), "call.js holds {absent}");
    }
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
        r#"post("/page/seen")"#,
        "PAGE_SEEN_EVERY = 30000",
        "setInterval(reportSeen, PAGE_SEEN_EVERY)",
        "clearInterval(",
        r#"document.visibilityState === "visible""#,
    ] {
        assert!(script.contains(wanted), "pages.js lacks {wanted}");
    }
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
        // The mic hides when neither API exists, and when a failure is remembered.
        "window.SpeechRecognition || window.webkitSpeechRecognition",
        r#"report("missing");"#,
        r#"const STORAGE_KEY = "harold-dictation";"#,
        "const REMEMBER_MS = 7 * 24 * 60 * 60 * 1000;",
        "age >= 0 && age < REMEMBER_MS",
        "localStorage.getItem(STORAGE_KEY)",
        "localStorage.setItem(STORAGE_KEY, String(Date.now()))",
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
