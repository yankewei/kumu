use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::diff::{
    Diff, DiffAnnotation, DiffFile, DiffLinePosition, DiffLineRange, DiffParseOptions, DiffState,
};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::review::{
    DraftComment, PatchKey, PatchSheet, PatchSource, ReviewComment, ReviewStore, SendNotice,
    SentReview, comment_location, compose_comments, patch_digest,
};
use super::{REFRESH_INTERVAL, empty};
use crate::plugins::{PaneTarget, PluginHost, SendOutcome};

/// The sheet body: one patch. The read and the patch parse run on the
/// background executor; the diff state is built on the UI thread once the files
/// are in hand, because it prepares syntax off-thread itself.
pub(super) struct PatchView {
    /// The project whose pane receives the review comments.
    project: i64,
    /// How this sheet reaches that pane.
    host: PluginHost,
    /// What this patch is, for finding its drafts again.
    key: PatchKey,
    /// The panel's shared review memory.
    reviews: Rc<RefCell<ReviewStore>>,
    /// Tells the panel a row marker changed while the sheet was open.
    notify_panel: Rc<dyn Fn(&mut App)>,
    /// Where the patch came from, so a stale sheet can be read again.
    source: PatchSource,
    /// Review comments, each anchored to the patch lines it discusses.
    pub(super) comments: Vec<ReviewComment>,
    /// Distinguishes comment element ids after a removal and a new comment.
    pub(super) next_comment: u64,
    /// The pane the user chose, or `None` for the project's focused pane.
    pub(super) target: Option<u64>,
    /// A digest of what the sheet read, compared by the watcher to notice the
    /// repository moving under the review.
    digest: Option<String>,
    /// The patch changed since it was read; the sheet keeps showing the old
    /// text until the user asks for the new one.
    pub(super) stale: bool,
    /// A read is running; a watcher tick does not start a second one.
    reading: bool,
    /// Drops a read that was replaced by a later one.
    read_generation: u64,
    /// What the patch belongs to, so the agent knows which line numbers the
    /// comments refer to.
    context: String,
    /// The last review this patch sent, shown until a new comment is written.
    sent: Option<SentReview>,
    /// Why the last send did not go through, shown beside the send button.
    notice: Option<SendNotice>,
    /// The commit message, shown above the patch; empty for a change.
    message: String,
    pub(super) state: Option<Entity<DiffState>>,
    pub(super) truncated: bool,
    pub(super) error: Option<String>,
}

impl PatchView {
    pub(super) fn new(sheet: PatchSheet, cx: &mut Context<Self>) -> Self {
        let PatchSheet {
            project,
            key,
            source,
            message,
            context,
            host,
            reviews,
            notify_panel,
        } = sheet;
        let targets = host.panes(project, cx);
        let target = targets
            .iter()
            .find(|target| target.focused)
            .or_else(|| targets.first())
            .map(|target| target.pane);
        let drafts = reviews
            .borrow()
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let sent = reviews.borrow().sent.get(&key).cloned();
        let mut comments = Vec::with_capacity(drafts.len());
        for (index, draft) in drafts.into_iter().enumerate() {
            comments.push(ReviewComment {
                index: index as u64,
                range: draft.range,
                text: draft.text,
                input: None,
            });
        }
        let next_comment = comments.len() as u64;
        let mut view = Self {
            project,
            host,
            key,
            reviews,
            notify_panel,
            source,
            comments,
            next_comment,
            target,
            digest: None,
            stale: false,
            reading: false,
            read_generation: 0,
            context,
            sent,
            notice: None,
            message,
            state: None,
            truncated: false,
            error: None,
        };
        view.reload(cx);
        // Commits addressed by full hash are immutable; only working-tree
        // changes need background staleness polling.
        if matches!(view.source, PatchSource::Change { .. }) {
            view.watch(cx);
        }
        view
    }

