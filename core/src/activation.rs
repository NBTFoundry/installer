// SPDX-License-Identifier: MPL-2.0
//! SSH activation relay, as the BMW SSH client does it: find the vehicle
//! gateway by HSFZ discovery, then relay between it (port 6801) and the
//! activation service.
//! Vehicle traffic is never persisted or logged; the service's status texts
//! are shown to the user.
use crate::{
    discovery, net,
    relay_codec::{ServerMessage, decode_server, encode_client},
};
use futures_util::{SinkExt, StreamExt};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, handshake::client::Response, protocol::WebSocketConfig,
    },
};

pub const SERVER: &str = "wss://pretzel.bavarian-aa.uk:8443";
const MAX_FRAME: usize = 16 * 1024 * 1024;
type Error = Box<dyn std::error::Error + Send + Sync>;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid activation response")
}

fn take_frame(buffer: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
    if buffer.len() < 6 {
        return Ok(None);
    }
    let length = u32::from_be_bytes(buffer[..4].try_into().map_err(|_| invalid())?) as usize;
    if length > MAX_FRAME {
        return Err(invalid());
    }
    let total = 6 + length;
    if buffer.len() < total {
        return Ok(None);
    }
    Ok(Some(buffer.drain(..total).collect()))
}

/// The gateway answering HSFZ discovery on the ENET link. This is the
/// gateway module, not necessarily the head unit's SSH address.
pub fn find_gateway() -> Result<Ipv4Addr, String> {
    let search = discovery::discover(Duration::from_secs(2))
        .map_err(|error| format!("Gateway search failed: {error}"))?;
    match search.gateways.as_slice() {
        [gateway] => Ok(gateway.ip),
        [] => Err(no_answer(&search.sources, net::is_phone())),
        many => Err(format!(
            "More than one vehicle gateway answered ({}); connect only one car, or type its gateway address under Connection settings.",
            list(many.iter().map(|gateway| gateway.ip))
        )),
    }
}

