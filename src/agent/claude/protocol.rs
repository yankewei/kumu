//! The slice of Claude Code's `stream-json` wire this backend reads.
//!
//! Hand-written, and pinned: `claude` publishes no schema generator for this
//! protocol, so the authority is captured traffic — one session's lines,
//! vendored under `schema/claude-stream-json/2.1.269/`. Every shape here was
//! checked against **claude 2.1.269**; re-capture before trusting them against
//! a newer CLI (the tests below read those captured lines directly).
//!
//! Looseness rules, for the same reason the Codex slice has them — the CLI
//! moves on its own schedule:
//!   - an unknown `type` reads as [`Incoming::Other`] rather than an error, and
//!     an unknown `subtype` stays a plain `String`, so a new event kind cannot
//!     break the stream;
//!   - only the fields kumu acts on are modelled; the rest are ignored;
//!   - `is_error` defaults to false, because the CLI omits it on success.

use serde::Deserialize;
use serde_json::Value;

/// One line of the CLI's stdout.
///
/// The control messages are absent on purpose: the transport consumes
/// `control_response` to settle the request it belongs to, and turns
/// `control_request` into its own [`crate::agent::claude::transport::Message`],
/// so neither ever reaches the session as a stream line.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(super) enum Incoming {
    #[serde(rename = "system")]
    System(SystemEvent),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "user")]
    User(AssistantMessage),
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEvent),
    #[serde(rename = "result")]
    Result(ResultEvent),
    /// A message kind this build does not model.
    #[serde(other)]
    Other,
}

/// A `system` message. The subtype decides which fields matter: `init` carries
/// the session's own report; `thinking_tokens`, `status`, `api_retry` and
/// `permission_denied` are progress chatter.
///
/// `thinking_tokens` is emitted per thinking chunk (674 of them in one short
/// turn), which is why it is a subtype to drop rather than to show.
#[derive(Debug, Deserialize)]
pub(super) struct SystemEvent {
    #[serde(default)]
    pub subtype: String,
    /// Durable conversation id reported by `system/init`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The model the session resolved to, on `init`. This is the effective
    /// model, not the value kumu asked for.
    #[serde(default)]
    pub model: Option<String>,
    /// The permission mode in force, on `init`. `"default"` here is the
    /// sentinel meaning "nothing overrides the session's own configuration".
    #[serde(rename = "permissionMode", default)]
    pub permission_mode: Option<String>,
    /// The tool a `permission_denied` message is about.
    #[serde(rename = "tool_name", default)]
    pub tool_name: Option<String>,
    /// The human-readable part of `permission_denied` and `api_retry`.
    #[serde(default)]
    pub message: Option<String>,
    /// The retry counters on `api_retry`. Their names come from the documented
    /// shape of that event rather than a captured one, so they are optional and
    /// a missing value only costs the number in a log line.
    #[serde(default)]
    pub attempt: Option<u64>,
    #[serde(rename = "max_retries", default)]
    pub max_retries: Option<u64>,
    /// The commands that only run in an interactive terminal. Sending one over
    /// this wire does nothing, so they are not offered as commands. This is the
    /// only signal that separates them from the rest, and it only arrives with
    /// the first turn.
    ///
    /// `system/init` also lists every command by name; that list is not modelled
    /// because the handshake's catalogue already carries them, with the hint each
    /// takes.
    #[serde(rename = "terminal_slash_commands", default)]
    pub terminal_slash_commands: Vec<String>,
}