    /// Read the patch again, replacing what the sheet shows. Drafts stay in
    /// their editors; a line that moved keeps the comment where it was written.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.reading {
            return;
        }
        self.reading = true;
        self.read_generation += 1;
        let generation = self.read_generation;
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    let patch = source.read()?;
                    // A cut patch ends in the middle of a hunk, which the
                    // parser takes when it is told to count the lines present.
                    let files = DiffFile::parse_with(
                        &patch.patch,
                        DiffParseOptions::new().truncated_hunks(patch.truncated),
                    )
                    .map_err(|error| format!("无法解析补丁：{error}"))?;
                    Ok::<_, String>((patch, files))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.read_generation != generation {
                    return;
                }
                view.reading = false;
                match read {
                    Ok((patch, files)) => {
                        let state = cx.new(|cx| DiffState::new(files, cx));
                        // Scrolling and selection notify the state; this view
                        // rebuilds the element so the new frame is painted.
                        cx.observe(&state, |_, _, cx| cx.notify()).detach();
                        view.truncated = patch.truncated;
                        view.digest = Some(patch_digest(&patch));
                        view.stale = false;
                        view.error = None;
                        view.state = Some(state);
                    }
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Notice a patch that changed since the sheet read it. The watcher never
    /// replaces the diff by itself: yanking the text while someone writes a
    /// comment is worse than an outdated frame with a refresh button.
    fn watch(&self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                // Skip polling while a read is running or once the sheet is
                // already marked stale.
                let generation = match this.update(cx, |view, _| {
                    (!view.reading && !view.stale).then_some(view.read_generation)
                }) {
                    Ok(Some(generation)) => generation,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let source = source.clone();
                let digest = cx
                    .background_spawn(
                        async move { source.read().map(|patch| patch_digest(&patch)) },
                    )
                    .await;
                let Ok(digest) = digest else {
                    continue;
                };
                let current = this.update(cx, |view, cx| {
                    if view.read_generation == generation
                        && !view.reading
                        && !view.stale
                        && view.digest.as_ref() != Some(&digest)
                    {
                        view.stale = true;
                        cx.notify();
                    }
                });
                if current.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Give every restored comment its editor. Restored text cannot be put in
    /// a textarea before the sheet has a window, so the first paint is when
    /// they come back.
    pub(super) fn ensure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing: Vec<(usize, u64, String)> = self
            .comments
            .iter()
            .enumerate()
            .filter(|(_, comment)| comment.input.is_none())
            .map(|(position, comment)| (position, comment.index, comment.text.clone()))
            .collect();
        for (position, index, text) in missing {
            let input = self.create_editor(&text, window, cx);
            self.listen(index, &input, cx);
            self.comments[position].input = Some(input);
        }
    }

    fn create_editor(
        &self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let input = cx.new(|cx| {
            TextareaState::new(&mut *window, cx)
                .placeholder("写下对这个改动片段的意见")
                .auto_grow(1, 6)
        });
        if !text.is_empty() {
            input.update(cx, |state, cx| {
                state.set_value(text.to_string(), window, cx)
            });
        }
        input
    }

    /// Follow one editor's changes into the comment and the panel's drafts.
    fn listen(&self, index: u64, input: &Entity<TextareaState>, cx: &mut Context<Self>) {
        cx.subscribe(input, move |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                view.sync_comment(index, cx);
            }
        })
        .detach();
    }

    fn sync_comment(&mut self, index: u64, cx: &mut Context<Self>) {
        let text = {
            let Some(comment) = self.comments.iter().find(|comment| comment.index == index) else {
                return;
            };
            let Some(input) = comment.input.as_ref() else {
                return;
            };
            input.read(cx).value().to_string()
        };
        if let Some(comment) = self
            .comments
            .iter_mut()
            .find(|comment| comment.index == index)
        {
            comment.text = text;
        }
        // A notice describes the previous text, so editing makes it stale.
        self.notice = None;
        self.save_drafts(cx);
        cx.notify();
    }

    /// Write what the sheet holds into the panel's memory. A comment the user
    /// emptied stops being a draft; the editor stays until the sheet closes.
    pub(super) fn save_drafts(&self, cx: &mut Context<Self>) {
        let drafts = self
            .comments
            .iter()
            .filter_map(|comment| {
                let text = comment.text.trim();
                (!text.is_empty()).then(|| DraftComment {
                    range: comment.range.clone(),
                    text: text.to_string(),
                })
            })
            .collect::<Vec<_>>();
        let changed = {
            let mut store = self.reviews.borrow_mut();
            let had_draft = store.drafts.contains_key(&self.key);
            if drafts.is_empty() {
                store.drafts.remove(&self.key);
            } else {
                store.drafts.insert(self.key.clone(), drafts);
            }
            had_draft != store.drafts.contains_key(&self.key)
        };
        if changed {
            (self.notify_panel)(cx);
        }
    }

    /// Anchor a comment beneath `range`, or focus the comment already there.
    pub(super) fn add_comment(
        &mut self,
        range: DiffLineRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(position) = self
            .comments
            .iter()
            .position(|comment| comment.range == range)
        {
            self.ensure_editors(window, cx);
            if let Some(input) = self.comments[position].input.clone() {
                let focus = input.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            return;
        }
        let index = self.next_comment;
        self.next_comment += 1;
        let input = self.create_editor("", window, cx);
        self.listen(index, &input, cx);
        self.notice = None;
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.comments.push(ReviewComment {
            index,
            range,
            text: String::new(),
            input: Some(input),
        });
        cx.notify();
    }

    fn remove_comment(&mut self, index: u64, cx: &mut Context<Self>) {
        let before = self.comments.len();
        self.comments.retain(|comment| comment.index != index);
        if self.comments.len() != before {
            self.notice = None;
            self.save_drafts(cx);
            cx.notify();
        }
    }

    /// The pane this sheet would send to: the chosen one when it still exists,
    /// otherwise the project's focused pane.
    fn active_target<'a>(&self, targets: &'a [PaneTarget]) -> Option<&'a PaneTarget> {
        self.target
            .and_then(|pane| targets.iter().find(|target| target.pane == pane))
            .or_else(|| targets.iter().find(|target| target.focused))
            .or_else(|| targets.first())
    }

    pub(super) fn set_target(&mut self, pane: u64, cx: &mut Context<Self>) {
        if self.target != Some(pane) {
            self.target = Some(pane);
            cx.notify();
        }
    }

    /// Hand every non-empty comment to the chosen terminal as one instruction.
    /// Returns whether the terminal took it, which is when the sheet can close.
    pub(super) fn send_comments(&mut self, cx: &mut Context<Self>) -> bool {
        let comments = self
            .comments
            .iter()
            .filter_map(|comment| {
                // The editor is the source of truth; its Change event is what
                // usually lands in `text`, but a programmatic value may not
                // emit one.
                let text = match comment.input.as_ref() {
                    Some(input) => input.read(cx).value().to_string(),
                    None => comment.text.clone(),
                };
                let text = text.trim().to_string();
                (!text.is_empty()).then(|| (comment.range.clone(), text))
            })
            .collect::<Vec<_>>();
        if comments.is_empty() {
            self.notice = Some(SendNotice::Hint("先写下评论再发送".to_string()));
            cx.notify();
            return false;
        }
        let count = comments.len();
        let message = compose_comments(&self.context, &comments);
        let targets = self.host.panes(self.project, cx);
        let target = self.active_target(&targets).cloned();
        let outcome = match &target {
            Some(target) => self
                .host
                .send_to_pane(self.project, target.pane, &message, cx),
            None => self.host.send_to_focused_pane(self.project, &message, cx),
        };
        match outcome {
            Ok(SendOutcome::Submitted) => {
                let record = SentReview {
                    count,
                    target: target
                        .map(|target| target.label())
                        .unwrap_or_else(|| "当前窗格".to_string()),
                };
                {
                    let mut store = self.reviews.borrow_mut();
                    store.drafts.remove(&self.key);
                    store.sent.insert(self.key.clone(), record.clone());
                }
                self.sent = Some(record);
                (self.notify_panel)(cx);
                self.comments.clear();
                self.notice = None;
                cx.notify();
                true
            }
            // The notes sit on a shell's command line. Keeping the editors
            // lets the user resend them after focusing an agent pane.
            Ok(SendOutcome::Pasted) => {
                let pane = target
                    .map(|target| target.label())
                    .unwrap_or_else(|| "当前窗格".to_string());
                self.notice = Some(SendNotice::Pasted(pane));
                cx.notify();
                false
            }
            Err(problem) => {
                self.notice = Some(SendNotice::Hint(problem));
                cx.notify();
                false
            }
        }
    }

    /// The footer's one line: what to do next, or why the last send stopped.
    fn footer_hint(&self, theme: &Theme) -> (String, Hsla) {
        match &self.notice {
            Some(SendNotice::Pasted(pane)) => {
                (format!("已粘贴到 {pane}，按 Enter 提交"), theme.warning)
            }
            Some(SendNotice::Hint(problem)) => (problem.clone(), theme.danger),
            None if self.comments.is_empty() => match &self.sent {
                Some(sent) => (
                    format!("上次已发送 {} 条评论到 {}", sent.count, sent.target),
                    theme.muted_foreground,
                ),
                None => (
                    "悬停 diff 行，点 + 添加评论".to_string(),
                    theme.muted_foreground,
                ),
            },
            None => (
                format!("{} 条评论", self.comments.len()),
                theme.muted_foreground,
            ),
        }
    }
}

