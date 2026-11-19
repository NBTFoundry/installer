//! Bench and in-car test tools: fixed remote commands for the debug log, the
//! test application, the controller configuration and complete removal.
//! Commands use only what the head unit's minimal image is known to have:
//! the QNX shell's built-ins (echo, test, umask, :), cat, chmod, rm, mount,
//! sloginfo and OnOffDSICommander, as typed in an SSH session (ssh.rs runs
//! each in a login shell).
//! It has no printf (the first in-car install failed on it). Each is used as
//! the QNX 6.5 utility reference documents it. That reference does not say
//! whether `rm -f` exits 0 when a file is missing, so no command depends on
//! rm's exit status: `test ! -e` checks the result instead.
//! Every command is a constant or built only from validated values; nothing
//! typed by the user reaches a shell except the controller configuration,
//! which travels on standard input after validation.

pub const ROOT: &str = "/var/opt/nbt-manager";
pub const PLUGINS: &str = "/opt/hmi/ID5/data/ro/Plugins";
pub const CAPTURE: &str = "/opt/hmi/ID5/data/ro/NbtCapture";
/// The capture shim since dev105, next to iDrive's environment file.
pub const CAPTURE_SHIM: &str = "/var/opt/nbt-manager/state/manager/libnbt-capture.so";
/// Makes the iDrive plugin folder writable (it is mounted read-only at boot),
/// run on the processor SSH lands on: QNX 6.5 `mount -u` (remount) with `-w`
/// (read/write). Install and removal run it first, every time; remounting an
/// already writable partition changes nothing.
pub const REMOUNT: &str = "mount -uw /net/hu-omap/fs/sda0 && echo OK";
/// Puts the partition back to read-only, as it is after boot: install and
/// removal run it last, whether they succeeded or not (`mount -ur`).
pub const REMOUNT_READ_ONLY: &str = "mount -ur /net/hu-omap/fs/sda0 && echo OK";
pub const LAUNCHER: &str = "/var/opt/nbt-manager/manager/nbt-manager-launcher";
/// The test application's id; the kit embeds its first package.
pub const TEST_APP: &str = "test";
pub const CONFIG_MAX: usize = 1024;
/// The documented controller settings, prefilled in the editors.
pub const CONFIG_EXAMPLE: &str = include_str!("../assets/controller.conf.example");
/// debug.log and debug.log.1, each at most 128 KiB plus a few lines.
pub const LOG_MAX: usize = 300 * 1024;
/// `sloginfo` text: the car's slogger keeps a 1 MiB binary buffer
/// (`slogger -s 1024k`), which printed as 5.2 MB of text in the inventory.
pub const SYSTEM_LOG_MAX: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    LogOn,
    LogOff,
    FetchLog,
    /// The QNX system log (slog) of the iDrive processor, where iDrive and
    /// other plugins such as NBT-AAD write.
    FetchSystemLog,
    ProvisionTestApp,
    WriteConfig(String),
    RemoveConfig,
    /// Removes the manager and its plugin; REMOUNT runs first and
    /// REMOUNT_READ_ONLY last (see `ssh.rs`).
    RemoveAll,
    /// Restarts the whole head unit (`OnOffDSICommander appreset`), as switching
    /// the car off and on does; SSH switches off. Not an iDrive-only restart.
    RestartIdrive,
    /// Experimental: restarts only iDrive's program (HmiMain3D) through the
    /// process manager, keeping SSH up (docs/hmi-restart.md).
    RestartHmiOnly,
    /// Restarts iDrive with the capture shim now and checks that it loaded the
    /// shim and keeps running (docs/hmi-restart.md). The manager otherwise
    /// arms and keeps the shim by itself.
    TryShim,
    /// Turns the automatic shim on again (after it failed or was turned off),
    /// or off.
    Shim(bool),
    /// One read-only report of what a car run needs (collect.rs): the logs,
    /// the audio connections, the processes and the manager's files.
    CollectAll,
}

