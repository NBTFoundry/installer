---
name: installer-change
description: Change the NBT Manager installer (NBTFoundry/installer) - the Rust core shared by the desktop window and the phone app, remote commands on the head unit, the release check, the test tools - with its tests and the phone app's rebuild. Use for any installer change, including a new manager release format or launcher mode it must follow.
---
<!-- SPDX-License-Identifier: MPL-2.0 -->
# Changing the installer

Read `README.md` first; `NBTFoundry/manager: docs/install-flow.md` is the
contract with the launcher.

| Crate | Path | Holds |
| --- | --- | --- |
| `nbt-installer-core` | `core/` | SSH session, Install, Update, test tools, gateway discovery, activation relay, head-unit search, the embedded release (`core/src/bundle.rs`); every socket through `core/src/net.rs` |
| `nbt-manager-installer` | `.` | the egui window and the terminal launcher |
| `nbt-installer-mobile` | `mobile/` | the core for the phone app through UniFFI |

Rules:

- **Remote commands** (`core/src/ssh.rs`): a login shell (`/bin/sh -l -c '…'`);
  only the QNX shell's built-ins plus `mkdir`, `cat`, `chmod`, `rm`, `mount`,
  `sloginfo`, `OnOffDSICommander`; no `printf`, no `rmdir` (the head unit has
  neither; a test keeps them out); never chain on `rm`'s exit status; every
  command ends with an `echo` acknowledgement the installer checks; no user
  input in a command line (data travels on standard input).
- **Install and Update** run the release's own launcher from a private
  `/tmp/nbt-manager-<random>`; remount the plugin partition read-only and
  remove the staging folder after a failure too. The installed launcher is
  never run.
- **Release format** (`core/src/release.rs`) checks what the manager's C
  verifier checks, independently. After a format change in the manager,
  rebuild the fixtures: `NBT_MANAGER_DIR=../manager python3
  core/tests/fixtures/release/regenerate.py`. A new manifest field breaks
  older installers: say so to the user before adding one.
- **Never reboot the head unit on its own**, never log passwords, never
  remove anything outside NBT Manager's own folders. Errors the user can act
  on go through `known_error` in `ssh.rs`.
- **Phone:** the same core runs in the phone app. After a core change, the
  app rebuilds it with `yarn installer-core` (NBTFoundry/mobile-app,
  `scripts/build-installer-core.sh`); a changed UniFFI interface changes its
  Kotlin binding and `modules/car-install`.

Check:

```sh
cargo test --locked --workspace
make -C ../manager build/launcher-host
NBT_TEST_LAUNCHER_HOST="$(realpath ../manager/build/launcher-host)" \
  cargo test --locked -p nbt-installer-core native_install_then_update_over_ssh -- --ignored
```

CI (`.github/workflows/ci.yml`) also tests Windows and macOS. A change to the
window: `tools/capture_window.sh` renews `docs/assets/installer.png`. The
user's in-car steps are the manager's `docs/in-car-procedure.md`; a kit with
the new installer: manager-kit (manager repository).
