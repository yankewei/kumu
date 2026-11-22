# Kumu UI design notes

Kumu's business UI is built with Pure Rust NSH. Keep the existing compact desktop
layout and the 12px/14px text scale; use semantic theme tokens, accessible NSH
controls, and app-owned styled components from `src/nsh_ui/components.rs`.
The macOS title-bar shell and Ghostty terminal surface remain native GPUI.

## NSH control consistency pass

The sidebar, tabs, pane chrome, and Agent picker use shared NSH controls for
press targets, hover, selected state, focus, and accessible names. Feature code
continues to own layout, labels, and domain actions.

### Motion inventory

| State change | Motion | Reduced motion |
| --- | --- | --- |
| Shared button hover / selected colors | 120ms color transition from the control token | Immediate |
| Shared button pressed state | Immediate opacity feedback; no geometry shift | Immediate |
| Keyboard focus | Visible semantic ring; no movement | Unchanged |
| Project-row secondary actions | Remain subtly visible for keyboard discovery; opacity changes immediately on hover | Unchanged |
| Existing tab, project, and focused-Agent selection | Existing selected surface updates; no added layout animation | Unchanged |
| Existing popovers, dialogs, and sheets | Existing overlay behavior; unchanged in this pass | Existing behavior |