impl Render for PatchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_editors(window, cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let message = self.message.trim_end();
        let body: AnyElement = match (&self.error, &self.state) {
            (Some(error), _) => empty(error, theme.danger),
            (None, Some(state)) if state.read(cx).files().is_empty() => {
                empty("没有内容变更", muted)
            }
            (None, Some(state)) => {
                let comments = Rc::new(
                    self.comments
                        .iter()
                        .filter_map(|comment| {
                            Some((
                                comment.annotation_id(),
                                comment.index,
                                comment.range.clone(),
                                comment.input.clone()?,
                            ))
                        })
                        .collect::<Vec<_>>(),
                );
                let view = cx.entity().downgrade();
                let annotations = comments.iter().map(|(id, _, range, _)| {
                    DiffAnnotation::line(
                        id.clone(),
                        DiffLinePosition::new(range.path().clone(), range.side(), range.start()),
                    )
                });
                let annotation_content = {
                    let comments = comments.clone();
                    let view = view.clone();
                    move |annotation: &DiffAnnotation, _window: &mut Window, cx: &mut App| {
                        let Some((_, index, range, input)) =
                            comments.iter().find(|(id, _, _, _)| id == annotation.id())
                        else {
                            return div().into_any_element();
                        };
                        render_comment(*index, range.clone(), input.clone(), view.clone(), cx)
                    }
                };
                let add_annotation = {
                    let view = view.clone();
                    move |range: &DiffLineRange, window: &mut Window, cx: &mut App| {
                        let range = range.clone();
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |this, cx| this.add_comment(range, window, cx));
                        }
                    }
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        Diff::new(state)
                            .annotations(annotations)
                            .render_annotation(annotation_content)
                            .on_add_annotation(add_annotation)
                            .size_full(),
                    )
                    .into_any_element()
            }
            (None, None) => empty("正在读取补丁…", muted),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .text_color(theme.sidebar_foreground)
            // The commit message lives here now that rows no longer open a
            // hover card, which would have been drawn over this sheet.
            .when(!message.is_empty(), |this| {
                this.child(
                    div()
                        .id("commit-diff-message")
                        .w_full()
                        .max_h(rems(12.))
                        .overflow_y_scroll()
                        .pb_2()
                        .text_sm()
                        .text_color(muted)
                        .child(message.to_string()),
                )
            })
            .when(self.truncated, |this| {
                this.child(
                    div()
                        .pb_2()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("补丁过大，只显示了前面的一部分"),
                )
            })
            .when(self.stale, |this| {
                let view = cx.entity().downgrade();
                this.child(
                    h_flex()
                        .w_full()
                        .pb_2()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.warning)
                                .child("补丁已经变化，当前显示的是旧内容"),
                        )
                        .child(
                            Button::new("git-diff-refresh")
                                .small()
                                .ghost()
                                .label("重新读取")
                                .on_click(move |_, _, cx| {
                                    if let Some(view) = view.upgrade() {
                                        view.update(cx, |view, cx| view.reload(cx));
                                    }
                                }),
                        ),
                )
            })
            .child(body)
    }
}

