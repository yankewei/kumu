#!/usr/bin/env python3
"""Verifies the signed DMG a second time from the inside, then saves the release.

Run from the repository root by packaging/release.sh, which exports:
KUMU_RELEASE_DIR, KUMU_VERSION, KUMU_APP, KUMU_DMG.
"""

import hashlib
import json
import os
import plistlib
import shutil
import subprocess
from pathlib import Path

root = Path(os.environ["KUMU_RELEASE_DIR"])
version = os.environ["KUMU_VERSION"]
dmg = Path(os.environ["KUMU_DMG"])
app = Path(os.environ["KUMU_APP"])

for kind in ("app", "dmg"):
    result = json.loads((root / f"{kind}-notarization.json").read_text())
    if result.get("status") != "Accepted" or result.get("issues"):
        raise SystemExit(f"先检查 {kind} 公证日志，尚不能交付")

mounted = subprocess.run(
    ["hdiutil", "attach", "-readonly", "-nobrowse", "-plist", str(dmg)],
    capture_output=True,
    check=True,
)
mount = next(
    Path(e["mount-point"])
    for e in plistlib.loads(mounted.stdout)["system-entities"]
    if "mount-point" in e
)
try:
    subprocess.run(
        ["codesign", "--verify", "--deep", "--strict", str(mount / "Kumu.app")],
        check=True,
    )
    subprocess.run(["xcrun", "stapler", "validate", str(mount / "Kumu.app")], check=True)
    gatekeeper = subprocess.run(
        ["spctl", "--assess", "--type", "execute", "--verbose=2", str(mount / "Kumu.app")],
        capture_output=True,
        text=True,
        check=True,
    )
    if "source=Notarized Developer ID" not in gatekeeper.stdout + gatekeeper.stderr:
        raise SystemExit("DMG 内的应用未通过 Gatekeeper 的公证检查")
    info = plistlib.load((mount / "Kumu.app/Contents/Info.plist").open("rb"))
    if info.get("CFBundleShortVersionString") != version:
        raise SystemExit(
            f"DMG 内应用版本为 {info.get('CFBundleShortVersionString')}，应为 {version}"
        )
    assert (mount / "Applications").is_symlink()
    assert os.readlink(mount / "Applications") == "/Applications"
finally:
    subprocess.run(["hdiutil", "detach", str(mount)], check=True)

out = Path("target/release")
if (out / "Kumu.app").exists():
    shutil.rmtree(out / "Kumu.app")
shutil.copytree(app, out / "Kumu.app", symlinks=True)
shutil.copy2(dmg, out / dmg.name)
for kind in ("app", "dmg"):
    shutil.copy2(
        root / f"{kind}-notarization.json",
        out / f"Kumu-{version}-{kind}-notarization.json",
    )
checksum = hashlib.sha256((out / dmg.name).read_bytes()).hexdigest()
(out / f"{dmg.name}.sha256").write_text(f"{checksum}  {dmg.name}\n")
(out / "SHA256SUMS").write_text(f"{checksum}  {dmg.name}\n")
print(f"可分发产物：{(out / dmg.name).resolve()}")
