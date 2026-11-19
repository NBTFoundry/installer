//! The installer's working parts, shared by the Windows installer (`desktop/`)
//! and the phone app (`desktop/mobile/`, through UniFFI): the SSH session with
//! its install, update and test-tool steps, the HSFZ gateway search, the SSH
//! activation relay, the head-unit search, and the signed manager release
//! embedded at build time.
//!
//! Every socket to the car or to the activation service is opened through
//! [`net`], which a phone ties to the right network.

pub mod activation;
pub mod bench;
pub mod bundle;
pub mod collect;
pub mod discovery;
pub mod net;
pub mod relay_codec;
pub mod release;
pub mod ssh;
pub mod sweep;

/// The head unit's connection details, prefilled so the only step is Connect.
///
/// These are the published NBT-EVO values from AAD's install and update guides,
/// not personal credentials: every NBT-EVO uses the same root password, and the
/// address is the head unit BMW's ENET addressing gives it (`169.254.199.99` is
/// a different module and was the wrong default).
pub const DEFAULT_HOST: &str = "169.254.199.119";
pub const DEFAULT_PORT: u16 = 22;
pub const DEFAULT_USER: &str = "root";
pub const DEFAULT_PASSWORD: &str = "ts&SK412";
