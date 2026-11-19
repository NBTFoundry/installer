# Desktop installer

This experimental Rust/egui application connects to the head unit over SSH.
**Install** verifies and streams the bundled release, provisions the manager,
and registers its iDrive plugin. **Update** stages a signed manager release on
an existing installation, with the release's own launcher, whatever launcher
is installed. Updates are confirmed after the manager GUI and worker
remain healthy for 30 seconds. Both paths require bench validation before use.


```sh
cargo run --locked
cargo test --locked --workspace
```

The crate is a workspace of three:

- `core/` (`nbt-installer-core`): everything that is not the window. The SSH
  session with Install, Update and the test tools, the gateway search, the
  activation relay, the head-unit search, and the signed release and test
  application embedded at build time (`core/build.rs`, `core/src/bundle.rs`).
  Every socket is opened through `core/src/net.rs`, which a phone ties to the
  right network (below).
- `.` (`nbt-manager-installer`): the egui window and the terminal launcher.
- `mobile/` (`nbt-installer-mobile`, library `nbt_installer`): the core for
  the phone app through UniFFI. A `Session` connects with a `Listener` for
  events; `confirm_activation`, `install`, `update`, `tool` and `disconnect`
  act on it.

For the end-to-end host test, build the native test launcher from the repository
root, then run the ignored integration test from `desktop/`:

```sh
make build/launcher-host
cd desktop
NBT_TEST_LAUNCHER_HOST="$PWD/../build/launcher-host" cargo test --locked -p nbt-installer-core native_install_then_update_over_ssh -- --ignored
```

The test uses a local mock SSH server and a temporary test signing key: a fresh
install into an empty manager root, then an update to a second release staged
and applied by the uploaded launcher.

The repository's `make release` builds all QNX components and the plugin, signs
and verifies the release, then builds this application with `cargo build --release
--locked`. See the root README for signing variables and output paths.

Standalone Cargo release builds require both `NBT_MANAGER_RELEASE` and `NBT_MANAGER_PUBLIC_KEY`
to point to the signed manager release and its trusted public key. Both are
embedded in the binary; the private key must remain on the signing workstation.
The build rejects invalid releases and the application verifies the embedded
release again when it starts:

```sh
NBT_MANAGER_RELEASE=/path/to/manager.nbtr \
  NBT_MANAGER_PUBLIC_KEY=/path/to/manager-public.pem cargo build --release --locked
```

Development builds may omit both files and report that no release is bundled.
Select Install or Update after connecting. Install takes five steps, the same
every time, whatever happened before:

1. Make the iDrive plugin folder writable: `mount -uw /net/hu-omap/fs/sda0`
   (the partition is mounted read-only at boot).
2. Upload the launcher to a private `/tmp/nbt-manager-<random>` folder.
3. Run it: it verifies the signed release, checks the plugin folder and the
   manager storage, and installs. A refusal names the failed check (for
   example `HMI layout: plugin filesystem is read-only`).
4. Remove the staging folder, also after a failure.
5. Make the plugin folder read-only again, also after a failure:
   `mount -ur /net/hu-omap/fs/sda0`. If that fails, the installer says so;
   the partition is read-only after the next reboot either way.

Update takes six: the same remount, upload and cleanup around two runs of the
uploaded launcher, `--stage-update <bytes> <sha256>` (the release on stdin,
expects `PENDING`) and `--refresh-setup` (replaces the installed launcher and
the iDrive plugin, `REFRESHED` or `REFRESHED plugin`). The installed launcher
is never run, so a launcher older than the release's format (`NBTRLS2`) needs
nothing first, and the installer carries one release. On a head unit without
NBT Manager it says to use Install.

Removal remounts writable first and read-only last, the same way. A failure leaves the connection usable,
so Install can be retried at once. After a successful installation, restart
iDrive (Test tools) or reboot the head unit to load the plugin. The
application never reboots it.

**Test tools** (after connecting) support in-car testing:

- Collect everything: one read-only report over SSH, saved as
  `collect-<time>.txt` in the logs folder below (the phone app offers it to
  share). Its sections, each with its command, output, standard error and
  exit status: `uname -a` and `mount`, the debug log (both files), `sloginfo`,
  `audioconn -l` (stopped after 10 s, as in the car inventory), `pidin arg`, the process manager's overview,
  the capture shim's state and files, `ls -lR` of the manager's folders and
  `/dev/shmem`. A section that fails or times out is noted and the others
  still run (`core/src/collect.rs`). Read it before sharing: the system log
  can hold phone names and Bluetooth or Wi-Fi addresses, and `pidin arg` the
  head unit's MAC address.
- Debug log: switch it on (creates `manager/debug.log`), save it to
  `Documents\NBT Manager logs` (or the app's data folder), or switch it off
  and delete it.
- System log: runs `sloginfo` on the iDrive processor over SSH and saves its
  output as `slog-<time>.log` in the same folder. iDrive and other plugins
  (NBT-AAD's `[NbtAadApp]` lines) write there. It works with or without NBT
  Manager installed.
- Install test application v1: provisions the application `test` from a
  signed package embedded at build time (`NBT_TEST_APP_PACKAGE`,
  `NBT_TEST_APP_KEY` and `NBT_TEST_APP_COMPAT`, all three or none) through
  the launcher's `--provision-app`. The head unit verifies it and refuses to
  replace an existing application.
- Controller settings: edit and write `manager/controller.conf` (prefilled
  with the documented defaults), or remove it.
- Restart the head unit: runs `OnOffDSICommander appreset`, which restarts
  the whole head unit (not iDrive alone), as switching the car off and on. It
  takes SSH down and switches it off, so the installer disconnects (after at
  most 20 s without an answer). Once the head unit is back, Connect offers to
  enable SSH again. A refusal is reported instead. The Test tools button and
  the end of an install or an update that replaced the iDrive plugin
  (`Event::RestartNeeded`) both open the modal **Restart the head unit?**
  (Later or Restart the head unit); the phone app asks the same.
- Remove NBT Manager: after a confirmation, deletes `/var/opt/nbt-manager`
  and the plugin folders `Plugins/NbtManager` and `Plugins/.NbtManager.setup`
  under `/opt/hmi/ID5/data/ro`, nothing else. Running copies keep their open
  files until the reboot that unloads the plugin.

Each remote command runs as if typed in an SSH session: in a login shell
(`/bin/sh -l -c '…'`), which reads the profile and so has the session's `PATH`
and `LD_LIBRARY_PATH`. Remote commands use only the QNX shell's built-ins
(`echo`, `test`, `umask`, `:`) and `mkdir`, `cat`, `chmod`, `rm`, `mount`,
`sloginfo` and `OnOffDSICommander`, which the head unit is known to have, and
contain no user input; the controller settings travel on standard input. The
head unit has no `printf` (the first in-car install
failed on it) and `rmdir` is not known to exist there; a test keeps both out.

The address (`169.254.199.119`), port, user and root password are built in
(`desktop/core/src/lib.rs`): they are the NBT-EVO's published defaults from AAD's
guides, not personal credentials, and the password field is plain text so it
can be read and corrected. `169.254.199.99`, the previous default, is another
module. Passwords are not written to the log. Every SSH host key is accepted
without a prompt, by the owner's decision: the installer only talks to a head
unit on a direct ENET cable, and a reflash changes the key. The key's SHA-256
fingerprint is logged.

**Open SSH terminal** (next to Connect, no connection needed) starts `ssh`
from the PATH in a new terminal window with the connection settings:
`-o HostKeyAlgorithms=+ssh-rsa -m hmac-sha1` for the old sshd, host keys not
recorded, `-p <port> <user>@<address>`. OpenSSH takes no password argument, so
the installer is ssh's askpass program (`SSH_ASKPASS_REQUIRE=force`, OpenSSH
8.4 and later) and answers with the password; an older ssh asks for it. The
fields are checked first (letters, digits, `.`, `-`, `_`; port 1-65535),
since Windows passes the command through `cmd /C start`.

The connection fields are under a collapsed **Connection settings** header.
Two bars over the log show the operation's steps (connecting: open, log in,
check, plus gateway search and Enable SSH when needed; install: the five
steps above) and the
current step's bytes, speed and time left. Upload progress counts bytes
written to the SSH socket, not bytes queued, since the SSH window lets
megabytes queue ahead of a slow link.

