# macOS DMG 发布流程

以后生成可分发的 Kumu DMG，默认执行本文的 **Xcode 云端签名 → 应用公证 → DMG 公证** 流程。2026-11-08 已在本机完整验证，产物 `Kumu-0.1.0-aarch64.dmg` 通过 Apple 公证和 Gatekeeper 检查。

## 签名方式与本机配置

| 项目 | 已验证配置 |
| --- | --- |
| Developer Team ID | `3BUD4GCZCQ` |
| 本机开发证书 | `Apple Development: kewei yan (GK4M5T86KV)` |
| 导出后的分发签名 | `Developer ID Application: kewei yan (3BUD4GCZCQ)` |
| 公证钥匙串 profile | `kumu` |
| Bundle ID | `com.yankewei.kumu` |
| 架构 | 本机为 Apple Silicon，产物为 `aarch64`，不是 Universal |

开发证书名称末尾的 `GK4M5T86KV` 不是这里应填写的 Team ID。需要核实时，对开发证书签名后的应用运行 `codesign -dv --verbose=4 <app>`，查看 `TeamIdentifier`。

本机不需要持有 Developer ID 的私钥：先用已有 Apple Development 证书签署归档，再通过 `xcodebuild -exportArchive` 的自动签名取得正式分发应用。Xcode 必须登录有该团队云端 Developer ID 签名权限的账号；本次使用 Admin 账号成功。

