---
name: release-macos
description: 构建、签名、公证并发布 Kumu 的可分发 macOS DMG。当用户要求发版、发布新版本、打包 DMG、修复签名或公证失败、上传 GitHub Release 时使用。
---

# 发布 macOS

唯一入口是 `packaging/release.sh`。日常发版不要手敲签名/公证命令，也不要重跑打包脚本覆盖产物。

## 本机准备（一次性，已就绪）

发布证书不进仓库，放在 `~/.kumu-signing/`（目录 `0700`）：

| 文件 | 用途 |
| --- | --- |
| `developer-id.p12` | Developer ID Application 证书 + 私钥（`0600`） |
| `developer-id.p12.password` | p12 密码（`0600`） |
| `env.sh` | `source` 后设置 `KUMU_SIGNING_P12` / `KUMU_SIGNING_P12_PASSWORD` |
| `developer-id.key` / `.pem` / `.csr` | 原始私钥/证书/CSR（`.csr` 签发后已可删除） |

日常发版先加载环境：

```bash
source ~/.kumu-signing/env.sh
```

`release.sh` 会把 p12 导入一个临时 keychain、签名、退出时删除，不写登录钥匙串。证书当前有效期到 **2027-12-18**。

### 首次准备 / 到期续期

1. 本机生成 CSR 和私钥（私钥不出本机）：

   ```bash
   umask 077; mkdir -p ~/.kumu-signing
   openssl req -new -newkey rsa:2048 -nodes \
     -keyout ~/.kumu-signing/developer-id.key \
     -out ~/.kumu-signing/developer-id.csr \
     -subj "/CN=<名字>/emailAddress=<邮箱>"
   ```

