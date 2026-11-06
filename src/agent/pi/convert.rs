//! Protocol -> app translation. Protocol shapes stop here.
//!
//! Everything Pi reports about a session arrives in the `data` object of a
//! `response`, so the functions below take that object rather than the whole
//! envelope — the session unwraps the frame once and the translation stays
//! free of transport detail.
//!
//! The labels are the agent's own where they are a scale the user reads back in
//! Pi itself (thinking levels), and derived from Pi's payload where the agent
//! reports them (models and their providers). Nothing here is a fixed
//! vocabulary: unlike Claude Code, Pi advertises its catalogue over the
//! protocol, so the menu is whatever this machine is configured for.

use serde_json::Value;

use crate::agent::event::{CommandInfo, Cost, ModelInfo, ReasoningInfo, Usage};

/// Provider and model id are joined with `/` because that is the form Pi's own
/// `--model` accepts, and therefore the form kumu sends back.
const SLUG_SEPARATOR: char = '/';

/// One model, as the session reports it.
struct ReportedModel {
    provider: String,
    id: String,
    name: String,
}

/// The models this machine can reach, grouped by provider so a list of a
/// hundred-odd entries reads as blocks rather than as one run.
///
/// The provider is taken from the payload, not parsed back out of the value:
/// Pi names a provider per model, and a provider id may itself contain no
/// separator, which is exactly why the value is built rather than assumed.
pub(super) fn model_catalog(data: &Value) -> Vec<ModelInfo> {
    let mut models: Vec<ReportedModel> = data
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    let field = |key: &str| {
                        model
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                    };
                    Some(ReportedModel {
                        provider: field("provider")?.to_string(),
                        id: field("id")?.to_string(),
                        // The label Pi shows, falling back to the id: naming the
                        // model is more use than repeating the slug.
                        name: field("name").unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    models.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });

    models
        .into_iter()
        .map(|model| ModelInfo {
            value: format!("{}{SLUG_SEPARATOR}{}", model.provider, model.id),
            name: if model.name.is_empty() {
                model.id
            } else {
                model.name
            },
            provider: model.provider,
        })
        .collect()
}

/// The model in force, as the `provider/model` value a session selects on.
pub(super) fn current_model(data: &Value) -> Option<String> {
    let model = data.get("model")?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let id = model.get("id").and_then(Value::as_str)?;
    (!provider.is_empty() && !id.is_empty()).then(|| format!("{provider}{SLUG_SEPARATOR}{id}"))
}

