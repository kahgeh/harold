//! The TTS server over a fake worker and a scratch model folder, and the gRPC service over a
//! real loopback connection.

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support;
use crate::tts::models::{InfoState, Phase};
use crate::tts::service::{TtsApi, require_local};
use crate::tts::test_support::*;
use harold_api::tts::v1::tts_service_client::TtsServiceClient;
use harold_api::tts::v1::tts_service_server::TtsServiceServer;
use harold_api::tts::v1::{
    AddModelRequest, ListModelsRequest, ModelState, RemoveModelRequest, SetDefaultRequest,
    SpeakRequest,
};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::timeout;
use tonic::Code;
use tonic::transport::{Channel, Server};

/// Serves the gRPC service on a loopback port and connects a client to it.
async fn client_of(server: &Arc<TtsServer>) -> TtsServiceClient<Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let api = TtsApi::new(Arc::clone(server));
    let (incoming, connections) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            if incoming
                .send(Ok::<_, std::io::Error>(socket))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    tokio::spawn(
        Server::builder()
            .add_service(TtsServiceServer::new(api))
            .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections)),
    );
    TtsServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap()
}

fn say(text: &str) -> SpeakRequest {
    SpeakRequest {
        text: text.to_string(),
        model: String::new(),
        voice: String::new(),
    }
}

async fn collect(
    client: &mut TtsServiceClient<Channel>,
    request: SpeakRequest,
) -> Result<Vec<harold_api::tts::v1::AudioChunk>, tonic::Status> {
    let mut stream = client.speak(request).await?.into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.message().await? {
        chunks.push(chunk);
    }
    Ok(chunks)
}

#[tokio::test(flavor = "current_thread")]
async fn speak_streams_audio_with_the_sample_rate_in_the_first_message() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;

    let chunks = collect(&mut client, say("Hello there.")).await.unwrap();

    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].sample_rate, 24_000);
    assert!(chunks[1..].iter().all(|chunk| chunk.sample_rate == 0));
    let pcm: Vec<u8> = chunks.into_iter().flat_map(|chunk| chunk.pcm).collect();
    assert_eq!(pcm, [0, 0, 0, 0, 1, 0, 1, 0, 2, 0, 2, 0]);
}

