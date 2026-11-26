// SPDX-License-Identifier: MPL-2.0
//! The installer for the phone app: the Windows installer's SSH session,
//! gateway search, activation relay and embedded release (`nbt-installer-core`)
//! behind a small UniFFI interface. Kotlin (Swift later) starts a [`Session`]
//! and receives [`InstallerEvent`]s through a [`Listener`].
//!
//! The phone never approves anything on the car's behalf: enabling SSH needs
//! the user's answer to `ConfirmActivation`, as in the Windows installer, and
//! nothing here installs without the user pressing Install or Update.

use nbt_installer_core::{bench, bundle, net, ssh};
use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex, mpsc},
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use zeroize::Zeroizing;

uniffi::setup_scaffolding!();

/// The manager release built into this app.
#[derive(uniffi::Record)]
pub struct Release {
    pub available: bool,
    pub version: String,
    pub sequence: u64,
    /// One line for the log: verified, missing or failed.
    pub message: String,
    pub test_app: bool,
}

#[uniffi::export]
pub fn bundled_release() -> Release {
    let (bundle, message) = bundle::load();
    Release {
        available: bundle.is_some(),
        version: bundle
            .as_ref()
            .map(|b| b.version.clone())
            .unwrap_or_default(),
        sequence: bundle.as_ref().map(|b| b.sequence).unwrap_or(0),
        message,
        test_app: bundle::test_app().is_some(),
    }
}

/// Where and how to log in. The defaults are the NBT-EVO's published ones.
#[derive(uniffi::Record)]
pub struct Connection {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// The vehicle gateway's address; `None` searches for it.
    pub gateway: Option<String>,
}

#[uniffi::export]
pub fn default_connection() -> Connection {
    Connection {
        host: nbt_installer_core::DEFAULT_HOST.into(),
        port: nbt_installer_core::DEFAULT_PORT,
        user: nbt_installer_core::DEFAULT_USER.into(),
        password: nbt_installer_core::DEFAULT_PASSWORD.into(),
        gateway: None,
    }
}

/// The documented controller settings, to prefill the editor.
#[uniffi::export]
pub fn controller_config_example() -> String {
    bench::CONFIG_EXAMPLE.into()
}

/// One IPv4 address of the car's network and its prefix length.
#[derive(uniffi::Record)]
pub struct Address {
    pub ip: String,
    pub prefix: u8,
}

/// Android network handles (`Network.getNetworkHandle()`): the Wi-Fi ENET
/// adapter's Wi-Fi for the car, mobile data for the activation service.
#[derive(uniffi::Record)]
pub struct Networks {
    pub car: Option<u64>,
    pub car_addresses: Vec<Address>,
    pub internet: Option<u64>,
}

/// `reason`, not `message`: Kotlin's exceptions already have a `message`.
#[derive(Debug, uniffi::Error)]
pub enum InstallerError {
    Invalid { reason: String },
}

impl std::fmt::Display for InstallerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { reason } => f.write_str(reason),
        }
    }
}

impl std::error::Error for InstallerError {}

fn invalid(message: &str) -> InstallerError {
    InstallerError::Invalid {
        reason: message.into(),
    }
}

/// The same checks as the Windows installer's Connect.
fn settings(connection: Connection) -> Result<ssh::Settings, InstallerError> {
    let host = connection.host.trim().to_ascii_lowercase();
    let user = connection.user.trim().to_owned();
    if !ssh::valid_host(&host)
        || connection.port == 0
        || user.is_empty()
        || user.len() > 128
        || user.chars().any(char::is_control)
        || connection.password.is_empty()
        || connection.password.len() > 1024
    {
        return Err(invalid(
            "Enter a valid address, port, user name and password.",
        ));
    }
    let gateway = match connection.gateway.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(
            text.parse::<Ipv4Addr>()
                .ok()
                .filter(|ip| !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast() && !ip.is_loopback())
                .ok_or_else(|| invalid("Enter the gateway address as four numbers, such as 192.168.16.105, or leave it empty."))?,
        ),
    };
    Ok(ssh::Settings {
        host,
        port: connection.port,
        user,
        password: Zeroizing::new(connection.password),
        gateway,
    })
}

fn networks(networks: Networks) -> Result<net::Networks, InstallerError> {
    let mut addresses = Vec::new();
    for address in networks.car_addresses {
        let ip = address
            .ip
            .parse::<Ipv4Addr>()
            .map_err(|_| invalid("Invalid network address."))?;
        if address.prefix > 32 {
            return Err(invalid("Invalid network prefix."));
        }
        let mask = u32::MAX
            .checked_shl(32 - u32::from(address.prefix))
            .unwrap_or(0);
        addresses.push((ip, Ipv4Addr::from_bits(mask)));
    }
    Ok(net::Networks {
        car: networks.car,
        car_addresses: (networks.car.is_some() || !addresses.is_empty()).then_some(addresses),
        internet: networks.internet,
    })
}