不要因为 `security find-identity` 没有本机 Developer ID，就停止打包或要求用户创建证书。手动创建本机 Developer ID 需要 Account Holder；云端签名有独立权限。见 [Apple 云端证书说明](https://developer.apple.com/help/account/certificates/cloud-managed-certificates/)和 [Developer ID 权限说明](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/)。

现有脚本的范围：

- `make-app.sh`、`make-dmg.sh` 默认生成临时签名的本地测试包。
- `release.sh` 要求本机 Developer ID 私钥，尚未接入本文的云端流程；它还会推送提交、标签并创建 GitHub Release。
- **不要用 `release.sh` 代替本文流程，也不要在云端签名完成后重跑 `make-app.sh` 或 `make-dmg.sh` 覆盖正式产物。**

## 1. 检查环境，准备版本

需要完整 Xcode、项目的 Rust/Zig 构建环境和 Python 3。Xcode 升级、账号权限或证书变化后，需要重新确认可用性。

从项目根目录进入 Bash；以下代码块按顺序在同一个 Bash 会话运行，任一步失败都先处理错误，再继续。

```bash
bash
set -euo pipefail
xcodebuild -version
xcrun --find notarytool
xcrun --find stapler
security find-identity -v -p codesigning
xcrun notarytool history --keychain-profile kumu
git status --short
```

正式发版前更新 `Cargo.toml` 的版本号，并提交要发布的代码。仅打包测试时可以保留工作区修改；不要擅自提交、暂存或丢弃用户改动。

`kumu` profile 已验证可用，正常不必重新配置。换机器或确实缺失时才运行以下命令，并在本机提示中输入 App 专用密码，不把密码写入文档或仓库：

```bash
xcrun notarytool store-credentials kumu \
  --apple-id '你的 Apple ID 邮箱' \
  --team-id 3BUD4GCZCQ
```

## 2. 编译并生成 Xcode 归档

中间产物放在独立临时目录，所有验证通过后才覆盖 `target/release/` 中的发布文件。记住输出的工作目录，失败时可以从该目录继续。

```bash
export KUMU_TEAM_ID=3BUD4GCZCQ
export KUMU_DEVELOPMENT_IDENTITY='Apple Development: kewei yan (GK4M5T86KV)'
export KUMU_NOTARY_PROFILE=kumu
export KUMU_VERSION="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"
export KUMU_ARCH="$(uname -m)"
if [[ "$KUMU_ARCH" == arm64 ]]; then export KUMU_ARCH=aarch64; fi
export KUMU_RELEASE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kumu-release.XXXXXX")"
echo "$KUMU_RELEASE_DIR"

cargo build --release

python3 <<'PY'
import datetime
import os
from pathlib import Path
import plistlib
import shutil

root = Path(os.environ['KUMU_RELEASE_DIR'])
archive = root / 'Kumu.xcarchive'
app = archive / 'Products/Applications/Kumu.app'
(app / 'Contents/MacOS').mkdir(parents=True)
(app / 'Contents/Resources').mkdir()
shutil.copy2('target/release/kumu', app / 'Contents/MacOS/kumu')
shutil.copy2('assets/AppIcon.icns', app / 'Contents/Resources/AppIcon.icns')
with open('packaging/Info.plist', 'rb') as f:
    info = plistlib.load(f)
version = os.environ['KUMU_VERSION']
info['CFBundleShortVersionString'] = version
info['CFBundleVersion'] = version
with (app / 'Contents/Info.plist').open('wb') as f:
    plistlib.dump(info, f)
with (archive / 'Info.plist').open('wb') as f:
    plistlib.dump({
        'ArchiveVersion': 2,
        'CreationDate': datetime.datetime.now(datetime.timezone.utc),
        'Name': 'Kumu',
        'SchemeName': 'Kumu',
        'ApplicationProperties': {
            'ApplicationPath': 'Applications/Kumu.app',
            'CFBundleIdentifier': info['CFBundleIdentifier'],
            'CFBundleShortVersionString': version,
            'CFBundleVersion': version,
            'SigningIdentity': os.environ['KUMU_DEVELOPMENT_IDENTITY'],
            'Team': os.environ['KUMU_TEAM_ID'],
        },
    }, f)
with (root / 'ExportOptions.plist').open('wb') as f:
    plistlib.dump({
        'method': 'developer-id',
        'signingStyle': 'automatic',
        'teamID': os.environ['KUMU_TEAM_ID'],
        'destination': 'export',
    }, f)
PY

codesign --force --options runtime --timestamp \
  --entitlements packaging/entitlements.plist \
  --sign "$KUMU_DEVELOPMENT_IDENTITY" \
  "$KUMU_RELEASE_DIR/Kumu.xcarchive/Products/Applications/Kumu.app"
```

## 3. Xcode 云端签名

```bash
xcodebuild -exportArchive \
  -archivePath "$KUMU_RELEASE_DIR/Kumu.xcarchive" \
  -exportOptionsPlist "$KUMU_RELEASE_DIR/ExportOptions.plist" \
  -exportPath "$KUMU_RELEASE_DIR/export" \
  -allowProvisioningUpdates

export KUMU_APP="$KUMU_RELEASE_DIR/export/Kumu.app"
codesign --verify --deep --strict "$KUMU_APP"
codesign -dv --verbose=4 "$KUMU_APP"
```

必须看到 `EXPORT SUCCEEDED`，应用签名的 `Authority` 为 `Developer ID Application: kewei yan (3BUD4GCZCQ)`，并包含 hardened runtime 的 `runtime` 标记。此时即使钥匙串仍只列出 Apple Development，也是正常的。

## 4. 应用公证并附加票据

```bash
ditto -c -k --keepParent "$KUMU_APP" "$KUMU_RELEASE_DIR/Kumu.zip"
xcrun notarytool submit "$KUMU_RELEASE_DIR/Kumu.zip" \
  --keychain-profile "$KUMU_NOTARY_PROFILE" --wait \
  | tee "$KUMU_RELEASE_DIR/app-submission.txt"

KUMU_APP_SUBMISSION_ID="$(sed -n 's/^  id: //p' "$KUMU_RELEASE_DIR/app-submission.txt" | head -n1)"
xcrun notarytool log "$KUMU_APP_SUBMISSION_ID" \
  --keychain-profile "$KUMU_NOTARY_PROFILE" \
  "$KUMU_RELEASE_DIR/app-notarization.json"

xcrun stapler staple "$KUMU_APP"
xcrun stapler validate "$KUMU_APP"
spctl --assess --type execute --verbose=2 "$KUMU_APP"
```

公证必须为 `Accepted`；Gatekeeper 必须输出 `accepted` 和 `source=Notarized Developer ID`。公证日志中有问题时先查看并处理。先给应用附加票据再制作 DMG，使拖入 Applications 的应用也携带票据。

## 5. 制作并公证 DMG

此流程签署的是应用；DMG 容器不另做本机 Developer ID 签名，但会单独公证并附加票据。这是本次实际验证通过的产物形式。

```bash
mkdir "$KUMU_RELEASE_DIR/dmg-stage"
ditto "$KUMU_APP" "$KUMU_RELEASE_DIR/dmg-stage/Kumu.app"
ln -s /Applications "$KUMU_RELEASE_DIR/dmg-stage/Applications"
export KUMU_DMG="$KUMU_RELEASE_DIR/Kumu-${KUMU_VERSION}-${KUMU_ARCH}.dmg"

hdiutil create -volname Kumu \
  -srcfolder "$KUMU_RELEASE_DIR/dmg-stage" -ov -format UDZO "$KUMU_DMG"

xcrun notarytool submit "$KUMU_DMG" \
  --keychain-profile "$KUMU_NOTARY_PROFILE" --wait \
  | tee "$KUMU_RELEASE_DIR/dmg-submission.txt"

KUMU_DMG_SUBMISSION_ID="$(sed -n 's/^  id: //p' "$KUMU_RELEASE_DIR/dmg-submission.txt" | head -n1)"
xcrun notarytool log "$KUMU_DMG_SUBMISSION_ID" \
  --keychain-profile "$KUMU_NOTARY_PROFILE" \
  "$KUMU_RELEASE_DIR/dmg-notarization.json"

xcrun stapler staple "$KUMU_DMG"
xcrun stapler validate "$KUMU_DMG"
hdiutil verify "$KUMU_DMG"
```

`hdiutil` 在本机 Xcode/macOS 环境会提示弃用，但上述命令仍正常工作；工具升级后应核实替代命令再迁移。

## 6. 检查安装包并保存产物

检查 DMG 内实际携带的应用，而不只检查制作前的目录。挂载点从工具返回值读取，避免假设一定是 `/Volumes/Kumu`。

```bash
python3 <<'PY'
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess

root = Path(os.environ['KUMU_RELEASE_DIR'])
dmg = Path(os.environ['KUMU_DMG'])
app = Path(os.environ['KUMU_APP'])
for kind in ('app', 'dmg'):
    result = json.loads((root / f'{kind}-notarization.json').read_text())
    if result.get('status') != 'Accepted' or result.get('issues'):
        raise SystemExit(f'先检查 {kind} 公证日志，尚不能交付')

mounted = subprocess.run(
    ['hdiutil', 'attach', '-readonly', '-nobrowse', '-plist', str(dmg)],
    capture_output=True, check=True,
)
mount = next(Path(e['mount-point']) for e in
             plistlib.loads(mounted.stdout)['system-entities'] if 'mount-point' in e)
try:
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(mount / 'Kumu.app')], check=True)
    subprocess.run(['xcrun', 'stapler', 'validate', str(mount / 'Kumu.app')], check=True)
    subprocess.run(['spctl', '--assess', '--type', 'execute', '--verbose=2', str(mount / 'Kumu.app')], check=True)
    assert (mount / 'Applications').is_symlink()
    assert os.readlink(mount / 'Applications') == '/Applications'
finally:
    subprocess.run(['hdiutil', 'detach', str(mount)], check=True)

out = Path('target/release')
if (out / 'Kumu.app').exists():
    shutil.rmtree(out / 'Kumu.app')
shutil.copytree(app, out / 'Kumu.app', symlinks=True)
shutil.copy2(dmg, out / dmg.name)
for kind in ('app', 'dmg'):
    shutil.copy2(root / f'{kind}-notarization.json',
                 out / f'Kumu-{os.environ["KUMU_VERSION"]}-{kind}-notarization.json')
checksum = hashlib.sha256((out / dmg.name).read_bytes()).hexdigest()
(out / f'{dmg.name}.sha256').write_text(f'{checksum}  {dmg.name}\n')
(out / 'SHA256SUMS').write_text(f'{checksum}  {dmg.name}\n')
print(f'可分发产物：{(out / dmg.name).resolve()}')
PY
```

最终交付 `target/release/Kumu-<version>-<arch>.dmg`、对应 `.dmg.sha256` 和 `SHA256SUMS`，保留应用及 DMG 的公证日志。成功后可删除这次创建的临时工作目录。用户安装后仍需自行安装、登录要使用的编码 Agent。

应用内更新器通过 GitHub Releases 读取最新正式版本，按 `Kumu-<version>-<arch>.dmg` 匹配安装包。每次发布都必须附带固定名称的 `SHA256SUMS`，不能只上传带版本号的 `.sha256` 文件。更新器要求 SHA256 匹配，并验证整个 `.app` 的版本号和签名团队 `3BUD4GCZCQ`；缺少校验文件或验证失败时不安装。若发布多个架构，需要将它们的校验行合并到同一个 `SHA256SUMS`。

当前使用 `gpui-updater-pre` 的原生 DMG 安装后端，安装位置必须可写；只读磁盘映像、App Translocation 和没有写权限的安装位置会提示手动安装。首次带更新器的版本仍需手动安装，旧版不会自动获得更新功能。开发依赖固定 `gpui-pre = 0.3.4`，已验证与 gpui-kit 0.6 兼容；升级这个版本时应先重新验证编译。

## 上传 GitHub Releases

只有用户要求发布到 GitHub 时再执行。先确认版本对应的代码已提交、工作区干净，且发布产物就是上文通过验证的文件；不再运行会重新打包的 `release.sh`。

```bash
gh auth status
git status --short
git tag -a "v$KUMU_VERSION" -m "Release v$KUMU_VERSION"
git push origin HEAD "refs/tags/v$KUMU_VERSION"
gh release create "v$KUMU_VERSION" \
  "target/release/$(basename "$KUMU_DMG")" \
  "target/release/$(basename "$KUMU_DMG").sha256" \
  "target/release/SHA256SUMS" \
  --title "v$KUMU_VERSION" --generate-notes --verify-tag
```

如果标签或 Release 已存在，先核对它对应的提交及已有附件，再决定是否补传；不要强制移动标签或覆盖其他版本。

## 遇到问题时

- **提示仅 Account Holder 能创建证书**：这是手动创建本机证书的限制，回到第 3 步云端导出。只有云端导出也明确缺少权限时，才需要处理账号权限。
- **`No Account for Team`**：先检查 Team ID。本机正确值是 `3BUD4GCZCQ`；不要填写开发证书名称里的 `GK4M5T86KV`。再检查 Xcode 登录状态。
- **`HTTPClientError.connectTimeout`**：本次公证上传前两次超时，第三次使用默认加速成功。保留已签名应用，检查上传状态后重试同一个文件，也可尝试 `--no-s3-acceleration`；不要重新编译或要求重新配置已验证有效的凭据。
- **只收到 submission ID，没有 `Successfully uploaded file`**：不代表上传完成。用 `notarytool info <id> --keychain-profile kumu` 查询；这类未完成上传的记录可能持续显示 `In Progress`，应重新提交文件。
- **已成功上传但等待命令超时**：用 `xcrun notarytool wait <id> --keychain-profile kumu` 继续等，或用 `info` 查询，不必重复上传。
- **`Invalid`、票据附加失败或 Gatekeeper 拒绝**：下载对应 submission 的 `notarytool log`，检查签名和 entitlements，不交付为正式安装包。网络恢复后可重新执行票据附加和验证步骤。
- **需要详细网络日志**：`notarytool submit ... --verbose` 可以辅助诊断；日志留在本机，分享前检查并清理凭据和上传认证信息。
