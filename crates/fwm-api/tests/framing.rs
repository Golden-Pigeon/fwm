use fwm_api::{
    codec::{read_frame, write_frame},
    protocol::{MAX_FRAME_BYTES, Request},
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::io;

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}

#[tokio::test]
async fn only_eof_at_a_frame_boundary_is_a_clean_disconnect() {
    assert!(
        read_frame::<Value, _>(&mut &[][..])
            .await
            .unwrap()
            .is_none()
    );
    let bytes = frame(br#"{"valid":true}"#);
    for length in 1..bytes.len() {
        let error = read_frame::<Value, _>(&mut &bytes[..length])
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "truncated at {length}"
        );
    }
}

#[tokio::test]
async fn zero_and_oversized_headers_are_rejected_without_waiting_for_payload() {
    for length in [0, MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
        let header = length.to_be_bytes();
        let error = read_frame::<Value, _>(&mut header.as_slice())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

#[tokio::test]
async fn malformed_json_utf8_and_wrong_payload_types_are_invalid_data() {
    for payload in [
        &b"{"[..],
        &b"not json"[..],
        &[0xff, 0xfe][..],
        &b"true false"[..],
    ] {
        let bytes = frame(payload);
        assert_eq!(
            read_frame::<Value, _>(&mut bytes.as_slice())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    let bytes = frame(br#"{"api_version":1,"request_id":"a","command":{"method":"unknown"}}"#);
    assert_eq!(
        read_frame::<Request, _>(&mut bytes.as_slice())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn sequential_frames_preserve_boundaries_and_unicode_byte_lengths() {
    let values = [json!({"message": "转发🔌"}), json!([1, 2, 3]), Value::Null];
    let mut bytes = vec![];
    for value in &values {
        write_frame(&mut bytes, value).await.unwrap();
    }
    let mut reader = bytes.as_slice();
    for value in values {
        assert_eq!(
            read_frame::<Value, _>(&mut reader).await.unwrap(),
            Some(value)
        );
    }
    assert!(reader.is_empty());
    assert!(read_frame::<Value, _>(&mut reader).await.unwrap().is_none());
}

#[tokio::test]
async fn maximum_size_frame_is_accepted_and_one_extra_byte_is_rejected_atomically() {
    let boundary = "x".repeat(MAX_FRAME_BYTES - 2); // Two JSON string delimiters.
    let mut bytes = vec![];
    write_frame(&mut bytes, &boundary).await.unwrap();
    assert_eq!(bytes.len(), MAX_FRAME_BYTES + 4);
    assert_eq!(
        read_frame::<String, _>(&mut bytes.as_slice())
            .await
            .unwrap(),
        Some(boundary)
    );
    let mut untouched = vec![];
    let error = write_frame(&mut untouched, &"x".repeat(MAX_FRAME_BYTES - 1))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(
        untouched.is_empty(),
        "rejected writes must not leave a partial header"
    );
}

#[tokio::test]
async fn serialization_failure_does_not_write_a_partial_frame() {
    struct Fails;
    impl Serialize for Fails {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("unsupported payload"))
        }
    }
    let mut bytes = vec![];
    let error = write_frame(&mut bytes, &Fails).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().contains("unsupported payload"));
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn transport_write_failure_is_preserved() {
    let (mut writer, reader) = tokio::io::duplex(32);
    drop(reader);
    assert_eq!(
        write_frame(&mut writer, &json!({"request":1}))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}