/// What the session reports, as `ssh::Event` without its reply channel.
#[derive(uniffi::Enum)]
pub enum InstallerEvent {
    Message {
        text: String,
    },
    /// SSH did not answer; ask the user, then call `confirm_activation`.
    ConfirmActivation {
        host: String,
        hint: Option<String>,
    },
    /// NBTApps is on the head unit; ask whether to move it aside, then call
    /// `confirm_remove_nbtapps` (no stops the Install or Update).
    ConfirmRemoveNbtApps {
        agent: bool,
        plugin: bool,
    },
    Note {
        text: String,
    },
    /// Diagnostic detail, shown only with the debug log on.
    Detail {
        text: String,
    },
    Phase {
        index: u32,
        count: u32,
        label: String,
        bytes: u64,
    },
    Connected,
    Target {
        ok: bool,
    },
    Progress {
        sent: u64,
        total: u64,
    },
    InstallFinished {
        ok: bool,
    },
    UpdateFinished {
        ok: bool,
    },
    /// The head unit has to restart to load the new plugin; offer a restart.
    RestartNeeded,
    ToolFinished {
        ok: bool,
        message: String,
    },
    LogFetched {
        system: bool,
        data: Vec<u8>,
    },
    /// The "Collect everything" report, to save as one file.
    Collected {
        data: Vec<u8>,
    },
    /// The session ended; nothing follows.
    Finished {
        message: String,
    },
}

#[uniffi::export(callback_interface)]
pub trait Listener: Send + Sync {
    fn on_event(&self, event: InstallerEvent);
}

/// A test tool, as in the Windows installer's Test tools.
#[derive(uniffi::Enum)]
pub enum Tool {
    LogOn,
    LogOff,
    FetchLog,
    FetchSystemLog,
    ProvisionTestApp,
    WriteConfig {
        text: String,
    },
    RemoveConfig,
    RemoveAll,
    RestartIdrive,
    CollectAll,
    /// The automatic capture shim on again (after it failed or was turned off),
    /// or off, as the Windows installer's Shim on and Shim off.
    Shim {
        on: bool,
    },
}

#[derive(uniffi::Object)]
pub struct Session {
    commands: UnboundedSender<ssh::Command>,
    confirmation: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
}

#[uniffi::export]
impl Session {
    /// Connects in the background; events follow until `Finished`.
    #[uniffi::constructor]
    pub fn connect(
        connection: Connection,
        networks: Networks,
        listener: Box<dyn Listener>,
    ) -> Result<Arc<Self>, InstallerError> {
        let settings = settings(connection)?;
        net::configure(self::networks(networks)?);
        let (send, receive) = mpsc::channel();
        let commands = ssh::start(settings, send);
        let confirmation = Arc::new(Mutex::new(None));
        let pending = confirmation.clone();
        std::thread::spawn(move || {
            while let Ok(event) = receive.recv() {
                let finished = matches!(event, ssh::Event::Finished(_));
                listener.on_event(convert(event, &pending));
                if finished {
                    break;
                }
            }
        });
        Ok(Arc::new(Self {
            commands,
            confirmation,
        }))
    }

    /// The user's answer to `ConfirmRemoveNbtApps` (the same open question).
    pub fn confirm_remove_nbtapps(&self, approve: bool) {
        self.confirm_activation(approve);
    }

    /// The user's answer to `ConfirmActivation`.
    pub fn confirm_activation(&self, approve: bool) {
        if let Some(respond) = self
            .confirmation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = respond.send(approve);
        }
    }

    /// Installs the bundled release; false when there is none or the session ended.
    pub fn install(&self) -> bool {
        bundle::load()
            .0
            .is_some_and(|bundle| self.commands.send(bundle.install()).is_ok())
    }

    /// Stages the bundled release on an existing installation.
    pub fn update(&self) -> bool {
        bundle::load()
            .0
            .is_some_and(|bundle| self.commands.send(bundle.update()).is_ok())
    }

    pub fn tool(&self, tool: Tool) -> bool {
        let action = match tool {
            Tool::LogOn => bench::Action::LogOn,
            Tool::LogOff => bench::Action::LogOff,
            Tool::FetchLog => bench::Action::FetchLog,
            Tool::FetchSystemLog => bench::Action::FetchSystemLog,
            Tool::ProvisionTestApp => bench::Action::ProvisionTestApp,
            Tool::WriteConfig { text } => bench::Action::WriteConfig(text),
            Tool::RemoveConfig => bench::Action::RemoveConfig,
            Tool::RemoveAll => bench::Action::RemoveAll,
            Tool::RestartIdrive => bench::Action::RestartIdrive,
            Tool::CollectAll => bench::Action::CollectAll,
            Tool::Shim { on } => bench::Action::Shim(on),
        };
        self.commands
            .send(ssh::Command::Bench {
                action,
                test_app: bundle::test_app(),
            })
            .is_ok()
    }

    /// Ends the session, also while connecting or waiting for an answer.
    pub fn disconnect(&self) {
        self.confirm_activation(false);
        let _ = self.commands.send(ssh::Command::Disconnect);
    }
}

