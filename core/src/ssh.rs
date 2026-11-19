#[path = "ssh_reconnect.rs"]
mod reconnect;

use crate::{activation, bench, discovery, sweep};
use rand::TryRng;
use russh::{
    ChannelMsg, Preferred, client,
    keys::{HashAlg, PublicKeyOrCertificate},
    mac,
};
use std::{
    borrow::Cow,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc as async_mpsc, oneshot},
};
use zeroize::Zeroizing;

pub enum Event {
    Message(&'static str),
    /// SSH did not answer at `host`; ask whether to enable it. `hint` says
    /// why the address is unreachable from this computer, when it is.
    ConfirmActivation {
        host: String,
        hint: Option<String>,
        respond: oneshot::Sender<bool>,
    },
    /// A message with run-time detail (activation service status, errors).
    Note(String),
    /// Diagnostic detail (commands, exit status, error text, host key),
    /// shown only with the debug log on.
    Detail(String),
    /// Step `index` (from 0) of `count` in the current operation begins;
    /// `bytes` is what it sends (0 for none). Progress events that follow
    /// belong to this step.
    Phase {
        index: usize,
        count: usize,
        label: String,
        bytes: usize,
    },
    Connected,
    Target(bool),
    Progress {
        sent: usize,
        total: usize,
    },
    UpdateFinished(bool),
    InstallFinished(bool),
    /// The head unit has to restart to load the plugin just installed or replaced.
    /// Sent before the `InstallFinished` or `UpdateFinished` it belongs to.
    RestartNeeded,
    /// A bench action ended; the message says how.
    BenchFinished {
        ok: bool,
        message: String,
    },
    /// The head unit's debug log (debug.log.1 then debug.log), or with
    /// `system` its QNX system log (sloginfo).
    LogFetched {
        system: bool,
        data: Vec<u8>,
    },
    /// The "Collect everything" report (collect.rs), to save as one file.
    Collected(Vec<u8>),
    Finished(&'static str),
}

/// The test application's first package, embedded by the kit build.
#[derive(Clone)]
pub struct TestApp {
    pub package: &'static [u8],
    pub key: &'static [u8],
    pub compatibility: String,
    pub sha256: String,
}

pub enum Command {
    Disconnect,
    Install {
        launcher: &'static [u8],
        release: &'static [u8],
        key: &'static [u8],
        sha256: String,
        compatibility: String,
    },
    /// Stages the release with its own launcher, uploaded for the purpose
    /// (`--stage-update`), which then replaces the installed launcher and the
    /// iDrive plugin (`--refresh-setup`): the installed launcher's age never
    /// matters, so one release serves every car.
    Update {
        launcher: &'static [u8],
        release: &'static [u8],
        sha256: String,
    },
    Bench {
        action: bench::Action,
        test_app: Option<TestApp>,
    },
}

pub struct Settings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Zeroizing<String>,
    /// The vehicle gateway for enabling SSH; `None` searches the cable.
    pub gateway: Option<std::net::Ipv4Addr>,
}

pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 120
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'))
}

