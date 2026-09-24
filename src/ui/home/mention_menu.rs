//! The composer's file-reference menu: typing `@` offers the project's files
//! and folders, filtered by what follows it.
//!
//! Choosing one writes `@path` into the message and nothing else — the agent
//! runs in the project directory and reads the file with its own tools, so a
//! reference is text every agent understands. It behaves like the command menu
//! beside it: the first row is what Enter completes to, Up/Down move the
//! highlight, and a completion ends in a space that closes the menu.

use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use crate::db::Session;

/// How many entries the menu lists; the query narrows it, not scrolling.
const MENU_LIMIT: usize = 50;

/// The reference being typed at `cursor`: where its `@` sits, and the query
/// after it.
///
/// An `@` only opens a reference at the start of the text or after
/// whitespace, so an e-mail address or `pkg@1.2` is left alone, and the
/// reference ends at the first whitespace — a completed one, followed by its
/// space, is just text.
pub(super) fn mention_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    if query.contains(char::is_whitespace) {
        return None;
    }
    let opens = before[..at]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace);
    opens.then_some((at, query))
}

/// How `path` is written as a reference: `@path`, quoted when it holds
/// whitespace, since an unquoted reference ends at the first space.
fn reference(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

impl KumuApp {
    /// The entries the reference at the caret could mean, or `None` when the
    /// caret is not in one. `Some` of an empty list is a reference nothing
    /// matches (or a project not listed yet).
    pub(super) fn mention_matches(&self, session: &Session, cx: &App) -> Option<Vec<String>> {
        let input = self.input_state.read(cx);
        let text = input.value();
        let (_, query) = mention_at(&text, input.cursor())?;
        Some(
            self.workspace
                .read(cx)
                .file_matches(&session.cwd, query, MENU_LIMIT),
        )
    }

    /// Replace the reference at the caret with `@entry `, the caret after it.
    pub(super) fn complete_mention(
        &mut self,
        entry: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_state.update(cx, |state, cx| {
            let text = state.value();
            let cursor = state.cursor();
            if let Some((at, _)) = mention_at(&text, cursor) {
                state.set_selected_range(at..cursor, cx);
                state.replace(format!("{} ", reference(entry)), window, cx);
            }
        });
        cx.notify();
    }

    /// Put references to `paths` into the message at the caret: relative to the
    /// session's directory when they are inside it, as they stand otherwise.
    ///
    /// This is how a file that is not an image joins a message when it is
    /// dropped or pasted.
    pub(super) fn insert_references(
        &mut self,
        session: &Session,
        paths: &[std::path::PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let root = std::path::Path::new(&session.cwd);
        let text: String = paths
            .iter()
            .map(|path| {
                let shown = path.strip_prefix(root).unwrap_or(path);
                format!("{} ", reference(&shown.display().to_string()))
            })
            .collect();
        self.input_state.update(cx, |state, cx| {
            let before = state.value();
            let cursor = state.cursor();
            // A reference only reads as one after whitespace.
            let lead = before[..cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace());
            let text = if lead {
                format!(" {text}")
            } else {
                text.clone()
            };
            state.insert(text, window, cx);
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{mention_at, reference};

    /// The reference is found wherever the caret is, not only at the end.
    #[test]
    fn the_reference_at_the_caret_is_found() {
        assert_eq!(mention_at("@", 1), Some((0, "")));
        assert_eq!(mention_at("看看 @src/ma", 14), Some((7, "src/ma")));
        let text = "@foo 然后";
        assert_eq!(
            mention_at(text, 4),
            Some((0, "foo")),
            "caret right after it"
        );
    }

    /// An `@` inside a word, or a reference already closed by a space, opens
    /// nothing.
    #[test]
    fn only_an_open_reference_counts() {
        for (text, cursor) in [
            ("me@example.com", 14),
            ("@src/main.rs ", 13),
            ("没有引用", 12),
            ("", 0),
        ] {
            assert_eq!(mention_at(text, cursor), None, "{text:?} at {cursor}");
        }
    }

    /// A path with a space stays one reference.
    #[test]
    fn a_spaced_path_is_quoted() {
        assert_eq!(reference("src/main.rs"), "@src/main.rs");
        assert_eq!(reference("My Notes.md"), "@\"My Notes.md\"");
    }
}