fn convert(event: ssh::Event, pending: &Mutex<Option<oneshot::Sender<bool>>>) -> InstallerEvent {
    use ssh::Event as E;
    match event {
        E::Message(text) => InstallerEvent::Message { text: text.into() },
        E::ConfirmActivation {
            host,
            hint,
            respond,
        } => {
            *pending.lock().unwrap_or_else(|p| p.into_inner()) = Some(respond);
            InstallerEvent::ConfirmActivation { host, hint }
        }
        E::ConfirmRemoveNbtApps {
            agent,
            plugin,
            respond,
        } => {
            *pending.lock().unwrap_or_else(|p| p.into_inner()) = Some(respond);
            InstallerEvent::ConfirmRemoveNbtApps { agent, plugin }
        }
        E::Note(text) => InstallerEvent::Note { text },
        E::Detail(text) => InstallerEvent::Detail { text },
        E::Phase {
            index,
            count,
            label,
            bytes,
        } => InstallerEvent::Phase {
            index: index as u32,
            count: count as u32,
            label,
            bytes: bytes as u64,
        },
        E::Connected => InstallerEvent::Connected,
        E::Target(ok) => InstallerEvent::Target { ok },
        E::Progress { sent, total } => InstallerEvent::Progress {
            sent: sent as u64,
            total: total as u64,
        },
        E::InstallFinished(ok) => InstallerEvent::InstallFinished { ok },
        E::UpdateFinished(ok) => InstallerEvent::UpdateFinished { ok },
        E::RestartNeeded => InstallerEvent::RestartNeeded,
        E::BenchFinished { ok, message } => InstallerEvent::ToolFinished { ok, message },
        E::LogFetched { system, data } => InstallerEvent::LogFetched { system, data },
        E::Collected(data) => InstallerEvent::Collected { data },
        E::Finished(text) => InstallerEvent::Finished {
            message: text.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_fields_are_checked_as_on_the_computer() {
        let ok = settings(default_connection()).unwrap();
        assert_eq!(ok.host, "169.254.199.119");
        assert_eq!(ok.gateway, None);
        let typed = Connection {
            gateway: Some(" 192.168.16.105 ".into()),
            ..default_connection()
        };
        assert_eq!(
            settings(typed).unwrap().gateway,
            Some(Ipv4Addr::new(192, 168, 16, 105))
        );
        for bad in [
            Connection {
                host: "head unit".into(),
                ..default_connection()
            },
            Connection {
                port: 0,
                ..default_connection()
            },
            Connection {
                password: String::new(),
                ..default_connection()
            },
            Connection {
                gateway: Some("255.255.255.255".into()),
                ..default_connection()
            },
            Connection {
                gateway: Some("gateway".into()),
                ..default_connection()
            },
        ] {
            assert!(settings(bad).is_err());
        }
    }

    #[test]
    fn prefixes_become_netmasks() {
        let converted = networks(Networks {
            car: Some(7),
            car_addresses: vec![
                Address {
                    ip: "192.168.16.120".into(),
                    prefix: 24,
                },
                Address {
                    ip: "10.0.0.1".into(),
                    prefix: 0,
                },
            ],
            internet: None,
        })
        .unwrap();
        assert_eq!(
            converted.car_addresses.unwrap(),
            [
                (
                    Ipv4Addr::new(192, 168, 16, 120),
                    Ipv4Addr::new(255, 255, 255, 0)
                ),
                (Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::UNSPECIFIED),
            ]
        );
        assert!(
            networks(Networks {
                car: None,
                car_addresses: vec![Address {
                    ip: "1.2.3.4".into(),
                    prefix: 33
                }],
                internet: None
            })
            .is_err()
        );
        // Nothing configured: the computer's own interfaces and routes.
        assert!(
            networks(Networks {
                car: None,
                car_addresses: vec![],
                internet: None
            })
            .unwrap()
            .car_addresses
            .is_none()
        );
    }

    #[test]
    fn a_refused_activation_answer_reaches_the_session() {
        let pending = Mutex::new(None);
        let (respond, mut answer) = oneshot::channel();
        let event = convert(
            ssh::Event::ConfirmActivation {
                host: "h:22".into(),
                hint: None,
                respond,
            },
            &pending,
        );
        assert!(matches!(event, InstallerEvent::ConfirmActivation { .. }));
        pending.lock().unwrap().take().unwrap().send(false).unwrap();
        assert_eq!(answer.try_recv(), Ok(false));
    }
}