The **Debug log** checkbox (off by default) shows diagnostic detail: each
remote command the installer runs (fixed commands with generated paths, sizes,
digests and the compatibility token), and for a failed step the exit status,
signal, standard output and standard error (up to 4 KiB, shown escaped), plus
the host key and handshake errors. Detail is always recorded, so ticking it
after a failure shows what led there. While ticked, the log is also written,
with times since start, to `Documents\NBT Manager logs\installer-<time>.log`
(else the application data folder). Output is read while input is still being
sent, so a command that exits early reports its error instead of a stalled
upload. **Copy log** and selecting log text use the system clipboard.

The head unit runs an old sshd. AAD's documented command needs
`ssh -o HostKeyAlgorithms=+ssh-rsa -m hmac-sha1`, so it predates rsa-sha2 and
hmac-sha2 and offers no curve25519/ECDH key exchange; group exchange is the only
key exchange it shares with a modern client. An sshd of that age without large
moduli answers it with the 2048-bit group 14, which russh's default minimum of
3072 bits refuses; that matches the car's "SSH connection or host verification
failed" (inferred, not yet observed on the car). The client therefore accepts
groups from 2048 bits, as OpenSSH does, and appends HMAC-SHA1,
the SHA-1 key exchanges and AES-CBC after the modern algorithms. `ssh-rsa` host
keys are accepted by russh's defaults. A test server configured like that sshd
covers this. After authentication the client runs `uname -s` to identify QNX.
Its output is bounded; a failed check logs it.

When the SSH connection cannot be opened (refused, unreachable or timed out),
the installer asks whether to enable SSH. Enabling works like the BMW SSH
client reconstructed in `../nbt_ssh`, which enables SSH on the car: it finds
the vehicle gateway by HSFZ discovery
(UDP 6811 broadcast), then relays between the gateway's port 6801 and the
third-party activation service (`wss://pretzel.bavarian-aa.uk:8443`), showing
the service's status messages. It sends diagnostic commands to the vehicle. It
does not capture traffic or log vehicle messages, then connects again. The
gateway is a different address from the head unit's SSH address. Failures
after the connection opens (handshake, authentication) never offer it.

If activation ends with a protocol or transport error, the installer keeps
that error in the log and checks whether SSH nevertheless became reachable.
An accepting endpoint continues through the usual SSH handshake and password
authentication in the same Connect attempt. An explicit negative activation
result stops the attempt. Decoder errors report their reason, tag and byte
count without recording message bodies.

The search (`core/src/discovery.rs`) sends the request from every local IPv4
address the computer has (`if-addrs`, with its `link-local` feature: without
it the crate leaves out `169.254` addresses on Windows), each to its own
subnet's broadcast and to `255.255.255.255`, plus once from an unbound socket
to `169.254.255.255` and `255.255.255.255` as before. Windows sends a limited
broadcast out of one interface only, and a car that took an address from a
Wi-Fi ENET adapter's DHCP server keeps it after the adapter is unplugged and
answers only on that subnet. Answers are merged by VIN, so one car answering
on two subnets is one gateway. A receive error ends only that address's part
of the search (Windows reports an earlier ICMP port unreachable as a reset,
which is skipped). When nothing answers, the log names the addresses asked
from and the adapter case. **Connection settings** → Gateway (empty by
default) skips the search and uses the typed address; it accepts one unicast
IPv4 address.

