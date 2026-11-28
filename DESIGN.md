# Design System: Kumu

Kumu is a compact native macOS workspace for running local AI coding agents in splitable terminal grids, paired with project tabs, a right-hand Git plugin drawer, and an integrated multi-file Git Diff review surface.

Kumu's business UI is built with Pure Rust NSH (`nsh-core` + `nsh-base` + `src/nsh_ui/components.rs`). The macOS title-bar shell and Ghostty terminal surface remain native GPUI.

## 1. Visual Theme & Atmosphere

Restrained, high-legibility developer workbench inspired by Zed and native macOS tooling. Surfaces rely on quiet slate/graphite neutrals (Dark default, with a warm paper Light counterpart), crisp `1px` structural hairlines, and compact desktop density. Hierarchy is created through font weight, muted vs primary foreground contrast, and subtle surface shifts rather than oversized typography or decorative drop shadows.

## 2. Color Palette & Roles

### Primary Foundation (Zed Dark default / Kumu Light)
- **Obsidian Canvas (`#22252b` dark / `#f9f9f8` light)**: Primary workspace, diff sheet, and panel background.
- **Slate Surface (`#262931` dark / `#fdfdfc` light)**: Elevated popovers, floating sheets, and inline review comment cards.
- **Muted Chrome (`#2f333d` dark / `#e9e9e6` light)**: Segmented controls, secondary buttons, collapsed separators, and neutral badges.
- **Sidebar Base (`#22252b` dark / `#f0f0ee` light)**: Project list and right-hand Git plugin sidebar background.
- **Hairline Border (`#30343d` dark / `#e4e4e1` light)**: `1px` structural dividers between panes, toolbars, file headers, and drawers.
- **Input Border (`#3f4450` dark / `#cfceca` light)**: Single-line and multiline text field outlines.

### Accent & Interactive
- **Cobalt Link (`#62adef` dark / `#5570e0` light)**: Primary actions, active tab/file focus, focus rings (`#62adef` / `#94a4f5`), and inline add-comment triggers.
- **Primary Foreground (`#0e1216` dark / `#1c1d21` light)**: High-contrast label text on primary filled buttons.
- **Accent Wash (`#2c313b` dark / `#e9e9e6` light)**: Hover and selected row fills for file lists, tabs, and ghost buttons.
- **Selection Tint (`#62adef40` dark / `#94a4f540` light)**: Selected code lines and text ranges.

### Typography & Text Hierarchy
- **Primary Ink (`#d7dadf` dark / `#403f44` light)**: Code lines, active titles, file names, and primary labels.
- **Muted Ink (`#7c828c` dark / `#8a8b90` light)**: Parent directory paths, line numbers, timestamps, secondary metadata, and placeholders.

### Functional States & Diff Semantics
- **Added Green (`#98c379` dark / `#7fae6a` light)**: Added files (`A`/`?`), `+N` line counts, `+` diff gutter signs, and translucent green added-line row/word tints.
- **Deleted Red (`#e06c75` dark / `#d1686f` light)**: Deleted files (`D`), `−N` line counts, `−` diff gutter signs, destructive actions, and translucent red deleted-line row/word tints.
- **Modified Amber (`#e5c07b` dark / `#d5ab60` light)**: Modified files (`M`), review drafts (`草稿`), stale patch banners, and merge indicators.
- **Renamed Cyan (`#56b6c2` dark / `#56b6c2` light)**: Renamed/copied files (`R`/`C`) and informational accents.

## 3. Typography Rules

Kumu enforces a strict two-step UI scale with zero oversized display headings:
- **System UI Sans (`.SystemUIFont` / Inter)**:
  - **Body & Section Titles (`14px` / `text_sm`)**: Sheet titles, section headings (`500` medium), commit messages, and empty states (`line-height: 1.4`).
  - **Secondary & Meta (`12px` / `text_xs`)**: File list rows, file headers (`500` medium for file name, `400` normal for parent dir), toolbar buttons, status pills, badges, and comment metadata (`line-height: 1.35`).
- **JetBrains Mono (`13px`, `line-height: 20px`)**:
  - Diff code lines, aligned tabular line numbers, `+`/`−` gutter signs, commit short hashes, hunk coordinates, and file:line comment anchors.

## 4. Component Stylings

- **Buttons (`radius: 6px`)**: Compact `24px`–`28px` height (`px-2.5 py-1`). Primary fill (`#62adef`, `#0e1216` text) reserved exclusively for decision commits (e.g. "发送" review comments). Internal navigation and view toggles use Ghost (`transparent` -> Accent Wash hover) or Outline (`1px` border).
- **Segmented Controls**: Muted Chrome pill container (`p-0.5`, `radius: 6px`) with Obsidian Canvas active segment (`font-medium`) for `合并 / 分栏` (Unified / Split) and `全部 / 已暂存 / 未暂存` scope switches.
- **Collapsible Diff File Header**: Full-width sticky/inline bar (`border-y-1`, Muted Chrome / Slate Surface tint) containing collapse chevron (`▾` / `▸`), single-letter status badge (`M`/`A`/`D`/`R`), bold file name + muted directory path, `+N −M` mono counters, and a compact "已查看" (Viewed) toggle checkbox that collapses the file when checked.
- **Artifacts Drawer & Reading Sheet**: Right panel (`300px`) grouped by agent session, with `44px` artifact rows, a kind badge (`MD` / `HTML` / `TXT`), relative time, and trailing reveal/copy/trash icon buttons. Clicking a row opens the same `1040px` right-docked reading sheet used by Diff review: Markdown uses native `TextView` typography, HTML renders in an embedded WebView below the sheet header.
- **Inline Review Annotation Card**: Embedded directly beneath the target diff line (`radius: 6px`, `1px` border, Slate Surface fill) with mono `path:line` anchor, delete icon button, and multiline comment input.
- **Inputs**: `1px` Input Border, Obsidian Canvas fill, `6px` radius, `12px` text, Cobalt Link focus ring.

## 5. Layout Principles

- **Desktop Workbench Standard (`1280px`–`1440px` viewport)**: `4px` / `8px` spatial grid, compact desktop density (`28px` file list rows, `32px` commit rows, `12px`–`16px` header/footer horizontal padding).
- **Combined Diff Review Sheet**: Right-docked wide sheet (`1040px` default, `max-w-[94%]`, expandable to `96%` full-width reading mode) over a dimmed backdrop (`rgba(0,0,0,0.35)`). Structured into:
  1. Top header + reading toolbar (`px-4 py-3`, `border-b`)
  2. Two-column body: collapsible left file navigation sidebar (`240px`–`260px` wide, `border-r`) beside the continuous scrollable multi-file Diff viewport
  3. Anchored bottom review bar (`px-4 py-2.5`, `border-t`) for review progress, target agent pane picker, and Send button.

## 6. Design System Notes for Stitch Generation

- **Atmosphere keywords**: Zed-inspired dark developer workbench, compact native macOS desktop app, Obsidian Canvas (`#22252b`), Slate Surface (`#262931`), `1px` structural hairlines (`#30343d`), Cobalt Link (`#62adef`) accents, JetBrains Mono code typography.
- **Component prompts**: Compact `26px` segmented controls for Unified/Split and All/Staged/Unstaged filters, collapsible multi-file diff headers with `+N -M` counters and Viewed checkboxes, left file tree sidebar with single-letter Git status badges, and inline code review comment cards anchored under diff lines.

---

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
