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