/// The sheet footer. It lives outside the body's padding, so it is its own
/// view and follows the patch it belongs to.
pub(super) struct PatchFooter {
    patch: Entity<PatchView>,
}

impl PatchFooter {
    pub(super) fn new(patch: Entity<PatchView>, cx: &mut Context<Self>) -> Self {
        cx.observe(&patch, |_, _, cx| cx.notify()).detach();
        Self { patch }
    }
}

impl Render for PatchFooter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let patch = self.patch.clone();
        let (count, (hint, color), project, host, target) = {
            let view = patch.read(cx);
            (
                view.comments.len(),
                view.footer_hint(theme),
                view.project,
                view.host.clone(),
                view.target,
            )
        };
        let targets = host.panes(project, cx);
        let active = target
            .and_then(|pane| targets.iter().find(|target| target.pane == pane))
            .or_else(|| targets.iter().find(|target| target.focused))
            .or_else(|| targets.first())
            .map(|target| (target.pane, target.label()));
        let target_button = active.as_ref().map(|(active_pane, label)| {
            let active_pane = *active_pane;
            let label = label.clone();
            let targets = targets.clone();
            let patch = patch.downgrade();
            Button::new("git-diff-target")
                .small()
                .ghost()
                .icon(Icon::new(IconName::ChevronDown).xsmall())
                .label(label)
                .tooltip("选择接收审查意见的窗格")
                .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                    let mut menu = menu;
                    for target in &targets {
                        let patch = patch.clone();
                        let pane = target.pane;
                        menu = menu.item(
                            PopupMenuItem::new(target.label())
                                .checked(pane == active_pane)
                                .on_click(move |_, _, cx| {
                                    let _ = patch.update(cx, |view, cx| view.set_target(pane, cx));
                                }),
                        );
                    }
                    menu
                })
        });
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(hint),
            )
            .when(count > 0, |this| this.children(target_button))
            .when(count > 0, |this| {
                this.child(
                    Button::new("git-diff-send")
                        .small()
                        .primary()
                        .label("发送")
                        .on_click({
                            let patch = patch.clone();
                            move |_, window, cx| {
                                if patch.update(cx, |patch, cx| patch.send_comments(cx)) {
                                    window.close_sheet(cx);
                                }
                            }
                        }),
                )
            })
    }
}

/// One comment's editor, drawn beneath the line it targets.
fn render_comment(
    index: u64,
    range: DiffLineRange,
    input: Entity<TextareaState>,
    view: WeakEntity<PatchView>,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let delete = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
        if let Some(view) = view.upgrade() {
            view.update(cx, |this, cx| this.remove_comment(index, cx));
        }
    };
    v_flex()
        .w_full()
        .gap_1()
        .p_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .font_medium()
                        .text_color(muted)
                        .child(comment_location(&range)),
                )
                .child(
                    Button::new(ElementId::named_usize(
                        "git-diff-comment-delete",
                        index as usize,
                    ))
                    .compact()
                    .ghost()
                    .icon(Icon::new(IconName::Close).xsmall())
                    .tooltip("删除评论")
                    .on_click(delete),
                ),
        )
        .child(Textarea::new(&input).appearance(false).bordered(false))
        .into_any_element()
}
