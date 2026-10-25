//! `voice_input` in a build without the `calls` feature: the types the web app, the projector
//! and the settings share, and no call. Every line Harold would say in a call finds none open.

#[path = "shared.rs"]
mod shared;

pub(crate) use shared::{
    AudioRefused, CallState, CallStatus, Missing, NO_CHIEF, PhoneStarted, PhoneView, Said, Speaker,
    TurnResult, VOICE_FAILED, without_hand_over_tag,
};

/// No phone call is open in this build.
pub(crate) fn say_in_phone_call(_line: &str, _by: Speaker) -> Option<bool> {
    None
}

/// No desk call is open in this build.
pub(crate) fn say_in_open_call(_line: &str, _by: Speaker) -> Option<bool> {
    None
}

/// The desk's calls in a build without `calls`: none can start, and none is ever set.
pub(crate) struct DaemonCalls;

impl DaemonCalls {
    fn not_set_up() -> CallStatus {
        CallStatus {
            state: CallState::NotSetUp,
            chief_pane_id: None,
            last_transcript: None,
            last_result: None,
            model_percent: None,
        }
    }

    pub(crate) async fn start(&self) -> CallStatus {
        Self::not_set_up()
    }

    pub(crate) async fn stop(&self) -> CallStatus {
        Self::not_set_up()
    }

    pub(crate) fn talk_now(&self) -> CallStatus {
        Self::not_set_up()
    }

    pub(crate) fn status(&self) -> CallStatus {
        Self::not_set_up()
    }

    pub(crate) async fn shutdown(&self) {}
}

/// Always `None`: a build without `calls` has no desk calls.
pub(crate) fn daemon_calls() -> Option<&'static DaemonCalls> {
    None
}

#[cfg(test)]
pub(crate) use shared::LineTaker;

/// No phone call is open in this build, whatever `phone` is.
#[cfg(test)]
pub(crate) fn say_in_phone_call_over(
    _phone: Option<&dyn LineTaker>,
    _line: &str,
    _by: Speaker,
) -> Option<bool> {
    None
}
