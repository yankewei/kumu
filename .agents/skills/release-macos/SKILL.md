---
name: release-macos
description: 构建、签名、公证并发布 Kumu 的可分发 macOS DMG。当用户要求发版、发布新版本、打包 DMG、修复签名或公证失败、上传 GitHub Release 时使用。
---

# macOS 发布

唯一入口是 `packaging/release.sh`。不要手敲签名/公证命令，也不要重跑打包脚本覆盖产物。

## 先确认（只读）

```bash
xcodebuild -version
xcrun notarytool history --keychain-profile kumu
sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1
git -C vendor/ghostty fetch origin --tags
echo "当前: $(git -C vendor/ghostty describe --tags --exact-match HEAD 2>/dev/null || echo 未在标签上)"
echo "最新: $(git -C vendor/ghostty tag -l 'vt-*' --sort=-v:refname | head -1)"
```

- 本机只有 `Apple Development` 证书是正常的：归档用它签，导出时由 Xcode 云端换成 Developer ID。不要把“没有本机 Developer ID 私钥”当成阻断。
- 不读取、不打印、不导出钥匙串里的密码。
- `vendor/ghostty` 只钉 `vt-*` 标签。两行一致就继续。当前未在标签上，或最新标签更新时，先停下来：`git -C vendor/ghostty checkout <vt-标签>`，提交子模块指针，编译并跑终端测试，然后再打包。不要在 `packaging/release.sh` 里自动升级。

## 版本与提交

- 发新版本时先改 `Cargo.toml` 的 `version`，再跑一次 `cargo build --release`，让 `Cargo.lock` 跟着更新。
- 要发布的代码必须先提交：`--publish` 会拒绝脏工作区。
- 不要手工打 tag 或 `gh release create`；脚本负责这两件事。

## 构建并验证

```bash
packaging/release.sh
```

7 个阶段：预检 → 编译/归档/开发证书签名 → Xcode 云端导出 → 应用公证 → DMG 公证 → 挂载复验 → 发布（默认跳过）。

应看到：

- 导出后 `Authority=Developer ID Application: … (3BUD4GCZCQ)`，并带 hardened runtime。
- 两次公证都是 `status: Accepted`；`stapler validate`、`spctl`（`source=Notarized Developer ID`）通过。
- 产物：`target/release/Kumu-<version>-<arch>.dmg`、`.dmg.sha256`、`SHA256SUMS`、两份 `Kumu-<version>-*-notarization.json`。

签名和导出可能弹出钥匙串/账号授权，需要用户在桌面上点“允许”。Agent 不要假设自己点过了；命令挂住时先问用户。

失败会保留临时目录并打印路径，里面有 `*-submission.txt` 和 `*-notarization.json`。

## 发布

只有用户明确要求时才：

```bash
packaging/release.sh --publish
```

脚本会复核工作区干净、本地和远端都没有该标签，然后打 `v<version>`、推送、`gh release create` 上传三个附件。

## 硬约束

- 不要用 `make-app.sh` / `make-dmg.sh` 覆盖云端签名后的产物；它们只做本地测试包。
- 不要重命名产物：应用内更新器按 `Kumu-<version>-<arch>.dmg` 匹配，并强制校验 `SHA256SUMS`；多架构时把校验行合并进同一个 `SHA256SUMS`。
- 不要移动或覆盖已有 tag / Release；已存在时先核对它指向的提交和附件。
- `--keep` 只用于排查；成功后让脚本清理临时目录。

## 常见失败

- **`No Account for Team`**：Team ID 是 `3BUD4GCZCQ`，不是证书名里的 `GK4M5T86KV`；再查 Xcode 登录状态。
- **公证上传超时 / 只拿到 submission id**：网络问题常见。脚本会打印“对同一文件重新 submit”的命令；不要重新编译或重新配置已验证有效的凭据。
- **公证 `Invalid` 或 Gatekeeper 拒绝**：看保留目录里的 `notarytool log`，修签名或 entitlements 后重跑；不要交付该包。
- **提示只有 Account Holder 能创建证书**：这是手动建本机证书的限制，回到云端导出，不要在此时要求用户创建 Developer ID。
