# kumu

A macOS desktop app idea: manage multiple local coding agents (ACP) in one UI.
Built with [GPUI](https://gpui-kit.com) (gpui-kit 0.6) + the official
[Agent Client Protocol Rust SDK](https://agentclientprotocol.com/libraries/rust).

> Status: **capability demo**, protocol v1, one agent per launch (default: Oh My
> Pi). Current screen: **Settings → Providers** — lists the model service
> providers the agent exposes over ACP, read-only (no credential editing).

## Current screen: Settings → Providers

Zed-settings look: system UI font, light surfaces, hairline dividers, periwinkle
accent, custom transparent title bar. Two-column settings layout (left: search +
settings nav; right: page rows with title/description + control):

```text
┌──────────────┬──────────────────────────────────┐
│ [搜索设置…]   │  服务商                           │
│ ▸ 服务商(选中) │  Opencode   SST Opencode · ACP v1 │
│   通用        │  ────────────────────────────────  │
│              │  Oh My Pi    omp · ACP v1    本地  │
└──────────────┴──────────────────────────────────┘
```

- **Look & feel** lives in `themes/*.json` ("Kumu Light" / "Zed Dark"), loaded
  into the `ThemeRegistry` at startup. App code only reads semantic tokens
  from `cx.theme()`; the UI font (`.SystemUIFont`) and radius are theme config too.
- **通用** page has a working 深色外观 switch that flips `Theme::change`
  (light ↔ dark) live.
`

- **Data source is ACP itself, not private config files.** The agent returns a
  `configOptions` array on `session/new`; the option with `category: "model"`
  is a select whose option values are `provider/model` ids. Grouping by the
  value prefix yields the provider list and per-provider model counts.
- Read-only: no credential editing (decided scope).
- Works for both agents: omp exposes `opencode-go` (36) + `rc` (4); opencode
  exposes `opencode-go` / `openai` / `rc-ai-gateway` / `kimi-for-coding`
  (53 total).

## Architecture

Module layout:

```
src/
  main.rs        entry: init gpui + themes, open the Home window
  ui/            GPUI views, one module per window
    home.rs      KumuApp: projects → sessions tree + conversation
    settings.rs  SettingsWindow: category nav + provider/general pages
    widgets.rs   theme-token helpers shared across windows (pill…)
  theme.rs       themes/*.json → ThemeRegistry (Kumu Light / Zed Dark)
  agents.rs      static registry of agents kumu can drive
  db.rs          local SQLite persistence (~/.kumu/kumu.db)
  acp.rs         ACP client layer (not wired into the UI yet)
```

ACP thread (when wired), on a dedicated tokio runtime inside src/acp.rs:
agent-client-protocol Client role → `<agent> acp` subprocess (stdio); on
session/new it parses the model config option and groups models by provider.

UI / ACP decoupling: ACP → UI via `std::sync::mpsc` (drained by a gpui poll
task); UI → ACP via a `tokio::sync::mpsc::UnboundedSender` (non-blocking send,
kept alive so the connection loop never sees its channel close).

## Run

```bash
cargo run                                  # omp (current default)
KUMU_AGENT=opencode cargo run              # opencode
```

- `omp` (Oh My Pi, default): `OMP_BIN` overrides the binary. Uses its own
  stored model config; no extra env needed.
- `opencode`: `OPENCODE_BIN` overrides the binary. Needs a reachable model;
  kumu injects `OPENCODE_API_KEY` (env first, else the RC-AI-Gateway key
  opencode already stored in `~/.local/share/opencode/auth.json`) so it works
  when launched from a GUI.

Headless verification:

```bash
KUMU_DEBUG=1 cargo run
```

Expected on stderr:

```
[acp] log: spawning: omp acp
[acp] log: initialized: agent=Some("oh-my-pi")
[acp] log: session ready: ses_...
[acp] providers: 40 models across 2 providers: [ProviderStat { id: "opencode-go", count: 36 }, ...]
```

## Verified findings

- **Opencode 1.18** and **Oh My Pi (omp) 18.1.13** both speak ACP v1
  (`initialize`/`session/new`/`session/prompt`) over stdio.
- **There is no "list all models" ACP method.** The protocol-standard way to
  learn a session's usable models is the `model` session config option
  (`category: "model"`) returned on `session/new` (and refreshed by
  `config_option_update`). That is what the Providers page renders.
- ACP has no provider-catalog protocol, which is why opencode/omp keep their
  full provider/model catalogs embedded in their binaries.
- Opencode's default model routing hit the built-in AI Gateway and failed
  (`AI Gateway Error`) until `OPENCODE_API_KEY` was injected — provider config
  uses `{env:OPENCODE_API_KEY}`. omp needs no injection.
- v1 `RequestPermissionRequest` has **no** `title`/`subject` (that's v2); it
  carries `tool_call` + `options`.

## Next steps (not yet implemented)

- Chat view + model switching via `session/set_config_option` (the
  ACP-standard "change model" path).
- Multiple sessions on one agent (one `session/new` per project `cwd`).
- Multiple agents per app process (one connection per agent).
- Permission UI instead of auto-approve; plan / tool-call / usage rendering.
- Protocol v2 behind a feature flag.