/// The remote command for an action, as typed in an SSH session (`ssh.rs`
/// runs it in a login shell), or None for ProvisionTestApp (see `provision`).
pub fn command(action: &Action) -> Option<String> {
    let manager = format!("{ROOT}/manager");
    Some(match action {
        Action::LogOn => format!(
            "umask 077; test -d {manager} || exit 3; : >> {manager}/debug.log && chmod 600 {manager}/debug.log && echo OK"
        ),
        Action::LogOff => format!(
            "test -d {manager} || exit 3; rm -f {manager}/debug.log {manager}/debug.log.1; test ! -e {manager}/debug.log && test ! -e {manager}/debug.log.1 && echo OK"
        ),
        Action::FetchLog => format!(
            "test -f {manager}/debug.log || test -f {manager}/debug.log.1 || exit 4; cat {manager}/debug.log.1 {manager}/debug.log 2>/dev/null; exit 0"
        ),
        Action::FetchSystemLog => "sloginfo".into(),
        Action::WriteConfig(_) => format!(
            "umask 077; test -d {manager} || exit 3; rm -f {manager}/controller.conf; test ! -e {manager}/controller.conf && cat > {manager}/controller.conf && chmod 600 {manager}/controller.conf && echo OK"
        ),
        Action::RemoveConfig => format!(
            "test -d {manager} || exit 3; rm -f {manager}/controller.conf; test ! -e {manager}/controller.conf && echo OK"
        ),
        // Running copies keep their open files until the reboot that unloads the plugin.
        // Only a launcher that installed the capture shim is asked to remove it: an
        // older one rejects --remove-capture (exit 2), which would block the removal.
        // With the shim present but no launcher, nothing is removed (starter.cfg
        // still names the shim).
        Action::RemoveAll => format!(
            "(test ! -e {CAPTURE} -a ! -e {CAPTURE_SHIM} || {LAUNCHER} --remove-capture >/dev/null) && rm -rf {PLUGINS}/NbtManager {PLUGINS}/.NbtManager.setup {ROOT}; test ! -e {ROOT} && test ! -e {PLUGINS}/NbtManager && test ! -e {PLUGINS}/.NbtManager.setup && test ! -e {CAPTURE} && echo REMOVED"
        ),
        Action::RestartIdrive => "OnOffDSICommander appreset".into(),
        Action::RestartHmiOnly => format!("{LAUNCHER} --restart-hmi"),
        Action::TryShim => format!("{LAUNCHER} --try-shim"),
        Action::Shim(on) => {
            format!("{LAUNCHER} --shim {}", if *on { "on" } else { "off" })
        }
        Action::ProvisionTestApp | Action::CollectAll => return None,
    })
}

/// What a successful command prints on standard output.
pub fn expected(action: &Action) -> &'static [u8] {
    match action {
        Action::RemoveAll => b"REMOVED\n",
        Action::ProvisionTestApp => b"PROVISIONED\n",
        Action::FetchLog
        | Action::FetchSystemLog
        | Action::RestartIdrive
        | Action::RestartHmiOnly
        | Action::TryShim
        | Action::Shim(_) => b"",
        _ => b"OK\n",
    }
}

/// For the launcher's step-by-step tools: the line start that means success.
/// Their lines are shown as details, whatever the outcome.
pub fn marker(action: &Action) -> Option<&'static [u8]> {
    match action {
        Action::RestartHmiOnly => Some(b"HMI_RESTART OK "),
        Action::TryShim => Some(b"SHIM_TRIAL OK "),
        Action::Shim(true) => Some(b"SHIM ON "),
        Action::Shim(false) => Some(b"SHIM OFF "),
        _ => None,
    }
}

fn token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
}

fn sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The launcher command that provisions the test application.
pub fn provision(compat: &str, key: usize, package: usize, sha256: &str) -> Option<String> {
    if !token(compat, 48)
        || key == 0
        || key > 16384
        || package == 0
        || package > 64 * 1024 * 1024 + 4096
        || !sha256_hex(sha256)
    {
        return None;
    }
    Some(format!(
        "{LAUNCHER} --provision-app {TEST_APP} {compat} {key} {package} {sha256}"
    ))
}

/// A controller configuration the GUI can parse: the NBTCTL1 tag, Unix line
/// endings, printable ASCII, at most CONFIG_MAX bytes. The GUI checks the keys
/// and values and falls back to defaults (with a notice) if they are wrong.
pub fn valid_config(text: &str) -> bool {
    text.len() <= CONFIG_MAX
        && text.starts_with("NBTCTL1\n")
        && text.ends_with('\n')
        && text
            .bytes()
            .all(|b| b == b'\n' || (0x20..0x7f).contains(&b))
}

