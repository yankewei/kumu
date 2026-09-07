# kumu

## 是什么

macOS 本地 coding agent 桌面壳：一个窗口管理多个本地 agent 的会话（项目 → 会话树 → 对话）。基于 GPUI（gpui-kit）+ Agent Client Protocol，会话与消息存在本地 SQLite（`~/.kumu/kumu.db`）。

目前是能力演示：Home（会话树 + 对话）和设置（服务商/通用、深色外观）两个窗口；ACP 客户端层已实现，尚未接入 UI。

## 怎么安装

需要 Rust（`cargo`）和一个本地 agent 二进制（默认 `omp`，可选 `opencode`）。

```bash
git clone git@github.com:yankewei/kumu.git
cd kumu
cargo run                        # 默认使用 omp
KUMU_AGENT=opencode cargo run    # 或使用 opencode
```

- `OMP_BIN` / `OPENCODE_BIN`：agent 不在 PATH 时指定二进制路径。
- opencode 需要 `OPENCODE_API_KEY`（env 优先，否则自动读取 `~/.local/share/opencode/auth.json`）；omp 无需配置。
- `KUMU_DEBUG=1 cargo run`：把 ACP 事件打到 stderr，用于无界面验证。