#[tokio::test(flavor = "current_thread")]
async fn speak_refuses_empty_text_and_text_over_4000_characters() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;

    for text in ["", "   \n", &"x".repeat(4_001)] {
        let refused = collect(&mut client, say(text)).await.unwrap_err();
        assert_eq!(
            refused.code(),
            Code::InvalidArgument,
            "{} chars",
            text.len()
        );
        assert_eq!(refused.message(), "invalid_text");
    }
    // 4,000 characters is the most; a character is not a byte.
    assert!(collect(&mut client, say(&"é".repeat(4_000))).await.is_ok());
    assert_eq!(fixture.script.launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn speak_refuses_an_unknown_model_and_an_unknown_voice() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;

    let unknown_model = SpeakRequest {
        model: "org/none".into(),
        ..say("Hello.")
    };
    let refused = collect(&mut client, unknown_model).await.unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "unknown_model")
    );

    let unknown_voice = SpeakRequest {
        voice: "nobody".into(),
        ..say("Hello.")
    };
    let refused = collect(&mut client, unknown_voice).await.unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "unknown_voice")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn until_the_model_is_checked_speak_answers_unavailable_model_downloading() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    let registry = scratch.registry();
    let supervisor = Supervisor::new(
        Arc::new(FakeLauncher(Arc::new(Script::default()))),
        Duration::from_secs(600),
    );
    let server = TtsServer::new(registry, supervisor);
    let mut client = client_of(&server).await;

    let refused = collect(&mut client, say("Hello.")).await.unwrap_err();

    assert_eq!(
        (refused.code(), refused.message()),
        (Code::Unavailable, "model_downloading")
    );
    let text = logs.text();
    assert!(text.contains("tts failed"), "{text}");
    assert!(text.contains("reason=\"model_downloading\""), "{text}");
    assert!(text.contains("caller=\"desk\""), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_that_failed_its_check_answers_unavailable_model_missing() {
    let scratch = Scratch::new();
    scratch
        .hub
        .downloads
        .lock()
        .unwrap()
        .push(Err("check_failed"));
    let registry = scratch.registry();
    registry.start_up();
    until(&registry, |registry| {
        matches!(registry.default_status(), DefaultStatus::Failed { .. })
    })
    .await;
    let supervisor = Supervisor::new(
        Arc::new(FakeLauncher(Arc::new(Script::default()))),
        Duration::from_secs(600),
    );
    let server = TtsServer::new(registry, supervisor);
    let mut client = client_of(&server).await;

    let refused = collect(&mut client, say("Hello.")).await.unwrap_err();

    assert_eq!(
        (refused.code(), refused.message()),
        (Code::Unavailable, "model_missing")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_worker_that_fails_answers_worker_failed_and_the_next_request_starts_it_again() {
    let logs = CapturedLogs::start();
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;
    collect(&mut client, say("One.")).await.unwrap();
    fixture.script.lines.lock().unwrap().push(Next::Exit);

    let refused = collect(&mut client, say("Two.")).await.unwrap_err();

    assert_eq!(
        (refused.code(), refused.message()),
        (Code::Unavailable, "worker_failed")
    );
    assert!(logs.text().contains("tts failed"));
    assert!(logs.text().contains("reason=\"worker_failed\""));
    collect(&mut client, say("Three.")).await.unwrap();
    assert_eq!(fixture.script.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn a_worker_that_fails_in_the_middle_ends_the_stream_with_unavailable() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;
    // The fake sends three pieces, then the line fails.
    fixture.script.lines.lock().unwrap().push(Next::SayThenFail);

    let result = collect(&mut client, say("Two.")).await;

    assert_eq!(result.unwrap_err().code(), Code::Unavailable);
}

#[test]
fn only_a_loopback_address_may_call() {
    let address = |text: &str| -> Option<SocketAddr> { Some(text.parse().unwrap()) };
    for local in ["127.0.0.1:5000", "127.9.9.9:1", "[::1]:5000"] {
        assert!(require_local(address(local)).is_ok(), "{local}");
    }
    for remote in [
        "192.168.1.5:5000",
        "10.0.0.2:1",
        "100.64.0.9:7",
        "[fe80::1]:5000",
        "8.8.8.8:1",
    ] {
        let refused = require_local(address(remote)).unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied, "{remote}");
        assert_eq!(refused.message(), "not_local");
    }
    assert_eq!(
        require_local(None).unwrap_err().message(),
        "not_local",
        "a call with no address is refused"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn list_models_shows_each_model_with_its_state_and_the_default() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;

    let models = client
        .list_models(ListModelsRequest {})
        .await
        .unwrap()
        .into_inner()
        .models;

    assert_eq!(models.len(), 1);
    let kokoro = &models[0];
    assert_eq!(kokoro.id, fixture.model);
    assert_eq!(kokoro.state, i32::from(ModelState::Ready));
    assert!(kokoro.is_default);
    assert_eq!(kokoro.default_voice, "af_heart");
    assert_eq!(kokoro.worker, WorkerKind_this());
    assert_eq!(kokoro.source, "huggingface");
    assert_eq!(kokoro.revision, crate::settings::KOKORO_REVISION);
    assert_eq!(kokoro.size_bytes, 102);
}

#[allow(non_snake_case)]
fn WorkerKind_this() -> &'static str {
    worker::WorkerKind::THIS_BUILD.as_str()
}

#[tokio::test(flavor = "current_thread")]
async fn add_model_refuses_what_it_must_with_the_words_of_the_api() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;
    let revision = "c".repeat(40);
    let add = |source: &str, repo: &str, revision: &str| AddModelRequest {
        source: source.to_string(),
        repo: repo.to_string(),
        revision: revision.to_string(),
    };

    // A revision that is not 40 hex characters.
    let refused = client
        .add_model(add("huggingface", "org/tts", "main"))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "invalid_revision")
    );

    // Not on the approved list: recorded as waiting for approval.
    let refused = client
        .add_model(add("huggingface", "org/tts", &revision))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::PermissionDenied, "not_approved")
    );
    let models = client
        .list_models(ListModelsRequest {})
        .await
        .unwrap()
        .into_inner()
        .models;
    let waiting = models.iter().find(|model| model.id == "org/tts").unwrap();
    assert_eq!(waiting.state, i32::from(ModelState::WaitingForApproval));

    // On the list, for the other worker.
    let other = if cfg!(target_os = "macos") {
        "onnx"
    } else {
        "mlx"
    };
    fixture
        .scratch
        .approve("org/other-worker", &revision, other);
    let refused = client
        .add_model(add("huggingface", "org/other-worker", &revision))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "unsupported_worker")
    );

    // On the list, with a weight file that can run code.
    fixture
        .scratch
        .approve("org/unsafe", &revision, WorkerKind_this());
    *fixture.scratch.hub.files.lock().unwrap() = vec![
        models::RemoteFile_for_test("model.safetensors"),
        models::RemoteFile_for_test("pytorch_model.bin"),
    ];
    let refused = client
        .add_model(add("huggingface", "org/unsafe", &revision))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "unsafe_format")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn add_model_downloads_an_approved_model_and_lists_it() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;
    let revision = "d".repeat(40);
    fixture.scratch.safe_files();
    fixture
        .scratch
        .approve("org/approved", &revision, WorkerKind_this());

    let added = client
        .add_model(AddModelRequest {
            source: "huggingface".into(),
            repo: "org/approved".into(),
            revision: revision.clone(),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(added.id, "org/approved");
    assert_eq!(added.state, i32::from(ModelState::Downloading));
    until(fixture.server.registry(), |registry| {
        registry
            .list()
            .iter()
            .any(|model| model.id == "org/approved" && model.state == InfoState::Ready)
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_model_refuses_the_default_and_set_default_is_kept() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;

    let refused = client
        .remove_model(RemoveModelRequest {
            id: fixture.model.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "default_model")
    );
    let refused = client
        .remove_model(RemoveModelRequest {
            id: "org/none".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::NotFound);

    client
        .set_default(SetDefaultRequest {
            model: fixture.model.clone(),
            voice: "am_adam".into(),
        })
        .await
        .unwrap();
    let models = client
        .list_models(ListModelsRequest {})
        .await
        .unwrap()
        .into_inner()
        .models;
    assert_eq!(models[0].default_voice, "am_adam");
    assert_eq!(
        fixture.scratch.default_row.0.lock().unwrap().clone(),
        Some((fixture.model.clone(), "am_adam".to_string()))
    );
    let refused = client
        .set_default(SetDefaultRequest {
            model: fixture.model.clone(),
            voice: "nobody".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "current_thread")]
async fn a_line_makes_a_span_with_the_fields_the_dashboard_reads() {
    test_support::install();
    let fixture = fixture().await;

    fixture
        .server
        .speak_all(Caller::Call, "A line for the span.")
        .await
        .unwrap();

    let spans = test_support::spans();
    let span = spans
        .iter()
        .find(|span| {
            span.name == "harold.tts.speak"
                && test_support::span_attr(span, "model").as_deref() == Some(fixture.model.as_str())
        })
        .expect("the line made a span");
    let attr = |key: &str| test_support::span_attr(span, key);
    assert_eq!(attr("caller").as_deref(), Some("call"));
    assert_eq!(attr("result").as_deref(), Some("spoke"));
    assert_eq!(attr("loaded").as_deref(), Some("true"));
    assert_eq!(attr("device").as_deref(), Some("apple_gpu"));
    assert!(attr("first_audio_ms").is_some());
    // No field holds the text.
    assert!(
        span.attributes
            .iter()
            .all(|kv| !kv.value.as_str().contains("A line for the span"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_line_makes_a_span_with_result_failed() {
    test_support::install();
    let fixture = fixture().await;
    fixture.script.lines.lock().unwrap().push(Next::Exit);

    let result = fixture.server.speak_all(Caller::Desk, "Fails.").await;

    assert_eq!(result.err(), Some(SpeakFailure::WorkerFailed));
    let spans = test_support::spans();
    let span = spans
        .iter()
        .find(|span| {
            span.name == "harold.tts.speak"
                && test_support::span_attr(span, "model").as_deref() == Some(fixture.model.as_str())
                && test_support::span_attr(span, "result").as_deref() == Some("failed")
        })
        .expect("the failed line made a span");
    assert_eq!(
        test_support::span_attr(span, "caller").as_deref(),
        Some("desk")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_second_line_does_not_load_the_worker_and_says_so_on_its_span() {
    test_support::install();
    let fixture = fixture().await;
    fixture
        .server
        .speak_all(Caller::Desk, "First.")
        .await
        .unwrap();
    fixture
        .server
        .speak_all(Caller::Desk, "Second.")
        .await
        .unwrap();

    let spans = test_support::spans();
    let loaded: Vec<_> = spans
        .iter()
        .filter(|span| {
            span.name == "harold.tts.speak"
                && test_support::span_attr(span, "model").as_deref() == Some(fixture.model.as_str())
        })
        .map(|span| test_support::span_attr(span, "loaded"))
        .collect();
    assert_eq!(
        loaded,
        [Some("true".to_string()), Some("false".to_string())]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tts_gpu_unavailable_is_logged_once_for_each_reason_in_a_run() {
    let logs = CapturedLogs::start();
    let fixture = fixture().await;
    // Two workers in one run, both on the CPU for the same reason; then a line that ran out of
    // GPU memory, twice.
    fixture
        .script
        .ready_gpu_reasons
        .lock()
        .unwrap()
        .extend(["no_gpu", "no_gpu"]);
    fixture
        .server
        .speak_all(Caller::Desk, "One.")
        .await
        .unwrap();
    fixture.server.supervisor().stop().await;
    fixture
        .server
        .speak_all(Caller::Desk, "Two.")
        .await
        .unwrap();
    fixture
        .script
        .done
        .lock()
        .unwrap()
        .extend([(worker::Device::Cpu, Some("gpu_out_of_memory")); 2]);
    fixture
        .server
        .speak_all(Caller::Desk, "Three.")
        .await
        .unwrap();
    fixture
        .server
        .speak_all(Caller::Desk, "Four.")
        .await
        .unwrap();

    let text = logs.text();
    assert_eq!(text.matches("tts gpu unavailable").count(), 2, "{text}");
    assert_eq!(text.matches("reason=\"no_gpu\"").count(), 1, "{text}");
    assert_eq!(
        text.matches("reason=\"gpu_out_of_memory\"").count(),
        1,
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_line_that_ran_on_the_cpu_for_lack_of_gpu_memory_reports_device_cpu() {
    test_support::install();
    let fixture = fixture().await;
    fixture
        .script
        .done
        .lock()
        .unwrap()
        .push((worker::Device::Cpu, Some("gpu_out_of_memory")));
    fixture
        .server
        .speak_all(Caller::Desk, "Out of memory.")
        .await
        .unwrap();
    fixture
        .server
        .speak_all(Caller::Desk, "Next line.")
        .await
        .unwrap();

    let spans = test_support::spans();
    let devices: Vec<_> = spans
        .iter()
        .filter(|span| {
            span.name == "harold.tts.speak"
                && test_support::span_attr(span, "model").as_deref() == Some(fixture.model.as_str())
        })
        .map(|span| test_support::span_attr(span, "device"))
        .collect();
    // The next line tries the GPU again.
    assert_eq!(
        devices,
        [Some("cpu".to_string()), Some("apple_gpu".to_string())]
    );
}

#[test]
fn a_desk_thread_gets_the_audio_from_the_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fixture = runtime.block_on(fixture());

    let audio = fixture
        .server
        .speak_all_blocking(Caller::Desk, "From a thread.")
        .unwrap();

    assert_eq!(audio.sample_rate, 24_000);
    assert_eq!(audio.pcm.len(), 12);
    runtime.block_on(async {
        drop(fixture);
    });
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_warms_the_worker_while_it_rings_and_keeps_it_until_the_call_ends() {
    let fixture = fixture().await;

    fixture.server.call_opened();
    fixture.server.warm_for_call();
    timeout(Duration::from_secs(10), async {
        while fixture.script.launches.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // The first line of the call does not load.
    let mut speech = fixture
        .server
        .speak(Caller::Call, "First line.", "", "")
        .await
        .unwrap();
    while speech.next().await.unwrap().is_some() {}
    assert_eq!(fixture.script.launches.load(Ordering::SeqCst), 1);
    assert!(!fixture.server.supervisor().stop_if_idle().await);
    fixture.server.call_ended();
}

#[tokio::test(flavor = "current_thread")]
async fn the_phase_of_a_fresh_registry_is_checking() {
    let scratch = Scratch::new();
    let registry = scratch.registry();
    assert_eq!(registry.list()[0].state, InfoState::Downloading);
    assert_eq!(Phase::Checking, Phase::Checking);
}

#[tokio::test(flavor = "current_thread")]
async fn a_line_dropped_before_its_end_makes_a_span_with_result_stopped() {
    test_support::install();
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;
    // The fake sends one piece, then holds the rest of the line.
    fixture.script.lines.lock().unwrap().push(Next::Hold);
    let mut stream = client
        .speak(say("Stops early."))
        .await
        .unwrap()
        .into_inner();
    assert!(stream.message().await.unwrap().is_some());
    drop(stream);
    drop(client);
    // The server sees the stream end before the rest of the line comes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    fixture.script.release.notify_one();

    let stopped = async {
        loop {
            let found = test_support::spans().iter().any(|span| {
                span.name == "harold.tts.speak"
                    && test_support::span_attr(span, "model").as_deref()
                        == Some(fixture.model.as_str())
                    && test_support::span_attr(span, "result").as_deref() == Some("stopped")
            });
            if found {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(Duration::from_secs(5), stopped)
        .await
        .unwrap_or_else(|_| {
            let seen: Vec<_> = test_support::spans()
                .iter()
                .filter(|span| span.name == "harold.tts.speak")
                .map(|span| {
                    (
                        test_support::span_attr(span, "model"),
                        test_support::span_attr(span, "result"),
                    )
                })
                .collect();
            panic!("no stopped span: {seen:?}")
        });
}

#[tokio::test(flavor = "current_thread")]
async fn with_the_voice_muted_speak_answers_voice_muted_and_the_worker_gets_no_request() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;

    crate::outbound::set_tts_muted(true);
    let refused = collect(&mut client, say("Muted.")).await;
    crate::outbound::set_tts_muted(false);

    let refused = refused.unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "voice_muted")
    );
    assert_eq!(fixture.script.launches.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn remove_model_of_a_model_the_worker_does_not_hold_leaves_the_worker_running() {
    let fixture = fixture().await;
    let mut client = client_of(&fixture.server).await;
    let revision = "e".repeat(40);
    fixture.scratch.safe_files();
    fixture
        .scratch
        .approve("org/other", &revision, WorkerKind_this());
    client
        .add_model(AddModelRequest {
            source: "huggingface".into(),
            repo: "org/other".into(),
            revision,
        })
        .await
        .unwrap();
    until(fixture.server.registry(), |registry| {
        registry
            .list()
            .iter()
            .any(|model| model.id == "org/other" && model.state == InfoState::Ready)
    })
    .await;
    collect(&mut client, say("Loads the default."))
        .await
        .unwrap();

    client
        .remove_model(RemoveModelRequest {
            id: "org/other".into(),
        })
        .await
        .unwrap();

    assert_eq!(fixture.script.closed.load(Ordering::SeqCst), 0);
    collect(&mut client, say("Still loaded.")).await.unwrap();
    assert_eq!(fixture.script.launches.load(Ordering::SeqCst), 1);
}
