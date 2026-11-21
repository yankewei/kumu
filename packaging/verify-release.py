#!/usr/bin/env python3
"""Verifies the signed, notarized DMG from the inside, saves the release, and
checks whether an existing release artifact can be reused for `--publish`.

Modes:
  Default (verify & save):
    Run from the repository root by packaging/release.sh, which exports:
    KUMU_RELEASE_DIR, KUMU_VERSION, KUMU_APP, KUMU_DMG, and optionally
    KUMU_ARCH, KUMU_COMMIT, KUMU_DIRTY.

  --check-existing:
    Checks whether target/release/Kumu-<version>-<arch>.dmg matches the current
    clean HEAD commit, receipt metadata, checksum files, notarization log, and
    local signature/staple validation so `--publish` can reuse it without
    rebuilding or re-notarizing. Exports: KUMU_VERSION, KUMU_ARCH, KUMU_COMMIT.

Only the DMG is notarized: the app inside is covered by the DMG's ticket and is
not stapled on its own. So the checks here are the app's Developer ID signature,
the DMG's notarization log, and Gatekeeper's verdict on the app (an online
lookup when the app has no ticket of its own).
"""

import argparse
import datetime
import hashlib
import json
import os
import platform
import plistlib
import shutil
import subprocess
import sys
from pathlib import Path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def normalize_arch(raw: str | None) -> str:
    value = raw or platform.machine()
    if value in ("arm64", "aarch64"):
        return "aarch64"
    return value


def receipt_path_for(out: Path, version: str, arch: str) -> Path:
    return out / f"Kumu-{version}-{arch}.release.json"


def check_notarization_log(path: Path) -> str | None:
    if not path.is_file():
        return f"缺少公证日志：{path}"
    try:
        result = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        return f"无法读取公证日志 {path}：{exc}"
    if result.get("status") != "Accepted" or result.get("issues"):
        return f"公证日志未通过：{path}"
    return None


