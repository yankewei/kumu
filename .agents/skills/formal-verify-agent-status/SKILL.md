---
name: formal-verify-agent-status
description: "Formally verify Kumu's dual-transport agent status arbitration and state machine (OSC 7501 vs OSC 777 Integration/Hooks, shell process watcher, and unseen attention badges) via exhaustive BFS state-space exploration over the real Rust decision functions in src/ui/home/agents.rs. Use whenever changing agent status tracking, OSC 7501/777 handling, shell agent reconciliation, or unseen markers."
---

# Formal verification: Agent status arbitration

Kumu receives asynchronous status signals for the same terminal pane from three sources: OSC 7501 (`note_program_status`), OSC 777 hooks/extensions (`note_agent_status`), and the background shell process watcher (`reconcile_shell_agents`), interleaved with user acknowledgements (`acknowledge_pane`). Unit tests check individual helpers in isolation; this verifier exhaustively explores **every reachable state and action interleaving** via BFS directly against the real Rust functions in `src/ui/home/agents.rs`.

## Tool choice

- **Tool:** Rust exhaustive BFS state-space explorer in [`src/ui/home/agents_formal_verify.rs`](../../../src/ui/home/agents_formal_verify.rs) (`cargo test --bin kumu ui::home::agents::formal_verify`).
- **Why:** Neither Java/TLC nor Kani is installed in this environment, whereas every Kumu developer already has `cargo` / `rustc`. Running BFS directly in Rust calls the real functions in `src/ui/home/agents.rs` (`resolve_program_status_agent`, `should_activate_osc7501`, `should_remove_shell_entry_on_clear`, `accepts_status`, `should_suppress_hook_attention`, `osc7501_status_signal`, `unseen_after_status`, `AgentEntry::switch_agent`), eliminating model-to-code drift on those decision functions and completing in under 1 second.

## Coverage & sync commit

- **Sync commit:** `8147d57667e17ff139b2d9a6d926ccb34a07ec0b`
- **Covered files and functions:**
  - `src/ui/home/agents.rs`: `AgentEntry`, `resolve_program_status_agent`, `should_activate_osc7501`, `should_remove_shell_entry_on_clear`, `accepts_status`, `should_suppress_hook_attention`, `osc7501_status_signal`, `unseen_after_status`, `AgentEntry::switch_agent`, and the state-transition logic of `KumuApp::note_agent_status`, `KumuApp::note_program_status`, `KumuApp::reconcile_shell_agents`, `KumuApp::acknowledge_pane`.
  - `src/agent/registry.rs`: `id_for`, `prefers_integration_status`.
  - `src/agent/status.rs`: `AgentState`, `StatusSignal`, `StatusSource`.
- **When a change touches covered code:** Check `git log 8147d57667e17ff139b2d9a6d926ccb34a07ec0b..HEAD -- src/ui/home/agents.rs src/agent/registry.rs src/agent/status.rs`. If any covered function changed, re-run the check, update `step()` in `agents_formal_verify.rs` if the control flow in `KumuApp::note_*` changed intentionally, and advance the sync commit above **only** after confirming the model still matches the code at the new commit.

## Properties verified

| ID | Property (plain words) | Harm if broken |
|---|---|---|
| **I1** | `unseen` is never `true` when `state` is `None` (idle) or `Some(Running)`. | Sidebar/tab shows a stuck unread dot for an idle or actively running agent. |
| **I2** | An OSC 7501 `Clear` or anonymous (`app == ""`) report never flips `osc7501_active` from `false` to `true`. | A nested tool or empty clear silently disables Hook updates for the rest of the pane's lifetime. |
| **I3** | Single-source authority: once the preferred transport is active (`OSC 7501` for hook agents like `claude`/`codex`; `Integration` for extension agents like `opencode`), the secondary transport cannot overwrite `state` or `unseen`, and hook alert suppression matches authority. | Status ping-pongs between two transports or fires duplicate macOS notifications. |
| **I4** | On a shell pane, an untracked (`process_tracked == false`) `osc7501_active` entry is removed on matching OSC 7501 `Clear`; launched or process-tracked entries are never removed by `Clear`. | Exited SSH/tmux agents stay pinned as zombie idle rows in the sidebar, or live launched agents vanish. |
| **I5** | On a launched agent pane (`launched = Some(L)`), `entry.agent` always remains `L` regardless of OSC 7501 or OSC 777 reports from other agents. | A nested agent (e.g. `codex` run inside a `claude` pane) hijacks the pane's identity and wipes its `osc7501_active` authority. |
| **I6** | Repeating an already-acknowledged actionable state (`Waiting`/`Completed`/`Failed`) never resurrects `unseen = true`, even while unattended. | Dismissed badges keep coming back on repeated hook/OSC ticks. |

## Scope, bounds, and assumptions

- **Bounds:** Single pane explored across 3 configurations (`Launched(Claude)`, `Launched(OpenCode)`, `Shell`), 3 representative agents (`claude`, `codex`, `opencode`) plus non-agent tool `"cargo"` and empty `""`, all 6 `ProgramState` values, all 7 `StatusSignal` values, both `attended` booleans, `ProcessWatcher` detection, and `Acknowledge`.
- **Out of scope:** Multi-pane interactions, wall-clock 2-second alert deduplication (`OSC7501_ALERT_DEDUPE`), title priority (`session_title` / `prompt_title` / `osc_title`), and GPUI view repaint scheduling (`sync_pane_agent_status`).

## How to run

```sh
# Ensure ghostty submodule is initialized in fresh worktrees
[ -f vendor/ghostty/build.zig ] || git submodule update --init --depth 1 vendor/ghostty

# Run core invariants (I1, I2, I3, I4, I6)
cargo test --bin kumu ui::home::agents::formal_verify::exhaustive_bfs_core_invariants_hold -- --nocapture

# Run all formal verification tests (including known finding I5)
cargo test --bin kumu ui::home::agents::formal_verify -- --include-ignored --nocapture
```

## Interpreting failures

1. **Read the shortest counterexample:** The output prints `init` and each `step N: Action -> ModelState` in the code's own enum names.
2. **Real bug vs. modeling drift:** Open `src/ui/home/agents.rs` and trace the counterexample steps through `note_agent_status`, `note_program_status`, `reconcile_shell_agents`, and `acknowledge_pane`.
   - If the real code follows the exact same path and produces the bad state, it is a **real bug** — fix the product code or report the finding without weakening the invariant.
   - If `KumuApp::note_*` was intentionally changed and `step()` in `agents_formal_verify.rs` no longer matches it, update `step()` to mirror the new code, re-run, and update the sync commit.

## Known findings at sync commit `8147d57667e17ff139b2d9a6d926ccb34a07ec0b`

- **I1, I2, I3, I4, I6:** Pass across all reachable states and transitions.
- **I5 (`exhaustive_bfs_i5_launched_agent_identity_preserved`):** Fails on the current code (reported as a finding; product code intentionally left untouched). When a launched `claude` pane receives an `OSC 777 Integration` status signal reporting `agent = "codex"` (e.g. a nested agent or stray hook), `note_agent_status` (`src/ui/home/agents.rs:396-415`) bypasses `accepts_status` because `existing.agent != resolved_agent` and `ensure_agent_entry` (`src/ui/home/agents.rs:337-354`) prioritizes `id_for(reported_agent)` over `launched`, calling `entry.switch_agent("codex")` and hijacking the launched pane (unlike `note_program_status`, which guards launched panes in `resolve_program_status_agent`).
