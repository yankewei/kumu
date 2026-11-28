use std::path::PathBuf;

use gpui::{AnyView, App, AppContext, Context, PathPromptOptions, Window, WindowBounds, px, size};

use super::KumuApp;
use crate::app::layout::PaneId;
use crate::git::WorktreeEntry;
use crate::nsh_ui::dialogs::{ActiveDialog, DialogOutcome, DialogSpec, WorktreeChoice};
use crate::theme::KumuTheme;
use crate::ui::SettingsWindow;

impl KumuApp {
    /// Open a full-window Pure Rust NSH modal dialog and clear it automatically
    /// when dismissed or submitted.
    pub(super) fn open_nsh_dialog(
        &mut self,
        spec: DialogSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_outcome: impl FnOnce(&mut Self, DialogOutcome, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let theme = KumuTheme::global(cx).clone();
        self.active_dialog = Some(ActiveDialog::open(
            spec,
            &theme,
            window,
            cx,
            move |app, outcome, window, cx| {
                app.active_dialog = None;
                on_outcome(app, outcome, window, cx);
            },
        ));
        cx.notify();
    }

    /// Add a project from a native folder picker; the picked folder's own
    /// name becomes the project name.
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
            self.refresh_project_branches(cx);
        }
    }

    /// Ask for a branch name and create a worktree next to the repository.
    pub(super) fn prompt_create_worktree(
        &mut self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        self.open_nsh_dialog(
            DialogSpec::TextInput {
                title: "新建 worktree".into(),
                description: Some(format!(
                    "将在「{}」的同级目录创建 worktree，并检出同名分支。",
                    project.path
                )),
                placeholder: "分支名，例如 feat/login".into(),
                initial_value: String::new(),
                ok_label: "创建".into(),
                cancel_label: "取消".into(),
                require_non_empty: true,
                select_all: false,
            },
            window,
            cx,
            move |app, outcome, window, cx| {
                if let DialogOutcome::SubmittedText(branch) = outcome
                    && !branch.is_empty()
                {
                    app.create_worktree(project_id, branch, cx);
                }
                app.focus_terminal(window, cx);
                cx.notify();
            },
        );
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
    /// to this project.
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

    /// Offer the worktrees git already knows about.
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
        let items: Vec<WorktreeChoice> = entries
            .into_iter()
            .filter(|entry| !entry.main)
            .map(|entry| {
                let open_project_id = self.workspace.read(cx).project_by_path(&entry.path);
                WorktreeChoice {
                    name: entry.name,
                    path: entry.path,
                    open_project_id,
                }
            })
            .collect();
        if items.is_empty() {
            self.open_nsh_dialog(
                DialogSpec::Message {
                    title: "打开已有 worktree".into(),
                    description: format!(
                        "「{}」没有其他 worktree。可在项目右键菜单中新建一个。",
                        project.name
                    ),
                    ok_label: "关闭".into(),
                },
                window,
                cx,
                |app, _, window, cx| {
                    app.focus_terminal(window, cx);
                    cx.notify();
                },
            );
            return;
        }

        self.open_nsh_dialog(
            DialogSpec::WorktreePicker {
                title: "打开已有 worktree".into(),
                description: format!(
                    "选择要加入「{}」的 worktree，它会作为子项目出现在侧栏。",
                    project.name
                ),
                items,
            },
            window,
            cx,
            move |app, outcome, window, cx| {
                if let DialogOutcome::PickedWorktree(choice) = outcome {
                    match choice.open_project_id {
                        Some(id) => {
                            app.ensure_project_layout(id, cx);
                            app.select_project(id, cx);
                        }
                        None => {
                            app.attach_worktree(project_id, choice.name, choice.path, cx);
                        }
                    }
                }
                app.focus_terminal(window, cx);
                cx.notify();
            },
        );
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

    /// Rename a project via Pure Rust NSH modal dialog.
    pub(super) fn prompt_rename_project(
        &mut self,
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
        self.open_nsh_dialog(
            DialogSpec::TextInput {
                title: "重命名项目".into(),
                description: None,
                placeholder: "项目名称".into(),
                initial_value: name,
                ok_label: "保存".into(),
                cancel_label: "取消".into(),
                require_non_empty: true,
                select_all: true,
            },
            window,
            cx,
            move |app, outcome, window, cx| {
                if let DialogOutcome::SubmittedText(next_name) = outcome
                    && !next_name.is_empty()
                {
                    app.workspace.update(cx, |workspace, cx| {
                        workspace.rename_project(project_id, next_name, cx)
                    });
                }
                app.focus_terminal(window, cx);
                cx.notify();
            },
        );
    }

    /// Ask before deleting a project and closing its terminals.
    pub(super) fn confirm_delete_project(
        &mut self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let name = project.name;
        let (title, description, ok_label) = if project.parent_id.is_some() {
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
        self.open_nsh_dialog(
            DialogSpec::Confirm {
                title: title.into(),
                description,
                ok_label: ok_label.into(),
                cancel_label: "取消".into(),
                destructive: true,
            },
            window,
            cx,
            move |app, outcome, window, cx| {
                if outcome == DialogOutcome::Confirmed {
                    app.delete_project(project_id, cx);
                }
                app.focus_terminal(window, cx);
                cx.notify();
            },
        );
    }

    /// Inspect a linked worktree for uncommitted changes, ask for confirmation,
    /// then remove its directory via `git worktree remove` and drop it from Kumu.
    pub(super) fn confirm_remove_worktree(
        &mut self,
        project_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let path = PathBuf::from(&project.path);
        let name = project.name.clone();
        cx.spawn_in(window, async move |this, cx| {
            let dirty_count = cx
                .background_executor()
                .spawn(async move { crate::git::changes(&path).map(|c| c.len()).unwrap_or(0) })
                .await;
            let _ = this.update_in(cx, |app, window, cx| {
                let force = dirty_count > 0;
                let description = if force {
                    format!(
                        "worktree「{name}」存在 {dirty_count} 处未提交改动！\n强制删除将永久清除磁盘目录「{}」及其未提交改动，并关闭关联终端。",
                        project.path
                    )
                } else {
                    format!(
                        "将通过 git worktree remove 删除磁盘目录「{}」，并关闭「{name}」内的所有终端。",
                        project.path
                    )
                };
                let ok_label = if force { "强制删除" } else { "删除 worktree" };
                let remove_path = PathBuf::from(&project.path);
                app.open_nsh_dialog(
                    DialogSpec::Confirm {
                        title: format!("删除 worktree「{name}」"),
                        description,
                        ok_label: ok_label.into(),
                        cancel_label: "取消".into(),
                        destructive: true,
                    },
                    window,
                    cx,
                    move |app, outcome, window, cx| {
                        if outcome == DialogOutcome::Confirmed {
                            app.execute_remove_worktree(project_id, remove_path, force, cx);
                        }
                        app.focus_terminal(window, cx);
                        cx.notify();
                    },
                );
            });
        })
        .detach();
    }

    fn execute_remove_worktree(
        &mut self,
        project_id: i64,
        path: PathBuf,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        let work = cx
            .background_executor()
            .spawn(async move { crate::git::remove_worktree(&path, force) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |app, cx| match result {
                Ok(()) => app.delete_project(project_id, cx),
                Err(message) => {
                    app.store_error = Some(format!("删除 worktree 失败：{message}"));
                    cx.notify();
                }
            });
        })
        .detach();
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
            return;
        }
        let selected_removed = self.selected.is_some_and(|id| removed.contains(&id));
        for id in &removed {
            if let Some(tabs) = self.tabs_by_project.remove(id) {
                for pane in tabs.panes() {
                    self.workspace
                        .update(cx, |workspace, _cx| workspace.drop_terminal(pane.0));
                    self.panes.remove(&pane);
                    self.native_registry.unregister_pane(pane.0);
                    self.launches.remove(&pane);
                    self.agent_status.remove(&pane);
                }
            }
        }
        if selected_removed {
            self.zoomed_pane = None;
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

    /// Focus the settings window if it's open, else open a new one.
    pub(super) fn open_settings(&mut self, show_updates: bool, cx: &mut Context<Self>) {
        let activate = |view: AnyView, window: &mut Window, cx: &mut App| {
            if show_updates && let Ok(settings) = view.downcast::<SettingsWindow>() {
                settings.update(cx, |settings, cx| settings.show_updates(window, cx));
            }
            window.activate_window();
        };
        if let Some(handle) = &self.settings_window
            && handle.update(cx, activate).is_ok()
        {
            return;
        }
        let mut options = crate::ui::titlebar::window_options();
        options.window_bounds = Some(WindowBounds::centered(None, size(px(680.), px(640.)), cx));
        options.window_min_size = Some(size(px(680.), px(640.)));
        let updater = self.updater.clone();
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            cx.new(|cx| SettingsWindow::new(updater, window, cx))
        }) {
            let handle: gpui::AnyWindowHandle = handle.into();
            handle.update(cx, activate).ok();
            self.settings_window = Some(handle);
        }
    }
}
