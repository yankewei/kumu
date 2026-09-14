//! The composer's slash-command menu: what the session's agent says it will run,
//! filtered by what has been typed so far.
//!
//! It sits directly above the input and shows the row Enter would complete to,
//! which is the whole of its behaviour: a command is an ordinary message once it
//! is typed (`/name args` — every agent kumu drives recognizes the prefix
//! itself), so nothing here sends anything. It exists to answer "what can I
//! type?" before the user has to remember it.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use crate::backend;
use crate::db::Session;
use crate::ui::AgentCommands;
use crate::ui::widgets::{PickerChrome, picker_list, picker_menu, select_row};

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
    commands: &'a [backend::CommandInfo],
    input: &str,
) -> Vec<&'a backend::CommandInfo> {
    let typed = match input.strip_prefix('/') {
        Some(typed) if !typed.contains(char::is_whitespace) => typed.to_lowercase(),
        _ => return Vec::new(),
    };
    commands
        .iter()
        .filter(|command| command.name.to_lowercase().starts_with(&typed))
        .collect()
}

impl KumuApp {
    /// The commands the session's agent offers, from the cache the probes and
    /// live sessions fill (`Event::Commands`).
    ///
    /// Empty for an agent that reports none — Codex's app-server exposes no such
    /// catalogue — which is what keeps the menu out of that session's composer
    /// rather than offering commands that could not run.
    pub(super) fn session_commands(
        &self,
        session: &Session,
        cx: &App,
    ) -> Vec<backend::CommandInfo> {
        cx.try_global::<AgentCommands>()
            .and_then(|catalogs| catalogs.0.get(session.agent.as_str()).cloned())
            .unwrap_or_default()
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

    /// The command menu, or nothing when the input is not a bare command, no
    /// command matches it, or a turn is in flight (the composer refuses to send
    /// then, so there is nothing to complete into).
    pub(super) fn command_menu(&self, session: &Session, cx: &Context<Self>) -> Option<AnyElement> {
        if self.turns.contains_key(&session.id) {
            return None;
        }
        let commands = self.session_commands(session, cx);
        if commands.is_empty() {
            return None;
        }
        let typed = self.input_state.read(cx).value();
        let matches = matching_commands(&commands, &typed);
        if matches.is_empty() {
            return None;
        }

        let theme = cx.theme();
        let chrome = PickerChrome::new(theme);
        let weak = cx.entity().downgrade();
        let mut list = picker_list();
        for (index, command) in matches.iter().enumerate() {
            let name = command.name.clone();
            let weak = weak.clone();
            list = list.child(select_row(
                &chrome,
                &format!("/{name}"),
                command.hint.clone(),
                // The first match is what Enter completes to, so it is the row
                // that reads as chosen — there is no separate cursor to move,
                // and typing narrows the list instead.
                index == 0,
                move |_, window, cx| {
                    let _ = weak.update(cx, |app, cx| {
                        app.complete_command(&name, window, cx);
                    });
                },
            ));
        }

        // The same backdrop recipe as the permission card above it: the two read
        // as one input region, on the composer's centered column.
        Some(
            v_flex()
                .flex_none()
                .px_5()
                .pt_3()
                .bg(theme.muted.opacity(0.5))
                .child(
                    v_flex()
                        .w_full()
                        .max_w(rems(super::CONTENT_MAX_REM))
                        .min_w(rems(super::CONTENT_MIN_REM))
                        .mx_auto()
                        .child(picker_menu("command-menu", &chrome, list)),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::matching_commands;
    use crate::backend::CommandInfo;

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

    /// An agent with no catalogue (Codex) never has a match, whatever is typed.
    #[test]
    fn no_catalogue_means_no_menu() {
        assert!(matching_commands(&[], "/").is_empty());
    }
}
