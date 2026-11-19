use super::*;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

#[tokio::test]
async fn malformed_completion_reports_shape_without_message_body() {
    let car_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = car_listener.local_addr().unwrap();
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = server_listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = server_listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        for _ in 0..3 {
            ws.next().await.unwrap().unwrap();
        }
        // A synthetic unknown completion, not an inferred service protocol.
        ws.send(Message::Binary(b"\x85PRIVATE_CANARY".to_vec().into()))
            .await
            .unwrap();
    });
    let error = timeout(
        Duration::from_secs(2),
        relay(gateway, &format!("ws://{address}"), |_| {}),
    )
    .await
    .unwrap()
    .unwrap_err()
    .to_string();
    assert!(error.contains("unknown server tag"), "{error}");
    assert!(error.contains("tag Some(133), 15 bytes"), "{error}");
    assert!(!error.contains("PRIVATE_CANARY"));
    server.await.unwrap();
}