/// A message for a failed action, from its exit status and standard error.
/// The launcher's message is the last line: the login profile prints first.
pub fn failure(action: &Action, exit: Option<u32>, errors: &[u8]) -> &'static str {
    let errors = errors
        .split(|&b| b == b'\n')
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or(&[]);
    match (action, exit) {
        (_, Some(3)) => "NBT Manager is not installed on this head unit.",
        (Action::FetchLog, Some(4)) => {
            "There is no debug log on the head unit. Switch it on first."
        }
        (Action::ProvisionTestApp, _) if errors == b"application already provisioned" => {
            "The test application is already installed: uninstall it in the manager first (hold its name, then Uninstall)."
        }
        (Action::ProvisionTestApp, _) if errors == b"manager operation is in progress" => {
            "NBT Manager is open on the car. Close it (back to the iDrive menu), then try again."
        }
        (Action::ProvisionTestApp, _) if errors == b"manager update awaiting confirmation" => {
            "The manager update is not confirmed yet. Open NBT Manager on the car, keep it open for 10 seconds, close it, then try again."
        }
        (Action::ProvisionTestApp, _) if errors.starts_with(b"manager layout missing") => {
            "Install NBT Manager first."
        }
        (Action::ProvisionTestApp, _) if errors.starts_with(b"package signature") => {
            "The head unit rejected the test package (signature, id, compatibility or digest)."
        }
        (Action::RemoveAll, _) => "Removal failed; the head unit's error is in the debug log.",
        (Action::RestartHmiOnly, _) => {
            "The iDrive-only restart did not complete; its steps are in the details."
        }
        (Action::TryShim, _) => "The shim trial did not pass; its steps are in the details.",
        (Action::Shim(_), _) => "The head unit refused; the reason is in the details.",
        _ => "The head unit reported an error.",
    }
}

