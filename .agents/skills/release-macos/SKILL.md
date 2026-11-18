---
name: release-macos
description: 构建、签名、公证并发布 Kumu 的可分发 macOS DMG。当用户要求发版、发布新版本、打包 DMG、修复签名或公证失败、上传 GitHub Release 时使用。
---

# 发布 macOS

唯一入口是 `packaging/release.sh`。不要手敲签名/公证命令，也不要重跑打包脚本覆盖产物。

## 签名与公证

发布优先用 **Developer ID Application** 身份在本地签名，只公证 DMG 一次：DMG 里的 app 由这张票据覆盖，不单独 staple。身份按以下顺序取：

- `KUMU_SIGNING_IDENTITY="Developer ID Application: …"`：身份已经在钥匙串里。
- `KUMU_SIGNING_P12=/path/cert.p12` + `KUMU_SIGNING_P12_PASSWORD=…`：导入一个临时 keychain，脚本退出时删除。
- 钥匙串里已有的 `Developer ID Application: …` 身份，或显式设置的 `CODESIGN_IDENTITY`。
- 都没有时回退到 Xcode 云端 Developer ID 签名（多约一分钟，需要 Xcode 登录团队）。本机只有 `Apple Development` 证书时这就是唯一的路径，不算阻断。

公证凭据来自 `xcrun notarytool store-credentials` 的 keychain profile（默认 `kumu`）：

```bash
xcrun notarytool store-credentials kumu --apple-id '<Apple ID>' --team-id 3BUD4GCZCQ
```

## 先确认（只读）

```bash
xcrun --find notarytool >/dev/null && xcrun notarytool history --keychain-profile kumu | head -3
sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1
security find-identity -v -p codesigning | grep 'Developer ID Application' || echo "没有本地 Developer ID，发布将走云端签名"
git -C vendor/ghostty fetch origin --tags
echo "当前: $(git -C vendor/ghostty describe --tags --exact-match HEAD 2>/dev/null || echo 未在标签上)"
echo "最新: $(git -C vendor/ghostty tag -l 'vt-*' --sort=-v:refname | head -1)"
```

- `vendor/ghostty` 只钉 `vt-*` 标签。两行一致就继续。当前未在标签上，或最新标签更新时，先停下来：`git -C vendor/ghostty checkout <vt-标签>`，提交子模块指针，编译并跑终端测试，然后再打包。
- 不读取、不打印、不导出钥匙串或 `.p12` 的密码。

## 版本与提交

- 发新版本时先改 `Cargo.toml` 的 `version`，再跑一次 `cargo build --release`，让 `Cargo.lock` 跟着更新。
- 要发布的代码必须先提交：`--publish` 会拒绝脏工作区。
- 不要手工打 tag 或 `gh release create`；脚本负责这两件事。

## 构建并验证

```bash
packaging/release.sh
```

6 个阶段：预检 → 编译/组包/签名 →（没有本地身份时）Xcode 云端签名 → 制作/签名/公证 DMG → 挂载复验 → 发布（默认跳过）。

应看到：

- 签名 `Authority=Developer ID Application: …`，并带 hardened runtime。
- 只有一次公证（DMG），`status: Accepted`；`stapler staple`/`validate` 通过；有本地身份时 `spctl` 对 DMG 和 DMG 内的 app 都是 `source=Notarized Developer ID`。
- 产物：`target/release/Kumu-<version>-<arch>.dmg`、`.dmg.sha256`、`SHA256SUMS`、`Kumu-<version>-dmg-notarization.json`。
- 有本地 Developer ID 时约 2 分钟；回退到云端签名约 3 分钟（公证等待占大头）。

签名和云端导出可能弹出钥匙串/账号授权，需要用户在桌面上点“允许”。Agent 不要假设自己点过了；命令挂住时先问用户。

说明：DMG 内的 app 没有自己的 staple，从 DMG 复制到 /Applications 后首次**离线**启动可能被 Gatekeeper 拦；在线时按票据校验正常。这是为了少等一次公证，和 herdr 的下载 DMG 一致。

失败会保留临时目录并打印路径，里面有 `*-submission.txt` 和 `*-notarization.json`。

## 发布

只有用户明确要求时才：

```bash
packaging/release.sh --publish
```

脚本会复核工作区干净、本地和远端都没有该标签，然后打 `v<version>`、推送、`gh release create` 上传三个附件。

## 硬约束

- 不要用 `make-app.sh` / `make-dmg.sh` 覆盖签名后的发布产物；它们只做本地测试包（`release.sh` 会在有 Developer ID 时调用 `make-app.sh`）。
- 不要重命名产物：应用内更新器按 `Kumu-<version>-<arch>.dmg` 匹配，并强制校验 `SHA256SUMS`；多架构时把校验行合并进同一个 `SHA256SUMS`。
- 不要移动或覆盖已有 tag / Release；已存在时先核对它指向的提交和附件。
- `--keep` 只用于排查；成功后让脚本清理临时目录。

## 常见失败

- **`stapler validate` 在刚 `staple` 后失败（CloudKit / base64 ticket）**：票据到 CDN 有延迟，脚本会对同一产物重试 5 次（每次间隔 20 秒）。不要重新编译或重新公证。
- **公证上传超时 / 只拿到 submission id**：网络问题常见。脚本会打印“对同一文件重新 submit”的命令；不要重新编译或重新配置已验证有效的凭据。
- **`No Account for Team`（仅云端回退路径）**：Team ID 是 `3BUD4GCZCQ`，不是证书名里的 `GK4M5T86KV`；再查 Xcode 登录状态。
- **公证 `Invalid` 或 Gatekeeper 拒绝**：看保留目录里的 `notarytool log`，修签名或 entitlements 后重跑；不要交付该包。
- **p12 导入后找不到身份**：确认 p12 里是 `Developer ID Application`（不是 Apple Development），且密码由 `KUMU_SIGNING_P12_PASSWORD` 提供。