Behind a Wi-Fi ENET adapter the computer takes a DHCP address from the adapter
and loses its own `169.254` address, so the built-in head-unit address becomes
unreachable and the connection fails at once. The head unit is still there: the
car's modules are DHCP clients, so the adapter gives it an address on that
network too. When the configured address does not answer, `core/src/sweep.rs`
searches for the SSH port on the networks a vehicle gateway answered on,
leaving out this computer's own addresses and the gateways themselves (the
gateway is a different module and refuses SSH; the car's password must not be
offered to it). Whatever answers still has to pass `uname -s`. Only those
networks are searched, never an ordinary home or office one, and the
link-local `169.254.0.0/16` is too large to search, so the built-in address
covers the cable. The search runs before the Enable SSH offer, since SSH is
often already on at the other address, and again after enabling it.

When this device has no `169.254` address (a phone, or a computer behind the
adapter), the built-in address is not tried at all, before or after enabling
SSH: it cannot answer. After enabling SSH the search then repeats every 2
seconds for up to 60 seconds (`sweep::wait`, one round takes about 2 seconds
for a /24), so a head unit that starts accepting late is still found. Over the
cable nothing changes: ten tries at the built-in address, then one search.

When nothing is found and the configured address is link-local while this
computer has none (`discovery::needs_link_local`), the log and the Enable SSH
window say so and give the addresses to set.

Confirmed on the car on 2026-11-15, with a capture on the ENET link: the
adapter bridges; the gateway answered at `192.168.16.105`; every module asks
for a DHCP address with a host name that names it
(`DIAGADR10BMWMACD8182B92E799` for the gateway,
`DIAGADR62BMWMAC001CD7A4F1CA` for the head unit, whose MAC a second capture
confirms by ARP as the holder of `169.254.199.119`), falling back to
link-local only because the bare cable serves no DHCP. `DIAGADR63` is the
module at `169.254.199.99`, the address this file warns is the wrong default.
The gateway also broadcasts its own identification to port `7811` unprompted;
the head unit does not, so listening there finds the gateway but cannot
replace this search.

Offline analysis of the supplied NBTEvo Android Auto Installer 1.0.3 confirms
that its file uploader also sends binary stdin to remote `cat`, using bundled
Plink. See [the transfer findings](../docs/installer-transfer-analysis.md) for
artifact hashes, code anchors and limits. Our installer uses its existing Rust
SSH transport; this comparison does not replace head-unit validation.

## On a phone

The phone app (`~/src/mobile/nbt-manager`, module `car-install`) runs the same
core over a Wi-Fi ENET adapter. Build the library with the Android NDK and
`cargo-ndk` (`rustup target add aarch64-linux-android`); release builds need
the signed release, as above:

```sh
cargo ndk -t arm64-v8a -P 29 build -p nbt-installer-mobile --release --locked
cargo build -p nbt-installer-mobile   # host build: the binding is read from it
cargo run -p nbt-installer-mobile --features bindgen --bin uniffi-bindgen -- \
  generate --library target/debug/libnbt_installer.so --language kotlin --out-dir <dir>
```

The release library is stripped, which removes the metadata the binding is
generated from; a host debug build carries the same interface.

A phone cannot rely on routes. The adapter's Wi-Fi has no internet, so Android
either keeps mobile data as its default network, and sends car traffic there,
or makes the adapter's Wi-Fi the default and loses the internet. The app
therefore passes `Networks`: the handle of the adapter's Wi-Fi
(`Network.getNetworkHandle()`) and its addresses, and the handle of mobile
data. `net.rs` ties every car socket to the first with
`android_setsocknetwork`, and the activation service's socket to the second,
resolving its name there with `android_getaddrinfofornetwork`. The gateway
search and the head-unit search then use only the adapter network's addresses.
A computer sets nothing and keeps the system's routes.
