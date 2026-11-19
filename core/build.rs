// SPDX-License-Identifier: MPL-2.0
use std::{
    env, fs,
    io::{self, Read},
    path::PathBuf,
};

#[path = "src/release.rs"]
mod release;

fn copy_bounded(var: &str, destination: PathBuf, limit: u64) -> io::Result<()> {
    let Some(source) = env::var_os(var) else {
        return fs::write(destination, []);
    };
    let mut data = Vec::new();
    fs::File::open(source)?
        .take(limit + 1)
        .read_to_end(&mut data)?;
    if data.is_empty() || data.len() as u64 > limit {
        return Err(io::Error::other("invalid embedded release file size"));
    }
    fs::write(destination, data)
}

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=NBT_MANAGER_RELEASE");
    println!("cargo:rerun-if-env-changed=NBT_MANAGER_COMPAT_RELEASE");
    println!("cargo:rerun-if-env-changed=NBT_MANAGER_PUBLIC_KEY");
    let release = env::var_os("NBT_MANAGER_RELEASE");
    let key = env::var_os("NBT_MANAGER_PUBLIC_KEY");
    if release.is_some() != key.is_some() {
        return Err(io::Error::other("set both release and public key"));
    }
    // One release per installer: Update stages it with its own launcher, so no
    // older-format companion is needed (docs/install-flow.md).
    if env::var_os("NBT_MANAGER_COMPAT_RELEASE").is_some() {
        return Err(io::Error::other(
            "NBT_MANAGER_COMPAT_RELEASE is no longer supported: one release per installer",
        ));
    }
    if release.is_none() && env::var("PROFILE").ok().as_deref() == Some("release") {
        return Err(io::Error::other(
            "release builds require a signed manager release and public key",
        ));
    }
    if let Some(ref path) = release {
        println!("cargo:rerun-if-changed={}", PathBuf::from(path).display());
    }
    if let Some(ref path) = key {
        println!("cargo:rerun-if-changed={}", PathBuf::from(path).display());
    }
    let out = PathBuf::from(
        env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("missing output directory"))?,
    );
    copy_bounded(
        "NBT_MANAGER_RELEASE",
        out.join("manager.nbtr"),
        128 * 1024 * 1024 + 4112 + 384,
    )?;
    copy_bounded("NBT_MANAGER_PUBLIC_KEY", out.join("manager.pub"), 16384)?;
    // Optional test application for in-car testing: its first signed package,
    // its public key and its compatibility id. The head unit verifies them.
    for var in [
        "NBT_TEST_APP_PACKAGE",
        "NBT_TEST_APP_KEY",
        "NBT_TEST_APP_COMPAT",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let test_package = env::var_os("NBT_TEST_APP_PACKAGE");
    let test_key = env::var_os("NBT_TEST_APP_KEY");
    let test_compat = env::var("NBT_TEST_APP_COMPAT").ok();
    if test_package.is_some() != test_key.is_some()
        || test_package.is_some() != test_compat.is_some()
    {
        return Err(io::Error::other(
            "set all of the test application's package, key and compatibility",
        ));
    }
    if let Some(compat) = &test_compat
        && (compat.is_empty()
            || compat.len() > 48
            || !compat
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b)))
    {
        return Err(io::Error::other(
            "invalid test application compatibility id",
        ));
    }
    for path in [&test_package, &test_key].into_iter().flatten() {
        println!("cargo:rerun-if-changed={}", PathBuf::from(path).display());
    }
    copy_bounded(
        "NBT_TEST_APP_PACKAGE",
        out.join("testapp.nbt"),
        64 * 1024 * 1024 + 4096,
    )?;
    copy_bounded("NBT_TEST_APP_KEY", out.join("testapp.pub"), 16384)?;
    fs::write(out.join("testapp.compat"), test_compat.unwrap_or_default())?;
    if release.is_some() {
        let bytes = fs::read(out.join("manager.nbtr"))?;
        let key = fs::read(out.join("manager.pub"))?;
        let verified = release::verify(&bytes, &key, None).map_err(io::Error::other)?;
        println!(
            "cargo:warning=Bundled launcher is {} bytes",
            verified.launcher.len()
        );
        println!(
            "cargo:warning=Bundled manager {} sequence {} ({}, NBTRLS{})",
            verified.version, verified.sequence, verified.compatibility, verified.format_version
        );
    }
    Ok(())
}
