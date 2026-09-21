use fwm_api::{
    client::Client,
    codec::{read_frame, write_frame},
    protocol::{API_VERSION, ApiError, Command, Request, Response},
};
use serde_json::json;
use std::io;

async fn reply(response: Option<Response>) -> io::Result<Response> {
    let (transport, mut daemon) = tokio::io::duplex(64);
    let task = tokio::spawn(async move {
        let request: Request = read_frame(&mut daemon).await.unwrap().unwrap();
        assert_eq!(request.request_id, "request-1");
        assert_eq!(request.expected_revision, Some(42));
        assert!(matches!(request.command, Command::Status));
        if let Some(response) = response {
            write_frame(&mut daemon, &response).await.unwrap();
        }
    });
    let mut request = Request::new("request-1", Command::Status);
    request.expected_revision = Some(42);
    let response = Client::new(transport).call(&request).await;
    task.await.unwrap();
    response
}

#[tokio::test]
async fn matched_response_preserves_version_correlation_and_payload() {
    let response = reply(Some(Response::success(
        "request-1".into(),
        json!({"forwards":[]}),
    )))
    .await
    .unwrap();
    assert_eq!(response.api_version, API_VERSION);
    assert_eq!(response.request_id, "request-1");
    assert!(response.ok);
    assert!(response.error.is_none());
    assert_eq!(response.data, json!({"forwards":[]}));
}

#[tokio::test]
async fn version_and_request_id_mismatch_cannot_be_accepted_as_success() {
    for (version, id) in [
        (API_VERSION + 1, "request-1"),
        (API_VERSION, "other-request"),
        (0, ""),
    ] {
        let mut response = Response::success(id.into(), json!({"saved":true}));
        response.api_version = version;
        assert_eq!(
            reply(Some(response)).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[tokio::test]
async fn clean_transport_disconnect_without_response_is_an_error() {
    let error = reply(None).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(error.to_string().contains("without a response"));
}

#[tokio::test]
async fn application_error_is_preserved_for_the_ui_to_interpret() {
    let response = reply(Some(Response::failure(
        "request-1".into(),
        ApiError::new("revision_conflict", "reload before editing"),
    )))
    .await
    .unwrap();
    assert!(!response.ok);
    assert!(response.data.is_null());
    let error = response.error.unwrap();
    assert_eq!(error.code, "revision_conflict");
    assert_eq!(error.message, "reload before editing");
}
