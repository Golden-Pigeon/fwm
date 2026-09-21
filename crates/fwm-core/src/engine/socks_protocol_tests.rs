use super::*;
use std::{net::Ipv6Addr, time::Duration};
use tokio::io::duplex;

fn request(address_type: u8, address: &[u8], port: u16) -> Vec<u8> {
    let mut request = vec![5, 1, 0, 5, 1, 0, address_type];
    if address_type == 3 {
        request.push(u8::try_from(address.len()).unwrap());
    }
    request.extend(address);
    request.extend(port.to_be_bytes());
    request
}

fn error_reply(status: u8) -> Vec<u8> {
    vec![5, 0, 5, status, 0, 1, 0, 0, 0, 0, 0, 0]
}

/// Half-close the request side so malformed/truncated input must finish without
/// waiting for the caller's normal handshake timeout. Bound the entire test too.
async fn exchange(input: &[u8]) -> (Result<Endpoint>, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (mut client, mut server) = duplex(4096);
        client.write_all(input).await.unwrap();
        client.shutdown().await.unwrap();
        let response = async {
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            response
        };
        let handshake = async {
            let target = handshake(&mut server).await;
            drop(server);
            target
        };
        tokio::join!(handshake, response)
    })
    .await
    .expect("SOCKS handshake hung after EOF")
}

#[tokio::test]
async fn connect_decodes_ipv4_ipv6_and_remote_dns_with_boundary_ports() {
    let ipv6: Ipv6Addr = "2001:db8::1".parse().unwrap();
    let longest = "a".repeat(255);
    let addresses = [
        (1, vec![127, 0, 0, 1], "127.0.0.1".to_owned()),
        (4, ipv6.octets().to_vec(), "2001:db8::1".to_owned()),
        (
            3,
            b"Db.Internal.Example".to_vec(),
            "Db.Internal.Example".to_owned(),
        ),
        (3, longest.as_bytes().to_vec(), longest),
    ];
    for (address_type, address, expected) in addresses {
        for port in [1, 443, 65535] {
            let (target, response) = exchange(&request(address_type, &address, port)).await;
            let target = target.unwrap();
            assert_eq!(target.host, expected);
            assert_eq!(target.port, port);
            // CONNECT success belongs to the later SSH channel-open result.
            assert_eq!(response, [5, 0]);
        }
    }
}

#[tokio::test]
async fn fragmented_input_and_multiple_auth_methods_preserve_following_application_bytes() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (mut client, mut server) = duplex(8);
        let server_task = tokio::spawn(async move {
            let target = handshake(&mut server).await.unwrap();
            let mut payload = [0; 4];
            server.read_exact(&mut payload).await.unwrap();
            (target, payload)
        });
        let mut input = vec![5, 3, 2, 0, 1];
        input.extend(&request(3, b"db.internal", 8080)[3..]);
        input.extend(b"HTTP");
        for byte in input {
            client.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        let mut method = [0; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);
        let (target, payload) = server_task.await.unwrap();
        assert_eq!(target.host, "db.internal");
        assert_eq!(target.port, 8080);
        assert_eq!(&payload, b"HTTP");
    })
    .await
    .expect("fragmented SOCKS request hung");
}

#[tokio::test]
async fn unsupported_greeting_versions_and_authentication_methods_fail_without_connect_reply() {
    for (input, expected) in [
        (vec![4, 1, 0], vec![]),
        (vec![6, 1, 0], vec![]),
        (vec![5, 0], vec![5, 255]),
        (vec![5, 2, 1, 2], vec![5, 255]),
    ] {
        let (target, response) = exchange(&input).await;
        assert!(target.is_err(), "{input:?}");
        assert_eq!(response, expected, "{input:?}");
    }
}

#[tokio::test]
async fn invalid_request_headers_return_protocol_specific_failure_replies() {
    for (header, status) in [
        ([4, 1, 0, 1], 1),
        ([5, 1, 1, 1], 1),
        ([5, 0, 0, 1], 7),
        ([5, 2, 0, 1], 7),
        ([5, 3, 0, 1], 7),
        ([5, 255, 0, 1], 7),
        ([5, 1, 0, 0], 8),
        ([5, 1, 0, 2], 8),
        ([5, 1, 0, 255], 8),
    ] {
        let mut input = vec![5, 1, 0];
        input.extend(header);
        let (target, response) = exchange(&input).await;
        assert!(target.is_err(), "{header:?}");
        assert_eq!(response, error_reply(status), "{header:?}");
    }
}

#[tokio::test]
async fn empty_invalid_encoding_nul_destinations_and_zero_ports_reply_before_closing() {
    for (input, status) in [
        (request(3, b"", 80), 8),
        (request(3, &[0xff, 0xfe], 80), 8),
        (request(3, b"db\0internal", 80), 1),
        (request(3, b"db.internal", 0), 1),
        (request(1, &[127, 0, 0, 1], 0), 1),
        (request(4, &Ipv6Addr::LOCALHOST.octets(), 0), 1),
    ] {
        let (target, response) = exchange(&input).await;
        assert!(target.is_err(), "{input:?}");
        assert_eq!(response, error_reply(status), "{input:?}");
    }
}

#[tokio::test]
async fn truncation_at_every_wire_field_returns_io_error_without_hanging_or_false_success() {
    for complete in [
        request(1, &[127, 0, 0, 1], 8080),
        request(4, &Ipv6Addr::LOCALHOST.octets(), 8080),
        request(3, b"db.internal", 8080),
    ] {
        for length in 0..complete.len() {
            let (target, response) = exchange(&complete[..length]).await;
            let error = target.unwrap_err();
            assert!(error.chain().any(|cause| cause.is::<std::io::Error>()));
            assert_eq!(response, if length >= 3 { vec![5, 0] } else { vec![] });
        }
    }
}
