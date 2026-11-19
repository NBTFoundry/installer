#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Rebuilds the release fixtures of core/src/release.rs with NBT Manager's own
packer, so the installer's verifier is tested against real packer output.

    NBT_MANAGER_DIR=../manager python3 core/tests/fixtures/release/regenerate.py

Writes, next to this script:
  release-v1.nbtr       NBTRLS1: launcher, gui, worker and the iDrive plugin
  release-v2.nbtr       NBTRLS2: the same plus the capture library
  malformed-N.nbtr      v1 with one manifest field broken, validly signed
  release.pub           the public half of a throwaway key (the private half
                        is deleted; nothing secret is kept)

Run it after a change to the release format; the test then checks the new files.
Needs python3 and openssl.
"""

import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
INSTALLER = HERE.parents[3]
MANAGER = Path(os.environ.get("NBT_MANAGER_DIR", INSTALLER.parent / "manager")).resolve()
PACKER = MANAGER / "tools" / "package_release.py"
SIGNATURE_SIZE = 384
BRIDGE_LINE = "bridge=1.4|show,status,live-settings,most,gamepad,controller\n"


def elf(object_type):
    """The 128-byte ARM ELF stand-in the test rebuilds (ET_EXEC 2, ET_DYN 3)."""
    data = bytearray(128)
    data[:7] = b"\x7fELF\x01\x01\x01"
    data[16] = object_type
    data[18] = 40
    return bytes(data)


def pack(work, key, output, version, sequence, capture=False):
    command = [sys.executable, str(PACKER), "--version", version, "--sequence", str(sequence),
               "--compat", "demo-1", "--key", str(key), "--output", str(output),
               "--launcher", str(work / "launcher"), "--gui", str(work / "gui"),
               "--worker", str(work / "worker"), "--plugin-dir", str(work / "plugin")]
    if capture:
        command += ["--capture-lib", str(work / "libnbt-capture.so")]
    subprocess.run(command, check=True)


def sign(key, data):
    return subprocess.run(["openssl", "dgst", "-sha256", "-sign", str(key)], input=data,
                          check=True, capture_output=True).stdout


def main():
    if not PACKER.is_file():
        sys.exit(f"no packer at {PACKER} (set NBT_MANAGER_DIR)")
    with tempfile.TemporaryDirectory(prefix="nbt-release-fixtures-") as temporary:
        work = Path(temporary)
        key = work / "key.pem"
        subprocess.run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt",
                        "rsa_keygen_bits:3072", "-out", str(key)], check=True,
                       capture_output=True)
        subprocess.run(["openssl", "pkey", "-in", str(key), "-pubout", "-out",
                        str(HERE / "release.pub")], check=True)
        for name in ("launcher", "gui", "worker"):
            (work / name).write_bytes(elf(2))
        plugin = work / "plugin"
        plugin.mkdir()
        (plugin / "libNbtManager.so").write_bytes(elf(3))
        for name in ("NbtManager.manifest", "NbtManager.xml", "NbtManager_ALL_Texts.zip",
                     "NbtManager_ALL_Images.zip"):
            (plugin / name).write_bytes(name.encode())
        (work / "libnbt-capture.so").write_bytes(elf(3))

        v1 = HERE / "release-v1.nbtr"
        pack(work, key, v1, "v1", 1)
        pack(work, key, HERE / "release-v2.nbtr", "v2", 2, capture=True)

        data = v1.read_bytes()
        manifest_size = struct.unpack(">I", data[8:12])[0]
        manifest_end = 16 + manifest_size
        manifest = data[16:manifest_end].decode()
        if BRIDGE_LINE not in manifest:
            sys.exit("the packer's bridge line changed; update BRIDGE_LINE and release.rs")
        broken = [
            manifest.replace("sequence=1\n", "sequence=01\n"),
            manifest.replace(BRIDGE_LINE, "bridge=1.00|\n"),
            manifest.replace(BRIDGE_LINE, "bridge=1.0|show,show\n"),
            manifest.replace(BRIDGE_LINE, "bridge=1.0|show,\n"),
            manifest.replace(BRIDGE_LINE, "bridge=2.0|\nbridge=1.0|\n"),
            manifest.replace(BRIDGE_LINE, "bridge=1.0|\nbridge=2.0|\nbridge=3.0|\n"),
            manifest.replace("compat=demo-1\n", "compat=../etc\n"),
            manifest.replace("version=v1\n", "version=v1\nversion=v1\n"),
        ]
        for old in HERE.glob("malformed-*.nbtr"):
            old.unlink()
        for number, text in enumerate(broken, 1):
            if text == manifest:
                sys.exit(f"malformed variant {number} did not change the manifest")
            body = text.encode()
            signed = b"NBTRLS1\n" + struct.pack(">II", len(body), SIGNATURE_SIZE) + body
            signed += sign(key, signed)
            signed += data[manifest_end + SIGNATURE_SIZE:]
            (HERE / f"malformed-{number}.nbtr").write_bytes(signed)
    print(f"fixtures written to {HERE}")


if __name__ == "__main__":
    main()
