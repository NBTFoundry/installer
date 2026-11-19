//! The signed manager release and the test application embedded at build time
//! (`build.rs`), verified again when they are loaded.

use crate::{release, ssh};

/// The embedded manager release, verified.
pub struct Bundle {
    pub release: &'static [u8],
    pub launcher: &'static [u8],
    pub key: &'static [u8],
    pub sha256: String,
    pub compatibility: String,
    pub version: String,
    pub sequence: u64,
}

impl Bundle {
    pub fn install(&self) -> ssh::Command {
        ssh::Command::Install {
            launcher: self.launcher,
            release: self.release,
            key: self.key,
            sha256: self.sha256.clone(),
            compatibility: self.compatibility.clone(),
        }
    }

    pub fn update(&self) -> ssh::Command {
        ssh::Command::Update {
            launcher: self.launcher,
            release: self.release,
            sha256: self.sha256.clone(),
        }
    }
}

static RELEASE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/manager.nbtr"));
static KEY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/manager.pub"));
static TEST_PACKAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/testapp.nbt"));
static TEST_KEY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/testapp.pub"));
static TEST_COMPAT: &str = include_str!(concat!(env!("OUT_DIR"), "/testapp.compat"));

/// The embedded release when there is one and it verifies, and a line for the
/// log saying which.
pub fn load() -> (Option<Bundle>, String) {
    if RELEASE.is_empty() {
        return (
            None,
            "No manager release is bundled in this development build.".into(),
        );
    }
    match release::verify(RELEASE, KEY, None) {
        Ok(verified) => {
            let message = format!(
                "Verified bundled release: {} (sequence {}, compatibility {}).",
                verified.version, verified.sequence, verified.compatibility
            );
            let bundle = Bundle {
                release: RELEASE,
                launcher: &RELEASE[verified.launcher.clone()],
                key: KEY,
                sha256: release::hash_hex(RELEASE),
                compatibility: verified.compatibility,
                version: verified.version,
                sequence: verified.sequence,
            };
            (Some(bundle), message)
        }
        Err(_) => (None, "Bundled manager release verification failed.".into()),
    }
}

/// The test application's first package, when the build embedded one.
pub fn test_app() -> Option<ssh::TestApp> {
    (!TEST_PACKAGE.is_empty()).then(|| ssh::TestApp {
        package: TEST_PACKAGE,
        key: TEST_KEY,
        compatibility: TEST_COMPAT.to_owned(),
        sha256: release::hash_hex(TEST_PACKAGE),
    })
}
