#!/usr/bin/env python3
"""Verifies the signed, notarized DMG from the inside, then saves the release.

Run from the repository root by packaging/release.sh, which exports:
KUMU_RELEASE_DIR, KUMU_VERSION, KUMU_APP, KUMU_DMG.

Only the DMG is notarized: the app inside is covered by the DMG's ticket and is
not stapled on its own. So the checks here are the app's Developer ID signature,
the DMG's notarization log, and Gatekeeper's verdict on the app (an online
lookup when the app has no ticket of its own).
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

result = json.loads((root / "dmg-notarization.json").read_text())
if result.get("status") != "Accepted" or result.get("issues"):
    raise SystemExit("先检查 DMG 公证日志，尚不能交付")

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
    bundled = mount / "Kumu.app"
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundled)], check=True)
    signature = subprocess.run(
        ["codesign", "-dvv", str(bundled)],
        capture_output=True,
        text=True,
        check=True,
    )
    if "Authority=Developer ID Application:" not in signature.stdout + signature.stderr:
        raise SystemExit("DMG 内的应用不是 Developer ID 签名")
    gatekeeper = subprocess.run(
        ["spctl", "--assess", "--type", "execute", "--verbose=2", str(bundled)],
        capture_output=True,
        text=True,
        check=True,
    )
    verdict = gatekeeper.stdout + gatekeeper.stderr
    if "source=Notarized Developer ID" not in verdict:
        raise SystemExit(f"DMG 内的应用未通过 Gatekeeper 的公证检查：{verdict.strip()}")
    info = plistlib.load((bundled / "Contents/Info.plist").open("rb"))
    if info.get("CFBundleShortVersionString") != version:
        raise SystemExit(
            f"DMG 内应用版本为 {info.get('CFBundleShortVersionString')}，应为 {version}"
        )
    assert (mount / "Applications").is_symlink()
    assert os.readlink(mount / "Applications") == "/Applications"
finally:
    subprocess.run(["hdiutil", "detach", str(mount)], check=True)

out = Path("target/release")
# A local sign puts the app in target/release already: it is both the source
# and the saved copy, and must not be removed before it is copied.
if app.resolve() != (out / "Kumu.app").resolve():
    if (out / "Kumu.app").exists():
        shutil.rmtree(out / "Kumu.app")
    shutil.copytree(app, out / "Kumu.app", symlinks=True)
shutil.copy2(dmg, out / dmg.name)
shutil.copy2(
    root / "dmg-notarization.json",
    out / f"Kumu-{version}-dmg-notarization.json",
)
checksum = hashlib.sha256((out / dmg.name).read_bytes()).hexdigest()
(out / f"{dmg.name}.sha256").write_text(f"{checksum}  {dmg.name}\n")
(out / "SHA256SUMS").write_text(f"{checksum}  {dmg.name}\n")
print(f"可分发产物：{(out / dmg.name).resolve()}")