2. 让团队的 **Account Holder** 签发（Admin 建不了 Developer ID）：
   [developer.apple.com](https://developer.apple.com/account/resources/certificates/list) → Certificates → **+** → Software 选 **Developer ID Application** → 上传 `developer-id.csr` → 下载 `.cer`。

   注意：App Store Connect 里的“账户持有人”角色和 Apple Developer Program 的 Account Holder 不是一回事；以开发者后台的 People/Membership 为准。

3. 把 `.cer`、私钥和 `OU=G2` 的中间证书打成 p12：

   ```bash
   cd ~/.kumu-signing
   openssl x509 -in ~/Downloads/developerID_application.cer -inform DER \
     -out developer-id.pem -outform PEM
   # 从钥匙串导出中间证书，取 subject 里 OU=G2 的那张
   security find-certificate -a -c "Developer ID Certification Authority" -p > /tmp/devid-cas.pem
   # 拆出 OU=G2 的证书存成 /tmp/devid-g2.pem 后：
   pw="$(openssl rand -base64 24 | tr -d '\n')"
   printf '%s' "$pw" > developer-id.p12.password
   openssl pkcs12 -export -inkey developer-id.key -in developer-id.pem \
     -certfile /tmp/devid-g2.pem \
     -name "Developer ID Application: <名字> (3BUD4GCZCQ)" \
     -out developer-id.p12 -passout pass:"$pw"
   chmod 600 developer-id.p12 developer-id.p12.password
   ```

4. 验证 p12 里有可用的身份（临时 keychain，检查完删掉）：

   ```bash
   work="$(mktemp -d)"; kc="$work/check.keychain-db"; kp="$(openssl rand -hex 16)"
   security create-keychain -p "$kp" "$kc"; security unlock-keychain -p "$kp" "$kc"
   security import ~/.kumu-signing/developer-id.p12 -k "$kc" \
     -P "$(cat ~/.kumu-signing/developer-id.p12.password)" -T /usr/bin/codesign
   security set-key-partition-list -S apple-tool:,apple: -s -k "$kp" "$kc" >/dev/null
   security find-identity -v -p codesigning "$kc"   # 应看到 Developer ID Application: … (3BUD4GCZCQ)
   security delete-keychain "$kc"; rm -rf "$work"
   ```

5. 写 `~/.kumu-signing/env.sh`：

   ```bash
   export KUMU_SIGNING_P12="$HOME/.kumu-signing/developer-id.p12"
   export KUMU_SIGNING_P12_PASSWORD="$(cat "$HOME/.kumu-signing/developer-id.p12.password")"
   ```

## 日常发版

1. 改 `Cargo.toml` 的 `version`，跑一次 `cargo build --release` 更新 `Cargo.lock`，提交（`--publish` 要求工作区干净）。
2. `source ~/.kumu-signing/env.sh`
3. `packaging/release.sh` 先本地验证一遍（不发布），确认产物和校验。
4. `packaging/release.sh --publish` 打标签、推送并上传 Release。

6 个阶段：预检 → 编译/组包/本地签名 →（本地已签名，跳过云端导出）→ 制作/签名/公证 DMG → 挂载复验 → 发布。**约 2 分钟**（公证等待占大头）。

脚本会：用临时 keychain 导入 p12 → `make-app.sh` 签 app（hardened runtime + entitlements）→ 签 DMG → 只公证 DMG 一次 → staple/validate → `hdiutil verify` → `spctl` 验 DMG → 挂载复验 app 的签名、Gatekeeper 判定、版本、`/Applications` 链接 → 写入 `.sha256` / `SHA256SUMS`。

## 签名与公证的取用顺序

`release.sh` 按以下顺序找 Developer ID 身份：

- `KUMU_SIGNING_IDENTITY="Developer ID Application: …"`（已在钥匙串）
- `KUMU_SIGNING_P12` + `KUMU_SIGNING_P12_PASSWORD`（临时 keychain）
- 钥匙串里已有的 `Developer ID Application: …`，或显式设置的 `CODESIGN_IDENTITY`
- 都没有时回退到 Xcode 云端 Developer ID 签名（约 3 分钟，需要 Xcode 登录团队）

公证凭据来自 `xcrun notarytool store-credentials` 的 keychain profile（默认 `kumu`）：

```bash
xcrun notarytool store-credentials kumu --apple-id '<Apple ID>' --team-id 3BUD4GCZCQ
```

## 先确认（只读）

```bash
source ~/.kumu-signing/env.sh 2>/dev/null; ls -l "$KUMU_SIGNING_P12" 2>/dev/null || echo "没有本地 p12，将走云端签名"
xcrun notarytool history --keychain-profile kumu | head -3
sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1
git -C vendor/ghostty fetch origin --tags
echo "当前: $(git -C vendor/ghostty describe --tags --exact-match HEAD 2>/dev/null || echo 未在标签上)"
echo "最新: $(git -C vendor/ghostty tag -l 'vt-*' --sort=-v:refname | head -1)"
```

- `vendor/ghostty` 只钉 `vt-*` 标签。两行一致就继续。当前未在标签上，或最新标签更新时，先停下来：`git -C vendor/ghostty checkout <vt-标签>`，提交子模块指针，编译并跑终端测试，然后再打包。
- `security find-identity` 看不到 p12 里的身份是正常的：它只在 `release.sh` 的临时 keychain 里存在。
- 不读取、不打印、不导出钥匙串或 `.p12` 的密码。

## 构建并验证

```bash
source ~/.kumu-signing/env.sh
packaging/release.sh
```

应看到：

- 签名身份：`Developer ID Application: kewei yan (3BUD4GCZCQ)`，`Authority=Developer ID Application: …`，带 hardened runtime。
- 只有一次公证（DMG），`status: Accepted`；`stapler staple`/`validate` 通过；`spctl` 对 DMG 和 DMG 内的 app 都是 `source=Notarized Developer ID`。
- 产物：`target/release/Kumu-<version>-<arch>.dmg`、`.dmg.sha256`、`SHA256SUMS`、`Kumu-<version>-dmg-notarization.json`。
- 约 2 分钟；回退到云端签名约 3 分钟。

签名和云端导出可能弹出钥匙串/账号授权，需要用户在桌面上点“允许”。Agent 不要假设自己点过了；命令挂住时先问用户。

说明：DMG 内的 app 没有自己的 staple，从 DMG 复制到 /Applications 后首次**离线**启动可能被 Gatekeeper 拦；在线时按票据校验正常。这是为了少等一次公证，和 herdr 的下载 DMG 一致。

失败会保留临时目录并打印路径，里面有 `*-submission.txt` 和 `*-notarization.json`。

## 发布

只有用户明确要求时才：

```bash
source ~/.kumu-signing/env.sh
packaging/release.sh --publish
```

脚本会复核工作区干净、本地和远端都没有该标签，然后打 `v<version>`、推送、`gh release create` 上传三个附件（DMG、`.dmg.sha256`、`SHA256SUMS`）。

## 硬约束

- 不要用 `make-app.sh` / `make-dmg.sh` 覆盖签名后的发布产物；它们只做本地测试包（`release.sh` 会在有 Developer ID 时调用 `make-app.sh`）。
- 不要重命名产物：应用内更新器按 `Kumu-<version>-<arch>.dmg` 匹配，并强制校验 `SHA256SUMS`；多架构时把校验行合并进同一个 `SHA256SUMS`。
- 不要移动或覆盖已有 tag / Release；已存在时先核对它指向的提交和附件。
- `--keep` 只用于排查；成功后让脚本清理临时目录。
- p12、私钥和密码不要提交进仓库，也不要贴进聊天记录。

## 常见失败

- **`stapler validate` 在刚 `staple` 后失败（CloudKit / base64 ticket）**：票据到 CDN 有延迟，脚本会对同一产物重试 5 次（每次间隔 20 秒）。不要重新编译或重新公证。
- **公证上传超时 / 只拿到 submission id**：网络问题常见。脚本会打印“对同一文件重新 submit”的命令；不要重新编译或重新配置已验证有效的凭据。
- **`No Account for Team`（仅云端回退路径）**：Team ID 是 `3BUD4GCZCQ`，不是证书名里的 `GK4M5T86KV`；再查 Xcode 登录状态。
- **公证 `Invalid` 或 Gatekeeper 拒绝**：看保留目录里的 `notarytool log`，修签名或 entitlements 后重跑；不要交付该包。
- **p12 导入后找不到身份**：确认 p12 里是 `Developer ID Application`（不是 Apple Development），中间证书是 `OU=G2`，且密码由 `KUMU_SIGNING_P12_PASSWORD` 提供。
- **`This operation can only be performed by the Account Holder`**：Developer ID 证书只能由 Apple Developer Program 的 Account Holder 创建；让 Account Holder 用你的 CSR 签，或转让角色。App Store Connect 的“账户持有人”角色不等同。