/// An `assistant` message, and — with the same envelope — a `user` message.
/// Only `content` is read: kumu renders the blocks, not the role.
#[derive(Debug, Deserialize)]
pub(super) struct AssistantMessage {
    #[serde(default)]
    pub message: Message,
    /// The `Task` call a subagent's message belongs to; `None` for the main
    /// conversation. A subagent runs in a context of its own, so its usage
    /// says nothing about how full the session's is.
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct Message {
    /// The message id, shared with the `stream_event`s that streamed it (see
    /// `session::Shared::streamed`).
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub content: ContentBlocks,
    /// The model that wrote the message, without the `[1m]`-style suffix the
    /// `result`'s `modelUsage` keys carry.
    #[serde(default)]
    pub model: Option<String>,
    /// The Messages API usage of the call that produced this message.
    #[serde(default)]
    pub usage: Option<MessageUsage>,
}

/// One model call's token counts, as the Messages API reports them.
#[derive(Debug, Default, Deserialize)]
pub(super) struct MessageUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl MessageUsage {
    /// What the context holds after this call: everything sent, cached or not,
    /// plus the reply, which the next call sends back.
    pub(super) fn context_tokens(&self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
    }
}

/// A message's content: the CLI sends an array of blocks, and a bare string
/// when it echoes text. Both must parse, since a shape mismatch here would drop
/// the whole message.
#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
pub(super) enum ContentBlocks {
    Blocks(Vec<ContentBlock>),
    Text(String),
    #[default]
    Empty,
}

impl ContentBlocks {
    /// The blocks, or an empty slice for a message that carries none.
    pub(super) fn blocks(&self) -> &[ContentBlock] {
        match self {
            Self::Blocks(blocks) => blocks,
            Self::Text(_) | Self::Empty => &[],
        }
    }
}

/// One content block. `text` and `thinking` blocks are the non-streaming
/// fallback; `tool_use` and `tool_result` are the two halves of a tool call.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(super) enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        #[serde(rename = "tool_use_id", default)]
        tool_use_id: String,
        #[serde(default)]
        is_error: bool,
        /// A string for most tools, an array of blocks for a few.
        #[serde(default)]
        content: Value,
    },
    #[serde(other)]
    Other,
}

/// A `stream_event`: one raw API streaming event, present only while
/// `--include-partial-messages` is on.
#[derive(Debug, Deserialize)]
pub(super) struct StreamEvent {
    #[serde(default)]
    pub event: ApiEvent,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiEvent {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: Option<Value>,
    #[serde(default)]
    pub delta: Delta,
}

impl ApiEvent {
    /// The id of the message this event belongs to, when it names one
    /// (`message_start`). Streaming events for later blocks of the same message
    /// carry no id, so the caller remembers the last one it saw.
    pub(super) fn message_id(&self) -> Option<&str> {
        self.message.as_ref()?.get("id")?.as_str()
    }
}

/// An incremental chunk of a block. Only text and thinking are rendered;
/// `input_json_delta` (tool input) and `signature_delta` are ignored, since the
/// complete block arrives with the `assistant` message.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Delta {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
}

/// The last line of a turn.
#[derive(Debug, Deserialize)]
pub(super) struct ResultEvent {
    /// `success`, or one of the `error_*` subtypes.
    #[serde(default)]
    pub subtype: String,
    /// Absent on success.
    #[serde(rename = "is_error", default)]
    pub is_error: bool,
    #[serde(rename = "stop_reason", default)]
    pub stop_reason: Option<String>,
    /// The final reply text, as the CLI would print it.
    #[serde(default)]
    pub result: Option<String>,
    /// Tool calls that nothing approved, in the order they were refused.
    #[serde(rename = "permission_denials", default)]
    pub permission_denials: Vec<Value>,
    /// What the CLI process has spent so far, in dollars. It counts this
    /// process only: a session resumed in a new one starts again from zero.
    #[serde(rename = "total_cost_usd", default)]
    pub total_cost_usd: Option<f64>,
    /// Per-model totals, keyed by the model's name with any context suffix
    /// (`opus[1m]`). Only the window is read.
    #[serde(rename = "modelUsage", default)]
    pub model_usage: std::collections::HashMap<String, ModelUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ModelUsage {
    #[serde(rename = "contextWindow", default)]
    pub context_window: Option<u64>,
}

impl ResultEvent {
    /// The context window of `model`, as the message that named it spelled it.
    ///
    /// The keys may carry a suffix the message's model does not (`opus[1m]`
    /// against `opus`), so a key that extends the name with `[` counts too.
    /// With no model, or no key for it, the one model the turn used is the
    /// answer; with several, none is guessed.
    pub(super) fn context_window(&self, model: Option<&str>) -> Option<u64> {
        let named = model.and_then(|model| {
            self.model_usage.iter().find(|(key, _)| {
                key.as_str() == model
                    || key
                        .strip_prefix(model)
                        .is_some_and(|rest| rest.starts_with('['))
            })
        });
        let usage = match named {
            Some((_, usage)) => usage,
            None if self.model_usage.len() == 1 => self.model_usage.values().next()?,
            None => return None,
        };
        usage.context_window.filter(|window| *window > 0)
    }