/// One bounded attempt at the SSH port, on the car's network (`net`).
async fn connect_tcp(host: &str, port: u16) -> Option<tokio::net::TcpStream> {
    let attempt = async {
        for address in tokio::net::lookup_host((host, port)).await.ok()? {
            if let Ok(stream) = crate::net::car_tcp(address).await {
                return Some(stream);
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_secs(10), attempt)
        .await
        .ok()
        .flatten()
}

/// Looks for the head unit on the car's own networks and connects to it, when
/// the configured address does not answer.
///
/// The car's modules are DHCP clients, so behind a Wi-Fi ENET adapter the head
/// unit holds an address on the adapter's network as well as its link-local
/// one, and this computer holds one there and none on `169.254`. The HSFZ
/// search says which networks the car is on; only those are searched.
///
/// `known` is a gateway already found or typed; the search runs when it is
/// empty or not on a network here. The networks are searched again every
/// `SEARCH_INTERVAL` until something accepts or `patience` has passed: just
/// after SSH is enabled, the head unit can take a while to accept.
/// On success `settings.host` becomes the address that answered.
async fn connect_elsewhere(
    settings: &mut Settings,
    events: &mpsc::Sender<Event>,
    known: Vec<std::net::Ipv4Addr>,
    patience: Duration,
) -> Option<tokio::net::TcpStream> {
    let skip = settings.host.parse::<std::net::Ipv4Addr>().ok();
    let mut gateways = known;
    let mut targets = sweep::around(&gateways, skip);
    if targets.is_empty() {
        gateways = car_gateways().await;
        targets = sweep::around(&gateways, skip);
    }
    if targets.is_empty() {
        return None;
    }
    let _ = events.send(Event::Message(
        "Looking for the head unit on the car's network…",
    ));
    let waiting = if patience.is_zero() {
        String::new()
    } else {
        format!(", for up to {} seconds", patience.as_secs())
    };
    let _ = events.send(Event::Detail(format!(
        "Vehicle gateway {}; searching {} addresses beside it for the SSH port{waiting}",
        list(&gateways),
        targets.len()
    )));
    let found = sweep::wait(settings.port, &targets, patience, SEARCH_INTERVAL).await;
    let first = *found.first()?;
    let _ = events.send(Event::Note(format!(
        "No answer at {}; SSH answers at {}. Using {first}.",
        settings.host,
        list(&found)
    )));
    settings.host = first.to_string();
    connect_tcp(&settings.host, settings.port).await
}

/// How long the search keeps looking after SSH was enabled, and how often.
const SEARCH_AFTER_ACTIVATION: Duration = Duration::from_secs(60);
const SEARCH_INTERVAL: Duration = Duration::from_secs(2);

/// The vehicle gateways answering the HSFZ search; empty when none does.
async fn car_gateways() -> Vec<std::net::Ipv4Addr> {
    match tokio::task::spawn_blocking(|| discovery::discover(Duration::from_secs(2))).await {
        Ok(Ok(search)) => search.gateways.iter().map(|gateway| gateway.ip).collect(),
        _ => Vec::new(),
    }
}

/// Whether ordinary routing can reach `host` from here. The head unit's
/// link-local address cannot be reached without a `169.254` address of our
/// own, as on a phone or behind a Wi-Fi ENET adapter: waiting on it only
/// delays the search.
fn reachable(host: &str) -> bool {
    !discovery::needs_link_local(&discovery::local_addresses(), host)
}

fn list(addresses: &[std::net::Ipv4Addr]) -> String {
    addresses
        .iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why the head unit is unreachable whatever SSH is doing, when this device
/// has no address on its network; `None` when ordinary routing should reach it.
///
/// The built-in address is the head unit's link-local one, which is what the
/// ENET cable gives it. Behind a Wi-Fi ENET adapter this device takes a DHCP
/// address from the adapter instead and has no `169.254` address, so the
/// connection fails at once and enabling SSH at that address cannot help; the
/// head unit has to be found on the adapter's network (`connect_elsewhere`).
fn unreachable_address(host: &str) -> Option<String> {
    let locals = discovery::local_addresses();
    if !discovery::needs_link_local(&locals, host) {
        return None;
    }
    // A phone cannot add addresses: say what to check instead.
    if crate::net::is_phone() {
        return Some(format!(
            "{host} is a link-local address, which a phone cannot reach over Wi-Fi, and the head unit \
             was not found on the adapter's network. Check that the phone is on the Wi-Fi ENET \
             adapter's Wi-Fi and the adapter is plugged into the car with the ignition on. If SSH \
             is still off, enable it: the search runs again afterwards."
        ));
    }
    Some(format!(
        "{host} is a link-local address and this computer has none, so nothing here can reach \
             the head unit, whether or not SSH is enabled. Give this network connection a fixed \
             address: 169.254.1.10, mask 255.255.0.0, gateway empty. Windows only adds addresses \
             once DHCP is off. Enabling SSH also needs the car's gateway, which answers on the \
             adapter's own network, so add a second fixed address under Advanced, for example \
             192.168.16.50 with mask 255.255.255.0."
    ))
}

const REMOTE_SYSTEM: &str = "uname -s";

/* Waits long enough for a slow link. A Wi-Fi ENET adapter bridges the car's
 * Ethernet onto Wi-Fi, which adds latency to every round trip of the old
 * sshd's handshake and of each channel; the car session of 2026-11-15 had
 * OpenSSH connecting while the installer gave up. Nothing is gained by
 * waiting less: every wait below is in a `select!` with the command channel,
 * so Disconnect and Cancel end it at once. */
const AUTH_SECONDS: u64 = 60;
const PROBE_SECONDS: u64 = 60;

/* Every command runs as if typed in an SSH session: in a login shell, which
 * reads the profile and so has the session's PATH and LD_LIBRARY_PATH. The
 * shell sshd starts for a command reads no profile (dev20, dev22:
 * OnOffDSICommander not found, then its libraries not found). Commands are
 * fixed text and validated values without single quotes. */
fn login_shell(command: &str) -> Option<String> {
    (!command.contains('\'')).then(|| format!("/bin/sh -l -c '{command}'"))
}

/* Counts the bytes written to the SSH socket. Upload progress follows what
 * has left the computer, not what was queued: the SSH window lets megabytes
 * queue ahead of a slow link. */
struct Counted<S> {
    inner: S,
    written: Arc<AtomicUsize>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(bytes)) = result {
            this.written.fetch_add(bytes, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// An authenticated session and its socket's byte counter.
struct Link {
    session: client::Handle<Handler>,
    wire: Arc<AtomicUsize>,
}

/// Standard error kept per command, for the log; the rest is dropped.
const STDERR_KEPT: usize = 4096;

/// What one remote command did, kept whole so the log can show it.
#[derive(Default)]
struct Output {
    exit: Option<u32>,
    signal: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Output beyond the kept bytes was dropped.
    truncated: bool,
}

fn keep(buffer: &mut Vec<u8>, data: &[u8], limit: usize, truncated: &mut bool) {
    let room = limit.saturating_sub(buffer.len());
    if data.len() > room {
        *truncated = true;
    }
    buffer.extend_from_slice(&data[..data.len().min(room)]);
}

/// Bytes from the head unit as one quoted, escaped line (at most 2 KiB shown).
fn printable(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(none)".into();
    }
    let shown = &bytes[..bytes.len().min(2048)];
    let mut text = format!("{:?}", String::from_utf8_lossy(shown));
    if shown.len() < bytes.len() {
        text.push('…');
    }
    text
}

impl Output {
    fn record(&mut self, message: ChannelMsg, max_stdout: usize) {
        match message {
            ChannelMsg::Data { data } => {
                keep(&mut self.stdout, &data, max_stdout, &mut self.truncated)
            }
            ChannelMsg::ExtendedData { data, .. } => {
                keep(&mut self.stderr, &data, STDERR_KEPT, &mut self.truncated)
            }
            ChannelMsg::ExitStatus { exit_status } => {
                self.exit.get_or_insert(exit_status);
            }
            ChannelMsg::ExitSignal {
                signal_name,
                error_message,
                ..
            } => {
                self.signal = Some(format!("{signal_name:?} {error_message}"));
            }
            _ => {}
        }
    }

    /// Exit status 0, no signal and exactly `expected` on standard output.
    fn succeeded(&self, expected: &[u8]) -> bool {
        self.exit == Some(0) && self.signal.is_none() && !self.truncated && self.stdout == expected
    }

    fn describe(&self) -> String {
        let mut text = match self.exit {
            Some(code) => format!("exit status {code}"),
            None => "no exit status".into(),
        };
        if let Some(signal) = &self.signal {
            text += &format!(", signal {signal}");
        }
        text += &format!(
            "; stdout {}; stderr {}",
            printable(&self.stdout),
            printable(&self.stderr)
        );
        if self.truncated {
            text += " (output truncated)";
        }
        text
    }
}

/* Runs `command`, streams `inputs` to its standard input followed by EOF and
 * collects its output until the channel closes. Output is read while input
 * is still being sent, so a command that exits early is reported with its
 * exit status and error text instead of as a stalled upload. An error names
 * the step that broke and includes what the command printed by then.
 * `progress` receives the input bytes that have reached the socket so far
 * (bounded by those queued), at most five times a second. */
async fn execute(
    link: &Link,
    command: &str,
    inputs: &[&[u8]],
    max_stdout: usize,
    seconds: u64,
    mut progress: impl FnMut(usize),
) -> Result<Output, String> {
    let session = &link.session;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let channel = match tokio::time::timeout_at(deadline, session.channel_open_session()).await {
        Ok(Ok(channel)) => channel,
        Ok(Err(error)) => return Err(format!("could not open an SSH channel: {error}")),
        Err(_) => return Err("timed out opening an SSH channel".into()),
    };
    let (mut reader, writer) = channel.split();
    let Some(command) = login_shell(command) else {
        return Err("command contains a single quote".into());
    };
    match tokio::time::timeout_at(deadline, writer.exec(true, command)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(format!("could not send the command: {error}")),
        Err(_) => return Err("timed out sending the command".into()),
    }
    let mut output = Output::default();
    // Input starts once the server accepts the command; anything else that
    // arrives first (window adjustments, early output) is kept or skipped.
    loop {
        match tokio::time::timeout_at(deadline, reader.wait()).await {
            Err(_) => {
                return Err(format!(
                    "no answer to the command request; {}",
                    output.describe()
                ));
            }
            Ok(None | Some(ChannelMsg::Close)) => {
                return Err(format!(
                    "the channel closed before the command started; {}",
                    output.describe()
                ));
            }
            Ok(Some(ChannelMsg::Failure)) => {
                return Err("the head unit refused to run the command".into());
            }
            Ok(Some(ChannelMsg::Success)) => break,
            Ok(Some(message)) => output.record(message, max_stdout),
        }
    }
    let total: usize = inputs.iter().map(|input| input.len()).sum();
    let start = link.wire.load(Ordering::Relaxed);
    let mut reported = 0usize;
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    let written = std::cell::Cell::new(0usize);
    let send = async {
        for input in inputs {
            for chunk in input.chunks(32768) {
                match tokio::time::timeout(
                    Duration::from_secs(30),
                    writer.data_bytes(chunk.to_vec()),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(format!("sending failed: {error}")),
                    Err(_) => {
                        return Err("the head unit accepted no data for 30 seconds".into());
                    }
                }
                written.set(written.get() + chunk.len());
            }
        }
        writer
            .eof()
            .await
            .map_err(|error| format!("could not end the input: {error}"))
    };
    let mut send = std::pin::pin!(send);
    let mut sending = true;
    let mut send_error = None;
    loop {
        tokio::select! {
            _ = ticker.tick(), if reported < total => {
                let on_wire = link
                    .wire
                    .load(Ordering::Relaxed)
                    .saturating_sub(start)
                    .min(written.get());
                if on_wire > reported {
                    reported = on_wire;
                    progress(on_wire);
                }
            }
            result = &mut send, if sending => {
                sending = false;
                if let Err(error) = result {
                    // Collect what the command says about it, briefly.
                    send_error = Some(error);
                    deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(5));
                }
            }
            message = tokio::time::timeout_at(deadline, reader.wait()) => match message {
                Err(_) => {
                    let reason = send_error.unwrap_or_else(|| format!("no end after {seconds} s"));
                    return Err(format!(
                        "{reason} ({} of {total} bytes sent); {}",
                        written.get(),
                        output.describe()
                    ));
                }
                Ok(None | Some(ChannelMsg::Close)) => break,
                Ok(Some(message)) => output.record(message, max_stdout),
            }
        }
    }
    // A command that exits with a failure before reading all its input refused
    // it (the launcher checks before it reads): its own output says why, and
    // the caller reports that, not a broken link.
    if written.get() < total && output.exit.map_or(true, |status| status == 0) {
        let reason = send_error.unwrap_or_else(|| "the command ended".into());
        return Err(format!(
            "{reason} after {} of {total} bytes; {}",
            written.get(),
            output.describe()
        ));
    }
    if reported < total {
        progress(total);
    }
    Ok(output)
}

async fn qnx_probe(link: &Link, command: &'static str, events: &mpsc::Sender<Event>) -> bool {
    let started = std::time::Instant::now();
    let result = execute(link, command, &[], 64, PROBE_SECONDS, |_| {}).await;
    let _ = events.send(Event::Detail(format!(
        "`{command}` took {:.1} s",
        started.elapsed().as_secs_f32()
    )));
    match result {
        Ok(output)
            if output.exit == Some(0)
                && (output.stdout == b"QNX\n" || output.stdout == b"QNX\r\n") =>
        {
            true
        }
        Ok(output) => {
            let _ = events.send(Event::Detail(format!("`{command}`: {}", output.describe())));
            false
        }
        Err(error) => {
            let _ = events.send(Event::Detail(format!("`{command}`: {error}")));
            false
        }
    }
}

/// The launcher's known refusals, explained; the raw text is logged as well.
/// Its reason is the last line: the login shell's profile can print first
/// (`/etc/profile[28]: bind: cannot bind, not a tty` on the car).
fn known_error(stderr: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stderr).ok()?.strip_suffix('\n')?;
    let text = text.rsplit('\n').next()?;
    if let Some(reason) = text.strip_prefix("HMI layout: ") {
        return Some(format!(
            "The head unit refused the iDrive plugin folder: {reason}."
        ));
    }
    if let Some(reason) = text.strip_prefix("manager storage: ") {
        return Some(format!(
            "The head unit refused the manager storage (/var/opt/nbt-manager): {reason}."
        ));
    }
    Some(match text {
        // The launcher found no /var/opt/nbt-manager (or not one it owns).
        "nbt-manager-launcher: managed root must be an owned, private directory" => {
            "NBT Manager is not installed on this head unit: use Install.".into()
        }
        "insufficient HMI storage" => "There is not enough space for the iDrive plugin.".into(),
        "unsupported platform" => "The head unit is not a supported QNX 6.5 ARMv7 host.".into(),
        "existing HMI plugin conflicts with release" => {
            "A different NBT Manager iDrive plugin is installed. Remove NBT Manager first.".into()
        }
        "manager operation is in progress" => {
            "NBT Manager is open or busy on the head unit. Close it and try again.".into()
        }
        "manager update awaiting confirmation" => "The manager update is not confirmed yet. Open \
            NBT Manager on the car, keep it open for 10 seconds, close it, then try again."
            .into(),
        "manager state requires recovery" => "NBT Manager's previous release on the head unit \
            failed verification. Remove NBT Manager, then install it again."
            .into(),
        "conflicting manager setup files" => {
            "NBT Manager is already installed with another release: use Update instead.".into()
        }
        "application already provisioned" => "This application is already installed. To replace it \
            with this kit's build, send the kit's phone package from Install or update in the manager, \
            or uninstall it in the manager (hold its name, then Uninstall) and install it again."
            .into(),
        _ => return None,
    })
}

fn phase(events: &mpsc::Sender<Event>, index: usize, count: usize, label: &str, bytes: usize) {
    let _ = events.send(Event::Phase {
        index,
        count,
        label: label.to_owned(),
        bytes,
    });
}

/* One logged command with its inputs; Ok when it exits 0 and prints exactly
 * `expected`, else an error naming the step with what the head unit said.
 * Commands carry only generated paths, sizes, digests and compatibility
 * tokens, never credentials. */
async fn step(
    link: &Link,
    label: &str,
    command: &str,
    inputs: &[&[u8]],
    expected: &[u8],
    seconds: u64,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let total: usize = inputs.iter().map(|input| input.len()).sum();
    let _ = events.send(Event::Detail(format!("{label}: {command}")));
    let output = match execute(link, command, inputs, 64, seconds, |done| {
        let _ = events.send(Event::Progress {
            sent: done.min(total),
            total,
        });
    })
    .await
    {
        Ok(output) => output,
        Err(error) => {
            let _ = events.send(Event::Detail(format!("{label} failed: {error}")));
            return Err(format!(
                "{label} failed: the connection broke or the head unit stopped answering."
            ));
        }
    };
    if output.succeeded(expected) {
        let _ = events.send(Event::Detail(format!("{label}: done.")));
        return Ok(());
    }
    let _ = events.send(Event::Detail(format!(
        "{label} failed: {} (expected stdout {})",
        output.describe(),
        printable(expected)
    )));
    Err(known_error(&output.stderr).unwrap_or_else(|| format!("{label} failed on the head unit.")))
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 32 lowercase hex digits from OS randomness.
fn nonce() -> Result<String, String> {
    let mut random = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut random)
        .map_err(|_| "No randomness available on this computer.".to_owned())?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/* The plugin folder is on a partition mounted read-only at boot; installing
 * and removing write to it, so they remount it first, every time. Whether it
 * worked is checked by the launcher (install) and by the removal command. */
async fn make_writable(link: &Link, events: &mpsc::Sender<Event>) -> Result<(), String> {
    step(
        link,
        "Making the iDrive plugin folder writable",
        bench::REMOUNT,
        &[],
        b"OK\n",
        30,
        events,
    )
    .await
    .map_err(|_| "The head unit refused to make the iDrive plugin folder writable.".to_owned())
}

/* Back to read-only, as after boot, whatever the outcome. A failure only
 * leaves the partition writable until the next reboot, so it is reported
 * as a note and does not change the operation's own result. */
async fn make_read_only(link: &Link, events: &mpsc::Sender<Event>) {
    if step(
        link,
        READ_ONLY_STEP,
        bench::REMOUNT_READ_ONLY,
        &[],
        b"OK\n",
        30,
        events,
    )
    .await
    .is_err()
    {
        let _ = events.send(Event::Note(
            "The iDrive plugin folder could not be made read-only again; it is read-only after the next reboot.".into(),
        ));
    }
}

const READ_ONLY_STEP: &str = "Making the iDrive plugin folder read-only again";

fn installer_valid(
    launcher: &[u8],
    release: &[u8],
    key: &[u8],
    digest: &str,
    compat: &str,
) -> bool {
    !launcher.is_empty()
        && launcher.len() <= 64 * 1024 * 1024
        && !key.is_empty()
        && key.len() <= 16384
        && !release.is_empty()
        && release.len() <= 128 * 1024 * 1024 + 4096 + 400
        && valid_digest(digest)
        && !compat.is_empty()
        && compat.len() <= 48
        && compat
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
}

const INSTALL_STEPS: [&str; 5] = [
    "Making the iDrive plugin folder writable",
    "Uploading the launcher",
    "Installing the manager and iDrive plugin",
    "Removing the staging folder",
    READ_ONLY_STEP,
];

/* Remount, upload the launcher to a private staging folder, let it verify
 * and install the signed release (it checks the head unit and names what it
 * refuses), always remove the staging folder, and always remount read-only.
 * The staging path is generated locally; every shell argument is fixed or
 * validated. */
async fn install(
    link: &Link,
    launcher: &'static [u8],
    release: &'static [u8],
    key: &'static [u8],
    digest: &str,
    compat: &str,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    if !installer_valid(launcher, release, key, digest, compat) {
        return Err(format!(
            "The bundled installer is invalid: launcher {} bytes, key {} bytes, release {} bytes, compatibility {compat:?}.",
            launcher.len(),
            key.len(),
            release.len()
        ));
    }
    let result = install_writable(link, launcher, release, key, digest, compat, events).await;
    phase(events, 4, INSTALL_STEPS.len(), INSTALL_STEPS[4], 0);
    make_read_only(link, events).await;
    result
}

async fn install_writable(
    link: &Link,
    launcher: &'static [u8],
    release: &'static [u8],
    key: &'static [u8],
    digest: &str,
    compat: &str,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let count = INSTALL_STEPS.len();
    let path = format!("/tmp/nbt-manager-{}", nonce()?);
    phase(events, 0, count, INSTALL_STEPS[0], 0);
    make_writable(link, events).await?;
    let _ = events.send(Event::Detail(format!(
        "Sending launcher ({} bytes), key ({} bytes) and release ({} bytes) to {path}.",
        launcher.len(),
        key.len(),
        release.len()
    )));
    phase(events, 1, count, INSTALL_STEPS[1], launcher.len());
    // A failed mkdir means the folder is not ours: nothing to clean up.
    step(
        link,
        INSTALL_STEPS[1],
        &upload_command(&path),
        &[launcher],
        b"READY\n",
        600,
        events,
    )
    .await?;
    let _ = events.send(Event::Message("Verifying and installing on the head unit…"));
    phase(
        events,
        2,
        count,
        INSTALL_STEPS[2],
        key.len() + release.len(),
    );
    let installed = step(
        link,
        INSTALL_STEPS[2],
        &format!(
            "{path}/launcher --bootstrap-manager {} {} {compat} {digest}",
            key.len(),
            release.len()
        ),
        &[key, release],
        b"INSTALLED\n",
        600,
        events,
    )
    .await;
    phase(events, 3, count, INSTALL_STEPS[3], 0);
    if let Err(error) = step(
        link,
        INSTALL_STEPS[3],
        &cleanup_command(&path),
        &[],
        b"",
        30,
        events,
    )
    .await
    {
        // /tmp is cleared at reboot; keep the installation's own result.
        let _ = events.send(Event::Note(format!(
            "{error} It is cleared at the next reboot."
        )));
    }
    installed
}

const UPDATE_STEPS: [&str; 6] = [
    "Making the iDrive plugin folder writable",
    "Uploading the launcher",
    "Sending the update",
    "Updating the launcher and iDrive entry",
    "Removing the staging folder",
    READ_ONLY_STEP,
];

/* Remount, upload the release's own launcher to a private staging folder, let
 * it stage the release (`--stage-update`) and replace the installed launcher
 * and iDrive plugin (`--refresh-setup`), always remove the staging folder and
 * always remount read-only. The installed launcher is not used: a launcher
 * older than the release's format (NBTRLS2) could not stage it. */
async fn update(
    link: &Link,
    launcher: &'static [u8],
    release: &'static [u8],
    digest: &str,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    if release.is_empty()
        || release.len() > 128 * 1024 * 1024 + 4096 + 400
        || !valid_digest(digest)
        || launcher.is_empty()
        || launcher.len() > 64 * 1024 * 1024
    {
        return Err("The bundled release or launcher is invalid.".into());
    }
    let result = update_writable(link, launcher, release, digest, events).await;
    phase(events, 5, UPDATE_STEPS.len(), UPDATE_STEPS[5], 0);
    make_read_only(link, events).await;
    result
}

async fn update_writable(
    link: &Link,
    launcher: &'static [u8],
    release: &'static [u8],
    digest: &str,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let count = UPDATE_STEPS.len();
    let path = format!("/tmp/nbt-manager-{}", nonce()?);
    phase(events, 0, count, UPDATE_STEPS[0], 0);
    make_writable(link, events).await?;
    phase(events, 1, count, UPDATE_STEPS[1], launcher.len());
    // A failed mkdir means the folder is not ours: nothing to clean up.
    step(
        link,
        UPDATE_STEPS[1],
        &upload_command(&path),
        &[launcher],
        b"READY\n",
        600,
        events,
    )
    .await?;
    let result = stage_and_refresh(link, &path, release, digest, events).await;
    phase(events, 4, count, UPDATE_STEPS[4], 0);
    if let Err(error) = step(
        link,
        UPDATE_STEPS[4],
        &cleanup_command(&path),
        &[],
        b"",
        30,
        events,
    )
    .await
    {
        // /tmp is cleared at reboot; keep the update's own result.
        let _ = events.send(Event::Note(format!(
            "{error} It is cleared at the next reboot."
        )));
    }
    result
}

async fn stage_and_refresh(
    link: &Link,
    path: &str,
    release: &'static [u8],
    digest: &str,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let count = UPDATE_STEPS.len();
    phase(events, 2, count, UPDATE_STEPS[2], release.len());
    step(
        link,
        UPDATE_STEPS[2],
        &format!("{path}/launcher --stage-update {} {digest}", release.len()),
        &[release],
        b"PENDING\n",
        600,
        events,
    )
    .await?;
    phase(events, 3, count, UPDATE_STEPS[3], 0);
    match refresh(link, path, events).await {
        Ok(true) => {
            let _ = events.send(Event::Message(
                "iDrive entry replaced. iDrive loads it after a restart.",
            ));
            let _ = events.send(Event::RestartNeeded);
        }
        Ok(false) => {
            let _ = events.send(Event::Message(
                "iDrive entry unchanged (same checksums): no iDrive restart needed.",
            ));
        }
        // The update is staged: NBT Manager replaces its launcher and iDrive
        // entry itself the first time it opens.
        Err(error) => {
            let _ = events.send(Event::Note(format!(
                "{error} The update is staged; NBT Manager updates its launcher and iDrive entry \
                 itself the first time it opens."
            )));
        }
    }
    Ok(())
}

/// Ok(true) when the iDrive plugin was replaced (iDrive loads it after a
/// restart); files whose checksums match the release are left alone.
async fn refresh(link: &Link, path: &str, events: &mpsc::Sender<Event>) -> Result<bool, String> {
    let label = UPDATE_STEPS[3];
    let command = format!("{path}/launcher --refresh-setup");
    let _ = events.send(Event::Detail(format!("{label}: {command}")));
    match execute(link, &command, &[], 64, 120, |_| {}).await {
        Ok(output) if output.succeeded(b"REFRESHED plugin\n") => {
            shim_refused(&output.stderr, events);
            Ok(true)
        }
        Ok(output) if output.succeeded(b"REFRESHED\n") => {
            shim_refused(&output.stderr, events);
            Ok(false)
        }
        Ok(output) => {
            let _ = events.send(Event::Detail(format!(
                "{label} failed: {}",
                output.describe()
            )));
            Err(known_error(&output.stderr)
                .unwrap_or_else(|| format!("{label} failed on the head unit.")))
        }
        Err(error) => {
            let _ = events.send(Event::Detail(format!("{label} failed: {error}")));
            Err(format!(
                "{label} failed: the connection broke or the head unit stopped answering."
            ))
        }
    }
}

/// The launcher names a capture shim its tests refused on standard error; the
/// rest of the update went on, so it is a note, not a failure.
fn shim_refused(stderr: &[u8], events: &mpsc::Sender<Event>) {
    for line in String::from_utf8_lossy(stderr).lines() {
        if let Some(reason) = line.strip_prefix("capture shim refused, nothing changed: ") {
            let _ = events.send(Event::Note(format!(
                "The capture shim failed its tests on the head unit and was not installed: {reason}"
            )));
        }
    }
}

/* Commands for the head unit's host processor use only what its minimal image
 * is known to have: the QNX shell's built-ins (echo, test, umask, :) and mkdir,
 * cat, chmod, rm (the car inventory ran them there). There is no printf there:
 * the first in-car install failed on it (exit status 126) after the upload. */
fn upload_command(path: &str) -> String {
    format!(
        "umask 077; mkdir {path} && cat > {path}/launcher && chmod 700 {path}/launcher && echo READY"
    )
}

/* `path` is the locally generated /tmp/nbt-manager-<32 hex> staging folder. */
fn cleanup_command(path: &str) -> String {
    format!("rm -rf {path}; test ! -e {path}")
}

/// How long the head unit restart waits for an answer before taking it as under way.
const RESTART_ANSWER_SECONDS: u64 = 5;

/// `execute` errors raised before the head unit had the command: a restart
/// that ends with one of these never started.
fn not_sent(error: &str) -> bool {
    [
        "could not open",
        "timed out opening",
        "command contains",
        "could not send",
        "timed out sending",
    ]
    .iter()
    .any(|prefix| error.starts_with(prefix))
}

/* Removal writes to the plugin folder: remount it writable first and
 * read-only last, whatever happened in between. */
async fn run_bench(
    link: &Link,
    action: bench::Action,
    test_app: Option<TestApp>,
    events: &mpsc::Sender<Event>,
) -> (bool, String) {
    if action == bench::Action::CollectAll {
        return run_collect(link, events).await;
    }
    if action != bench::Action::RemoveAll {
        return run_bench_command(link, action, test_app, events).await;
    }
    let result = match make_writable(link, events).await {
        Ok(()) => run_bench_command(link, action, test_app, events).await,
        Err(error) => (false, error),
    };
    make_read_only(link, events).await;
    result
}

/// Runs every collect.rs section in turn; one that fails is noted in the
/// report and the others still run.
async fn run_collect(link: &Link, events: &mpsc::Sender<Event>) -> (bool, String) {
    let sections = crate::collect::sections();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut report = crate::collect::title(now);
    for (index, section) in sections.iter().enumerate() {
        phase(events, index, sections.len(), section.name, 0);
        let _ = events.send(Event::Detail(format!("Collecting: {}", section.command)));
        match execute(
            link,
            &section.command,
            &[],
            section.limit,
            section.seconds,
            |_| {},
        )
        .await
        {
            Ok(output) => crate::collect::append(
                &mut report,
                section,
                Ok(crate::collect::Output {
                    stdout: &output.stdout,
                    stderr: &output.stderr,
                    exit: output.exit,
                    truncated: output.truncated,
                }),
            ),
            Err(error) => crate::collect::append(&mut report, section, Err(error.as_str())),
        }
    }
    let bytes = report.len();
    let _ = events.send(Event::Collected(report));
    (true, bench::done(&bench::Action::CollectAll, bytes))
}

async fn run_bench_command(
    link: &Link,
    action: bench::Action,
    test_app: Option<TestApp>,
    events: &mpsc::Sender<Event>,
) -> (bool, String) {
    let (command, inputs): (Option<String>, Vec<&[u8]>) = match (&action, &test_app) {
        (bench::Action::ProvisionTestApp, Some(app)) => (
            bench::provision(
                &app.compatibility,
                app.key.len(),
                app.package.len(),
                &app.sha256,
            ),
            vec![app.key, app.package],
        ),
        (bench::Action::ProvisionTestApp, None) => {
            return (
                false,
                "This installer has no embedded test application.".into(),
            );
        }
        (bench::Action::WriteConfig(text), _) if !bench::valid_config(text) => {
            return (false, "The controller configuration must start with NBTCTL1 and contain only plain text lines (at most 1024 bytes).".into());
        }
        (bench::Action::WriteConfig(text), _) => (bench::command(&action), vec![text.as_bytes()]),
        _ => (bench::command(&action), Vec::new()),
    };
    let Some(command) = command else {
        return (false, "Invalid request.".into());
    };
    let limit = match action {
        bench::Action::FetchLog => bench::LOG_MAX,
        bench::Action::FetchSystemLog => bench::SYSTEM_LOG_MAX,
        bench::Action::RestartIdrive
        | bench::Action::RestartHmiOnly
        | bench::Action::TryShim
        | bench::Action::Shim(_) => 4096,
        _ => 64,
    };
    let total: usize = inputs.iter().map(|input| input.len()).sum();
    let label = if total > 0 {
        "Sending to the head unit"
    } else {
        "Running on the head unit"
    };
    phase(events, 0, 1, label, total);
    let _ = events.send(Event::Detail(format!("Running: {command}")));
    // The iDrive restart takes SSH down with it before its exit status gets
    // back (every car log waited out the limit): once the command was sent,
    // no answer or a dropped connection means it is under way. A refusal
    // (exit status) comes at once, so a few seconds are enough to wait.
    let restart = action == bench::Action::RestartIdrive;
    let seconds = if restart { RESTART_ANSWER_SECONDS } else { 300 };
    let output = match execute(link, &command, &inputs, limit, seconds, |done| {
        let _ = events.send(Event::Progress {
            sent: done.min(total),
            total,
        });
    })
    .await
    {
        Ok(output) => output,
        Err(error) if restart && !not_sent(&error) => {
            let _ = events.send(Event::Detail(format!(
                "No answer from the restart: {error}"
            )));
            return (true, bench::done(&action, 0));
        }
        Err(error)
            if matches!(
                action,
                bench::Action::RestartHmiOnly | bench::Action::TryShim
            ) =>
        {
            return (
                false,
                format!(
                    "The connection dropped while iDrive restarted ({error}): the head unit probably reset itself."
                ),
            );
        }
        Err(error) => return (false, format!("The command did not complete: {error}")),
    };
    if let Some(marker) = bench::marker(&action) {
        // The launcher's progress lines, then success only with its final OK.
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let _ = events.send(Event::Detail(line.to_string()));
        }
        if output.exit != Some(0) || !output.stdout.windows(marker.len()).any(|w| w == marker) {
            return (
                false,
                bench::failure(&action, output.exit, &output.stderr).into(),
            );
        }
        return (true, bench::done(&action, 0));
    }
    if output.exit != Some(0) {
        return (
            false,
            format!(
                "{} ({})",
                bench::failure(&action, output.exit, &output.stderr),
                output.describe()
            ),
        );
    }
    if action == bench::Action::FetchLog || action == bench::Action::FetchSystemLog {
        let system = action == bench::Action::FetchSystemLog;
        if output.truncated {
            return (
                false,
                format!(
                    "The {} log is larger than the installer accepts.",
                    if system { "system" } else { "debug" }
                ),
            );
        }
        let bytes = output.stdout.len();
        let _ = events.send(Event::LogFetched {
            system,
            data: output.stdout,
        });
        return (true, bench::done(&action, bytes));
    }
    if action == bench::Action::RestartIdrive {
        let _ = events.send(Event::Detail(output.describe()));
    } else if !output.succeeded(bench::expected(&action)) {
        return (
            false,
            format!(
                "The head unit gave an unexpected answer: {}",
                output.describe()
            ),
        );
    }
    (true, bench::done(&action, 0))
}

struct Handler {
    events: mpsc::Sender<Event>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    /* Every host key is accepted, by the user's decision: the installer only
     * talks to a head unit on a direct ENET cable, and asking about a key
     * nobody can check beforehand only got in the way. The fingerprint is
     * still logged. */
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256);
        let _ = self.events.send(Event::Detail(format!(
            "Host key {} {fingerprint} accepted without verification.",
            key.public_key().algorithm()
        )));
        Ok(true)
    }
}

