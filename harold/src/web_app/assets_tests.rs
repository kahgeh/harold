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
    ] {
        assert!(script.contains(wanted), "call.js lacks {wanted}");
    }
    for absent in ["eval(", "new Function", "innerHTML", "http://", "https://"] {
        assert!(!script.contains(absent), "call.js holds {absent}");
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
