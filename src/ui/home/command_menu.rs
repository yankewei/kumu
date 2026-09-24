//! The composer's slash-command menu: what the session's agent says it will run,
//! filtered by what has been typed so far.
//!
//! It sits directly above the input and highlights the row Enter would complete
//! to — the first match, or whichever one Up/Down moved to. That is the whole of
//! its behaviour: a command is an ordinary message once it is typed (`/name
//! args` — every agent kumu drives recognizes the prefix itself), so nothing
//! here sends anything. It exists to answer "what can I type?" before the user
//! has to remember it.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use crate::agent;
use crate::db::Session;
use crate::ui::widgets::{PickerChrome, select_row, tracked_picker_menu};

/// The commands a partly typed `/…` could mean, in the agent's own order.
///
/// The input has to *be* a bare command: a leading `/` with nothing but the name
/// so far. Once a space is typed the rest is arguments, and `/usr/bin` is a path
/// rather than a command — neither opens the menu, and the text is sent as the
/// message it looks like.
///
/// The prefix decides, case-insensitively, because that is what tells one
/// command from another; the agent's order is kept for everything that matches,
/// so the list does not reshuffle under a keystroke.
pub(super) fn matching_commands<'a>(
    commands: &'a [agent::CommandInfo],
    input: &str,
) -> Vec<&'a agent::CommandInfo> {
    let typed = match input.strip_prefix('/') {
        Some(typed) if !typed.contains(char::is_whitespace) => typed.to_lowercase(),
        _ => return Vec::new(),
    };
    commands
        .iter()
        .filter(|command| command.name.to_lowercase().starts_with(&typed))
        .collect()
}

/// The menu open above the input, with the rows it lists: a file reference's,
/// or the slash-command menu — never both.
///
/// Decided once, in [`KumuApp::open_menu`], and read by everything that acts on
/// it — the render, Enter and Up/Down — so they cannot disagree about which
/// menu is on screen.
pub(super) enum OpenMenu {
    Mention(Vec<String>),
    Command(Vec<agent::CommandInfo>),
}

impl OpenMenu {
    fn len(&self) -> usize {
        match self {
            Self::Mention(entries) => entries.len(),
            Self::Command(commands) => commands.len(),
        }
    }
}

impl KumuApp {
    /// The commands the session can run (see
    /// [`crate::app::Workspace::session_commands`]).
    pub(super) fn session_commands(&self, session: &Session, cx: &App) -> Vec<agent::CommandInfo> {
        self.workspace.read(cx).session_commands(session).to_vec()
    }

    /// The menu open above the input, if any.
    ///
    /// A file reference at the caret comes first: its menu is about the word
    /// being typed wherever it is. The command menu needs the input to be a
    /// bare command, and is closed while a turn runs — the composer refuses to
    /// send then, so there is nothing to complete into. A menu with nothing to
    /// list is no menu.
    pub(super) fn open_menu(&self, session: &Session, cx: &App) -> Option<OpenMenu> {
        if let Some(entries) = self.mention_matches(session, cx)
            && !entries.is_empty()
        {
            return Some(OpenMenu::Mention(entries));
        }
        if self.workspace.read(cx).has_turn(session.id) {
            return None;
        }
        let commands = self.session_commands(session, cx);
        let typed = self.input_state.read(cx).value();
        let matches: Vec<agent::CommandInfo> = matching_commands(&commands, &typed)
            .into_iter()
            .cloned()
            .collect();
        (!matches.is_empty()).then_some(OpenMenu::Command(matches))
    }

    /// Complete the open menu's highlighted row, returning whether a menu was
    /// open to complete: Enter completes first and sends second.
    ///
    /// The cursor is clamped rather than trusted, because a list can be
    /// replaced (a live session reporting its own commands, a project listed
    /// again) without the input changing.
    pub(super) fn complete_menu(
        &mut self,
        session: &Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.open_menu(session, cx) else {
            return false;
        };
        let cursor = self.menu_cursor.min(menu.len() - 1);
        match menu {
            OpenMenu::Mention(entries) => self.complete_mention(&entries[cursor], window, cx),
            OpenMenu::Command(commands) => {
                self.complete_command(&commands[cursor].name, window, cx)
            }
        }
        true
    }