pub fn start(
    settings: Settings,
    events: mpsc::Sender<Event>,
) -> async_mpsc::UnboundedSender<Command> {
    let (commands, receive) = async_mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(run(settings, events.clone(), receive)),
            Err(_) => "Could not start the SSH worker.",
        };
        let _ = events.send(Event::Finished(result));
    });
    commands
}

async fn run(
    mut settings: Settings,
    events: mpsc::Sender<Event>,
    mut commands: async_mpsc::UnboundedReceiver<Command>,
) -> &'static str {
    // The NBT-EVO runs an old sshd: AAD's documented command needs
    // `ssh -o HostKeyAlgorithms=+ssh-rsa -m hmac-sha1`, so it predates
    // rsa-sha2 signatures and hmac-sha2 (OpenSSH < 5.9) and has no
    // curve25519/ECDH key exchange. russh already accepts `ssh-rsa` host keys;
    // HMAC-SHA1 and the legacy key exchanges and ciphers are appended after
    // the modern ones, so they are only used when nothing better is offered.
    let mut macs = Preferred::default().mac.to_vec();
    macs.push(mac::HMAC_SHA1);
    let mut keys = Preferred::default().key.to_vec();
    keys.push(russh::keys::Algorithm::Dsa);
    let mut kex = Preferred::default().kex.to_vec();
    kex.extend([
        russh::kex::DH_GEX_SHA1,
        russh::kex::DH_G14_SHA1,
        russh::kex::DH_G1_SHA1,
    ]);
    let mut ciphers = Preferred::default().cipher.to_vec();
    ciphers.extend([russh::cipher::AES_128_CBC, russh::cipher::AES_256_CBC]);
    let config = client::Config {
        preferred: Preferred {
            kex: Cow::Owned(kex),
            key: Cow::Owned(keys),
            cipher: Cow::Owned(ciphers),
            mac: Cow::Owned(macs),
            ..Preferred::default()
        },
        // Group exchange is the only key exchange such a server shares with
        // a modern client, and an sshd that old without large moduli answers
        // with the 2048-bit group 14. russh requires 3072 bits by default,
        // which matches the car's "SSH connection or host verification
        // failed" (inferred from the documented flags, not yet observed);
        // OpenSSH's client, which works on the car, accepts 2048 bits.
        gex: client::GexParams::new(2048, 3072, 8192).expect("valid group exchange sizes"),
        ..Default::default()
    };
    let handler = Handler {
        events: events.clone(),
    };
    let phase = |index: usize, count: usize, label: &str| {
        let _ = events.send(Event::Phase {
            index,
            count,
            label: label.into(),
            bytes: 0,
        });
    };
    // Connecting takes three steps, six when SSH has to be enabled first.
    let (mut base, mut count) = (0, 3);
    let _ = events.send(Event::Message("Connecting over SSH…"));
    phase(base, count, "Opening the SSH connection");
    let direct = reachable(&settings.host);
    let tcp = if direct {
        tokio::select! {
            result = connect_tcp(&settings.host, settings.port) => result,
            _ = commands.recv() => return "Connection cancelled.",
        }
    } else {
        let _ = events.send(Event::Detail(format!(
            "No 169.254 address here, so {} cannot be reached; searching instead",
            settings.host
        )));
        None
    };
    // The head unit may hold another address, on the car's own network, with
    // SSH already enabled there: look before concluding SSH is off.
    let tcp = match tcp {
        Some(stream) => Some(stream),
        None => {
            let known = settings.gateway.into_iter().collect();
            tokio::select! {
                result = connect_elsewhere(&mut settings, &events, known, Duration::ZERO) => result,
                _ = commands.recv() => return "Connection cancelled.",
            }
        }
    };
    let stream = match tcp {
        Some(stream) => stream,
        // Refused, unreachable or timed out: SSH is probably not enabled yet,
        // unless this computer has no address on the head unit's network.
        // (Failures after the TCP connection opens, such as a wrong password or
        // a changed host key, mean SSH is on and never offer activation.)
        None => {
            let hint = unreachable_address(&settings.host);
            if let Some(text) = &hint {
                let _ = events.send(Event::Note(text.clone()));
            }
            let (send, receive) = oneshot::channel();
            if events
                .send(Event::ConfirmActivation {
                    host: format!("{}:{}", settings.host, settings.port),
                    hint,
                    respond: send,
                })
                .is_err()
            {
                return "Connection cancelled.";
            }
            let approved = tokio::select! {
                result = receive => result.unwrap_or(false),
                _ = commands.recv() => return "Connection cancelled.",
            };
            if !approved {
                return "SSH did not answer; enabling it was declined.";
            }
            // Activation talks to the vehicle gateway, found by HSFZ discovery
            // as the working BMW SSH client does; it is not the SSH address.
            (base, count) = (3, 6);
            // A gateway address typed under Connection settings skips the
            // search: after a Wi-Fi ENET adapter, the car can keep an address
            // the search does not reach.
            let found = if let Some(ip) = settings.gateway {
                phase(
                    1,
                    count,
                    "Using the gateway address from Connection settings",
                );
                Ok(ip)
            } else {
                phase(1, count, "Searching for the vehicle gateway");
                let _ = events.send(Event::Message("Searching for the vehicle gateway…"));
                tokio::select! {
                    result = tokio::task::spawn_blocking(activation::find_gateway) => {
                        result.unwrap_or_else(|_| Err("Gateway search failed.".into()))
                    }
                    _ = commands.recv() => return "Connection cancelled.",
                }
            };
            let ip = match found {
                Ok(ip) => ip,
                Err(error) => {
                    let _ = events.send(Event::Note(error));
                    return "Enabling SSH failed.";
                }
            };
            let _ = events.send(Event::Note(format!(
                "Vehicle gateway {ip}; enabling SSH through {}…",
                activation::SERVER
            )));
            phase(2, count, "Enabling SSH");
            let notes = events.clone();
            let result = tokio::select! {
                result = activation::run(std::net::SocketAddr::from((ip, 6801)), move |text| {
                    let _ = notes.send(Event::Note(text));
                }) => result,
                _ = commands.recv() => return "Connection cancelled.",
            };
            phase(base, count, "Opening the SSH connection");
            match reconnect::after_activation(
                result.map_err(|error| error.to_string()),
                &mut settings,
                &events,
                &mut commands,
                ip,
                direct,
            )
            .await
            {
                Ok(stream) => stream,
                Err(error) => return error,
            }
        }
    };
    let wire = Arc::new(AtomicUsize::new(0));
    let handshake_started = std::time::Instant::now();
    let connect = tokio::time::timeout(
        Duration::from_secs(120),
        client::connect_stream(
            Arc::new(config),
            Counted {
                inner: stream,
                written: wire.clone(),
            },
            handler,
        ),
    );
    let connected = tokio::select! {
        result = connect => result,
        _ = commands.recv() => return "Connection cancelled.",
    };
    let mut session = match connected {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => {
            let _ = events.send(Event::Detail(format!("SSH handshake: {error}")));
            return "SSH connection failed.";
        }
        Err(_) => {
            let _ = events.send(Event::Detail(
                "SSH handshake: no answer within 120 s".into(),
            ));
            return "SSH connection failed.";
        }
    };
    // A slow handshake is the sign of a bridged link, and explains a slow
    // install; the figure makes a later report specific.
    let _ = events.send(Event::Detail(format!(
        "SSH handshake took {:.1} s",
        handshake_started.elapsed().as_secs_f32()
    )));
    phase(base + 1, count, "Logging in");
    let secret = std::mem::take(&mut *settings.password);
    let auth = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(AUTH_SECONDS), session.authenticate_password(settings.user, secret)) => result,
        _ = commands.recv() => return "Connection cancelled.",
    };
    match auth {
        Ok(Ok(ref result)) if result.success() => {}
        Ok(Ok(_)) => return "SSH authentication failed: the head unit rejected the password.",
        Ok(Err(error)) => {
            let _ = events.send(Event::Detail(format!("SSH authentication: {error}")));
            return "SSH authentication failed.";
        }
        Err(_) => return "SSH authentication failed: no answer in time.",
    }
    let _ = events.send(Event::Connected);
    let mut link = Link { session, wire };
    phase(base + 2, count, "Checking the head unit");
    let target = tokio::select! {
        result = qnx_probe(&link, REMOTE_SYSTEM, &events) => result,
        _ = commands.recv() => return "Connection cancelled.",
    };
    let _ = events.send(Event::Message(if target {
        "QNX responded. Storage and installation compatibility remain unverified."
    } else {
        "Could not verify that the head unit runs QNX. Installation remains unavailable."
    }));
    let _ = events.send(Event::Target(target));
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // Each operation checks the head unit again: `target` only
                // enables the buttons.
                Some(Command::Install { launcher, release, key, sha256, compatibility }) => {
                    let result = install(&link, launcher, release, key, &sha256, &compatibility, &events).await;
                    match &result {
                        // A new plugin loads only when iDrive restarts.
                        Ok(()) => { let _ = events.send(Event::RestartNeeded); }
                        Err(error) => { let _ = events.send(Event::Note(error.clone())); }
                    }
                    let _ = events.send(Event::InstallFinished(result.is_ok()));
                }
                Some(Command::Update { launcher, release, sha256 }) => {
                    let result = update(&link, launcher, release, &sha256, &events).await;
                    if let Err(error) = &result {
                        let _ = events.send(Event::Note(error.clone()));
                    }
                    let _ = events.send(Event::UpdateFinished(result.is_ok()));
                }
                Some(Command::Bench { action, test_app }) => {
                    let restart = action == bench::Action::RestartIdrive;
                    let (ok, message) = run_bench(&link, action, test_app, &events).await;
                    let _ = events.send(Event::BenchFinished { ok, message });
                    // The restart drops SSH anyway: close it now rather than
                    // wait for it to time out.
                    if restart && ok {
                        // Bounded: the head unit's end may already be gone.
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            link.session.disconnect(russh::Disconnect::ByApplication, "closed", "en"),
                        )
                        .await;
                        return "Disconnected while iDrive restarts, which also switches SSH off. Once iDrive is back, press Connect: the installer offers to enable SSH again.";
                    }
                }
                _ => break,
            },
            _ = &mut link.session => return "SSH connection closed.",
        }
    }
    let _ = link
        .session
        .disconnect(russh::Disconnect::ByApplication, "closed", "en")
        .await;
    "Disconnected."
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::{
        keys::{Algorithm, PrivateKey},
        server::{self, Server as _},
    };
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{net::SocketAddr, path::PathBuf};

    /// The simulated head unit's folders are only used on Unix hosts.
    #[cfg(unix)]
    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    fn set_mode(_: &std::path::Path, _: u32) {}

    #[test]
    fn known_errors_are_read_past_the_login_profile() {
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\napplication already provisioned\n";
        assert!(
            known_error(car)
                .unwrap()
                .starts_with("This application is already installed.")
        );
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\nconflicting manager setup files\n";
        assert!(known_error(car).unwrap().contains("use Update instead"));
        assert!(
            known_error(b"HMI layout: owner\n")
                .unwrap()
                .contains("plugin folder: owner.")
        );
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\nmanager operation is in progress\n";
        assert!(known_error(car).unwrap().contains("Close it and try again"));
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\nmanager update awaiting confirmation\n";
        assert!(
            known_error(car)
                .unwrap()
                .contains("keep it open for 10 seconds")
        );
        assert!(known_error(b"something else\n").is_none());
    }
    #[cfg(unix)]
    fn group_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().gid()
    }
    #[cfg(not(unix))]
    fn group_of(_: &std::path::Path) -> u32 {
        0
    }

    #[derive(Clone)]
    struct NativeTarget {
        executable: PathBuf,
        root: PathBuf,
        hmi: PathBuf,
        launcher: Vec<u8>,
    }

    /// Runs the real remote commands with /bin/sh, the head unit's paths mapped
    /// to temporary directories and the launcher to launcher-host.
    #[derive(Clone)]
    struct ShellTarget {
        root: PathBuf,
        plugins: PathBuf,
        launcher: Option<PathBuf>,
    }

    #[derive(Clone)]
    struct TestServer {
        response: u8,
        upload: Option<(String, Vec<u8>)>,
        install_path: Option<String>,
        native: Option<NativeTarget>,
        shell: Option<ShellTarget>,
        /// Answer group exchange with the 2048-bit group 14 whatever the
        /// client asks, as an old sshd without large moduli does.
        legacy_gex: bool,
        /// The remount command ran.
        remounted: bool,
    }

    impl server::Server for TestServer {
        type Handler = Self;
        fn new_client(&mut self, _: Option<SocketAddr>) -> Self {
            self.clone()
        }
    }

    impl server::Handler for TestServer {
        type Error = russh::Error;
        async fn lookup_dh_gex_group(
            &mut self,
            gex_params: &russh::client::GexParams,
        ) -> Result<Option<russh::kex::dh::groups::DhGroup>, Self::Error> {
            if self.legacy_gex {
                return Ok(Some(russh::kex::dh::groups::DH_GROUP14));
            }
            // Otherwise the group closest to the client's preferred size, as
            // russh's own default does.
            use russh::kex::dh::groups::{
                DH_GROUP14, DH_GROUP15, DH_GROUP16, DH_GROUP17, DH_GROUP18,
            };
            Ok([DH_GROUP14, DH_GROUP15, DH_GROUP16, DH_GROUP17, DH_GROUP18]
                .into_iter()
                .filter(|group| {
                    group.bit_size() >= gex_params.min_group_size()
                        && group.bit_size() <= gex_params.max_group_size()
                })
                .min_by_key(|group| group.bit_size().abs_diff(gex_params.preferred_group_size())))
        }
        async fn channel_open_session(
            &mut self,
            _channel: russh::Channel<server::Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }
        async fn exec_request(
            &mut self,
            channel: russh::ChannelId,
            command: &[u8],
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            let command = command
                .strip_prefix(b"/bin/sh -l -c '")
                .and_then(|command| command.strip_suffix(b"'"))
                .expect("every command runs in a login shell");
            // The remount makes the simulated plugin folder writable, as the
            // car's makes /fs/sda0 writable; response 11 refuses it.
            // The read-only remount restores the boot state; response 12
            // refuses it.
            if command == bench::REMOUNT_READ_ONLY.as_bytes() {
                session.channel_success(channel)?;
                if self.response == 12 {
                    session.extended_data(channel, 1, b"mount: Resource busy\n".to_vec())?;
                    session.exit_status_request(channel, 1)?;
                } else {
                    if let Some(shell) = &self.shell {
                        set_mode(&shell.plugins, 0o555);
                    }
                    if let Some(native) = &self.native {
                        set_mode(&native.hmi, 0o555);
                    }
                    session.data(channel, b"OK\n".to_vec())?;
                    session.exit_status_request(channel, 0)?;
                }
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if command == bench::REMOUNT.as_bytes() {
                session.channel_success(channel)?;
                if self.response == 11 {
                    session.extended_data(channel, 1, b"mount: Permission denied\n".to_vec())?;
                    session.exit_status_request(channel, 1)?;
                } else {
                    if let Some(shell) = &self.shell {
                        set_mode(&shell.plugins, 0o755);
                    }
                    if let Some(native) = &self.native {
                        set_mode(&native.hmi, 0o775);
                    }
                    self.remounted = true;
                    session.data(channel, b"OK\n".to_vec())?;
                    session.exit_status_request(channel, 0)?;
                }
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if self.shell.is_some() && command != REMOTE_SYSTEM.as_bytes() {
                self.upload = Some((String::from_utf8_lossy(command).into_owned(), Vec::new()));
                session.channel_success(channel)?;
                return Ok(());
            }
            if self.response >= 4
                && ((!command.starts_with(b"/") && command != REMOTE_SYSTEM.as_bytes())
                    || (command.starts_with(b"/tmp/nbt-manager-")
                        && (command
                            .windows(19)
                            .any(|part| part == b"--bootstrap-manager")
                            || command.windows(14).any(|part| part == b"--stage-update")
                            || command.ends_with(b"--refresh-setup"))))
            {
                if self.response == 8 {
                    session.channel_success(channel)?;
                    session.eof(channel)?;
                    session.close(channel)?;
                    return Ok(());
                }
                // The staging command fails at once, without reading its input.
                if self.response == 10 && command.windows(9).any(|part| part == b"umask 077") {
                    session.channel_success(channel)?;
                    session.extended_data(
                        channel,
                        1,
                        b"mkdir: /tmp/nbt-manager-x: Read-only file system\n".to_vec(),
                    )?;
                    session.exit_status_request(channel, 1)?;
                    session.eof(channel)?;
                    session.close(channel)?;
                    return Ok(());
                }
                self.upload = Some((String::from_utf8_lossy(command).into_owned(), Vec::new()));
                session.channel_success(channel)?;
                return Ok(());
            }
            if command != REMOTE_SYSTEM.as_bytes() {
                session.channel_failure(channel)?;
                return Ok(());
            }
            session.channel_success(channel)?;
            let output: &[u8] = match self.response {
                1 => b"Linux\n",
                2 => &[b'X'; 65],
                _ => b"QNX\n",
            };
            session.data(channel, output.to_vec())?;
            session.exit_status_request(channel, if self.response == 3 { 1 } else { 0 })?;
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }
        async fn data(
            &mut self,
            _channel: russh::ChannelId,
            data: &[u8],
            _session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            if let Some((_, bytes)) = &mut self.upload {
                bytes.extend_from_slice(data);
            }
            Ok(())
        }
        async fn channel_eof(
            &mut self,
            channel: russh::ChannelId,
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            if let Some((command, bytes)) = self.upload.take() {
                if let Some(shell) = &self.shell {
                    use std::io::Write as _;
                    use std::process::{Command, Stdio};
                    let mut script = command.clone();
                    if let Some(launcher) = &shell.launcher {
                        script = script.replace(bench::LAUNCHER, launcher.to_str().unwrap());
                    }
                    let hmi_root = shell.plugins.parent().unwrap();
                    script = script
                        .replace(bench::PLUGINS, shell.plugins.to_str().unwrap())
                        .replace(
                            bench::CAPTURE,
                            hmi_root.join("NbtCapture").to_str().unwrap(),
                        )
                        .replace(bench::ROOT, shell.root.to_str().unwrap());
                    let mut child = Command::new("/bin/sh")
                        .arg("-c")
                        .arg(script)
                        .env("NBT_TEST_MANAGER_ROOT", &shell.root)
                        .env("NBT_TEST_HMI_ROOT", hmi_root)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    child.stdin.take().unwrap().write_all(&bytes).unwrap();
                    let output = child.wait_with_output().unwrap();
                    if !output.stdout.is_empty() {
                        session.data(channel, output.stdout)?;
                    }
                    if !output.stderr.is_empty() {
                        session.extended_data(channel, 1, output.stderr)?;
                    }
                    session
                        .exit_status_request(channel, output.status.code().unwrap_or(1) as u32)?;
                    session.eof(channel)?;
                    session.close(channel)?;
                    return Ok(());
                }
                if command.contains("--stage-update")
                    && let Some(native) = &self.native
                {
                    let path = self.install_path.as_ref().unwrap();
                    assert!(command.starts_with(&format!("{path}/launcher --stage-update ")));
                    let output = run_native(native, &command, &bytes);
                    send_output(session, channel, output)?;
                } else if command.contains("--stage-update") {
                    // Staged by the launcher just uploaded, never the installed one.
                    let path = self.install_path.as_ref().unwrap();
                    assert_eq!(
                        command,
                        format!(
                            "{path}/launcher --stage-update {} {}",
                            bytes.len(),
                            crate::release::hash_hex(&bytes)
                        )
                    );
                    session.data(
                        channel,
                        if self.response == 4 || self.response >= 14 {
                            b"PENDING\n".to_vec()
                        } else {
                            b"BAD\n".to_vec()
                        },
                    )?;
                    session.exit_status_request(channel, 0)?;
                } else if command.contains("umask 077; mkdir ") {
                    let path = command
                        .split("umask 077; mkdir ")
                        .nth(1)
                        .unwrap()
                        .split(" &&")
                        .next()
                        .unwrap();
                    let nonce = path.strip_prefix("/tmp/nbt-manager-").unwrap();
                    assert!(nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()));
                    assert!(self.remounted, "uploaded before the remount");
                    if let Some(native) = &self.native {
                        assert_eq!(bytes, native.launcher);
                    } else {
                        assert_eq!(bytes, b"test-launcher");
                    }
                    self.install_path = Some(path.to_owned());
                    session.data(channel, b"READY\n".to_vec())?;
                    session.exit_status_request(channel, 0)?;
                } else if command.ends_with("--refresh-setup")
                    && let Some(native) = &self.native
                {
                    let path = self.install_path.as_ref().unwrap();
                    assert_eq!(command, format!("{path}/launcher --refresh-setup"));
                    assert!(bytes.is_empty() && self.remounted);
                    let output = run_native(native, &command, &bytes);
                    send_output(session, channel, output)?;
                } else if command.ends_with("--refresh-setup") {
                    let path = self.install_path.as_ref().unwrap();
                    assert_eq!(command, format!("{path}/launcher --refresh-setup"));
                    assert!(bytes.is_empty() && self.remounted);
                    // 14: the iDrive entry replaced, the release's shim refused.
                    if self.response == 14 {
                        session.data(channel, b"REFRESHED plugin\n".to_vec())?;
                        session.extended_data(
                            channel,
                            1,
                            b"capture shim refused, nothing changed: live test: FAILED step 2\n"
                                .to_vec(),
                        )?;
                    } else {
                        session.data(channel, b"REFRESHED\n".to_vec())?;
                    }
                    session.exit_status_request(channel, 0)?;
                } else if command.contains("--bootstrap-manager") {
                    let path = self.install_path.as_ref().unwrap();
                    if let Some(native) = &self.native {
                        use std::io::Write as _;
                        use std::process::{Command, Stdio};
                        let words: Vec<_> = command.split_ascii_whitespace().collect();
                        assert_eq!(words.len(), 6);
                        assert_eq!(words[0], format!("{path}/launcher"));
                        assert_eq!(words[1], "--bootstrap-manager");
                        let group = group_of(&native.hmi);
                        let mut child = Command::new(&native.executable)
                            .args(&words[1..])
                            .env("NBT_TEST_MANAGER_ROOT", &native.root)
                            .env("NBT_TEST_HMI_ROOT", &native.hmi)
                            // The car's folder is root:root 0775: this group stands in for root's.
                            .env("NBT_TEST_HMI_TRUSTED_GID", group.to_string())
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn()
                            .unwrap();
                        child.stdin.take().unwrap().write_all(&bytes).unwrap();
                        let output = child.wait_with_output().unwrap();
                        if !output.stdout.is_empty() {
                            session.data(channel, output.stdout)?;
                        }
                        if !output.stderr.is_empty() {
                            session.extended_data(channel, 1, output.stderr)?;
                        }
                        session.exit_status_request(
                            channel,
                            output.status.code().unwrap_or(1) as u32,
                        )?;
                    } else {
                        assert_eq!(
                            command,
                            format!(
                                "{path}/launcher --bootstrap-manager 3 12 demo-1 {}",
                                crate::release::hash_hex(b"test-release")
                            )
                        );
                        assert_eq!(bytes, b"keytest-release");
                        if self.response == 7 {
                            session.extended_data(
                                channel,
                                1,
                                b"HMI layout: plugin folder writable by other users\n".to_vec(),
                            )?;
                            session.exit_status_request(channel, 1)?;
                        } else {
                            session.data(channel, b"INSTALLED\n".to_vec())?;
                            session.exit_status_request(channel, 0)?;
                        }
                    }
                } else {
                    let path = self.install_path.take().unwrap();
                    assert_eq!(command, format!("rm -rf {path}; test ! -e {path}"));
                    assert!(bytes.is_empty());
                    session.exit_status_request(channel, 0)?;
                }
                session.eof(channel)?;
                session.close(channel)?;
            }
            Ok(())
        }
        async fn auth_password(
            &mut self,
            user: &str,
            password: &str,
        ) -> Result<server::Auth, Self::Error> {
            Ok(if user == "root" && password == "test-secret" {
                server::Auth::Accept
            } else {
                server::Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                }
            })
        }
    }

    /// Runs the host build of the launcher for an uploaded launcher's command,
    /// as the head unit would, on the native target's folders.
    fn run_native(native: &NativeTarget, command: &str, input: &[u8]) -> std::process::Output {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let words: Vec<_> = command.split_ascii_whitespace().collect();
        let mut child = Command::new(&native.executable)
            .args(&words[1..])
            .env("NBT_TEST_MANAGER_ROOT", &native.root)
            .env("NBT_TEST_HMI_ROOT", &native.hmi)
            // The car's folder is root:root 0775: this group stands in for root's.
            .env(
                "NBT_TEST_HMI_TRUSTED_GID",
                group_of(&native.hmi).to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn send_output(
        session: &mut server::Session,
        channel: russh::ChannelId,
        output: std::process::Output,
    ) -> Result<(), russh::Error> {
        if !output.stdout.is_empty() {
            session.data(channel, output.stdout)?;
        }
        if !output.stderr.is_empty() {
            session.extended_data(channel, 1, output.stderr)?;
        }
        session.exit_status_request(channel, output.status.code().unwrap_or(1) as u32)
    }

    fn serve(response: u8) -> u16 {
        serve_with_native(response, None)
    }

    fn serve_with_native(response: u8, native: Option<NativeTarget>) -> u16 {
        serve_target(response, native, None, Algorithm::Ed25519)
    }

    fn serve_target(
        response: u8,
        native: Option<NativeTarget>,
        shell: Option<ShellTarget>,
        host_key: Algorithm,
    ) -> u16 {
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                send.send(listener.local_addr().unwrap().port()).unwrap();
                let key = PrivateKey::random(&mut rand::rng(), host_key.clone()).unwrap();
                let config = server::Config {
                    keys: vec![key],
                    preferred: Preferred {
                        key: Cow::Owned(vec![host_key]),
                        mac: Cow::Owned(vec![mac::HMAC_SHA1]),
                        cipher: Cow::Owned(vec![russh::cipher::AES_128_CTR]),
                        ..Preferred::default()
                    },
                    auth_rejection_time: Duration::ZERO,
                    auth_rejection_time_initial: Some(Duration::ZERO),
                    ..Default::default()
                };
                let _ = tokio::time::timeout(
                    Duration::from_secs(30),
                    TestServer {
                        response,
                        upload: None,
                        install_path: None,
                        native,
                        shell,
                        legacy_gex: false,
                        remounted: false,
                    }
                    .run_on_socket(Arc::new(config), &listener),
                )
                .await;
            });
        });
        receive.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    fn settings(port: u16, password: &str) -> Settings {
        Settings {
            host: "127.0.0.1".into(),
            port,
            user: "root".into(),
            password: Zeroizing::new(password.into()),
            gateway: None,
        }
    }

    /// The next event that is not a log note, detail or step start.
    fn next(receive: &mpsc::Receiver<Event>) -> Event {
        loop {
            match receive.recv_timeout(Duration::from_secs(15)).unwrap() {
                Event::Note(_) | Event::Detail(_) | Event::Phase { .. } => {}
                event => return event,
            }
        }
    }

    /// The head unit's host processor has no printf (exit status 126 in the car)
    /// and rmdir is not known to exist there: no remote command may use them.
    #[test]
    fn remote_commands_use_only_known_head_unit_tools() {
        let path = "/tmp/nbt-manager-0123456789abcdef0123456789abcdef";
        let mut commands = vec![upload_command(path), cleanup_command(path)];
        for action in [
            bench::Action::LogOn,
            bench::Action::LogOff,
            bench::Action::FetchLog,
            bench::Action::FetchSystemLog,
            bench::Action::WriteConfig("NBTCTL1\n".into()),
            bench::Action::RemoveConfig,
            bench::Action::RemoveAll,
            bench::Action::RestartIdrive,
        ] {
            commands.push(bench::command(&action).unwrap());
        }
        commands.push(bench::REMOUNT.into());
        commands.push(bench::REMOUNT_READ_ONLY.into());
        commands.push(bench::provision("nbt-test-1", 1, 1, &"ab".repeat(32)).unwrap());
        commands.push(format!(
            "{path}/launcher --stage-update 1 {}",
            "ab".repeat(32)
        ));
        for command in &commands {
            // Each runs in a login shell, quoted once (`login_shell`).
            assert_eq!(
                login_shell(command),
                Some(format!("/bin/sh -l -c '{command}'"))
            );
            for word in command.split(|c: char| !c.is_ascii_alphanumeric()) {
                assert!(!["printf", "rmdir"].contains(&word), "{command}");
            }
        }
        assert!(upload_command(path).ends_with("&& echo READY"));
        assert_eq!(
            cleanup_command(path),
            format!("rm -rf {path}; test ! -e {path}")
        );
        // rm's exit status is undocumented for missing files (QNX 6.5): no
        // command chains on it.
        for command in &commands {
            assert!(
                !command.contains("rm -f")
                    || !command
                        .split("rm -f")
                        .nth(1)
                        .unwrap()
                        .split(';')
                        .next()
                        .unwrap()
                        .contains("&&"),
                "{command}"
            );
            assert!(
                !command.contains("rm -rf")
                    || !command
                        .split("rm -rf")
                        .nth(1)
                        .unwrap()
                        .split(';')
                        .next()
                        .unwrap()
                        .contains("&&"),
                "{command}"
            );
        }
    }

    #[test]
    fn password_authentication_accepts_and_logs_any_host_key() {
        let port = serve(0);
        for _ in 0..2 {
            let (send, receive) = mpsc::channel();
            let commands = start(settings(port, "test-secret"), send);
            assert!(matches!(
                receive.recv_timeout(Duration::from_secs(10)).unwrap(),
                Event::Message(_)
            ));
            assert!(matches!(
                receive.recv_timeout(Duration::from_secs(10)).unwrap(),
                Event::Phase {
                    index: 0,
                    count: 3,
                    ..
                }
            ));
            match receive.recv_timeout(Duration::from_secs(10)).unwrap() {
                Event::Detail(note) => assert!(
                    note.starts_with("Host key ")
                        && note.contains(" SHA256:")
                        && note.ends_with(" accepted without verification."),
                    "{note}"
                ),
                _ => panic!("expected the host key note"),
            }
            assert!(matches!(next(&receive), Event::Connected));
            assert!(matches!(
                next(&receive),
                Event::Message(
                    "QNX responded. Storage and installation compatibility remain unverified."
                )
            ));
            assert!(matches!(next(&receive), Event::Target(true)));
            commands.send(Command::Disconnect).unwrap();
            assert!(matches!(next(&receive), Event::Finished("Disconnected.")));
        }

        let (send, receive) = mpsc::channel();
        let _commands = start(settings(port, "wrong"), send);
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(
            next(&receive),
            Event::Finished("SSH authentication failed: the head unit rejected the password.")
        ));
    }

    #[test]
    fn first_install_streams_launcher_and_release_and_handles_target_failure() {
        // 6 installs, 7 is refused by the launcher, 12 installs but the
        // read-only remount is refused.
        for response in [6, 7, 12] {
            let port = serve(response);
            let (send, receive) = mpsc::channel();
            let commands = start(settings(port, "test-secret"), send);
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Connected));
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Target(true)));
            commands
                .send(Command::Install {
                    launcher: b"test-launcher",
                    release: b"test-release",
                    key: b"key",
                    sha256: crate::release::hash_hex(b"test-release"),
                    compatibility: "demo-1".into(),
                })
                .unwrap();
            // Each step reports its own bytes; steps are numbered of five.
            let mut steps = Vec::new();
            let mut progress = 0;
            let mut sent_total = 0;
            let mut saw_error = false;
            let mut saw_writable = false;
            let mut restart = false;
            loop {
                match receive.recv_timeout(Duration::from_secs(10)).unwrap() {
                    Event::Phase {
                        index,
                        count,
                        label,
                        bytes,
                    } => {
                        sent_total += progress;
                        progress = 0;
                        steps.push((index, count, label, bytes));
                    }
                    Event::Progress { sent, total } => {
                        assert!(sent > progress && sent <= total);
                        assert_eq!(total, steps.last().unwrap().3);
                        progress = sent;
                    }
                    Event::Note(message)
                        if message
                            == "The head unit refused the iDrive plugin folder: plugin folder writable by other users." =>
                    {
                        saw_error = true
                    }
                    Event::Note(message)
                        if message.starts_with(
                            "The iDrive plugin folder could not be made read-only again",
                        ) =>
                    {
                        saw_writable = true
                    }
                    Event::Message(_) | Event::Note(_) | Event::Detail(_) => {}
                    Event::RestartNeeded => restart = true,
                    Event::InstallFinished(success) => {
                        assert_eq!(success, response != 7);
                        // Only a completed install asks for an iDrive restart.
                        assert_eq!(restart, success);
                        break;
                    }
                    _ => panic!("unexpected install event"),
                }
            }
            sent_total += progress;
            assert_eq!(
                sent_total,
                b"test-launcher".len() + b"key".len() + b"test-release".len()
            );
            assert_eq!(
                steps,
                [
                    (
                        0,
                        5,
                        "Making the iDrive plugin folder writable".to_owned(),
                        0
                    ),
                    (1, 5, "Uploading the launcher".to_owned(), 13),
                    (
                        2,
                        5,
                        "Installing the manager and iDrive plugin".to_owned(),
                        15
                    ),
                    (3, 5, "Removing the staging folder".to_owned(), 0),
                    (
                        4,
                        5,
                        "Making the iDrive plugin folder read-only again".to_owned(),
                        0
                    ),
                ]
            );
            assert_eq!(saw_error, response == 7);
            assert_eq!(saw_writable, response == 12);
            commands.send(Command::Disconnect).unwrap();
            assert!(matches!(next(&receive), Event::Finished(_)));
        }
    }

    /// Every install step is logged with its command; a failed one with the
    /// remote exit status and error text, also when the command exits
    /// before taking its input.
    #[test]
    fn failed_install_steps_log_exit_status_and_error_text() {
        // Larger than the SSH window, so the command exits mid-upload.
        let large: &'static [u8] = Box::leak(vec![0u8; 8 << 20].into_boxed_slice());
        for (response, launcher, step, error) in [
            (
                10,
                large,
                "Uploading the launcher failed: exit status 1",
                "Read-only file system",
            ),
            (
                7,
                &b"test-launcher"[..],
                "Installing the manager and iDrive plugin failed: exit status 1",
                "HMI layout: plugin folder writable by other users",
            ),
        ] {
            let port = serve(response);
            let (send, receive) = mpsc::channel();
            let commands = start(settings(port, "test-secret"), send);
            while !matches!(next(&receive), Event::Target(true)) {}
            commands
                .send(Command::Install {
                    launcher,
                    release: b"test-release",
                    key: b"key",
                    sha256: crate::release::hash_hex(b"test-release"),
                    compatibility: "demo-1".into(),
                })
                .unwrap();
            let mut notes = Vec::new();
            loop {
                match receive.recv_timeout(Duration::from_secs(10)).unwrap() {
                    Event::Detail(note) => notes.push(note),
                    Event::InstallFinished(success) => {
                        assert!(!success);
                        break;
                    }
                    _ => {}
                }
            }
            assert!(
                notes.iter().any(|note| note
                    .starts_with("Uploading the launcher: umask 077; mkdir /tmp/nbt-manager-")),
                "{notes:#?}"
            );
            assert!(
                notes.iter().any(|note| note.starts_with(step)
                    && note.contains("exit status 1")
                    && note.contains(error)),
                "{notes:#?}"
            );
            commands.send(Command::Disconnect).unwrap();
            assert!(matches!(next(&receive), Event::Finished(_)));
        }
    }

    #[test]
    fn interrupted_install_and_invalid_compatibility_do_not_proceed() {
        // 8: the head unit closes the upload; 11: the remount is refused,
        // so nothing is uploaded (the mock asserts that); bad compatibility.
        for (response, compat) in [(8, "demo-1"), (11, "demo-1"), (6, "demo;bad")] {
            let port = serve(response);
            let (send, receive) = mpsc::channel();
            let commands = start(settings(port, "test-secret"), send);
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Connected));
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Target(true)));
            commands
                .send(Command::Install {
                    launcher: b"test-launcher",
                    release: b"test-release",
                    key: b"key",
                    sha256: crate::release::hash_hex(b"test-release"),
                    compatibility: compat.into(),
                })
                .unwrap();
            loop {
                match next(&receive) {
                    Event::Message(_) | Event::Progress { .. } => {}
                    Event::InstallFinished(success) => {
                        assert!(!success);
                        break;
                    }
                    _ => panic!("unexpected install event"),
                }
            }
            commands.send(Command::Disconnect).unwrap();
            assert!(matches!(next(&receive), Event::Finished(_)));
        }
    }

    /* NBT Manager's release packer: $NBT_MANAGER_DIR/tools/package_release.py,
     * by default in the sibling NBTFoundry/manager checkout. */
    #[cfg(unix)]
    fn manager_packer() -> PathBuf {
        std::env::var_os("NBT_MANAGER_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../manager"))
            .join("tools/package_release.py")
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires NBT_TEST_LAUNCHER_HOST (NBTFoundry/manager: make build/launcher-host)"]
    fn native_install_then_update_over_ssh() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command as ProcessCommand, Stdio};
        let launcher_host = PathBuf::from(
            std::env::var_os("NBT_TEST_LAUNCHER_HOST").expect("set NBT_TEST_LAUNCHER_HOST"),
        );
        let dir = std::env::temp_dir().join(format!(
            "nbt-native-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // As in the car: read-only until the installer remounts it, then
        // root:root 0775 (dev9 failed there on the group write bit).
        let hmi = dir.join("hmi");
        std::fs::create_dir(&hmi).unwrap();
        std::fs::set_permissions(&hmi, std::fs::Permissions::from_mode(0o555)).unwrap();
        let key = dir.join("key.pem");
        let public = dir.join("key.pub");
        let run = |command: &mut ProcessCommand| {
            let result = command
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(result.success());
        };
        run(ProcessCommand::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:3072",
                "-out",
            ])
            .arg(&key));
        run(ProcessCommand::new("openssl")
            .args(["pkey", "-in"])
            .arg(&key)
            .arg("-pubout")
            .arg("-out")
            .arg(&public));
        let mut elf = [0u8; 128];
        elf[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        elf[16] = 2;
        elf[18] = 40;
        for name in ["launcher", "gui", "worker"] {
            std::fs::write(dir.join(name), elf).unwrap();
        }
        let plugin = dir.join("plugin");
        std::fs::create_dir(&plugin).unwrap();
        elf[16] = 3;
        std::fs::write(plugin.join("libNbtManager.so"), elf).unwrap();
        for name in [
            "NbtManager.manifest",
            "NbtManager.xml",
            "NbtManager_ALL_Texts.zip",
            "NbtManager_ALL_Images.zip",
        ] {
            std::fs::write(plugin.join(name), name).unwrap();
        }
        let package = dir.join("release.nbtr");
        run(ProcessCommand::new("python3")
            .arg(manager_packer())
            .args([
                "--version",
                "v1",
                "--sequence",
                "1",
                "--compat",
                "demo-1",
                "--key",
            ])
            .arg(&key)
            .arg("--output")
            .arg(&package)
            .arg("--launcher")
            .arg(dir.join("launcher"))
            .arg("--gui")
            .arg(dir.join("gui"))
            .arg("--worker")
            .arg(dir.join("worker"))
            .arg("--plugin-dir")
            .arg(&plugin));
        let release = std::fs::read(&package).unwrap();
        let public = std::fs::read(&public).unwrap();
        let public2 = public.clone();
        let info = crate::release::verify(&release, &public, Some("demo-1")).unwrap();
        let launcher = release[info.launcher].to_vec();
        let target = NativeTarget {
            executable: launcher_host,
            root: dir.join("manager"),
            hmi: hmi.clone(),
            launcher: launcher.clone(),
        };
        let port = serve_with_native(9, Some(target));
        let (send, receive) = mpsc::channel();
        let commands = start(settings(port, "test-secret"), send);
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(next(&receive), Event::Connected));
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(next(&receive), Event::Target(true)));
        let sha256 = crate::release::hash_hex(&release);
        commands
            .send(Command::Install {
                launcher: Box::leak(launcher.into_boxed_slice()),
                release: Box::leak(release.into_boxed_slice()),
                key: Box::leak(public.into_boxed_slice()),
                sha256,
                compatibility: "demo-1".into(),
            })
            .unwrap();
        let mut restart = false;
        loop {
            match next(&receive) {
                Event::Message(_) | Event::Progress { .. } => {}
                Event::RestartNeeded => restart = true,
                Event::InstallFinished(success) => {
                    assert!(success && restart);
                    break;
                }
                _ => panic!("unexpected install event"),
            }
        }
        assert!(dir.join("manager/state/manager/activation").is_file());
        assert!(dir.join("manager/manager/nbt-manager-launcher").is_file());
        assert!(hmi.join("Plugins/NbtManager/NbtManager.manifest").is_file());
        // Read-only again after the install, as after boot.
        assert_eq!(
            std::fs::metadata(&hmi).unwrap().permissions().mode() & 0o777,
            0o555
        );
        // Update to v2: the uploaded launcher stages it and replaces the
        // installed launcher and the iDrive plugin; the installed launcher is
        // never run (the fake checks every command's path).
        std::fs::write(plugin.join("NbtManager.manifest"), "NbtManager.manifest v2").unwrap();
        let package2 = dir.join("release2.nbtr");
        run(ProcessCommand::new("python3")
            .arg(manager_packer())
            .args([
                "--version",
                "v2",
                "--sequence",
                "2",
                "--compat",
                "demo-1",
                "--key",
            ])
            .arg(&key)
            .arg("--output")
            .arg(&package2)
            .arg("--launcher")
            .arg(dir.join("launcher"))
            .arg("--gui")
            .arg(dir.join("gui"))
            .arg("--worker")
            .arg(dir.join("worker"))
            .arg("--plugin-dir")
            .arg(&plugin));
        let release2 = std::fs::read(&package2).unwrap();
        let info2 = crate::release::verify(&release2, &public2, Some("demo-1")).unwrap();
        let launcher2 = release2[info2.launcher].to_vec();
        commands
            .send(Command::Update {
                launcher: Box::leak(launcher2.into_boxed_slice()),
                sha256: crate::release::hash_hex(&release2),
                release: Box::leak(release2.into_boxed_slice()),
            })
            .unwrap();
        loop {
            match receive.recv_timeout(Duration::from_secs(15)).unwrap() {
                Event::UpdateFinished(success) => {
                    assert!(success);
                    break;
                }
                Event::Note(note) => panic!("unexpected note: {note}"),
                Event::Finished(message) => panic!("{message}"),
                _ => {}
            }
        }
        assert_eq!(
            std::fs::read(hmi.join("Plugins/NbtManager/NbtManager.manifest")).unwrap(),
            b"NbtManager.manifest v2"
        );
        set_mode(&hmi, 0o755);
        commands.send(Command::Disconnect).unwrap();
        assert!(matches!(next(&receive), Event::Finished(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A shim refused by its tests on the head unit is a note: the update stands.
    #[test]
    fn update_notes_a_refused_shim() {
        let port = serve(14);
        let (send, receive) = mpsc::channel();
        let commands = start(settings(port, "test-secret"), send);
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(next(&receive), Event::Connected));
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(next(&receive), Event::Target(true)));
        let release: &'static [u8] = b"verified test release";
        commands
            .send(Command::Update {
                launcher: b"test-launcher",
                release,
                sha256: crate::release::hash_hex(release),
            })
            .unwrap();
        let (mut noted, mut restart) = (false, false);
        loop {
            match receive.recv_timeout(Duration::from_secs(15)).unwrap() {
                Event::Note(note) => {
                    noted |= note.contains("failed its tests") && note.contains("FAILED step 2")
                }
                Event::RestartNeeded => restart = true,
                Event::UpdateFinished(success) => {
                    assert!(success);
                    break;
                }
                Event::Finished(message) => panic!("{message}"),
                _ => {}
            }
        }
        assert!(noted && restart);
        commands.send(Command::Disconnect).unwrap();
    }

    #[test]
    fn update_stream_checks_acknowledgement() {
        for response in [4, 5] {
            let port = serve(response);
            let (send, receive) = mpsc::channel();
            let commands = start(settings(port, "test-secret"), send);
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Connected));
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Target(true)));
            let release: &'static [u8] = b"verified test release";
            commands
                .send(Command::Update {
                    launcher: b"test-launcher",
                    release,
                    sha256: crate::release::hash_hex(release),
                })
                .unwrap();
            // Staged, then (response 4) the launcher and plugin replaced at once.
            let mut refreshed = false;
            loop {
                match next(&receive) {
                    Event::UpdateFinished(success) => {
                        assert_eq!(success, response == 4);
                        break;
                    }
                    Event::Message(message) => {
                        refreshed |= message
                            == "iDrive entry unchanged (same checksums): no iDrive restart needed."
                    }
                    Event::Note(note) => panic!("unexpected note: {note}"),
                    Event::RestartNeeded => panic!("unchanged plugin, but a restart was asked"),
                    _ => {}
                }
            }
            assert_eq!(refreshed, response == 4);
            commands.send(Command::Disconnect).unwrap();
            assert!(matches!(next(&receive), Event::Finished(_)));
        }
    }

    /// An NBTRLS2 release updates any installed launcher: the uploaded one
    /// stages it (the fake checks the path), and nothing asks the installed
    /// launcher what it can read.
    #[test]
    fn v2_update_is_staged_by_the_uploaded_launcher() {
        let v2_release: &'static [u8] = b"NBTRLS2\npayload";
        let port = serve(4);
        let (send, receive) = mpsc::channel();
        let commands = start(settings(port, "test-secret"), send);
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(next(&receive), Event::Connected));
        assert!(matches!(next(&receive), Event::Message(_)));
        assert!(matches!(next(&receive), Event::Target(true)));
        commands
            .send(Command::Update {
                launcher: b"test-launcher",
                release: v2_release,
                sha256: crate::release::hash_hex(v2_release),
            })
            .unwrap();
        loop {
            match receive.recv_timeout(Duration::from_secs(15)).unwrap() {
                Event::UpdateFinished(ok) => {
                    assert!(ok);
                    break;
                }
                Event::Note(note) => panic!("unexpected note: {note}"),
                Event::Finished(message) => panic!("{message}"),
                _ => {}
            }
        }
        commands.send(Command::Disconnect).unwrap();
        assert!(matches!(next(&receive), Event::Finished(_)));
    }

    #[test]
    fn update_on_a_car_without_the_manager_says_to_install() {
        let said = known_error(
            b"/etc/profile[28]: bind: cannot bind, not a tty\n\
              nbt-manager-launcher: managed root must be an owned, private directory\n",
        );
        assert_eq!(
            said.as_deref(),
            Some("NBT Manager is not installed on this head unit: use Install.")
        );
    }

    #[test]
    fn refused_ssh_requires_activation_confirmation_and_can_be_declined() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (send, receive) = mpsc::channel();
        let _commands = start(settings(port, "test-secret"), send);
        // A refused connection asks first, before any gateway search.
        loop {
            match next(&receive) {
                Event::Message(_) => {}
                Event::ConfirmActivation {
                    host,
                    hint,
                    respond,
                } => {
                    assert_eq!(host, format!("127.0.0.1:{port}"));
                    // A routable address: reachable by ordinary routing, so no advice.
                    assert_eq!(hint, None);
                    respond.send(false).unwrap();
                    break;
                }
                _ => panic!("expected activation confirmation"),
            }
        }
        assert!(matches!(
            next(&receive),
            Event::Finished("SSH did not answer; enabling it was declined.")
        ));
    }

    #[test]
    fn unsupported_or_invalid_target_probe_fails_closed() {
        for response in 1..=3 {
            let port = serve(response);
            let (send, receive) = mpsc::channel();
            let commands = start(settings(port, "test-secret"), send);
            assert!(matches!(next(&receive), Event::Message(_)));
            assert!(matches!(next(&receive), Event::Connected));
            assert!(matches!(
                next(&receive),
                Event::Message(
                    "Could not verify that the head unit runs QNX. Installation remains unavailable."
                )
            ));
            assert!(matches!(next(&receive), Event::Target(false)));
            commands.send(Command::Disconnect).unwrap();
            assert!(matches!(next(&receive), Event::Finished(_)));
        }
    }

    #[cfg(unix)]
    fn connect_trusted(port: u16) -> (async_mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
        let (send, receive) = mpsc::channel();
        let commands = start(settings(port, "test-secret"), send);
        loop {
            match next(&receive) {
                Event::Target(target) => {
                    assert!(target);
                    break;
                }
                Event::Finished(message) => panic!("{message}"),
                _ => {}
            }
        }
        (commands, receive)
    }

    #[cfg(unix)]
    fn bench_result(
        commands: &async_mpsc::UnboundedSender<Command>,
        receive: &mpsc::Receiver<Event>,
        action: bench::Action,
        test_app: Option<TestApp>,
    ) -> (bool, String, Option<Vec<u8>>) {
        commands.send(Command::Bench { action, test_app }).unwrap();
        let mut log = None;
        loop {
            match receive.recv_timeout(Duration::from_secs(60)).unwrap() {
                Event::LogFetched { data, .. } | Event::Collected(data) => log = Some(data),
                Event::BenchFinished { ok, message } => return (ok, message, log),
                Event::Finished(message) => panic!("{message}"),
                _ => {}
            }
        }
    }

    /// The launcher's step-by-step tools (iDrive-only restart, shim trial,
    /// keep on/off) pass on their final OK line alone; their lines reach the
    /// details either way.
    #[cfg(unix)]
    #[test]
    fn launcher_tools_need_their_ok_line() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nbt-hmi-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cases = [
            (
                bench::Action::RestartHmiOnly,
                "--restart-hmi",
                "HMI_RESTART OK HmiMain3D pid 7 after 900 ms",
                0,
                "stayed connected",
            ),
            (
                bench::Action::RestartHmiOnly,
                "--restart-hmi",
                "HMI_RESTART FAILED HmiMain3D did not stop",
                1,
                "did not complete",
            ),
            (
                bench::Action::TryShim,
                "--try-shim",
                "SHIM_TRIAL OK iDrive pid 7 runs with the shim",
                0,
                "ran 20 s",
            ),
            (
                bench::Action::TryShim,
                "--try-shim",
                "SHIM_TRIAL iDrive pid 7 loaded the shim",
                0,
                "did not pass",
            ),
            (
                bench::Action::Shim(true),
                "--shim on",
                "SHIM FAILED no shim installed: run Update",
                1,
                "refused",
            ),
            (
                bench::Action::Shim(false),
                "--shim off",
                "SHIM OFF iDrive starts without the shim",
                0,
                "Shim off",
            ),
        ];
        for (index, (action, args, last, code, expect)) in cases.into_iter().enumerate() {
            let launcher = dir.join(format!("launcher-{index}"));
            std::fs::write(
                &launcher,
                format!(
                    "#!/bin/sh\n[ \"$*\" = \"{args}\" ] || exit 9\necho 'STEP one'\necho '{last}'\nexit {code}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
            let port = serve_target(
                0,
                None,
                Some(ShellTarget {
                    root: dir.join("root"),
                    plugins: dir.join("Plugins"),
                    launcher: Some(launcher),
                }),
                Algorithm::Ed25519,
            );
            let (commands, receive) = connect_trusted(port);
            let ok_expected =
                expect != "did not complete" && expect != "did not pass" && expect != "refused";
            commands
                .send(Command::Bench {
                    action,
                    test_app: None,
                })
                .unwrap();
            let mut details = Vec::new();
            let (ok, message) = loop {
                match receive.recv_timeout(Duration::from_secs(60)).unwrap() {
                    Event::Detail(line) => details.push(line),
                    Event::BenchFinished { ok, message } => break (ok, message),
                    Event::Finished(message) => panic!("{message}"),
                    _ => {}
                }
            };
            assert_eq!(ok, ok_expected, "{args}: {message}");
            assert!(message.contains(expect), "{message}");
            assert!(details.iter().any(|line| line == "STEP one"), "{details:?}");
            assert!(details.iter().any(|line| line == last), "{details:?}");
            commands.send(Command::Disconnect).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An sshd as old as the NBT-EVO's: AAD's OpenSSH command needs
    /// `HostKeyAlgorithms=+ssh-rsa -m hmac-sha1`, so the server predates
    /// rsa-sha2 and hmac-sha2 (OpenSSH < 5.9) and has no curve25519/ECDH key
    /// exchange. Group exchange is the only overlap with a modern client, and
    /// such a server answers it with the 2048-bit group 14. OpenSSH accepts
    /// groups from 2048 bits; russh's default minimum is 3072, which failed
    /// the handshake ("SSH connection or host verification failed").
    #[cfg(unix)]
    #[test]
    fn connects_to_a_legacy_sshd() {
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                send.send(listener.local_addr().unwrap().port()).unwrap();
                let key =
                    PrivateKey::random(&mut rand::rng(), Algorithm::Rsa { hash: None }).unwrap();
                let config = server::Config {
                    keys: vec![key],
                    preferred: Preferred {
                        kex: Cow::Owned(vec![
                            russh::kex::DH_GEX_SHA256,
                            russh::kex::DH_GEX_SHA1,
                            russh::kex::DH_G14_SHA1,
                            russh::kex::DH_G1_SHA1,
                        ]),
                        key: Cow::Owned(vec![Algorithm::Rsa { hash: None }]),
                        mac: Cow::Owned(vec![mac::HMAC_SHA1]),
                        cipher: Cow::Owned(vec![russh::cipher::AES_128_CTR]),
                        ..Preferred::default()
                    },
                    auth_rejection_time: Duration::ZERO,
                    auth_rejection_time_initial: Some(Duration::ZERO),
                    ..Default::default()
                };
                let _ = tokio::time::timeout(
                    Duration::from_secs(30),
                    TestServer {
                        response: 0,
                        upload: None,
                        install_path: None,
                        native: None,
                        shell: None,
                        legacy_gex: true,
                        remounted: false,
                    }
                    .run_on_socket(Arc::new(config), &listener),
                )
                .await;
            });
        });
        let port = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        let (commands, _receive) = connect_trusted(port);
        commands.send(Command::Disconnect).unwrap();
    }

    /// The iDrive restart takes SSH down: no answer means it is under way, and
    /// the installer disconnects. A refusal is reported and the link stays.
    #[cfg(unix)]
    #[test]
    fn restart_disconnects_unless_refused() {
        let port = serve_target(0, None, None, Algorithm::Ed25519);
        let (commands, receive) = connect_trusted(port);
        let started = std::time::Instant::now();
        let (ok, message, _) =
            bench_result(&commands, &receive, bench::Action::RestartIdrive, None);
        assert!(ok, "{message}");
        // Not the 20 s every car restart used to wait out.
        assert!(
            started.elapsed() < Duration::from_secs(RESTART_ANSWER_SECONDS + 3),
            "{:?}",
            started.elapsed()
        );
        loop {
            match receive.recv_timeout(Duration::from_secs(30)).unwrap() {
                Event::Finished(message) => {
                    assert!(
                        message.starts_with("Disconnected while iDrive restarts"),
                        "{message}"
                    );
                    break;
                }
                _ => {}
            }
        }

        // The test machine has no OnOffDSICommander: the shell refuses at once.
        // The restart touches neither folder.
        let unused = std::env::temp_dir().join("nbt-restart-unused");
        let port = serve_target(
            0,
            None,
            Some(ShellTarget {
                root: unused.join("root"),
                plugins: unused.join("Plugins"),
                launcher: None,
            }),
            Algorithm::Ed25519,
        );
        let (commands, receive) = connect_trusted(port);
        let (ok, message, _) =
            bench_result(&commands, &receive, bench::Action::RestartIdrive, None);
        assert!(!ok && message.contains("reported an error"), "{message}");
        let (ok, _, _) = bench_result(&commands, &receive, bench::Action::RestartIdrive, None);
        assert!(!ok);
        commands.send(Command::Disconnect).unwrap();
    }

    #[test]
    fn a_restart_that_was_never_sent_is_not_under_way() {
        for error in [
            "timed out opening an SSH channel",
            "could not send the command: x",
            "timed out sending the command",
        ] {
            assert!(not_sent(error), "{error}");
        }
        for error in [
            "no end after 5 s (0 of 0 bytes sent)",
            "no answer to the command request; x",
            "the channel closed before the command started; x",
        ] {
            assert!(!not_sent(error), "{error}");
        }
    }

    /// Older notes recorded a DSA (ssh-dss) host key; keep accepting it.
    #[cfg(unix)]
    #[test]
    fn connects_to_a_dsa_host_key() {
        let port = serve_target(0, None, None, Algorithm::Dsa);
        let (commands, _receive) = connect_trusted(port);
        commands.send(Command::Disconnect).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_tools_run_their_commands_over_ssh() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!(
            "nbt-bench-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("root");
        let plugins = base.join("Plugins");
        for part in [
            "",
            "apps",
            "data",
            "state",
            "state/manager",
            "trust",
            "trust/apps",
            "trust/keys",
            "manager",
        ] {
            std::fs::create_dir_all(root.join(part)).unwrap();
            std::fs::set_permissions(root.join(part), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        std::fs::create_dir_all(plugins.join("NbtManager")).unwrap();
        std::fs::write(plugins.join("NbtManager/libNbtManager.so"), b"plugin").unwrap();
        std::fs::create_dir_all(plugins.join("Other")).unwrap();
        let launcher = std::env::var_os("NBT_TEST_LAUNCHER_HOST").map(PathBuf::from);
        let port = serve_target(
            0,
            None,
            Some(ShellTarget {
                root: root.clone(),
                plugins: plugins.clone(),
                launcher: launcher.clone(),
            }),
            Algorithm::Ed25519,
        );
        let (commands, receive) = connect_trusted(port);
        let manager = root.join("manager");
        let run = |action| bench_result(&commands, &receive, action, None);

        let (ok, message, _) = run(bench::Action::FetchLog);
        assert!(!ok && message.contains("no debug log"), "{message}");
        assert!(run(bench::Action::LogOn).0);
        let mode = std::fs::metadata(manager.join("debug.log"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::write(manager.join("debug.log.1"), b"older\n").unwrap();
        std::fs::write(manager.join("debug.log"), b"newer\n").unwrap();
        let (ok, message, log) = run(bench::Action::FetchLog);
        assert!(ok, "{message}");
        assert_eq!(log.unwrap(), b"older\nnewer\n");
        // Collect everything: this host lacks the head unit's tools (sloginfo,
        // pidin, audioconn); their sections say so and the others still run.
        let (ok, message, report) = run(bench::Action::CollectAll);
        assert!(ok, "{message}");
        let report = String::from_utf8_lossy(&report.unwrap()).into_owned();
        for section in crate::collect::sections() {
            assert!(
                report.contains(&format!("===== {} =====", section.name)),
                "{report}"
            );
        }
        assert!(report.contains("===== debug log =====\n$ cat "), "{report}");
        assert!(report.contains("debug.log 2>/dev/null; exit 0\nolder\nnewer\n----- exit 0 -----"));
        let audio = report
            .split("===== audio connections =====")
            .nth(1)
            .unwrap();
        assert!(
            !audio
                .split("=====")
                .next()
                .unwrap()
                .contains("----- exit 0 -----")
        );
        assert!(
            message.contains(&format!("Collected {} bytes", report.len())),
            "{message}"
        );
        assert!(manager.join("debug.log").exists() && manager.join("debug.log.1").exists());
        assert!(run(bench::Action::LogOff).0);
        assert!(!manager.join("debug.log").exists() && !manager.join("debug.log.1").exists());
        // This host has no sloginfo: the failure is reported and nothing is saved.
        let (ok, message, log) = run(bench::Action::FetchSystemLog);
        assert!(!ok && message.contains("reported an error"), "{message}");
        assert!(log.is_none());

        let config = "NBTCTL1\nrelease=1\n".to_owned();
        assert!(run(bench::Action::WriteConfig(config.clone())).0);
        assert_eq!(
            std::fs::read_to_string(manager.join("controller.conf")).unwrap(),
            config
        );
        let mode = std::fs::metadata(manager.join("controller.conf"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let (ok, message, _) = run(bench::Action::WriteConfig("x=1\n".into()));
        assert!(!ok && message.contains("NBTCTL1"), "{message}");
        assert_eq!(
            std::fs::read_to_string(manager.join("controller.conf")).unwrap(),
            config
        );
        assert!(run(bench::Action::RemoveConfig).0);
        assert!(!manager.join("controller.conf").exists());

        let (ok, message, _) = run(bench::Action::ProvisionTestApp);
        assert!(
            !ok && message.contains("no embedded test application"),
            "{message}"
        );
        // With launcher-host and a package (set by the kit build), provision for real.
        if let (Some(_), Some(package), Some(key)) = (
            &launcher,
            std::env::var_os("NBT_TEST_PROVISION_PACKAGE"),
            std::env::var_os("NBT_TEST_PROVISION_KEY"),
        ) {
            let package: &'static [u8] =
                Box::leak(std::fs::read(package).unwrap().into_boxed_slice());
            let key: &'static [u8] = Box::leak(std::fs::read(key).unwrap().into_boxed_slice());
            let app = TestApp {
                package,
                key,
                compatibility: std::env::var("NBT_TEST_PROVISION_COMPAT").unwrap(),
                sha256: crate::release::hash_hex(package),
            };
            let (ok, message, _) = bench_result(
                &commands,
                &receive,
                bench::Action::ProvisionTestApp,
                Some(app.clone()),
            );
            assert!(ok, "{message}");
            assert!(
                root.join("apps/test/bin/app").exists()
                    && root.join("trust/apps/test.conf").exists()
            );
            let (ok, message, _) = bench_result(
                &commands,
                &receive,
                bench::Action::ProvisionTestApp,
                Some(app),
            );
            assert!(!ok && message.contains("already installed"), "{message}");
        }

        // Read-only until the removal's own remount, as in the car.
        std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o555)).unwrap();
        let (ok, message, _) = run(bench::Action::RemoveAll);
        assert!(ok, "{message}");
        assert!(
            !root.exists()
                && !plugins.join("NbtManager").exists()
                && plugins.join("Other").exists()
        );
        assert_eq!(
            std::fs::metadata(&plugins).unwrap().permissions().mode() & 0o777,
            0o555
        );
        set_mode(&plugins, 0o755);
        let (ok, message, _) = run(bench::Action::LogOn);
        assert!(!ok && message.contains("not installed"), "{message}");
        commands.send(Command::Disconnect).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }
}