def check_existing_artifact(
    out: Path,
    version: str,
    arch: str,
    commit: str,
    *,
    run_macos_checks: bool = True,
) -> tuple[bool, str]:
    dmg_name = f"Kumu-{version}-{arch}.dmg"
    dmg_path = out / dmg_name
    sha_path = out / f"{dmg_name}.sha256"
    sums_path = out / "SHA256SUMS"
    notary_log = out / f"Kumu-{version}-dmg-notarization.json"
    receipt_path = receipt_path_for(out, version, arch)

    if not receipt_path.is_file():
        return False, f"未找到可复用的构建记录：{receipt_path}"

    try:
        receipt = json.loads(receipt_path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        return False, f"构建记录损坏（{receipt_path}）：{exc}"

    if receipt.get("version") != version:
        return False, f"现有产物版本为 {receipt.get('version')}，与当前 {version} 不一致"
    if receipt.get("arch") != arch:
        return False, f"现有产物架构为 {receipt.get('arch')}，与当前 {arch} 不一致"
    if receipt.get("dmg") != dmg_name:
        return False, f"现有产物文件名不匹配：{receipt.get('dmg')}"
    if receipt.get("dirty") is not False:
        return False, "现有产物构建时工作区不干净，不能直接复用"
    if receipt.get("commit") != commit:
        old = str(receipt.get("commit") or "未知")[:12]
        return False, f"现有产物对应提交 {old}，与当前 HEAD {commit[:12]} 不一致"
    if receipt.get("notarized") is not True:
        return False, "现有产物未标记为已公证"

    for required in (dmg_path, sha_path, sums_path, notary_log):
        if not required.is_file():
            return False, f"缺少产物文件：{required}"

    log_err = check_notarization_log(notary_log)
    if log_err:
        return False, log_err

    actual_sha = sha256_file(dmg_path)
    if receipt.get("sha256") != actual_sha:
        return False, f"{dmg_name} 的 SHA256 与构建记录不一致（可能已被覆盖）"

    expected_line = f"{actual_sha}  {dmg_name}"
    if sha_path.read_text().strip() != expected_line:
        return False, f"{sha_path.name} 与当前 DMG 校验值不一致"
    sums_lines = [line.strip() for line in sums_path.read_text().splitlines() if line.strip()]
    if expected_line not in sums_lines:
        return False, f"{sums_path.name} 缺少当前 DMG 的校验行"

    if run_macos_checks:
        verify_sign = subprocess.run(
            ["codesign", "--verify", "--strict", str(dmg_path)],
            capture_output=True,
            text=True,
            check=False,
        )
        if verify_sign.returncode != 0:
            detail = (verify_sign.stderr or verify_sign.stdout).strip()
            return False, f"现有 DMG 签名校验失败：{detail}"

        validate_staple = subprocess.run(
            ["xcrun", "stapler", "validate", str(dmg_path)],
            capture_output=True,
            text=True,
            check=False,
        )
        if validate_staple.returncode != 0:
            detail = (validate_staple.stderr or validate_staple.stdout).strip()
            return False, f"现有 DMG staple 票据校验失败：{detail}"

    return True, f"复用已验证产物：{dmg_path.resolve()}（commit {commit[:12]}）"


def verify_and_save() -> None:
    root = Path(os.environ["KUMU_RELEASE_DIR"])
    version = os.environ["KUMU_VERSION"]
    dmg = Path(os.environ["KUMU_DMG"])
    app = Path(os.environ["KUMU_APP"])
    arch = normalize_arch(os.environ.get("KUMU_ARCH"))
    commit = os.environ.get("KUMU_COMMIT", "")
    dirty_env = os.environ.get("KUMU_DIRTY", "")
    if not commit:
        commit = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    if dirty_env in ("true", "1"):
        dirty = True
    elif dirty_env in ("false", "0"):
        dirty = False
    else:
        dirty = bool(
            subprocess.run(
                ["git", "status", "--porcelain"],
                capture_output=True,
                text=True,
                check=True,
            ).stdout.strip()
        )

    log_err = check_notarization_log(root / "dmg-notarization.json")
    if log_err:
        raise SystemExit(f"先检查 DMG 公证日志，尚不能交付（{log_err}）")

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
        with (bundled / "Contents/Info.plist").open("rb") as fh:
            info = plistlib.load(fh)
        if info.get("CFBundleShortVersionString") != version:
            raise SystemExit(
                f"DMG 内应用版本为 {info.get('CFBundleShortVersionString')}，应为 {version}"
            )
        apps_link = mount / "Applications"
        if not apps_link.is_symlink() or os.readlink(apps_link) != "/Applications":
            raise SystemExit("DMG 内缺少指向 /Applications 的 Applications 符号链接")
    finally:
        detach = subprocess.run(["hdiutil", "detach", str(mount)], check=False)
        if detach.returncode != 0:
            subprocess.run(["hdiutil", "detach", "-force", str(mount)], check=False)

    out = Path("target/release")
    out.mkdir(parents=True, exist_ok=True)
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
    checksum = sha256_file(out / dmg.name)
    (out / f"{dmg.name}.sha256").write_text(f"{checksum}  {dmg.name}\n")
    (out / "SHA256SUMS").write_text(f"{checksum}  {dmg.name}\n")
    receipt = {
        "version": version,
        "arch": arch,
        "commit": commit,
        "dirty": dirty,
        "dmg": dmg.name,
        "sha256": checksum,
        "notarized": True,
        "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }
    receipt_path_for(out, version, arch).write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"可分发产物：{(out / dmg.name).resolve()}")


def main() -> None:
    parser = argparse.ArgumentParser(add_help=True)
    parser.add_argument(
        "--check-existing",
        action="store_true",
        help="Validate an existing target/release DMG for reuse during --publish",
    )
    parser.add_argument(
        "--out-dir",
        default="target/release",
        help="Artifact output directory (default: target/release)",
    )
    args = parser.parse_args()

    if args.check_existing:
        version = os.environ["KUMU_VERSION"]
        arch = normalize_arch(os.environ.get("KUMU_ARCH"))
        commit = os.environ["KUMU_COMMIT"]
        ok, message = check_existing_artifact(Path(args.out_dir), version, arch, commit)
        if ok:
            print(message)
            return
        print(message, file=sys.stderr)
        raise SystemExit(1)

    verify_and_save()


if __name__ == "__main__":
    main()
