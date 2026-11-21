use std::path::PathBuf;

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use super::panes::PaneId;
use crate::git::WorktreeEntry;
use crate::ui::SettingsWindow;

impl KumuApp {
    /// Add a project from a native folder picker; the picked folder's own
    /// name becomes the project name, matching how Zed/waku open a project
    /// (no separate naming step). Picking a folder that's already a project
    /// selects it instead of inserting a duplicate row.
    pub(super) fn add_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择项目目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |app, cx| app.finish_add_project(path, cx));
        })
        .detach();
    }

    /// Show the picked project — whether it was just added or was one already.
    pub(super) fn finish_add_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = path.to_string_lossy().into_owned();
        let id = self
            .workspace
            .update(cx, |workspace, cx| workspace.add_project(&path, cx));
        if let Some(id) = id {
            self.ensure_project_layout(id, cx);
            self.select_project(id, cx);
        }
    }

    /// Ask for a branch name and create a worktree next to the repository.
    pub(super) fn prompt_create_worktree(
        &self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("分支名，例如 feat/login"));
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title("新建 worktree")
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "将在「{}」的同级目录创建 worktree，并检出同名分支。",
                            project.path
                        )),
                )
                .child(Input::new(&dialog_input).small())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("创建")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok({
                    let weak = weak.clone();
                    let input = dialog_input.clone();
                    move |_, _, cx| {
                        let value = input.read(cx).value().to_string();
                        let branch = value.trim().to_string();
                        if branch.is_empty() {
                            return false;
                        }
                        let _ = weak.update(cx, move |app, cx| {
                            app.create_worktree(project_id, branch, cx)
                        });
                        true
                    }
                })
                .centered()
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            cx.notify();
        });
    }

    /// Create the worktree off the UI thread, then add it as a project under
    /// its repository. Failures stay in the banner above the panes.
    pub(super) fn create_worktree(
        &mut self,
        project_id: i64,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .workspace
            .read(cx)
            .project(project_id)
            .map(|project| PathBuf::from(&project.path))
        else {
            return;
        };
        let work = cx
            .background_executor()
            .spawn(async move { crate::git::create_worktree(&path, &branch) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |app, cx| match result {
                Ok(worktree) => {
                    let path = worktree.path.to_string_lossy().into_owned();
                    let id = app.workspace.update(cx, |workspace, cx| {
                        workspace.add_worktree(project_id, &worktree.name, &path, cx)
                    });
                    if let Some(id) = id {
                        app.ensure_project_layout(id, cx);
                        app.select_project(id, cx);
                    }
                }
                Err(message) => {
                    app.store_error = Some(format!("创建 worktree 失败：{message}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// List the repository's existing worktrees and let the user attach one
    /// to this project. Listing runs off the UI thread because worktree
    /// metadata can live on a slow volume.
    pub(super) fn prompt_open_worktree(
        &self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let path = PathBuf::from(&project.path);
            let entries = cx
                .background_executor()
                .spawn(async move {
                    crate::git::list_worktrees(&path).map(|entries| {
                        entries
                            .into_iter()
                            .filter(|entry| !entry.main && entry.path.exists())
                            .collect::<Vec<_>>()
                    })
                })
                .await;
            let _ = this.update_in(cx, |app, window, cx| match entries {
                Ok(entries) => app.show_worktree_picker(project_id, entries, window, cx),
                Err(message) => {
                    app.store_error = Some(format!("读取 worktree 失败：{message}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Offer the worktrees git already knows about. A worktree Kumu shows
    /// already is labelled and selected instead of being added twice.
    pub(super) fn show_worktree_picker(
        &mut self,
        project_id: i64,
        entries: Vec<WorktreeEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let items: Vec<(String, PathBuf, Option<i64>)> = entries
            .into_iter()
            .filter(|entry| !entry.main)
            .map(|entry| {
                let open = self.workspace.read(cx).project_by_path(&entry.path);
                (entry.name, entry.path, open)
            })
            .collect();
        if items.is_empty() {
            window.open_dialog(cx, move |dialog, _, cx| {
                dialog
                    .title("打开已有 worktree")
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "「{}」没有其他 worktree。可在项目右键菜单中新建一个。",
                                project.name
                            )),
                    )
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("关闭")
                            .show_cancel(false),
                    )
                    .centered()
            });
            return;
        }

        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let (muted, hover, radius) = (theme.muted_foreground, theme.accent, theme.radius);
            let rows = items.iter().enumerate().map(|(index, (name, path, open))| {
                let shown = path.to_string_lossy().into_owned();
                let row_name = name.clone();
                let row_path = path.clone();
                let open = *open;
                let weak = weak.clone();
                v_flex()
                    .id(("open-worktree", index))
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_0p5()
                    .rounded(radius)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover.opacity(0.5)))
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |app, cx| match open {
                            Some(id) => {
                                app.ensure_project_layout(id, cx);
                                app.select_project(id, cx);
                            }
                            None => app.attach_worktree(
                                project_id,
                                row_name.clone(),
                                row_path.clone(),
                                cx,
                            ),
                        });
                        window.close_dialog(cx);
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_between()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_medium()
                                    .child(name.clone()),
                            )
                            .when_some(open, |row, _| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(muted)
                                        .child("已打开"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .child(shown),
                    )
            });
            dialog
                .title("打开已有 worktree")
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "选择要加入「{}」的 worktree，它会作为子项目出现在侧栏。",
                            project.name
                        )),
                )
                .child(
                    div()
                        .id("open-worktree-list")
                        .w_full()
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .child(v_flex().w_full().children(rows)),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("关闭")
                        .show_cancel(false),
                )
                .centered()
        });
    }

    /// Attach an existing worktree below its repository and show it.
    pub(super) fn attach_worktree(
        &mut self,
        parent_id: i64,
        name: String,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let path = path.to_string_lossy().into_owned();
        let id = self.workspace.update(cx, |workspace, cx| {
            workspace.open_worktree(parent_id, &name, &path, cx)
        });
        let Some(id) = id else {
            return;
        };
        self.ensure_project_layout(id, cx);
        self.select_project(id, cx);
    }

    /// Rename a project: the dialog starts on the current name.
    pub(super) fn prompt_rename_project(
        &self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .workspace
            .read(cx)
            .projects()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("重命名项目")
                .child(Input::new(&dialog_input).small())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("保存")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok({
                    let weak = weak.clone();
                    let input = dialog_input.clone();
                    move |_, _, cx| {
                        let name = input.read(cx).value().trim().to_string();
                        if !name.is_empty() {
                            let _ = weak.update(cx, move |app, cx| {
                                app.workspace.update(cx, |workspace, cx| {
                                    workspace.rename_project(project_id, name, cx)
                                })
                            });
                        }
                        true
                    }
                })
                .centered()
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// Ask before deleting a project and closing its terminals.
    pub(super) fn confirm_delete_project(
        &self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let name = project.name;
        // A worktree row is only a view of a directory; removing it must not
        // pretend the checkout on disk goes away.
        let (title, description, ok_text) = if project.parent_id.is_some() {
            (
                "移除 worktree",
                format!("「{name}」将从侧边栏移除，磁盘上的 worktree 不会被删除。"),
                "移除",
            )
        } else {
            (
                "删除项目",
                format!("「{name}」及其 worktree、终端将被关闭，此操作无法撤销。"),
                "删除",
            )
        };
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title(title)
                .description(description.clone())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text)
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, move |app, cx| app.delete_project(project_id, cx));
                    true
                })
                .centered()
        });
    }

    /// Delete a project and its worktrees, then show another project when
    /// one remains. Worktree directories stay on disk.
    pub(super) fn delete_project(&mut self, project_id: i64, cx: &mut Context<Self>) {
        let removed = self.workspace.read(cx).project_tree_ids(project_id);
        if self
            .workspace
            .update(cx, |workspace, cx| workspace.delete_project(project_id, cx))
            .is_err()
        {
            // The failure is already on its way to the banner; leave the
            // project and its panes exactly where they are.
            return;
        }
        let selected_removed = self.selected.is_some_and(|id| removed.contains(&id));
        for id in &removed {
            if let Some(tabs) = self.tabs_by_project.remove(id) {
                for pane in tabs.panes() {
                    self.workspace
                        .update(cx, |workspace, _| workspace.drop_terminal(pane.0));
                    self.panes.remove(&pane);
                    self.launches.remove(&pane);
                    self.agent_status.remove(&pane);
                }
            }
        }
        if self
            .pending_close_tab
            .is_some_and(|(project, _)| removed.contains(&project))
        {
            self.pending_close_tab = None;
        }
        if selected_removed {
            let next = self.workspace.read(cx).projects().first().map(|p| p.id);
            self.selected = next;
            if let Some(next) = next {
                self.select_project(next, cx);
                return;
            }
            self.focused = PaneId(1);
            self.pending_focus = None;
        }
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    pub(super) fn open_settings(&mut self, show_updates: bool, cx: &mut Context<Self>) {
        let activate = |view: AnyView, window: &mut Window, cx: &mut App| {
            if show_updates
                && let Ok(root) = view.downcast::<Root>()
                && let Ok(settings) = root.read(cx).view().clone().downcast::<SettingsWindow>()
            {
                settings.update(cx, |settings, cx| settings.show_updates(window, cx));
            }
            window.activate_window();
        };
        if let Some(handle) = &self.settings_window
            && handle.update(cx, activate).is_ok()
        {
            return;
        }
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(size(px(680.), px(640.)), cx));
        options.window_min_size = Some(size(px(680.), px(640.)));
        let updater = self.updater.clone();
        if let Ok((handle, _)) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| SettingsWindow::new(updater, window, cx))
        }) {
            handle.update(cx, activate).ok();
            self.settings_window = Some(handle);
        }
    }
}
