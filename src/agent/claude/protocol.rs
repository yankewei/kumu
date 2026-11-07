//! The slice of Claude Code's `stream-json` wire the settings probe reads.
//!
//! Hand-written, and pinned: `claude` publishes no schema generator for this
//! protocol, so the authority is captured traffic — one session's lines,
//! vendored under `schema/claude-stream-json/2.1.269/`. Every shape here was
//! checked against **claude 2.1.269**; re-capture before trusting them against
//! a newer CLI (the tests below read those captured lines directly).
//!
//! The probe sends one `initialize` control request. The transport correlates
//! the `control_response` by [`ControlResponse::request_id`] and the probe
//! reads [`InitializeResponse::commands`]. Other fields on those objects, and
//! every other line the CLI writes, are ignored.

use serde::Deserialize;
use serde_json::Value;

/// The body of a control response.
///
/// The frame is `{"type":"control_response","response":{…}}`: the inner object
/// *is* this, with the payload one level deeper in `response`. The transport
/// reads it to settle the request it belongs to.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ControlResponse {
    #[serde(default)]
    pub subtype: String,
    #[serde(rename = "request_id", default)]
    pub request_id: String,
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

/// What the `initialize` control request comes back with.
///
/// The same payload also carries the account's model catalogue and the
/// permission mode in force. The probe does not read either, so they are not
/// fields here.
#[derive(Debug, Default, Deserialize)]
pub(super) struct InitializeResponse {
    /// The slash commands the CLI will run for this session, in its own order.
    #[serde(default)]
    pub commands: Vec<CommandEntry>,
}

/// One entry of the CLI's command catalogue.
#[derive(Debug, Default, Deserialize)]
pub(super) struct CommandEntry {
    #[serde(default)]
    pub name: String,
    /// What the command takes after its name, e.g. `[low|medium|high]`. Absent
    /// or empty for a command that takes nothing.
    #[serde(rename = "argumentHint", default)]
    pub argument_hint: Option<String>,
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the crate root brings gpui's own `test` attribute
    // into scope through a glob; keep the built-in one.
    use core::prelude::v1::test;

    use super::*;

    /// The captured lines under `schema/claude-stream-json/2.1.269/` are the
    /// pin for these shapes, so the tests read them rather than hand-rolled
    /// JSON: a CLI upgrade that changes a field fails here.
    fn captured(name: &str) -> Value {
        serde_json::from_str(match name {
            "control-initialize-response" => include_str!(
                "../../../schema/claude-stream-json/2.1.269/control-initialize-response.json"
            ),
            other => panic!("no captured line named {other}"),
        })
        .unwrap()
    }

    /// The handshake is where the CLI lists the commands it will run, each
    /// with the hint it takes — the only place a hint appears.
    #[test]
    fn initialize_reports_the_command_catalogue() {
        let response: InitializeResponse = serde_json::from_value(
            captured("control-initialize-response")["response"]["response"].clone(),
        )
        .unwrap();
        assert!(
            !response.commands.is_empty(),
            "the captured session offers commands"
        );
        assert!(
            response
                .commands
                .iter()
                .all(|command| !command.name.is_empty()),
            "every entry names a command"
        );
        let compact = response
            .commands
            .iter()
            .find(|command| command.name == "compact")
            .expect("the captured session offers /compact");
        assert!(
            compact.argument_hint.is_some(),
            "compact takes an optional instruction"
        );
    }

    /// The frame around a control response, and the id inside it.
    ///
    /// This is the shape the transport's correlation depends on, and getting it
    /// wrong is silent: a mis-parsed id matches no waiter, so the request simply
    /// never returns. Hence the pin.
    #[test]
    fn a_control_response_carries_the_id_it_answers() {
        let captured = captured("control-initialize-response");
        let response: ControlResponse =
            serde_json::from_value(captured["response"].clone()).unwrap();
        assert_eq!(response.subtype, "success");
        assert!(
            !response.request_id.is_empty(),
            "the id sits directly under `response`, not one level deeper"
        );
        assert!(
            response.response.is_some(),
            "the payload is one level deeper still"
        );
        assert!(response.error.is_none());
    }

    /// An error answer carries its reason instead of a payload.
    #[test]
    fn an_error_control_response_carries_its_reason() {
        let response: ControlResponse = serde_json::from_value(serde_json::json!({
            "subtype": "error", "request_id": "kumu-1", "error": "unsupported",
        }))
        .unwrap();
        assert_eq!(response.subtype, "error");
        assert_eq!(response.request_id, "kumu-1");
        assert_eq!(response.error.as_deref(), Some("unsupported"));
        assert!(response.response.is_none());
    }
}