pub fn done(action: &Action, bytes: usize) -> String {
    match action {
        Action::LogOn => "Debug log on. The manager logs from its next start; the iDrive entry from its next selection.".into(),
        Action::LogOff => "Debug log off; both log files removed.".into(),
        Action::FetchLog => format!("Fetched {bytes} bytes of debug log."),
        Action::FetchSystemLog => format!("Fetched {bytes} bytes of system log."),
        Action::ProvisionTestApp => "Test application v1 installed. It appears in NBT Manager's list.".into(),
        Action::WriteConfig(_) => "controller.conf written. It applies the next time the manager opens.".into(),
        Action::RemoveConfig => "controller.conf removed; defaults apply from the next start.".into(),
        Action::RemoveAll => "NBT Manager and its iDrive plugin were removed. Switch the car off and on to unload the plugin.".into(),
        Action::RestartIdrive => "The head unit is restarting (the whole system): the screen goes dark and SSH switches off.".into(),
        Action::RestartHmiOnly => "iDrive restarted on its own; the installer stayed connected.".into(),
        Action::TryShim => "iDrive ran 20 s with the shim; it keeps it for the next starts.".into(),
        Action::Shim(true) => "Shim on: iDrive loads it at its next start and keeps it after 60 s of running.".into(),
        Action::Shim(false) => "Shim off: iDrive starts without it from its next start.".into(),
        Action::CollectAll => format!("Collected {bytes} bytes: logs, audio connections, processes and the manager's files."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_fixed_and_quoted_once() {
        for action in [
            Action::LogOn,
            Action::LogOff,
            Action::FetchLog,
            Action::WriteConfig("x".into()),
            Action::RemoveConfig,
            Action::RemoveAll,
        ] {
            let command = command(&action).unwrap();
            // ssh.rs quotes each command once for the login shell.
            assert!(!command.contains('\''), "{command}");
        }
        // The configuration text never appears in the command.
        assert!(
            !command(&Action::WriteConfig("NBTCTL1\nrelease=1\n".into()))
                .unwrap()
                .contains("release")
        );
        assert!(command(&Action::ProvisionTestApp).is_none());
        assert_eq!(
            command(&Action::RestartIdrive).unwrap(),
            "OnOffDSICommander appreset"
        );
        assert_eq!(
            command(&Action::RestartHmiOnly).unwrap(),
            "/var/opt/nbt-manager/manager/nbt-manager-launcher --restart-hmi"
        );
        assert_eq!(
            command(&Action::TryShim).unwrap(),
            "/var/opt/nbt-manager/manager/nbt-manager-launcher --try-shim"
        );
        assert_eq!(
            command(&Action::Shim(false)).unwrap(),
            "/var/opt/nbt-manager/manager/nbt-manager-launcher --shim off"
        );
        // The iDrive processor's slog, printed to standard output; it reads nothing.
        assert_eq!(command(&Action::FetchSystemLog).unwrap(), "sloginfo");
        assert_eq!(expected(&Action::FetchSystemLog), b"");
        let removal = command(&Action::RemoveAll).unwrap();
        assert!(removal.starts_with(
            "(test ! -e /opt/hmi/ID5/data/ro/NbtCapture -a ! -e /var/opt/nbt-manager/state/manager/libnbt-capture.so || /var/opt/nbt-manager/manager/nbt-manager-launcher --remove-capture >/dev/null) && rm -rf"
        ));
        assert!(removal.contains("rm -rf /opt/hmi/ID5/data/ro/Plugins/NbtManager /opt/hmi/ID5/data/ro/Plugins/.NbtManager.setup /var/opt/nbt-manager; test ! -e"));
        assert!(removal.contains("test ! -e /opt/hmi/ID5/data/ro/NbtCapture"));
    }

    #[test]
    fn provision_command_takes_only_validated_values() {
        let digest = "ab".repeat(32);
        assert_eq!(
            provision("nbt-test-1", 451, 2000, &digest).unwrap(),
            format!(
                "/var/opt/nbt-manager/manager/nbt-manager-launcher --provision-app test nbt-test-1 451 2000 {digest}"
            )
        );
        for compat in ["", "A", "a b", "a;b", "a'b", &"a".repeat(49)] {
            assert!(provision(compat, 1, 1, &digest).is_none(), "{compat}");
        }
        assert!(provision("x", 0, 1, &digest).is_none());
        assert!(provision("x", 1, 0, &digest).is_none());
        assert!(provision("x", 16385, 1, &digest).is_none());
        assert!(provision("x", 1, 1, &"AB".repeat(32)).is_none());
        assert!(provision("x", 1, 1, "ab").is_none());
    }

    #[test]
    fn controller_config_validation() {
        assert!(valid_config("NBTCTL1\n"));
        assert!(valid_config(CONFIG_EXAMPLE));
        for bad in [
            "",
            "NBTCTL1",
            "NBTCTL2\n",
            "NBTCTL1\r\n",
            "NBTCTL1\nx=1\r\n",
            "NBTCTL1\n\u{e9}\n",
            "NBTCTL1\n\t\n",
        ] {
            assert!(!valid_config(bad), "{bad:?}");
        }
        assert!(!valid_config(&format!(
            "NBTCTL1\n{}\n",
            "#".repeat(CONFIG_MAX)
        )));
    }

    #[test]
    fn failures_are_explained() {
        assert_eq!(
            failure(&Action::LogOn, Some(3), b""),
            "NBT Manager is not installed on this head unit."
        );
        assert!(failure(&Action::FetchLog, Some(4), b"").contains("no debug log"));
        assert!(
            failure(
                &Action::ProvisionTestApp,
                Some(1),
                b"application already provisioned\n"
            )
            .contains("Uninstall")
        );
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\nmanager layout missing; install the manager first\n";
        assert_eq!(
            failure(&Action::ProvisionTestApp, Some(1), car),
            "Install NBT Manager first."
        );
        // dev67/dev69 in the car: the raw "manager operation is in progress".
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\nmanager operation is in progress\n";
        assert!(failure(&Action::ProvisionTestApp, Some(1), car).contains("Close it"));
        let car = b"/bin/sh: /etc/profile[28]: bind: cannot bind, not a tty\nmanager update awaiting confirmation\n";
        assert!(failure(&Action::ProvisionTestApp, Some(1), car).contains("10 seconds"));
        assert!(failure(&Action::RemoveAll, Some(1), b"").contains("Removal failed"));
    }
}