    /// What to show the user when the turn failed: the CLI puts the failure
    /// text in `result`, and the `error_*` subtype is the fallback.
    pub(super) fn failure(&self) -> String {
        self.result
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| self.subtype.clone())
    }
}

/// A request the CLI sends the host and waits on.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ControlRequest {
    #[serde(rename = "request_id", default)]
    pub request_id: String,
    #[serde(default)]
    pub request: ControlRequestBody,
}

/// The body of a [`ControlRequest`]. `can_use_tool` is the one kumu answers;
/// the rest (`hook_callback`, `mcp_message`, …) are for hosts that integrate
/// more deeply and are rejected rather than left unanswered.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ControlRequestBody {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub input: Value,
    #[serde(rename = "tool_use_id", default)]
    pub tool_use_id: Option<String>,
}

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
/// This is the richest description of a Claude Code session kumu gets: the
/// account's model catalogue (with the effort levels each model supports), the
/// permission mode in force, and which account is signed in.
#[derive(Debug, Default, Deserialize)]
pub(super) struct InitializeResponse {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(rename = "current_permission_mode", default)]
    pub current_permission_mode: Option<String>,
    /// The slash commands the CLI will run for this session, in its own order.
    ///
    /// This is the only catalogue of them: it is here on the handshake and not
    /// on `system/init`, which carries nothing but a list of names (see
    /// [`SystemEvent::slash_commands`]).
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

