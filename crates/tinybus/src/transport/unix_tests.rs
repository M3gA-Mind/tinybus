use super::*;
use crate::name::{BusName, InterfaceName, MemberName, ObjectPath};

fn message(member: &str, body: serde_json::Value) -> Message {
    Message::method_call(
        BusName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        ObjectPath::new("/ai/tinyhumans/openhuman/Voice").unwrap(),
        InterfaceName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        MemberName::new(member).unwrap(),
        body,
    )
}

#[tokio::test]
async fn a_message_survives_the_socket_with_newlines_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bus");
    let listener = UnixListenerAdapter::bind(&path).await.unwrap();

    let client = UnixTransport::connect(&path).await.unwrap();
    let server = listener.accept().await.unwrap().expect("a peer");

    let sent = message("Transcribe", serde_json::json!(["a\nb\nc"]));
    client.send(sent.clone()).await.unwrap();
    assert_eq!(server.recv().await.unwrap().unwrap(), sent);
}

#[tokio::test]
async fn a_clean_hangup_is_none_rather_than_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bus");
    let listener = UnixListenerAdapter::bind(&path).await.unwrap();
    let client = UnixTransport::connect(&path).await.unwrap();
    let server = listener.accept().await.unwrap().expect("a peer");
    drop(client);
    assert!(server.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn binding_over_a_stale_socket_succeeds_but_over_a_live_one_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bus");

    let first = UnixListenerAdapter::bind(&path).await.unwrap();
    let err = UnixListenerAdapter::bind(&path).await.unwrap_err();
    assert!(err.to_string().contains("already listening"), "{err}");

    // Dropping unlinks; recreate the file to stand in for a crashed broker
    // that never got to clean up after itself.
    drop(first);
    std::fs::write(&path, b"").unwrap();
    UnixListenerAdapter::bind(&path)
        .await
        .expect("stale socket cleared");
}

#[tokio::test]
async fn a_mid_frame_hangup_is_a_protocol_error_and_close_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bus");
    let listener = UnixListenerAdapter::bind(&path).await.unwrap();
    let mut raw = UnixStream::connect(&path).await.unwrap();
    let server = listener.accept().await.unwrap().expect("a peer");

    raw.write_all(&10u32.to_be_bytes()).await.unwrap();
    raw.write_all(b"short").await.unwrap();
    drop(raw);
    assert!(
        server
            .recv()
            .await
            .unwrap_err()
            .to_string()
            .contains("mid-frame")
    );
    server.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn socket_labels_and_failed_dials_are_operator_useful() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");
    let error = UnixTransport::connect(&path)
        .await
        .err()
        .expect("dial fails");
    assert!(error.to_string().contains("could not connect"));

    let listener = UnixListenerAdapter::bind(&path).await.unwrap();
    let client = UnixTransport::connect(&path).await.unwrap();
    assert_eq!(client.describe(), format!("unix:{}", path.display()));
    assert_eq!(listener.describe(), format!("unix:{}", path.display()));
    assert_eq!(listener.path(), path.as_path());
}

#[test]
fn transient_accept_errors_are_classified_without_hiding_fatal_errors() {
    assert!(is_per_connection(&std::io::Error::from(
        std::io::ErrorKind::ConnectionReset
    )));
    assert!(is_per_connection(&std::io::Error::from(
        std::io::ErrorKind::Interrupted
    )));
    assert!(!is_per_connection(&std::io::Error::from(
        std::io::ErrorKind::PermissionDenied
    )));
}
