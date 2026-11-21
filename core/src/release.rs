// SPDX-License-Identifier: MPL-2.0
use rsa::{
    RsaPublicKey,
    pkcs1v15::{Signature, VerifyingKey},
    pkcs8::DecodePublicKey,
    signature::Verifier,
    traits::PublicKeyParts,
};
use sha2::{Digest, Sha256};

/* The fixed roles of every format; NBTRLS2 adds capture_lib, NBTRLS3 up to
 * eight named components (NBT Manager's docs/manager-release.md). */
const ROLES: [&str; 8] = [
    "launcher",
    "gui",
    "worker",
    "plugin_lib",
    "plugin_manifest",
    "plugin_xml",
    "plugin_text",
    "plugin_images",
];
const CAPTURE: &str = "capture_lib";
const RUNNER: &str = "runner";
const MAX_NAMED: usize = 8;
const SIGNATURE_SIZE: usize = 384;
const MAX_MANIFEST: usize = 4096;
const MAX_COMPONENT: u64 = 64 * 1024 * 1024;
const MAX_PAYLOAD: u64 = 128 * 1024 * 1024;

pub struct Release {
    pub version: String,
    pub sequence: u64,
    pub compatibility: String,
    #[allow(dead_code)]
    pub format_version: u8,
    #[allow(dead_code)]
    pub component_count: usize,
    pub launcher: std::ops::Range<usize>,
    #[allow(dead_code)]
    pub capture_lib: Option<std::ops::Range<usize>>,
}

#[allow(dead_code)]
pub fn supports_capability_format(stdout: &[u8], format: &str) -> bool {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return false;
    };
    text.lines().any(|line| {
        line.strip_prefix("formats=")
            .is_some_and(|list| list.split(',').any(|token| token == format))
    })
}

fn field<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    name: &str,
) -> Result<&'a str, &'static str> {
    let line = lines.next().ok_or("missing field")?;
    line.strip_prefix(name)
        .and_then(|s| s.strip_prefix('='))
        .ok_or("invalid field")
}

fn token(text: &str, upper: bool) -> bool {
    !text.is_empty()
        && text.len() <= 48
        && text.bytes().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, b'-' | b'_' | b'.')
                || (upper && c.is_ascii_uppercase())
        })
}

fn number(text: &str) -> Result<u64, &'static str> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|c| c.is_ascii_digit()) {
        return Err("invalid integer");
    }
    text.parse::<u64>().map_err(|_| "invalid integer")
}

fn bridge_line(text: &str, previous: &mut u16) -> Result<(), &'static str> {
    let (version, caps) = text.split_once('|').ok_or("invalid bridge")?;
    let (major, minor) = version.split_once('.').ok_or("invalid bridge")?;
    fn part(text: &str) -> Result<u16, &'static str> {
        if text.is_empty()
            || (text.len() > 1 && text.starts_with('0'))
            || !text.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("invalid bridge version");
        }
        text.parse().map_err(|_| "invalid bridge version")
    }
    let major = part(major)?;
    part(minor)?;
    if major <= *previous || caps.len() > 255 {
        return Err("invalid bridge");
    }
    let mut names = Vec::new();
    if !caps.is_empty() {
        for name in caps.split(',') {
            if name.is_empty()
                || name.len() > 24
                || !name.as_bytes()[0].is_ascii_lowercase()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || names.contains(&name)
            {
                return Err("invalid bridge capabilities");
            }
            names.push(name);
        }
    }
    *previous = major;
    Ok(())
}

fn valid_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub fn hash_hex(data: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(data) {
        write!(out, "{b:02x}").unwrap();
    }
    out
}

/* A named component: [a-z][a-z0-9_]{0,23}. */
fn component_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 24
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/* The name the next manifest line gives as `<name>_size=`. */
fn next_name(line: &str) -> Option<&str> {
    line.split_once('=')
        .and_then(|(key, _)| key.strip_suffix("_size"))
        .filter(|name| component_name(name))
}

fn arm_elf(data: &[u8], plugin: bool) -> bool {
    data.len() >= 52
        && &data[..7] == b"\x7fELF\x01\x01\x01"
        && data[18..20] == [40, 0]
        && if plugin {
            data[16..18] == [3, 0]
        } else {
            matches!(&data[16..18], [2, 0] | [3, 0])
        }
}

