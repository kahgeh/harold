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
        "animation: dot-rise 1.2s ease-in-out infinite;",
        ".dots i:nth-child(2) {\n  animation-delay: 0.2s;",
        ".dots i:nth-child(3) {\n  animation-delay: 0.4s;",
        "@media (prefers-reduced-motion: reduce) {\n  .dots i {\n    animation: none;",
    ] {
        assert!(css.contains(wanted), "app.css lacks {wanted}");
    }
}
