// SPDX-License-Identifier: MPL-2.0
//! NBTApps (github.com/tom-pryor/nbtapps), another installer for these head
//! units. Its iDrive plugin asks for the same focus id as NBT Manager's App
//! Manager entry, so iDrive loads only one of the two. Before Install and
//! Update the installer looks for it and, when the user agrees, moves it
//! aside; otherwise the operation stops.
//!
//! Moved aside, never deleted: its agent is stopped, and its two folders are
//! renamed in place with a `.removed-<seconds>` suffix (the agent's state, its
//! iDrive plugin out of iDrive's plugin folder), so moving them back restores
//! it. Applications it installed stay where they are. The paths are those of
//! NBTApps' own install guide (`headunit/INSTALL.md`): the agent and its state
//! in `/var/opt/nbtapp`, process `nbtagent`, the plugin in `Plugins/NbtApp`.
//! `mv` and `slay` are the commands that guide gives for the same steps on
//! the head unit.

/// The agent, its state and paired phones.
pub const AGENT: &str = "/var/opt/nbtapp";
/// Its iDrive plugin, in iDrive's plugin folder (`bench::PLUGINS`).
pub const PLUGIN: &str = "/opt/hmi/ID5/data/ro/Plugins/NbtApp";
/// Where the plugin goes: beside the plugin folder, on the same partition,
/// where iDrive does not look for plugins.
pub const HMI_RO: &str = "/opt/hmi/ID5/data/ro";

/// What the check found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub agent: bool,
    pub plugin: bool,
}

impl Found {
    pub fn any(&self) -> bool {
        self.agent || self.plugin
    }
}

/// Read-only: which of the two folders is there, then `CHECKED`.
pub fn check_command() -> String {
    format!("test -d {AGENT} && echo AGENT; test -d {PLUGIN} && echo PLUGIN; echo CHECKED")
}

/// The check's output: `AGENT` and `PLUGIN` at most once each, in that order,
/// then `CHECKED`; None for anything else.
pub fn parse_check(stdout: &[u8]) -> Option<Found> {
    let text = std::str::from_utf8(stdout)
        .ok()?
        .strip_suffix("CHECKED\n")?;
    match text {
        "" => Some(Found::default()),
        "AGENT\n" => Some(Found {
            agent: true,
            plugin: false,
        }),
        "PLUGIN\n" => Some(Found {
            agent: false,
            plugin: true,
        }),
        "AGENT\nPLUGIN\n" => Some(Found {
            agent: true,
            plugin: true,
        }),
        _ => None,
    }
}

/// Stops the agent (its failure, when none runs, does not matter) and moves
/// both folders aside; `REMOVED` only when neither is left in place. Runs
/// with the plugin partition writable. `stamp`: seconds by this computer's
/// clock, so a second removal never lands in the first one's folder.
pub fn removal_command(stamp: u64) -> String {
    format!(
        "slay nbtagent >/dev/null 2>&1; \
         if test -d {PLUGIN}; then mv {PLUGIN} {HMI_RO}/NbtApp.removed-{stamp}; fi; \
         if test -d {AGENT}; then mv {AGENT} {AGENT}.removed-{stamp}; fi; \
         test ! -e {PLUGIN} && test ! -e {AGENT} && echo REMOVED"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_plugin_folder() {
        assert_eq!(PLUGIN, format!("{}/NbtApp", crate::bench::PLUGINS));
        assert_eq!(format!("{HMI_RO}/Plugins"), crate::bench::PLUGINS);
    }

    #[test]
    fn check_output_is_read_strictly() {
        assert_eq!(parse_check(b"CHECKED\n"), Some(Found::default()));
        assert_eq!(
            parse_check(b"AGENT\nCHECKED\n"),
            Some(Found {
                agent: true,
                plugin: false
            })
        );
        assert_eq!(
            parse_check(b"PLUGIN\nCHECKED\n"),
            Some(Found {
                agent: false,
                plugin: true
            })
        );
        assert_eq!(
            parse_check(b"AGENT\nPLUGIN\nCHECKED\n"),
            Some(Found {
                agent: true,
                plugin: true
            })
        );
        for bad in [
            &b""[..],
            b"CHECKED",
            b"PLUGIN\nAGENT\nCHECKED\n",
            b"AGENT\nAGENT\nCHECKED\n",
            b"something\nCHECKED\n",
            b"\xffCHECKED\n",
        ] {
            assert_eq!(parse_check(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn removal_moves_aside_and_checks() {
        let command = removal_command(1795680000);
        assert!(command.contains(
            "mv /opt/hmi/ID5/data/ro/Plugins/NbtApp /opt/hmi/ID5/data/ro/NbtApp.removed-1795680000"
        ));
        assert!(command.contains("mv /var/opt/nbtapp /var/opt/nbtapp.removed-1795680000"));
        assert!(command.ends_with("&& echo REMOVED"));
        assert!(!command.contains("rm "));
        assert!(!command.contains('\''));
    }
}
