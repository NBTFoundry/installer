//! Check the actual SSH endpoint after the one approved activation attempt.
use super::{Command, Event, SEARCH_AFTER_ACTIVATION, Settings, connect_elsewhere, connect_tcp};
use std::{net::Ipv4Addr, sync::mpsc, time::Duration};
use tokio::{net::TcpStream, sync::mpsc::UnboundedReceiver};

pub(super) async fn after_activation(
    activation: Result<bool, String>,
    settings: &mut Settings,
    events: &mpsc::Sender<Event>,
    commands: &mut UnboundedReceiver<Command>,
    gateway: Ipv4Addr,
    direct: bool,
) -> Result<TcpStream, &'static str> {
    let confirmed = match activation {
        Ok(true) => true,
        Ok(false) => return Err("The activation service reported failure; SSH is not enabled."),
        Err(error) => {
            let _ = events.send(Event::Note(format!(
                "Activation did not complete: {error}. Checking whether SSH became reachable…"
            )));
            false
        }
    };
    if confirmed {
        let _ = events.send(Event::Message("SSH enabled; connecting…"));
    }
    let mut result = None;
    for _ in 0..if direct { 10 } else { 0 } {
        tokio::select! {
            attempt = connect_tcp(&settings.host, settings.port) => if attempt.is_some() { result = attempt; break; },
            _ = commands.recv() => return Err("Connection cancelled."),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            _ = commands.recv() => return Err("Connection cancelled."),
        }
    }
    if result.is_none() {
        let patience = if direct {
            Duration::ZERO
        } else {
            SEARCH_AFTER_ACTIVATION
        };
        result = tokio::select! {
            found = connect_elsewhere(settings, events, vec![gateway], patience) => found,
            _ = commands.recv() => return Err("Connection cancelled."),
        };
    }
    if result.is_none() && !direct {
        let _ = events.send(Event::Note(format!(
            "Nothing on the car's network accepted SSH within {} seconds. If you \
             know the head unit's address on the adapter's network, type it under \
             Connection settings and press Connect.",
            SEARCH_AFTER_ACTIVATION.as_secs()
        )));
    }
    result.ok_or(if confirmed {
        "Activation reported success, but SSH did not become reachable."
    } else {
        "Activation did not complete and SSH did not become reachable."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use zeroize::Zeroizing;

    #[tokio::test]
    async fn protocol_error_after_enabling_ssh_still_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = Settings {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            user: "root".into(),
            password: Zeroizing::new(String::new()),
            gateway: None,
        };
        let (events, notes) = mpsc::channel();
        let (_send, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let connected = after_activation(
            Err("Invalid activation response".into()),
            &mut settings,
            &events,
            &mut commands,
            Ipv4Addr::LOCALHOST,
            true,
        )
        .await;
        assert!(connected.is_ok(), "SSH is already accepting: {connected:?}");
        let notes: Vec<_> = notes.try_iter().collect();
        assert!(notes.iter().any(|event| matches!(event, Event::Note(text) if text.contains("Invalid activation response"))));
        assert!(
            !notes
                .iter()
                .any(|event| matches!(event, Event::Message("SSH enabled; connecting…")))
        );
        assert!(listener.accept().await.is_ok());
    }

    #[tokio::test]
    async fn explicit_refusal_does_not_attempt_ssh() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = Settings {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            user: "root".into(),
            password: Zeroizing::new(String::new()),
            gateway: None,
        };
        let (events, _notes) = mpsc::channel();
        let (_send, mut commands) = tokio::sync::mpsc::unbounded_channel();
        assert!(
            after_activation(
                Ok(false),
                &mut settings,
                &events,
                &mut commands,
                Ipv4Addr::LOCALHOST,
                true
            )
            .await
            .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_activation_check_can_be_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut settings = Settings {
            host: "127.0.0.1".into(),
            port,
            user: "root".into(),
            password: Zeroizing::new(String::new()),
            gateway: None,
        };
        let (events, _notes) = mpsc::channel();
        let (send, mut commands) = tokio::sync::mpsc::unbounded_channel();
        send.send(Command::Disconnect).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            after_activation(
                Err("Invalid activation response".into()),
                &mut settings,
                &events,
                &mut commands,
                Ipv4Addr::LOCALHOST,
                true,
            ),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err(), "Connection cancelled.");
    }
}
