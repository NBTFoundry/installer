use rsa::{
    RsaPublicKey,
    pkcs1v15::{Signature, VerifyingKey},
    pkcs8::DecodePublicKey,
    signature::Verifier,
    traits::PublicKeyParts,
};
use sha2::{Digest, Sha256};

const ROLES_V1: [&str; 8] = [
    "launcher",
    "gui",
    "worker",
    "plugin_lib",
    "plugin_manifest",
    "plugin_xml",
    "plugin_text",
    "plugin_images",
];
const ROLES_V2: [&str; 9] = [
    "launcher",
    "gui",
    "worker",
    "plugin_lib",
    "plugin_manifest",
    "plugin_xml",
    "plugin_text",
    "plugin_images",
    "capture_lib",
];
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
    let (format_version, roles): (u8, &[&str]) = match &bytes[..8] {
        b"NBTRLS1\n" => (1, &ROLES_V1),
        b"NBTRLS2\n" => (2, &ROLES_V2),
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
    let mut pos = payload_start;
    let mut total = 0u64;
    let mut launcher = 0..0;
    let mut capture_lib = None;
    for (index, role) in roles.iter().enumerate() {
        let is_elf = index < 4 || index == 8;
        let must_be_so = index == 3 || index == 8;
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
        } else if index == 8 {
            capture_lib = Some(pos..end);
        }
        let payload = &bytes[pos..end];
        if hash_hex(payload) != hash || (is_elf && !arm_elf(payload, must_be_so)) {
            return Err("invalid component");
        }
        pos = end;
    }
    if lines.next().is_some() || pos != bytes.len() {
        return Err("unexpected release data");
    }
    Ok(Release {
        version: version.to_owned(),
        sequence,
        compatibility: compat.to_owned(),
        format_version,
        component_count: roles.len(),
        launcher,
        capture_lib,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        process::{Command, Stdio},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn run(command: &mut Command) {
        let result = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
    }

    #[test]
    fn verifies_real_packer_output_and_rejects_tampering() {
        let dir = std::env::temp_dir().join(format!(
            "nbt-release-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let key = dir.join("key.pem");
        let public = dir.join("key.pub");
        run(Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:3072",
                "-out",
            ])
            .arg(&key));
        run(Command::new("openssl")
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
            fs::write(dir.join(name), elf).unwrap();
        }
        let plugin = dir.join("plugin");
        fs::create_dir(&plugin).unwrap();
        elf[16] = 3;
        fs::write(plugin.join("libNbtManager.so"), elf).unwrap();
        for name in [
            "NbtManager.manifest",
            "NbtManager.xml",
            "NbtManager_ALL_Texts.zip",
            "NbtManager_ALL_Images.zip",
        ] {
            fs::write(plugin.join(name), name).unwrap();
        }
        let package = dir.join("release.nbtr");
        let tool = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/package_release.py");
        run(Command::new("python3")
            .arg(&tool)
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
        let bytes = fs::read(package).unwrap();
        let public = fs::read(public).unwrap();
        let release = verify(&bytes, &public, Some("demo-1")).unwrap();
        assert_eq!(
            (
                release.version.as_str(),
                release.sequence,
                release.compatibility.as_str()
            ),
            ("v1", 1, "demo-1")
        );
        assert_eq!(
            &bytes[release.launcher.clone()],
            fs::read(dir.join("launcher")).unwrap()
        );
        assert!(verify(&bytes, &public, Some("other")).is_err());
        for pos in [0, 16, bytes.len() - 1] {
            let mut tampered = bytes.clone();
            tampered[pos] ^= 1;
            assert!(verify(&tampered, &public, None).is_err());
        }
        assert!(verify(&bytes[..bytes.len() - 1], &public, None).is_err());
        let mut extra = bytes.clone();
        extra.push(1);
        assert!(verify(&extra, &public, None).is_err());
        let manifest_size = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let manifest_end = 16 + manifest_size;
        let manifest = std::str::from_utf8(&bytes[16..manifest_end]).unwrap();
        for malformed in [
            manifest.replace("sequence=1\n", "sequence=01\n"),
            manifest.replace(
                "bridge=1.4|show,status,live-settings,most,gamepad,controller\n",
                "bridge=1.00|\n",
            ),
            manifest.replace(
                "bridge=1.4|show,status,live-settings,most,gamepad,controller\n",
                "bridge=1.0|show,show\n",
            ),
            manifest.replace(
                "bridge=1.4|show,status,live-settings,most,gamepad,controller\n",
                "bridge=1.0|show,\n",
            ),
            manifest.replace(
                "bridge=1.4|show,status,live-settings,most,gamepad,controller\n",
                "bridge=2.0|\nbridge=1.0|\n",
            ),
            manifest.replace(
                "bridge=1.4|show,status,live-settings,most,gamepad,controller\n",
                "bridge=1.0|\nbridge=2.0|\nbridge=3.0|\n",
            ),
            manifest.replace("compat=demo-1\n", "compat=../etc\n"),
            manifest.replace("version=v1\n", "version=v1\nversion=v1\n"),
        ] {
            use std::io::Write;
            let mut signed = b"NBTRLS1\n".to_vec();
            signed.extend_from_slice(&(malformed.len() as u32).to_be_bytes());
            signed.extend_from_slice(&(SIGNATURE_SIZE as u32).to_be_bytes());
            signed.extend_from_slice(malformed.as_bytes());
            let mut child = Command::new("openssl")
                .args(["dgst", "-sha256", "-sign"])
                .arg(&key)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&signed).unwrap();
            let signature = child.wait_with_output().unwrap();
            assert!(signature.status.success());
            signed.extend_from_slice(&signature.stdout);
            signed.extend_from_slice(&bytes[manifest_end + SIGNATURE_SIZE..]);
            assert!(verify(&signed, &public, None).is_err());
        }
        // NBTRLS2 with capture_lib (component 9, ARM ET_DYN)
        let mut cap_elf = [0u8; 128];
        cap_elf[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        cap_elf[16] = 3;
        cap_elf[18] = 40;
        fs::write(dir.join("libnbt-capture.so"), cap_elf).unwrap();
        let package_v2 = dir.join("release-v2.nbtr");
        run(Command::new("python3")
            .arg(&tool)
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
            .arg(&package_v2)
            .arg("--launcher")
            .arg(dir.join("launcher"))
            .arg("--gui")
            .arg(dir.join("gui"))
            .arg("--worker")
            .arg(dir.join("worker"))
            .arg("--plugin-dir")
            .arg(&plugin)
            .arg("--capture-lib")
            .arg(dir.join("libnbt-capture.so")));
        let bytes_v2 = fs::read(package_v2).unwrap();
        let rel_v2 = verify(&bytes_v2, &public, Some("demo-1")).unwrap();
        assert_eq!((rel_v2.format_version, rel_v2.component_count), (2, 9));
        assert_eq!(&bytes_v2[rel_v2.capture_lib.unwrap()], cap_elf.as_slice());
        assert!(supports_capability_format(
            b"formats=NBTRLS1,NBTRLS2\n",
            "NBTRLS2"
        ));
        assert!(!supports_capability_format(b"formats=NBTRLS1\n", "NBTRLS2"));
        fs::remove_dir_all(dir).unwrap();
    }
}
