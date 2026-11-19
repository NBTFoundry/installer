//! Opens an interactive OpenSSH session to the head unit in a terminal window,
//! for debugging by hand. `ssh` is taken from the PATH (Windows ships it).
//!
//! OpenSSH takes no password argument. The installer itself is given to ssh as
//! its askpass program (`SSH_ASKPASS` with `SSH_ASKPASS_REQUIRE=force`, OpenSSH
//! 8.4 and later): started that way, it prints the password and exits. An older
//! ssh ignores both and asks; the password is shown in Connection settings.

use std::io;
use std::process::Command;

/// Set only in the environment of the ssh this module starts: the password the
/// askpass invocation prints.
pub const ASKPASS_PASSWORD: &str = "NBT_INSTALLER_ASKPASS_PASSWORD";

#[cfg(windows)]
const NO_KNOWN_HOSTS: &str = "NUL";
#[cfg(not(windows))]
const NO_KNOWN_HOSTS: &str = "/dev/null";

/// Host names and addresses: letters, digits, dots and hyphens only. On Windows
/// the command line passes through cmd.exe (`start`), so nothing else may
/// reach it.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 32
        && !user.starts_with('-')
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// ssh's arguments, or None for an invalid field. The head unit's old sshd
/// needs the ssh-rsa host key (the key the installer logs) and HMAC-SHA1, as
/// AAD's guide uses. Host keys are not recorded, as in the installer: the
/// cable is direct and a reflash changes the key.
pub fn ssh_args(host: &str, port: &str, user: &str) -> Option<Vec<String>> {
    let port: u16 = port.parse().ok().filter(|port| *port != 0)?;
    if !valid_host(host) || !valid_user(user) {
        return None;
    }
    Some(
        [
            "-o",
            "HostKeyAlgorithms=+ssh-rsa",
            "-m",
            "hmac-sha1",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            &format!("UserKnownHostsFile={NO_KNOWN_HOSTS}"),
            "-p",
            &port.to_string(),
            &format!("{user}@{host}"),
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect(),
    )
}

/// Starts ssh in a new terminal window; returns once the window is started.
pub fn open(args: &[String], password: &str) -> io::Result<()> {
    let askpass = std::env::current_exe()?;
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "NBT head unit", "ssh"]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("x-terminal-emulator");
        command.args(["-e", "ssh"]);
        command
    };
    command
        .args(args)
        .env("SSH_ASKPASS", askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(ASKPASS_PASSWORD, password)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_carry_the_old_sshd_options_and_only_validated_fields() {
        let args = ssh_args("169.254.199.119", "22", "root").unwrap();
        assert_eq!(
            args[..4],
            ["-o", "HostKeyAlgorithms=+ssh-rsa", "-m", "hmac-sha1"]
        );
        assert_eq!(args.last().unwrap(), "root@169.254.199.119");
        assert!(args.windows(2).any(|pair| pair == ["-p", "22"]));
        // Nothing cmd.exe or ssh would interpret gets through.
        for host in [
            "",
            "a&calc",
            "a b",
            "-oProxyCommand=x",
            "a|b",
            "a\"b",
            "a^b",
        ] {
            assert!(ssh_args(host, "22", "root").is_none(), "{host}");
        }
        for user in ["", "-l", "r&t", "r t"] {
            assert!(ssh_args("10.0.0.1", "22", user).is_none(), "{user}");
        }
        for port in ["", "0", "70000", "22 ", "2&2"] {
            assert!(ssh_args("10.0.0.1", port, "root").is_none(), "{port}");
        }
    }
}
