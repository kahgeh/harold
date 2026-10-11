use super::{Denied, PeerEndpoint, authorize_forward, authorize_report, secrets_match};
use crate::settings::{PeerHub, PeerMember, PeerRole, PeersSettings};

fn member(name: &str, secret: &str) -> PeerMember {
    PeerMember {
        name: name.into(),
        url: "http://127.0.0.1:1".into(),
        secret: secret.into(),
    }
}

fn hub_endpoint() -> PeerEndpoint {
    PeerEndpoint::Hub {
        members: vec![member("laptop", "s3cret"), member("mini", "other")],
    }
}

fn member_endpoint() -> PeerEndpoint {
    PeerEndpoint::Member {
        hub: PeerHub {
            url: "http://127.0.0.1:1".into(),
            secret: "s3cret".into(),
        },
    }
}

#[test]
fn equal_secrets_match_and_different_ones_do_not() {
    assert!(secrets_match("s3cret", "s3cret"));
    assert!(!secrets_match("s3cret", "s3creT"));
    assert!(!secrets_match("s3cret", "s3cre"));
    assert!(!secrets_match("s3cret", "s3cret "));
}

#[test]
fn empty_secret_never_matches() {
    assert!(!secrets_match("", ""));
    assert!(!secrets_match("s3cret", ""));
    assert!(!secrets_match("", "s3cret"));
}

#[test]
fn hub_accepts_a_report_from_a_member_with_its_secret() {
    let endpoint = hub_endpoint();

    let accepted = authorize_report(&endpoint, "laptop", "s3cret");

    assert_eq!(accepted.map(|member| member.name.as_str()), Ok("laptop"));
}

#[test]
fn report_matches_the_member_name_ignoring_case() {
    let endpoint = hub_endpoint();

    let accepted = authorize_report(&endpoint, "LAPTOP", "s3cret");

    assert_eq!(accepted.map(|member| member.name.as_str()), Ok("laptop"));
}

#[test]
fn hub_denies_a_report_with_a_wrong_or_another_members_secret() {
    let endpoint = hub_endpoint();

    for secret in ["wrong", "", "other"] {
        assert_eq!(
            authorize_report(&endpoint, "laptop", secret),
            Err(Denied::WrongSecret),
            "{secret:?}"
        );
    }
}

#[test]
fn hub_denies_a_report_from_a_machine_that_is_not_a_member() {
    assert_eq!(
        authorize_report(&hub_endpoint(), "stranger", "s3cret"),
        Err(Denied::UnknownMember)
    );
}

#[test]
fn only_a_hub_accepts_a_report() {
    for endpoint in [member_endpoint(), PeerEndpoint::Off] {
        assert_eq!(
            authorize_report(&endpoint, "laptop", "s3cret"),
            Err(Denied::NotTheHub),
            "{endpoint:?}"
        );
    }
}

#[test]
fn member_accepts_a_forwarded_reply_with_the_hub_secret() {
    assert_eq!(authorize_forward(&member_endpoint(), "s3cret"), Ok(()));
}

#[test]
fn member_denies_a_forwarded_reply_with_a_wrong_secret() {
    for secret in ["wrong", ""] {
        assert_eq!(
            authorize_forward(&member_endpoint(), secret),
            Err(Denied::WrongSecret),
            "{secret:?}"
        );
    }
}

#[test]
fn only_a_member_accepts_a_forwarded_reply() {
    for endpoint in [hub_endpoint(), PeerEndpoint::Off] {
        assert_eq!(
            authorize_forward(&endpoint, "s3cret"),
            Err(Denied::NotAMember),
            "{endpoint:?}"
        );
    }
}

fn peers(role: PeerRole) -> PeersSettings {
    PeersSettings {
        role,
        members: vec![member("laptop", "s3cret")],
        hub: Some(PeerHub {
            url: "http://127.0.0.1:1".into(),
            secret: "s3cret".into(),
        }),
    }
}

#[test]
fn endpoint_follows_the_role_on_telegram() {
    assert_eq!(
        PeerEndpoint::from_settings("telegram", &peers(PeerRole::Hub)),
        PeerEndpoint::Hub {
            members: vec![member("laptop", "s3cret")]
        }
    );
    assert_eq!(
        PeerEndpoint::from_settings("telegram", &peers(PeerRole::Member)),
        member_endpoint()
    );
}

#[test]
fn endpoint_is_off_on_imessage_whatever_the_role() {
    for role in [PeerRole::Hub, PeerRole::Member] {
        assert_eq!(
            PeerEndpoint::from_settings("imessage", &peers(role)),
            PeerEndpoint::Off
        );
    }
}