fn list(addresses: impl Iterator<Item = Ipv4Addr>) -> String {
    addresses
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The search's failure, naming the case a Wi-Fi ENET adapter leaves behind:
/// the car keeps the address the adapter's DHCP server gave it, outside the
/// cable's 169.254 network. A phone is always on an adapter and cannot add
/// addresses, so it is told what to check instead.
fn no_answer(sources: &[Ipv4Addr], phone: bool) -> String {
    let from = if sources.is_empty() {
        String::new()
    } else {
        format!(" (asked from {})", list(sources.iter().copied()))
    };
    if phone {
        return format!(
            "No vehicle gateway answered{from}. Check that the phone is on the Wi-Fi ENET adapter's Wi-Fi, \
             the adapter is plugged into the car and the ignition is on. If you know the gateway's address, \
             type it under Connection settings."
        );
    }
    format!(
        "No vehicle gateway answered{from}. Check the ENET cable and the ignition. \
         If a Wi-Fi ENET adapter was used on this car, the car may still have the address the adapter gave it: \
         type the gateway address under Connection settings, or give this computer an address in the adapter's network \
         (often 192.168.16.x)."
    )
}

/// The activation service's WebSocket, over a connection opened by `net` so
/// that a phone reaches it over mobile data.
async fn open_service(
    server: &str,
    config: WebSocketConfig,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        Response,
    ),
    Error,
> {
    let request = server.into_client_request()?;
    let uri = request.uri();
    let host = uri.host().ok_or_else(invalid)?.to_owned();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let stream = net::internet_tcp(&host, port).await?;
    Ok(client_async_tls_with_config(request, stream, Some(config), None).await?)
}

pub async fn run<F: FnMut(String)>(gateway: SocketAddr, status: F) -> Result<bool, Error> {
    timeout(Duration::from_secs(120), relay(gateway, SERVER, status)).await?
}

async fn relay<F: FnMut(String)>(
    gateway: SocketAddr,
    server: &str,
    mut status: F,
) -> Result<bool, Error> {
    let mut car = timeout(Duration::from_secs(10), net::car_tcp(gateway)).await??;
    car.set_nodelay(true)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME + 16))
        .max_frame_size(Some(MAX_FRAME + 16));
    let (mut ws, _) = timeout(Duration::from_secs(15), open_service(server, config)).await??;
    for frame in [
        encode_client(6, &[b"0.1.0"])?,
        encode_client(1, &[b"", b"", b""])?,
        encode_client(2, &[b"ssh_enable"])?,
    ] {
        ws.send(Message::Binary(frame.into())).await?;
    }
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        tokio::select! {
            incoming = ws.next() => match incoming {
                Some(Ok(Message::Binary(bytes))) => match decode_server(&bytes).map_err(|reason| {
                    io::Error::new(io::ErrorKind::InvalidData, format!(
                        "Invalid activation response: {reason} (tag {:?}, {} bytes)",
                        bytes.first().copied(), bytes.len()
                    ))
                })? {
                    ServerMessage::Data(frame) => {
                        if frame.len() > MAX_FRAME + 6 { return Err(invalid().into()); }
                        car.write_all(&frame).await?;
                    }
                    ServerMessage::Status { text, .. } => status(text),
                    ServerMessage::Flag(success) => {
                        let _ = ws.close(None).await;
                        return Ok(success);
                    }
                },
                Some(Ok(Message::Ping(bytes))) => ws.send(Message::Pong(bytes)).await?,
                Some(Ok(Message::Pong(_))) => {},
                Some(Ok(Message::Close(_))) | None => {
                    return Err(io::Error::other("the activation service closed the connection").into());
                }
                Some(Ok(Message::Text(_))) => return Err(invalid().into()),
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error.into()),
            },
            read = car.read(&mut chunk) => {
                let count = read?;
                if count == 0 {
                    // Tell the service the vehicle side ended, as the original client does.
                    let _ = ws.send(Message::Binary(encode_client(5, &[])?.into())).await;
                    return Err(io::Error::other("the vehicle gateway closed the connection").into());
                }
                buffered.extend_from_slice(&chunk[..count]);
                while let Some(frame) = take_frame(&mut buffered)? {
                    ws.send(Message::Binary(encode_client(3, &[&frame])?.into())).await?;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "activation_error_tests.rs"]
mod error_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_tungstenite::accept_async;

    #[test]
    fn frames_are_bounded_and_reassembled() {
        let mut buffer = vec![0, 0, 0, 2, 0, 1, 0xf4];
        assert!(take_frame(&mut buffer).unwrap().is_none());
        buffer.extend([0x63, 0, 0, 0, 0, 0, 0x11]);
        assert_eq!(
            take_frame(&mut buffer).unwrap().unwrap(),
            [0, 0, 0, 2, 0, 1, 0xf4, 0x63]
        );
        assert_eq!(
            take_frame(&mut buffer).unwrap().unwrap(),
            [0, 0, 0, 0, 0, 0x11]
        );
        assert!(take_frame(&mut vec![255, 255, 255, 255, 0, 0]).is_err());
    }

    #[test]
    fn no_answer_names_the_searched_addresses_and_the_adapter_case() {
        let text = no_answer(
            &[
                Ipv4Addr::new(169, 254, 1, 10),
                Ipv4Addr::new(192, 168, 16, 120),
            ],
            false,
        );
        assert!(
            text.starts_with(
                "No vehicle gateway answered (asked from 169.254.1.10, 192.168.16.120)."
            )
        );
        assert!(text.contains("Connection settings"));
        assert!(no_answer(&[], false).starts_with("No vehicle gateway answered. Check"));
        let phone = no_answer(&[Ipv4Addr::new(192, 168, 16, 120)], true);
        assert!(phone.contains("adapter's Wi-Fi") && !phone.contains("computer"));
    }

    #[tokio::test]
    async fn relays_mock_session_without_capture() {
        let car_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = car_listener.local_addr().unwrap();
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let car = tokio::spawn(async move {
            let (mut socket, _) = car_listener.accept().await.unwrap();
            let mut request = [0; 8];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [0, 0, 0, 2, 0, 1, 0xf4, 0x63]);
            socket.write_all(&[0, 0, 0, 2, 0, 1, 0x63]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            socket.write_all(&[0xf4]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let server = tokio::spawn(async move {
            let (socket, _) = ws_listener.accept().await.unwrap();
            let mut ws = accept_async(socket).await.unwrap();
            for expected in [
                encode_client(6, &[b"0.1.0"]).unwrap(),
                encode_client(1, &[b"", b"", b""]).unwrap(),
                encode_client(2, &[b"ssh_enable"]).unwrap(),
            ] {
                assert_eq!(
                    ws.next().await.unwrap().unwrap().into_data().as_ref(),
                    expected
                );
            }
            ws.send(Message::Binary(
                vec![0x81, 0, 0, 0, 8, 0, 0, 0, 2, 0, 1, 0xf4, 0x63].into(),
            ))
            .await
            .unwrap();
            assert_eq!(
                ws.next().await.unwrap().unwrap().into_data().as_ref(),
                encode_client(3, &[&[0, 0, 0, 2, 0, 1, 0x63, 0xf4]]).unwrap()
            );
            ws.send(Message::Binary(vec![0x83, 1].into()))
                .await
                .unwrap();
        });
        assert!(
            relay(gateway, &format!("ws://{ws_addr}"), |_| {})
                .await
                .unwrap()
        );
        car.await.unwrap();
        server.await.unwrap();
    }
}