    /// Move the open menu's highlight by one row, wrapping at either end.
    /// Returns whether a menu is open to take the key — when none is, Up/Down
    /// belong to the textarea.
    pub(super) fn move_menu_cursor(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.selected_session(cx) else {
            return false;
        };
        let Some(len) = self.open_menu(&session, cx).map(|menu| menu.len()) else {
            return false;
        };
        let current = self.menu_cursor.min(len - 1);
        self.menu_cursor = if down {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.menu_scroll.scroll_to_item(self.menu_cursor);
        cx.notify();
        true
    }

    /// Put a command into the input, ready for its arguments.
    ///
    /// The trailing space is deliberate: it closes the menu (the text is no
    /// longer a bare command) and it is where the user starts typing the rest.
    /// Enter then sends, which is the second half of "complete first, send
    /// second".
    ///
    /// The caret is put back at the end by hand: `set_value` clears a multi-line
    /// input's selection to `0..0`, which would otherwise leave the arguments
    /// being typed in front of the command just completed.
    pub(super) fn complete_command(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = format!("/{name} ");
        self.input_state.update(cx, |state, cx| {
            state.set_value(&text, window, cx);
            let end = state.text().len();
            state.set_selected_range(end..end, cx);
        });
        cx.notify();
    }

    /// The open menu, drawn above the input.
    pub(super) fn render_open_menu(&self, menu: OpenMenu, cx: &Context<Self>) -> AnyElement {
        let chrome = PickerChrome::new(cx.theme());
        let weak = cx.entity().downgrade();
        // The highlighted row is what Enter completes to.
        let cursor = self.menu_cursor.min(menu.len() - 1);
        let (id, rows): (&str, Vec<_>) = match menu {
            OpenMenu::Mention(entries) => (
                "mention-menu",
                entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let weak = weak.clone();
                        select_row(
                            &chrome,
                            &entry.clone(),
                            None,
                            index == cursor,
                            move |_, window, cx| {
                                let _ = weak
                                    .update(cx, |app, cx| app.complete_mention(&entry, window, cx));
                            },
                        )
                    })
                    .collect(),
            ),
            OpenMenu::Command(commands) => (
                "command-menu",
                commands
                    .into_iter()
                    .enumerate()
                    .map(|(index, command)| {
                        let name = command.name;
                        let weak = weak.clone();
                        select_row(
                            &chrome,
                            &format!("/{name}"),
                            command.hint,
                            index == cursor,
                            move |_, window, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.complete_command(&name, window, cx);
                                });
                            },
                        )
                    })
                    .collect(),
            ),
        };
        super::input_region(
            cx.theme().muted,
            tracked_picker_menu(id, &chrome, &self.menu_scroll, rows),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::matching_commands;
    use crate::agent::CommandInfo;

    fn commands() -> Vec<CommandInfo> {
        vec![
            CommandInfo {
                name: "compact".into(),
                hint: Some("<instructions>".into()),
            },
            CommandInfo {
                name: "clear".into(),
                hint: None,
            },
            CommandInfo {
                name: "code-review".into(),
                hint: None,
            },
            CommandInfo {
                name: "skill:brave-search".into(),
                hint: None,
            },
        ]
    }

    fn names<'a>(matches: &'a [&'a CommandInfo]) -> Vec<&'a str> {
        matches
            .iter()
            .map(|command| command.name.as_str())
            .collect()
    }

    /// A lone slash offers everything, in the agent's own order — the list must
    /// not reshuffle as the user types.
    #[test]
    fn a_bare_slash_offers_every_command_in_the_agents_order() {
        let commands = commands();
        assert_eq!(
            names(&matching_commands(&commands, "/")),
            ["compact", "clear", "code-review", "skill:brave-search"]
        );
    }

    /// The prefix decides, case-insensitively, and every command that matches
    /// stays: `co` is both `compact` and `code-review`.
    #[test]
    fn the_prefix_narrows_the_list_without_reordering_it() {
        let commands = commands();
        assert_eq!(
            names(&matching_commands(&commands, "/co")),
            ["compact", "code-review"]
        );
        assert_eq!(
            names(&matching_commands(&commands, "/CO")),
            ["compact", "code-review"]
        );
        assert_eq!(names(&matching_commands(&commands, "/cl")), ["clear"]);
        assert_eq!(
            names(&matching_commands(&commands, "/skill:")),
            ["skill:brave-search"]
        );
    }

    /// Arguments, paths and plain messages open nothing: once the first word is
    /// over (or never started with a slash), the text is just a message.
    #[test]
    fn only_a_bare_command_opens_the_menu() {
        let commands = commands();
        for input in [
            "",
            "写点什么",
            "/compact 重点放在测试上",
            "/usr/bin/env",
            "见 /compact",
            " /compact",
            "//",
        ] {
            assert!(
                matching_commands(&commands, input).is_empty(),
                "{input:?} is not a bare command"
            );
        }
        assert!(
            matching_commands(&commands, "/zzz").is_empty(),
            "a prefix nothing matches offers nothing"
        );
    }

    /// An agent with no catalogue never has a match, whatever is typed.
    #[test]
    fn no_catalogue_means_no_menu() {
        assert!(matching_commands(&[], "/").is_empty());
    }
}