#[test]
fn endpoint_is_off_for_a_member_without_a_hub() {
    let peers = PeersSettings {
        role: PeerRole::Member,
        ..PeersSettings::default()
    };

    assert_eq!(
        PeerEndpoint::from_settings("telegram", &peers),
        PeerEndpoint::Off
    );
}

#[test]
fn endpoint_debug_output_has_no_secret() {
    for endpoint in [hub_endpoint(), member_endpoint()] {
        let printed = format!("{endpoint:?}");
        assert!(!printed.contains("s3cret"), "{printed}");
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::{
    PEER_TIMEOUT, forward_reply, report_notification, report_notification_blocking,
    report_notification_within,
};
use crate::inbound::ownership::recorded_notifications;
use crate::tests::{closed_port_url, inbound_messages, start_peer_server};

fn hub_at(url: &str, secret: &str) -> PeerHub {
    PeerHub {
        url: url.into(),
        secret: secret.into(),
    }
}

fn hub_endpoint_with(name: &str) -> PeerEndpoint {
    PeerEndpoint::Hub {
        members: vec![member(name, "s3cret")],
    }
}

#[test]
fn peer_calls_time_out_after_five_seconds() {
    assert_eq!(PEER_TIMEOUT, Duration::from_secs(5));
}

#[tokio::test]
async fn report_reaches_the_hub() {
    crate::settings::init_settings_for_test();
    let server = start_peer_server(hub_endpoint_with("laptop-client-ok")).await;

    let reported = report_notification(&hub_at(&server.url, "s3cret"), "laptop-client-ok").await;

    assert_eq!(reported, Ok(()));
    assert!(recorded_notifications().contains(&"laptop-client-ok".to_string()));
}

#[tokio::test]
async fn rejected_call_reports_the_status_without_the_secret() {
    crate::settings::init_settings_for_test();
    let server = start_peer_server(hub_endpoint_with("laptop-client-denied")).await;

    let error = report_notification(
        &hub_at(&server.url, "not-the-secret"),
        "laptop-client-denied",
    )
    .await
    .unwrap_err();

    assert!(error.contains("PermissionDenied"), "{error}");
    assert!(!error.contains("not-the-secret"), "{error}");
    assert!(!recorded_notifications().contains(&"laptop-client-denied".to_string()));
}

#[tokio::test]
async fn forwarded_reply_reaches_the_member_store() {
    let server = start_peer_server(member_endpoint()).await;
    let to = PeerMember {
        name: "laptop".into(),
        url: server.url.clone(),
        secret: "s3cret".into(),
    };

    let forwarded = forward_reply(&to, "yes, merge it").await;

    assert_eq!(forwarded, Ok(()));
    let stored = inbound_messages(&server.store).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].text, "yes, merge it");
    assert!(stored[0].forwarded);
}

#[tokio::test]
async fn forward_to_a_harold_that_denies_it_is_an_error() {
    let server = start_peer_server(PeerEndpoint::Off).await;
    let to = PeerMember {
        name: "laptop".into(),
        url: server.url.clone(),
        secret: "s3cret".into(),
    };

    let error = forward_reply(&to, "yes, merge it").await.unwrap_err();

    assert!(error.contains("PermissionDenied"), "{error}");
}

#[tokio::test]
async fn unreachable_peer_is_an_error() {
    let hub = hub_at(&closed_port_url(), "s3cret");

    assert!(report_notification(&hub, "laptop").await.is_err());
}

#[tokio::test]
async fn invalid_peer_url_is_an_error_not_a_panic() {
    let hub = hub_at("http://not a url", "s3cret");

    assert!(report_notification(&hub, "laptop").await.is_err());
}

#[tokio::test]
async fn hung_peer_fails_within_the_timeout_and_is_called_once() {
    // Accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let hung = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(socket);
        }
    });

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        report_notification_within(
            &hub_at(&url, "s3cret"),
            "laptop",
            Duration::from_millis(300),
        ),
    )
    .await
    .expect("the call outlived its timeout");

    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the call must not be retried"
    );
    hung.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_report_works_on_a_blocking_thread_of_the_runtime() {
    crate::settings::init_settings_for_test();
    let server = start_peer_server(hub_endpoint_with("laptop-client-blocking")).await;
    let hub = hub_at(&server.url, "s3cret");

    let reported = tokio::task::spawn_blocking(move || {
        report_notification_blocking(&hub, "laptop-client-blocking")
    })
    .await
    .unwrap();

    assert_eq!(reported, Ok(()));
    assert!(recorded_notifications().contains(&"laptop-client-blocking".to_string()));
}

#[test]
fn blocking_report_outside_a_runtime_is_an_error_not_a_panic() {
    let hub = hub_at("http://127.0.0.1:1", "s3cret");

    let reported = std::thread::spawn(move || report_notification_blocking(&hub, "laptop"))
        .join()
        .unwrap();

    assert!(reported.is_err());
}