/// One model the account offers.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ModelEntry {
    /// The value to send back: `default`, an alias (`sonnet`), or a model id.
    #[serde(default)]
    pub value: String,
    /// The model the value resolves to for this account. A gateway can point
    /// every alias at the same model, so this is what the session runs.
    #[serde(rename = "resolvedModel", default)]
    pub resolved_model: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(rename = "supportsEffort", default)]
    pub supports_effort: bool,
    /// The efforts this model accepts, ascending. An unsupported level clamps
    /// down rather than failing, so the union across models is what a session
    /// is offered.
    #[serde(rename = "supportedEffortLevels", default)]
    pub supported_effort_levels: Vec<String>,
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
            "init" => include_str!("../../../schema/claude-stream-json/2.1.269/init.json"),
            "assistant-tool-use" => {
                include_str!("../../../schema/claude-stream-json/2.1.269/assistant-tool-use.json")
            }
            "user-tool-result" => {
                include_str!("../../../schema/claude-stream-json/2.1.269/user-tool-result.json")
            }
            "permission-denied" => {
                include_str!("../../../schema/claude-stream-json/2.1.269/permission-denied.json")
            }
            "result" => include_str!("../../../schema/claude-stream-json/2.1.269/result.json"),
            "control-can-use-tool" => {
                include_str!("../../../schema/claude-stream-json/2.1.269/control-can-use-tool.json")
            }
            "control-initialize-response" => include_str!(
                "../../../schema/claude-stream-json/2.1.269/control-initialize-response.json"
            ),
            other => panic!("no captured line named {other}"),
        })
        .unwrap()
    }

    /// `system/init` carries the model the session resolved to and the mode in
    /// force — the two things a session is labelled with.
    #[test]
    fn init_reports_the_effective_model_and_mode() {
        let Incoming::System(init) = from_captured("init") else {
            panic!("init.json is not a system message");
        };
        assert_eq!(init.subtype, "init");
        assert_eq!(init.model.as_deref(), Some("deepseek-v4-pro[1m]"));
        assert_eq!(
            init.permission_mode.as_deref(),
            Some("default"),
            "the sentinel meaning 'nothing overrides the session'"
        );
    }

    /// A tool call is two messages: the `assistant` half names the tool and
    /// carries the input, the `user` half carries the result under the same
    /// tool-use id.
    #[test]
    fn a_tool_call_is_an_assistant_block_and_a_user_result() {
        let Incoming::Assistant(call) = from_captured("assistant-tool-use") else {
            panic!("not an assistant message");
        };
        let [ContentBlock::ToolUse { id, name, input }] = call.message.content.blocks() else {
            panic!("expected one tool_use block");
        };
        assert_eq!(name, "Write");
        assert!(!id.is_empty());
        assert!(input.get("file_path").is_some());

        let Incoming::User(result) = from_captured("user-tool-result") else {
            panic!("not a user message");
        };
        let [
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                content,
            },
        ] = result.message.content.blocks()
        else {
            panic!("expected one tool_result block");
        };
        assert_eq!(tool_use_id, id, "the result answers that call");
        assert!(!is_error);
        assert!(content.is_string());
    }

    /// The CLI omits `is_error` on success and sets it on a refusal, so the
    /// field has to default rather than be required.
    #[test]
    fn tool_results_read_both_outcomes() {
        let denied: Value = captured("permission-denied");
        assert_eq!(denied["subtype"], "permission_denied");
        assert_eq!(denied["tool_name"], "Write");

        // The same block shape with the error flag set, as the CLI sends it
        // when nothing approved the call.
        let refused: AssistantMessage = serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "call_1",
                "is_error": true, "content": "not granted"}]},
        }))
        .unwrap();
        let [ContentBlock::ToolResult { is_error, .. }] = refused.message.content.blocks() else {
            panic!("expected one tool_result block");
        };
        assert!(is_error);
    }

    /// `can_use_tool` is what kumu answers; its input and the tool-use id link
    /// the request to the row the tool call opened.
    #[test]
    fn can_use_tool_carries_the_input_and_its_row_id() {
        let request: ControlRequest =
            serde_json::from_value(captured("control-can-use-tool")).unwrap();
        assert_eq!(request.request.subtype, "can_use_tool");
        assert!(request.request.input.get("file_path").is_some());
        assert_eq!(
            request.request.tool_use_id.as_deref(),
            Some("call_00_P8yukSl0EITLfI4IQ37H1745"),
            "the request links back to the row the tool call opened"
        );
        assert!(!request.request_id.is_empty());
    }

    /// The initialize response is the model catalogue: the value to send, what
    /// it resolves to, and the efforts each model accepts.
    #[test]
    fn initialize_reports_the_model_catalogue_and_the_mode() {
        let response: InitializeResponse = serde_json::from_value(
            captured("control-initialize-response")["response"]["response"].clone(),
        )
        .unwrap();
        assert!(
            response.models.len() > 1,
            "an account offers more than one entry"
        );
        let first = &response.models[0];
        assert!(!first.value.is_empty());
        assert!(
            first.resolved_model.is_some(),
            "an alias resolves to a real model"
        );
        assert!(first.supports_effort);
        assert!(!first.supported_effort_levels.is_empty());
        assert_eq!(response.current_permission_mode.as_deref(), Some("default"));
    }

    /// The handshake is also where the CLI lists the commands it will run, each
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

    /// `system/init` names the commands that only work in an interactive
    /// terminal, which is the one signal that separates them from the rest — and
    /// it only arrives with the first turn.
    #[test]
    fn init_names_the_terminal_only_commands() {
        let Incoming::System(init) = from_captured("init") else {
            panic!("init.json is not a system message");
        };
        assert!(
            init.terminal_slash_commands
                .iter()
                .any(|name| name == "doctor"),
            "the terminal-only commands are the ones to stop offering"
        );
    }

    /// A turn ends with a `result`; the error flag is absent on success.
    #[test]
    fn result_reports_the_outcome() {
        let Incoming::Result(result) = from_captured("result") else {
            panic!("not a result");
        };
        assert_eq!(result.subtype, "success");
        assert!(!result.is_error);
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
        assert!(result.permission_denials.is_empty());
    }

    /// The captured turn used two models: the one the reply came from is found
    /// under its suffixed key, and its window is the answer — not the helper
    /// model's, and not a guess when the model is unknown.
    #[test]
    fn the_window_is_the_replying_models() {
        let Incoming::Result(result) = from_captured("result") else {
            panic!("not a result");
        };
        assert_eq!(
            result.context_window(Some("deepseek-v4-pro")),
            Some(1_000_000)
        );
        assert_eq!(
            result.context_window(Some("deepseek-v4-flash")),
            Some(200_000)
        );
        assert_eq!(result.context_window(None), None, "two models, none named");
        assert!(result.total_cost_usd.is_some());

        let Incoming::Assistant(message) = from_captured("assistant-tool-use") else {
            panic!("not an assistant message");
        };
        assert_eq!(message.message.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(message.message.usage.unwrap().context_tokens(), 20_378);
    }

    /// The streaming events are raw API events; only the deltas carry text, and
    /// the message id on `message_start` is what ties them to the `assistant`
    /// message that follows.
    #[test]
    fn stream_events_carry_text_and_thinking_deltas_under_a_message_id() {
        let events: Vec<Value> = serde_json::from_str(include_str!(
            "../../../schema/claude-stream-json/2.1.269/stream-events.json"
        ))
        .unwrap();
        let mut streamed = Vec::new();
        let mut saw_id = false;
        for line in events {
            let Incoming::StreamEvent(event) = serde_json::from_value(line).unwrap() else {
                panic!("every entry is a stream_event");
            };
            if event.event.message_id().is_some() {
                saw_id = true;
            }
            match (event.event.kind.as_str(), event.event.delta.kind.as_str()) {
                ("content_block_delta", "text_delta") => {
                    streamed.push(event.event.delta.text.unwrap_or_default())
                }
                ("content_block_delta", "thinking_delta") => {
                    streamed.push(event.event.delta.thinking.unwrap_or_default())
                }
                _ => {}
            }
        }
        assert!(streamed.iter().all(|chunk| !chunk.is_empty()));
        assert!(
            saw_id,
            "message_start names the message the deltas belong to"
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

    /// A shape this build has never seen must parse rather than break the
    /// stream: an unknown message type, an unknown block type, and an unknown
    /// system subtype all survive.
    #[test]
    fn unknown_shapes_read_as_other_instead_of_failing() {
        assert!(matches!(
            serde_json::from_value::<Incoming>(serde_json::json!({"type": "something_new"}))
                .unwrap(),
            Incoming::Other
        ));

        let message: AssistantMessage = serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "image", "source": {}}]},
        }))
        .unwrap();
        assert!(matches!(
            message.message.content.blocks(),
            [ContentBlock::Other]
        ));

        let system: SystemEvent =
            serde_json::from_value(serde_json::json!({"type": "system", "subtype": "brand_new"}))
                .unwrap();
        assert_eq!(system.subtype, "brand_new");
    }

    /// A message whose content is a bare string (the CLI echoes text that way)
    /// must not fail the parse.
    #[test]
    fn string_content_parses_as_no_blocks() {
        let message: AssistantMessage = serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "plain text"},
        }))
        .unwrap();
        assert!(message.message.content.blocks().is_empty());
    }

    fn from_captured(name: &str) -> Incoming {
        serde_json::from_value(captured(name)).unwrap()
    }
}