pub fn verify(
    bytes: &[u8],
    public_key: &[u8],
    compatibility: Option<&str>,
) -> Result<Release, &'static str> {
    if bytes.len() < 16 {
        return Err("invalid release header");
    }
    let format_version: u8 = match &bytes[..8] {
        b"NBTRLS1\n" => 1,
        b"NBTRLS2\n" => 2,
        b"NBTRLS3\n" => 3,
        _ => return Err("invalid release header"),
    };
    let len = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let sig_len = u32::from_be_bytes(bytes[12..16].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_MANIFEST || sig_len != SIGNATURE_SIZE {
        return Err("invalid release lengths");
    }
    let manifest_end = 16usize.checked_add(len).ok_or("invalid release lengths")?;
    let payload_start = manifest_end
        .checked_add(sig_len)
        .ok_or("invalid release lengths")?;
    if payload_start > bytes.len() || public_key.len() > 16384 {
        return Err("invalid release lengths");
    }
    let key_text = std::str::from_utf8(public_key).map_err(|_| "invalid manager key")?;
    let key = RsaPublicKey::from_public_key_pem(key_text).map_err(|_| "invalid manager key")?;
    if key.n().bits() != 3072 {
        return Err("invalid manager key");
    }
    let signature = Signature::try_from(&bytes[manifest_end..payload_start])
        .map_err(|_| "invalid signature")?;
    VerifyingKey::<Sha256>::new(key)
        .verify(&bytes[..manifest_end], &signature)
        .map_err(|_| "invalid signature")?;
    let manifest = std::str::from_utf8(&bytes[16..manifest_end]).map_err(|_| "invalid manifest")?;
    if !manifest.ends_with('\n') || !manifest.is_ascii() || manifest.contains('\0') {
        return Err("invalid manifest");
    }
    let mut lines = manifest.split_terminator('\n').peekable();
    let version = field(&mut lines, "version")?;
    if !token(version, true) {
        return Err("invalid version");
    }
    let sequence = number(field(&mut lines, "sequence")?)?;
    if field(&mut lines, "platform")? != "qnx6.5-armv7le" {
        return Err("incompatible platform");
    }
    let compat = field(&mut lines, "compat")?;
    if !token(compat, false) || compatibility.is_some_and(|expected| expected != compat) {
        return Err("incompatible release");
    }
    let mut previous = 0u16;
    let mut bridge_count = 0;
    while lines.peek().is_some_and(|line| line.starts_with("bridge=")) {
        if bridge_count == 2 {
            return Err("too many bridge majors");
        }
        bridge_line(field(&mut lines, "bridge")?, &mut previous)?;
        bridge_count += 1;
    }
    let mut names: Vec<&str> = ROLES.to_vec();
    if format_version == 2 {
        names.push(CAPTURE);
    }
    let mut pos = payload_start;
    let mut total = 0u64;
    let mut launcher = 0..0;
    let mut capture_lib = None;
    let mut index = 0;
    loop {
        if index == names.len() {
            // NBTRLS3: named components after the fixed roles, until the end.
            let Some(&line) = lines.peek().filter(|_| format_version == 3) else {
                break;
            };
            let name = next_name(line).ok_or("invalid component")?;
            if names.contains(&name) || names.len() == ROLES.len() + MAX_NAMED {
                return Err("invalid component");
            }
            names.push(name);
        }
        let role = names[index];
        // The fixed programs and the named ones this installer knows are ARM
        // ELF; any other named component is only hashed.
        let is_elf = index < 4 || role == CAPTURE || role == RUNNER;
        let must_be_so = index == 3 || role == CAPTURE;
        let size = number(field(&mut lines, &format!("{role}_size"))?)?;
        let hash = field(&mut lines, &format!("{role}_sha256"))?;
        if !valid_hash(hash) || size > MAX_COMPONENT || (is_elf && size < 52) {
            return Err("invalid component");
        }
        total = total
            .checked_add(size)
            .filter(|v| *v <= MAX_PAYLOAD)
            .ok_or("release too large")?;
        let end = pos
            .checked_add(size as usize)
            .filter(|v| *v <= bytes.len())
            .ok_or("truncated release")?;
        if index == 0 {
            launcher = pos..end;
        } else if role == CAPTURE {
            capture_lib = Some(pos..end);
        }
        let payload = &bytes[pos..end];
        if hash_hex(payload) != hash || (is_elf && !arm_elf(payload, must_be_so)) {
            return Err("invalid component");
        }
        pos = end;
        index += 1;
    }
    if lines.next().is_some() || pos != bytes.len() {
        return Err("unexpected release data");
    }
    Ok(Release {
        version: version.to_owned(),
        sequence,
        compatibility: compat.to_owned(),
        format_version,
        component_count: names.len(),
        launcher,
        capture_lib,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Packages made by NBT Manager's own packer (tools/package_release.py), so
     * the verifier is checked against real packer output; see
     * tests/fixtures/release/regenerate.py. */
    const PUBLIC: &[u8] = include_bytes!("../tests/fixtures/release/release.pub");
    const RELEASE_V1: &[u8] = include_bytes!("../tests/fixtures/release/release-v1.nbtr");
    const RELEASE_V2: &[u8] = include_bytes!("../tests/fixtures/release/release-v2.nbtr");
    /* NBTRLS3: capture_lib, runner and a component this installer does not know. */
    const RELEASE_V3: &[u8] = include_bytes!("../tests/fixtures/release/release-v3.nbtr");
    /* v1 with one manifest field broken and a valid signature: a sequence
     * with a leading zero, malformed or out-of-order bridge lines, a path in
     * the compatibility, a repeated field. */
    const MALFORMED: [&[u8]; 8] = [
        include_bytes!("../tests/fixtures/release/malformed-1.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-2.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-3.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-4.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-5.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-6.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-7.nbtr"),
        include_bytes!("../tests/fixtures/release/malformed-8.nbtr"),
    ];

    /* The 128-byte ARM ELF stand-ins the fixtures were packed from. */
    fn elf(object_type: u8) -> [u8; 128] {
        let mut elf = [0u8; 128];
        elf[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        elf[16] = object_type;
        elf[18] = 40;
        elf
    }

    #[test]
    fn verifies_real_packer_output_and_rejects_tampering() {
        let bytes = RELEASE_V1.to_vec();
        let release = verify(&bytes, PUBLIC, Some("demo-1")).unwrap();
        assert_eq!(
            (
                release.version.as_str(),
                release.sequence,
                release.compatibility.as_str()
            ),
            ("v1", 1, "demo-1")
        );
        assert_eq!(&bytes[release.launcher.clone()], elf(2).as_slice());
        assert!(verify(&bytes, PUBLIC, Some("other")).is_err());
        for pos in [0, 16, bytes.len() - 1] {
            let mut tampered = bytes.clone();
            tampered[pos] ^= 1;
            assert!(verify(&tampered, PUBLIC, None).is_err());
        }
        assert!(verify(&bytes[..bytes.len() - 1], PUBLIC, None).is_err());
        let mut extra = bytes.clone();
        extra.push(1);
        assert!(verify(&extra, PUBLIC, None).is_err());
        for (index, malformed) in MALFORMED.iter().enumerate() {
            assert!(
                verify(malformed, PUBLIC, None).is_err(),
                "malformed-{}.nbtr was accepted",
                index + 1
            );
        }

        // NBTRLS2 with capture_lib (component 9, ARM ET_DYN)
        let rel_v2 = verify(RELEASE_V2, PUBLIC, Some("demo-1")).unwrap();
        assert_eq!((rel_v2.format_version, rel_v2.component_count), (2, 9));
        assert_eq!(&RELEASE_V2[rel_v2.capture_lib.unwrap()], elf(3).as_slice());
        assert!(supports_capability_format(
            b"formats=NBTRLS1,NBTRLS2\n",
            "NBTRLS2"
        ));
        assert!(!supports_capability_format(b"formats=NBTRLS1\n", "NBTRLS2"));

        // NBTRLS3: named components, an unknown one only hashed
        let rel_v3 = verify(RELEASE_V3, PUBLIC, Some("demo-1")).unwrap();
        assert_eq!((rel_v3.format_version, rel_v3.component_count), (3, 11));
        assert_eq!(&RELEASE_V3[rel_v3.capture_lib.unwrap()], elf(3).as_slice());
        let mut tampered = RELEASE_V3.to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(verify(&tampered, PUBLIC, None).is_err());
        // The NBTRLS3 manifest under another header: refused.
        let mut relabelled = RELEASE_V3.to_vec();
        relabelled[6] = b'2';
        assert!(verify(&relabelled, PUBLIC, None).is_err());
        assert!(next_name("runner_size=52").is_some());
        assert!(next_name("Runner_size=52").is_none());
        assert!(next_name("runner_sha256=00").is_none());
        assert!(next_name(&format!("{}_size=1", "a".repeat(25))).is_none());
    }
}
