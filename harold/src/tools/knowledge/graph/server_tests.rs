use std::net::SocketAddr;

use super::*;

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

#[test]
fn only_a_loopback_address_is_allowed() {
    assert!(require_loopback(address("127.0.0.1:1729")).is_ok());
    for refused in [
        "0.0.0.0:1729",
        "192.168.1.5:1729",
        "[::]:1729",
        "[::1]:1729",
    ] {
        assert!(require_loopback(address(refused)).is_err(), "{refused}");
    }
}

#[test]
fn the_config_refuses_an_address_that_is_not_loopback() {
    let folder = Path::new("/x/typedb");
    assert!(config_text(address("0.0.0.0:1"), address("127.0.0.1:2"), folder).is_err());
    assert!(config_text(address("127.0.0.1:1"), address("0.0.0.0:2"), folder).is_err());
}

#[test]
fn the_config_listens_on_loopback_and_reports_nothing() {
    let text = config_text(
        address("127.0.0.1:1"),
        address("127.0.0.1:2"),
        Path::new("/x/ty\"pedb"),
    )
    .unwrap();
    assert!(text.contains("    listen-address: 127.0.0.1:1\n"));
    assert!(text.contains("        listen-address: 127.0.0.1:2\n"));
    assert!(!text.contains("0.0.0.0"));
    assert!(text.contains("    reporting:\n        metrics: false\n        errors: false\n"));
    assert!(text.contains("    monitoring:\n        enabled: false\n"));
    assert!(text.contains("data-directory: \"/x/ty\\\"pedb/data\""));
    assert!(text.ends_with('\n'));
}

#[test]
fn two_free_ports_differ_while_both_are_held() {
    let first = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let second = free_port().unwrap();
    assert_ne!(first.local_addr().unwrap().port(), second);
}