/// The thinking levels the selected model honours.
///
/// Asked for the *current* model rather than read off the catalogue: the levels
/// live on each model, and Pi is the one that knows which of them the selected
/// one supports. `off` is among them — Pi lists it even for a model with no
/// reasoning support, which is what makes the list a complete answer for the
/// axis instead of needing a value added back.
pub(super) fn reasoning_catalog(data: &Value) -> Vec<ReasoningInfo> {
    data.get("levels")
        .and_then(Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|level| !level.is_empty())
                .map(|level| ReasoningInfo {
                    // A scale the user passes back and reads in Pi's own header,
                    // so the value is its label rather than a translation.
                    value: level.to_string(),
                    name: level.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The commands this Pi will run, from a `get_commands` reply.
///
/// Pi's own documentation says these are exactly the ones a client may invoke
/// over RPC — by sending `/name` as a prompt — and deliberately excludes the
/// commands its TUI handles itself. Its `source` (`extension` / `prompt` /
/// `skill`) is not carried: the name already says which namespace it is in
/// (`skill:` is part of it), and no row in a kumu menu names a source.
///
/// Pi reports a description for each and kumu carries none: a list row is the
/// name and nothing more, so it is dropped here rather than at render time.
pub(super) fn command_catalog(data: &Value) -> Vec<CommandInfo> {
    data.get("commands")
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| command.get("name").and_then(Value::as_str))
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| CommandInfo {
                    name: name.to_string(),
                    // A command takes the rest of the line, but Pi says nothing
                    // about what that should be.
                    hint: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The thinking level in force.
pub(super) fn current_reasoning(data: &Value) -> Option<String> {
    let level = data.get("thinkingLevel").and_then(Value::as_str)?;
    let level = level.trim();
    (!level.is_empty()).then(|| level.to_string())
}

/// The name Pi gave this session, if it has one.
pub(super) fn session_title(data: &Value) -> Option<String> {
    let name = data.get("sessionName").and_then(Value::as_str)?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Split a `provider/model` pick into the two fields `set_model` takes.
///
/// Validated rather than passed through: Pi rejects a bare model id, and
/// discovering that from a failed turn would report it as a turn failure rather
/// than as the unactionable pick it is.
pub(super) fn parse_model_slug(value: &str) -> Result<(&str, &str), String> {
    let Some((provider, id)) = value.trim().split_once(SLUG_SEPARATOR) else {
        return Err(format!("模型要用 provider/model 形式：{value}"));
    };
    if provider.is_empty() || id.is_empty() {
        return Err(format!("模型要用 provider/model 形式：{value}"));
    }
    Ok((provider, id))
}

/// The row title for a tool call: Pi's tool name plus whatever makes the call
/// recognisable.
///
/// A tool that labels its own call wins, because those labels say what the call
/// is *for* ("Inspect Pi source") where the arguments only say what it touches.
/// Otherwise the name leads and the subject follows, matching the other
/// backends' rows — `bash` and `read` differ by a subject the user recognises,
/// not by a name alone.
///
/// The argument names are Pi's own: `bash` takes `command`, and its file tools
/// take `path` (both checked against live turns, see
/// `schema/pi-rpc/0.85.1/`).
pub(super) fn tool_title(name: &str, args: &Value) -> String {
    let field = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let subject = field("title").or_else(|| match name {
        "bash" => field("command"),
        "read" | "write" | "edit" | "ls" => field("path"),
        "grep" | "find" | "search" => field("pattern"),
        _ => None,
    });
    match subject {
        Some(subject) => format!("{name} · {subject}"),
        None => name.to_string(),
    }
}

/// A tool result's text.
///
/// Pi sends `{content: [{type:"text",text}, …], details: {…}}` for both a
/// partial and a final result, and the array is empty while a call has produced
/// nothing yet — which flattens to nothing, leaving the row without a body
/// rather than with an empty one.
pub(super) fn tool_content(result: &Value) -> String {
    match result {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Object(_) => result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        other => other.to_string(),
    }
}

/// The text of a completed assistant message, for the case where Pi sent no
/// deltas for it.
///
/// Only the block's `text` is read. Pi also stamps a `textSignature` on the
/// block, which is provider bookkeeping rather than something the user should
/// be shown.
pub(super) fn message_text(message: &Value) -> Option<String> {
    message_blocks(message, "text", "text")
}

/// The reasoning of a completed assistant message, same fallback as
/// [`message_text`].
pub(super) fn message_thinking(message: &Value) -> Option<String> {
    message_blocks(message, "thinking", "thinking")
}

fn message_blocks(message: &Value, kind: &str, field: &str) -> Option<String> {
    let text = message
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|block| block.get(field).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Why a run failed, when the agent says it did.
///
/// A model call that cannot run does not arrive as an error event: Pi finishes
/// the turn with an assistant message whose `stopReason` is `error` and whose
/// `errorMessage` carries the provider's own words, then settles normally.
/// Checked against two providers that both refused a turn (see
/// `schema/pi-rpc/0.85.1/`), which is why the failure is read from the message
/// rather than from the event stream.
pub(super) fn failure(message: &Value) -> Option<String> {
    if message.get("stopReason").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let detail = message
        .get("errorMessage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|detail| !detail.is_empty());
    Some(detail.unwrap_or("pi 的这一轮失败了").to_string())
}

/// The context and cost from `get_session_stats`.
///
/// `contextUsage` is Pi's own estimate of the context — the one its footer and
/// compaction use — and is absent without a model, while its `tokens` is `null`
/// right after a compaction until the next reply measures it. `cost` covers the
/// whole session file, so it survives a reconnect. `None` when the reply holds
/// neither.
pub(super) fn session_usage(data: &Value) -> Option<Usage> {
    let context = data.get("contextUsage");
    let usage = Usage::reported(
        context
            .and_then(|c| c.get("tokens"))
            .and_then(Value::as_u64),
        context
            .and_then(|c| c.get("contextWindow"))
            .and_then(Value::as_u64),
        data.get("cost").and_then(Value::as_f64).map(Cost::usd),
    );
    (usage != Usage::default()).then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The stats reply's context estimate and session cost become the usage;
    /// right after a compaction the context is unknown but the window and cost
    /// still stand.
    #[test]
    fn session_stats_become_usage() {
        let usage = session_usage(&json!({
            "tokens": {"input": 50000, "output": 10000, "total": 105000},
            "cost": 0.45,
            "contextUsage": {"tokens": 60000, "contextWindow": 200000, "percent": 30},
        }))
        .unwrap();
        assert_eq!(usage.used, Some(60_000));
        assert_eq!(usage.window, Some(200_000));
        assert_eq!(usage.cost, Some(Cost::usd(0.45)));

        let compacted = session_usage(&json!({
            "cost": 0.45,
            "contextUsage": {"tokens": null, "contextWindow": 200000, "percent": null},
        }))
        .unwrap();
        assert_eq!(compacted.used, None);
        assert_eq!(compacted.window, Some(200_000));

        assert_eq!(session_usage(&json!({"sessionId": "x"})), None);
    }

    /// The command catalogue is what Pi says a client may invoke — extension
    /// commands, prompt templates and skills alike — in Pi's own order, with the
    /// `skill:` prefix that makes a skill addressable part of the name. An entry
    /// without a name is not an offer, and nothing here invents a hint.
    #[test]
    fn the_command_catalogue_is_the_sessions_own() {
        let commands = command_catalog(&json!({
            "commands": [
                {"name": "session-name", "description": "Set or clear session name", "source": "extension"},
                {"name": "fix-tests", "description": "Fix failing tests", "source": "prompt"},
                {"name": "skill:brave-search", "description": "Web search", "source": "skill"},
                {"description": "nameless", "source": "prompt"},
            ]
        }));
        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["session-name", "fix-tests", "skill:brave-search"]
        );
        assert!(
            commands.iter().all(|command| command.hint.is_none()),
            "pi describes no input, so no row claims one"
        );
    }

    /// The catalogue is built from what the session reports, as `provider/id`
    /// values, and reads grouped by provider.
    #[test]
    fn the_catalogue_is_the_sessions_own_and_reads_grouped() {
        let models = model_catalog(&json!({
            "models": [
                {"id": "sonnet-5", "name": "Sonnet 5", "provider": "anthropic"},
                {"id": "gpt-5.5", "name": "GPT-5.5", "provider": "openai"},
                {"id": "haiku-4", "name": "Haiku 4", "provider": "anthropic"},
            ]
        }));
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["anthropic/haiku-4", "anthropic/sonnet-5", "openai/gpt-5.5"],
            "grouped by provider, then by label"
        );
        assert_eq!(models[0].provider, "anthropic");
        assert_eq!(models[0].name, "Haiku 4");
    }

    /// A model whose provider or id is missing cannot be turned into a value
    /// `set_model` would accept, so it is left out rather than offered as an
    /// unusable entry.
    #[test]
    fn entries_without_a_provider_or_id_are_left_out() {
        let models = model_catalog(&json!({
            "models": [
                {"id": "gpt-5.5", "provider": "openai"},
                {"name": "No provider", "id": "x"},
                {"id": "", "provider": "openai"},
                {"id": "no-name", "provider": "openai"},
            ]
        }));
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["openai/gpt-5.5", "openai/no-name"]
        );
        // A model Pi names nothing still gets a label.
        assert_eq!(models[1].name, "no-name");
        assert!(model_catalog(&json!({})).is_empty());
    }

    /// The model in force is the pair Pi reports, joined the way `set_model`
    /// takes it back.
    #[test]
    fn the_current_model_is_the_reported_pair() {
        assert_eq!(
            current_model(&json!({"model": {"provider": "opencode-go", "id": "kimi-k3"}}))
                .as_deref(),
            Some("opencode-go/kimi-k3")
        );
        assert_eq!(current_model(&json!({"model": null})), None);
        assert_eq!(current_model(&json!({"model": {"id": "x"}})), None);
    }

    /// The levels come from the current model's own list, which includes `off`.
    #[test]
    fn thinking_levels_come_from_the_model_in_force() {
        let levels = reasoning_catalog(&json!({
            "levels": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        }));
        assert_eq!(
            levels.iter().map(|l| l.value.as_str()).collect::<Vec<_>>(),
            ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert!(
            levels.iter().all(|level| level.name == level.value),
            "the value is the label, as Pi names its own scale"
        );
        assert_eq!(
            current_reasoning(&json!({"thinkingLevel": "high"})).as_deref(),
            Some("high")
        );
        assert_eq!(current_reasoning(&json!({"thinkingLevel": "  "})), None);
        assert!(reasoning_catalog(&json!({})).is_empty());
    }

    /// A pick is only usable when it is a pair, and the split is on the first
    /// separator so a provider path with no separator still reaches Pi intact.
    #[test]
    fn a_model_pick_must_be_a_provider_and_an_id() {
        assert_eq!(
            parse_model_slug("anthropic/claude-sonnet-5"),
            Ok(("anthropic", "claude-sonnet-5"))
        );
        assert_eq!(parse_model_slug(" anthropic/x "), Ok(("anthropic", "x")));
        assert!(
            parse_model_slug("sonnet").is_err(),
            "a bare id is not usable"
        );
        assert!(parse_model_slug("/sonnet").is_err());
        assert!(parse_model_slug("anthropic/").is_err());
    }

    /// A session's own name becomes the session title, and a nameless session
    /// reports nothing rather than an empty title.
    #[test]
    fn a_named_session_reports_its_title() {
        assert_eq!(
            session_title(&json!({"sessionName": "fix the parser"})).as_deref(),
            Some("fix the parser")
        );
        assert_eq!(session_title(&json!({"sessionName": "  "})), None);
        assert_eq!(session_title(&json!({})), None);
    }

    /// Tool rows lead with the tool's name and then the recognisable part of
    /// the call; a tool that labelled its own call wins over its arguments.
    #[test]
    fn tool_titles_name_the_tool_and_its_subject() {
        assert_eq!(
            tool_title("bash", &json!({"command": "ls /tmp"})),
            "bash · ls /tmp"
        );
        assert_eq!(
            tool_title("read", &json!({"path": "/etc/hosts"})),
            "read · /etc/hosts"
        );
        assert_eq!(
            tool_title("edit", &json!({"path": "src/main.rs"})),
            "edit · src/main.rs"
        );
        assert_eq!(
            tool_title(
                "read",
                &json!({"path": "a.rs", "title": "Inspect the parser"})
            ),
            "read · Inspect the parser",
            "the agent's own label says more than the path does"
        );
        assert_eq!(
            tool_title("todo_write", &json!({"todos": []})),
            "todo_write"
        );
    }

    /// A result is the accumulated output, and an empty one is no body at all.
    #[test]
    fn tool_results_flatten_to_their_text() {
        assert_eq!(
            tool_content(
                &json!({"content": [{"type": "text", "text": "127.0.0.1 localhost"}], "details": {}})
            ),
            "127.0.0.1 localhost"
        );
        assert_eq!(tool_content(&json!({"content": []})), "");
        assert_eq!(tool_content(&json!({"content": [{"type": "image"}]})), "");
        assert_eq!(tool_content(&Value::Null), "");
    }

    /// Only a message Pi marked as an error carries a failure, and the
    /// provider's own words are what it says.
    #[test]
    fn a_failure_is_read_off_the_message_that_reports_it() {
        assert_eq!(
            failure(&json!({
                "stopReason": "error",
                "errorMessage": "OpenAI API error (401): Insufficient balance."
            }))
            .as_deref(),
            Some("OpenAI API error (401): Insufficient balance.")
        );
        assert_eq!(
            failure(&json!({"stopReason": "stop"})),
            None,
            "a run that stopped is not a failure"
        );
        assert_eq!(
            failure(&json!({"stopReason": "toolUse"})),
            None,
            "a tool step is mid-run, not a failure"
        );
        assert_eq!(
            failure(&json!({"stopReason": "error"})).as_deref(),
            Some("pi 的这一轮失败了"),
            "an error without a reason still fails"
        );
    }

    /// A completed message is the fallback for text and reasoning that never
    /// streamed, and its provider signature is not part of the text.
    #[test]
    fn a_completed_message_supplies_text_and_reasoning() {
        let message = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "weighing it"},
                {"type": "text", "text": "ok", "textSignature": "{\"v\":1}"},
            ]
        });
        assert_eq!(message_text(&message).as_deref(), Some("ok"));
        assert_eq!(message_thinking(&message).as_deref(), Some("weighing it"));

        // A tool-only message has neither, so nothing is emitted for it.
        let tool_only = json!({"content": [{"type": "toolCall", "name": "read"}]});
        assert_eq!(message_text(&tool_only), None);
        assert_eq!(message_thinking(&tool_only), None);
        assert_eq!(message_text(&json!({"content": []})), None);
    }
}
