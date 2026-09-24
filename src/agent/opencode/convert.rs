//! opencode v2 -> app translation: the catalogue reads, and the event stream.
//!
//! Where protocol shapes stop. The payloads are read as JSON as they arrive
//! rather than pinned as types: the server describes none of its events (see
//! `schema/opencode-v2/2.0.16/README.md`), so the fields read here are exactly
//! the ones the captured traffic shows, and a missing one degrades to nothing
//! rather than failing the whole event.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::agent::event::{
    CommandInfo, Cost, Event, ModeInfo, ModelInfo, PermissionKind, PermissionOptionInfo,
    ReasoningInfo, Usage,
};
use crate::agent::session::{CANCELLED, SessionOptionsBuilder};

/// One model as `GET /api/model` lists it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
    /// `limit.context`: the context window, in tokens.
    pub window: Option<u64>,
    /// The model's own reasoning levels (`variants`), in its order.
    pub variants: Vec<String>,
}

impl ModelEntry {
    /// The value kumu stores and the picker sends back: `provider/id`. The id
    /// may contain `/` itself (`deepseek/deepseek-v4.1-flash`), so a value is
    /// split at its first `/` only — which is also the shape the ACP backend
    /// stored, so a session's saved pick carries over.
    pub fn value(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// The models a location offers. A model the server marks `enabled: false` is
/// not offered.
pub(super) fn model_entries(response: &Value) -> Vec<ModelEntry> {
    let Some(models) = response.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|model| model.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|model| {
            let text = |key: &str| model.get(key).and_then(Value::as_str).map(str::to_string);
            let provider = text("providerID")?;
            let id = text("id")?;
            Some(ModelEntry {
                name: text("name").unwrap_or_else(|| id.clone()),
                window: model.pointer("/limit/context").and_then(Value::as_u64),
                variants: model
                    .get("variants")
                    .and_then(Value::as_array)
                    .map(|variants| {
                        variants
                            .iter()
                            .filter_map(|v| v.get("id").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                provider,
                id,
            })
        })
        .collect()
}

/// A `Model.Ref` (`{id, providerID, variant?}`) as the stored value and its
/// variant.
pub(super) fn model_ref(value: &Value) -> Option<(String, Option<String>)> {
    let provider = value.get("providerID")?.as_str()?;
    let id = value.get("id")?.as_str()?;
    let variant = value
        .get("variant")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((format!("{provider}/{id}"), variant))
}

/// The modes a session can switch between: the primary agents the user can
/// pick (`build`, `plan`). Subagents and the hidden ones opencode runs itself
/// (title, summary, compaction) are not modes.
pub(super) fn mode_catalog(response: &Value) -> Vec<ModeInfo> {
    let Some(agents) = response.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    agents
        .iter()
        .filter(|agent| agent.get("mode").and_then(Value::as_str) == Some("primary"))
        .filter(|agent| agent.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|agent| {
            let id = agent.get("id")?.as_str()?.to_string();
            let name = agent
                .get("name")
                .and_then(Value::as_str)
                .map_or_else(|| id.clone(), str::to_string);
            Some(ModeInfo { value: id, name })
        })
        .collect()
}

/// The commands a session accepts as `/name`, in the server's order.
pub(super) fn command_catalog(response: &Value) -> Vec<CommandInfo> {
    response
        .get("data")
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| command.get("name")?.as_str())
                .map(|name| CommandInfo {
                    name: name.to_string(),
                    hint: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What the session offers and what is in force: the catalogue plus the
/// session's current picks, from which the options event is rebuilt whenever
/// one of them changes.
#[derive(Clone, Debug, Default)]
pub(super) struct Catalog {
    pub models: Vec<ModelEntry>,
    pub modes: Vec<ModeInfo>,
    /// The model in force, as `provider/id`.
    pub model: Option<String>,
    /// Its reasoning level; `None` is the model's own default.
    pub variant: Option<String>,
    pub mode: Option<String>,
}

impl Catalog {
    fn current(&self) -> Option<&ModelEntry> {
        let (provider, id) = self.model.as_deref()?.split_once('/')?;
        self.models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == id)
    }

    /// The context window of the model in force.
    pub fn window(&self) -> Option<u64> {
        self.current()?.window
    }

    /// The `Model.Ref` that switches to `model` at `variant`, keeping the
    /// variant only when that model offers it; `None` for a model this
    /// location does not list.
    pub fn model_ref(&self, model: &str, variant: Option<&str>) -> Option<Value> {
        let entry = self.models.iter().find(|entry| entry.value() == model)?;
        let mut reference = json!({"providerID": entry.provider, "id": entry.id});
        if let Some(variant) = variant.filter(|v| entry.variants.iter().any(|known| known == v)) {
            reference["variant"] = json!(variant);
        }
        Some(reference)
    }

    /// The session's options. Reasoning is the model in force's own list, so
    /// switching model changes what the effort picker offers.
    pub fn options(&self, agent: &'static str) -> Event {
        let models = self
            .models
            .iter()
            .map(|entry| ModelInfo {
                value: entry.value(),
                name: entry.name.clone(),
                provider: entry.provider.clone(),
            })
            .collect();
        let reasoning = self
            .current()
            .map(|entry| {
                entry
                    .variants
                    .iter()
                    .map(|variant| ReasoningInfo {
                        value: variant.clone(),
                        name: variant.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        // No access or sandbox axis: opencode's permission rules are its own
        // configuration, which kumu does not override.
        SessionOptionsBuilder::new(agent)
            .models(models, self.model.clone())
            .modes(self.modes.clone(), self.mode.clone())
            .reasoning(reasoning, self.variant.clone())
            .build()
    }
}

/// The body of a prompt: the text, and each image as a `file://` URI the
/// server reads itself — it answered for both a file and a `data:` URI, and the
/// attachment is already a file.
pub(super) fn prompt_body(text: &str, images: &[crate::agent::ImageAttachment]) -> Value {
    let mut body = json!({"text": text});
    if !images.is_empty() {
        body["files"] = images
            .iter()
            .map(|image| {
                let mut file = json!({"uri": format!("file://{}", image.path.display())});
                if let Some(name) = image.path.file_name() {
                    file["name"] = json!(name.to_string_lossy());
                }
                file
            })
            .collect();
    }
    body
}

/// A prompt that names one of the session's commands (`/review main`) as the
/// command and its arguments; `None` for anything else, which is sent as text.
pub(super) fn command_call<'a>(
    text: &'a str,
    commands: &[CommandInfo],
) -> Option<(&'a str, &'a str)> {
    let rest = text.strip_prefix('/')?;
    let name = rest.split(char::is_whitespace).next()?;
    commands
        .iter()
        .any(|command| command.name == name)
        .then(|| (name, rest[name.len()..].trim()))
}

/// The permission options: opencode's three replies, as their own values.
pub(super) fn permission_options() -> Vec<PermissionOptionInfo> {
    vec![
        PermissionOptionInfo {
            id: "once".to_string(),
            name: "允许一次".to_string(),
            kind: PermissionKind::AllowOnce,
        },
        PermissionOptionInfo {
            id: "always".to_string(),
            name: "总是允许".to_string(),
            kind: PermissionKind::AllowAlways,
        },
        PermissionOptionInfo {
            id: "reject".to_string(),
            name: "拒绝".to_string(),
            kind: PermissionKind::RejectOnce,
        },
    ]
}

/// How a turn ended, as the event stream reports it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Settle {
    Succeeded,
    Failed(String),
    Interrupted,
}

impl Settle {
    /// The stop reason and failure the shared settle reports.
    pub fn outcome(self) -> (&'static str, Option<String>) {
        match self {
            Self::Succeeded => ("completed", None),
            Self::Failed(message) => ("error", Some(message)),
            Self::Interrupted => (CANCELLED, None),
        }
    }
}

/// A tool call that has to be authorized before it runs.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PermissionAsk {
    /// The request id the reply goes to.
    pub id: String,
    /// The session that asked: this one, or a subagent's child session.
    pub session_id: String,
    pub tool_call_id: String,
    pub title: String,
    pub detail: String,
}

/// What one event means for the session.
#[derive(Default)]
pub(super) struct Translated {
    pub events: Vec<Event>,
    pub settle: Option<Settle>,
    pub permission: Option<PermissionAsk>,
}

/// The event stream's state across events: what a later event needs from an
/// earlier one.
#[derive(Default)]
pub(super) struct Stream {
    /// Each tool call's name, from `tool.input.started`; the later events of
    /// the same call carry only its id.
    tool_names: HashMap<String, String>,
    /// The text and reasoning blocks that arrived as deltas, so the `ended`
    /// event that repeats them whole is not shown twice.
    streamed: HashSet<String>,
    /// Tokens in the context after the last step, and the session's cost.
    used: Option<u64>,
    cost: Option<f64>,
    /// The context window of the model in force, which the usage reports are
    /// measured against. Set by the session, which is what switches models.
    window: Option<u64>,
    compactions: u64,
}

impl Stream {
    pub fn new(window: Option<u64>) -> Self {
        Self {
            window,
            ..Self::default()
        }
    }

    pub fn set_window(&mut self, window: Option<u64>) {
        self.window = window;
    }

    /// Translate one event of `session_id`'s stream; events of other sessions
    /// and the server's own chatter mean nothing here — except a permission.
    /// The server is private to this session, so any other session on it is
    /// one this session started (a subagent's, through the task tool), and
    /// its question blocks this session's turn until someone answers it.
    pub fn translate(&mut self, event: &Value, session_id: &str) -> Translated {
        let mut out = Translated::default();
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return out;
        };
        let data = event.get("data").unwrap_or(&Value::Null);
        if kind != "permission.asked"
            && data.get("sessionID").and_then(Value::as_str) != Some(session_id)
        {
            return out;
        }
        let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
        let block = |kind: &str| {
            let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
            format!("{kind}:{}:{ordinal}", text("assistantMessageID"))
        };
        match kind {
            "session.text.delta" => {
                self.streamed.insert(block("text"));
                out.events.push(Event::Agent(text("delta").to_string()));
            }
            // The whole block, for a model that sent it without deltas.
            "session.text.ended" if !self.streamed.remove(&block("text")) => {
                if !text("text").is_empty() {
                    out.events.push(Event::Agent(text("text").to_string()));
                }
            }
            "session.reasoning.delta" => {
                self.streamed.insert(block("reasoning"));
                out.events.push(Event::Thought {
                    text: text("delta").to_string(),
                    message: Some(block("reasoning")),
                });
            }
            // Encrypted reasoning ends with empty text: there is nothing to show.
            "session.reasoning.ended" if !self.streamed.remove(&block("reasoning")) => {
                if !text("text").is_empty() {
                    out.events.push(Event::Thought {
                        text: text("text").to_string(),
                        message: Some(block("reasoning")),
                    });
                }
            }
            "session.tool.input.started" => {
                let id = text("id").to_string();
                let name = text("name").to_string();
                self.tool_names.insert(id.clone(), name.clone());
                out.events.push(Event::ToolCall {
                    id,
                    title: name,
                    status: "pending",
                    content: String::new(),
                });
            }
            "session.tool.called" => {
                let id = text("id");
                let name = self
                    .tool_names
                    .get(id)
                    .map(String::as_str)
                    .unwrap_or("tool");
                out.events.push(Event::ToolCall {
                    id: id.to_string(),
                    title: tool_title(name, data.get("input").unwrap_or(&Value::Null)),
                    status: "running",
                    content: String::new(),
                });
            }
            "session.tool.success" => out.events.push(Event::ToolCall {
                id: text("id").to_string(),
                title: String::new(),
                status: "done",
                content: tool_output(data.get("content").unwrap_or(&Value::Null)),
            }),
            "session.tool.failed" => out.events.push(Event::ToolCall {
                id: text("id").to_string(),
                title: String::new(),
                status: "failed",
                content: error_message(data),
            }),
            // A step's own tokens are what the context holds now; the session
            // total in `usage.updated` is not.
            "session.step.ended" | "session.step.failed" => {
                if let Some(tokens) = data.get("tokens") {
                    let count = |pointer: &str| tokens.pointer(pointer).and_then(Value::as_u64);
                    let used = [
                        count("/input"),
                        count("/output"),
                        count("/cache/read"),
                        count("/cache/write"),
                    ]
                    .into_iter()
                    .flatten()
                    .sum::<u64>();
                    if used > 0 {
                        self.used = Some(used);
                        out.events.push(self.usage());
                    }
                }
            }
            "session.usage.updated" => {
                if let Some(cost) = data.get("cost").and_then(Value::as_f64) {
                    self.cost = Some(cost);
                    out.events.push(self.usage());
                }
            }
            "session.renamed" if !text("title").trim().is_empty() => {
                out.events.push(Event::SessionTitle {
                    title: text("title").trim().to_string(),
                });
            }
            "session.compaction.started" => {
                self.compactions += 1;
                out.events.push(self.compaction("running"));
            }
            "session.compaction.ended" => out.events.push(self.compaction("done")),
            "session.compaction.failed" => out.events.push(self.compaction("failed")),
            "permission.asked" => {
                let resources: Vec<&str> = data
                    .get("resources")
                    .and_then(Value::as_array)
                    .map(|r| r.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let action = text("action");
                let title = if resources.is_empty() {
                    action.to_string()
                } else {
                    format!("{action} · {}", resources.join(", "))
                };
                let metadata = data.get("metadata").unwrap_or(&Value::Null);
                let detail = ["diff", "command"]
                    .iter()
                    .find_map(|key| metadata.get(key).and_then(Value::as_str))
                    .map(str::to_string)
                    .unwrap_or_else(|| resources.join("\n"));
                out.permission = Some(PermissionAsk {
                    id: text("id").to_string(),
                    session_id: text("sessionID").to_string(),
                    tool_call_id: data
                        .pointer("/source/id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    title,
                    detail,
                });
            }
            "session.execution.succeeded" => out.settle = Some(Settle::Succeeded),
            "session.execution.failed" => out.settle = Some(Settle::Failed(error_message(data))),
            "session.execution.interrupted" => out.settle = Some(Settle::Interrupted),
            // Lifecycle chatter that carries nothing the UI shows. Listed rather
            // than matched away so that a kind this build has never seen falls
            // through to the log and announces itself.
            "session.text.started"
            | "session.text.ended"
            | "session.reasoning.started"
            | "session.reasoning.ended"
            | "session.tool.input.delta"
            | "session.tool.input.ended"
            | "session.tool.progress"
            | "session.step.started"
            | "session.step.streamed"
            | "session.execution.started"
            | "session.inbox.enqueued"
            | "session.inbox.delivered"
            | "session.instructions.updated"
            | "session.usage.recorded"
            | "session.renamed"
            | "session.created"
            | "session.compaction.delta"
            // The echo of a model or agent switch; the switch itself already
            // updated the options when its request was answered.
            | "session.model.selected"
            | "session.agent.selected"
            | "permission.replied" => {}
            other => out.events.push(Event::Log(format!("update: {other}"))),
        }
        out
    }

    fn usage(&self) -> Event {
        Event::Usage(Usage::reported(
            self.used,
            self.window,
            self.cost.map(Cost::usd),
        ))
    }

    fn compaction(&self, status: &'static str) -> Event {
        Event::ToolCall {
            id: format!("compaction-{}", self.compactions),
            title: "压缩上下文".to_string(),
            status,
            content: String::new(),
        }
    }
}

/// A tool row's title: the tool, and the one argument that says what it is
/// doing. The argument names are opencode's own (`shell` takes `command`, the
/// file tools take `path`; checked against captured turns), with `filePath`
/// kept for the tools that still use it.
fn tool_title(name: &str, input: &Value) -> String {
    let field = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let subject = match name {
        "shell" | "bash" => field("command").map(str::to_string),
        "read" | "write" | "edit" | "list" => field("path")
            .or_else(|| field("filePath"))
            .map(str::to_string),
        "glob" | "grep" => field("pattern").map(str::to_string),
        "webfetch" => field("url").map(str::to_string),
        "websearch" => field("query").map(str::to_string),
        "task" => field("description").map(str::to_string),
        // A patch names its files in its own header lines.
        "patch" => field("patchText").and_then(|patch| {
            let files: Vec<&str> = patch
                .lines()
                .filter_map(|line| {
                    ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                        .iter()
                        .find_map(|prefix| line.strip_prefix(prefix))
                })
                .collect();
            (!files.is_empty()).then(|| files.join(", "))
        }),
        _ => None,
    };
    match subject {
        Some(subject) => format!("{name} · {subject}"),
        None => name.to_string(),
    }
}

/// A tool's output: its text blocks, joined.
fn tool_output(content: &Value) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The `error.message` of a failure event, else its type.
fn error_message(data: &Value) -> String {
    let error = data.get("error").unwrap_or(&Value::Null);
    error
        .get("message")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("opencode 报告了一个错误")
        .to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn captured(name: &str) -> Value {
        let text = match name {
            "turn-shell-tool" => {
                include_str!("../../../schema/opencode-v2/2.0.16/turn-shell-tool.json")
            }
            "turn-permission" => {
                include_str!("../../../schema/opencode-v2/2.0.16/turn-permission.json")
            }
            "turn-interrupt" => {
                include_str!("../../../schema/opencode-v2/2.0.16/turn-interrupt.json")
            }
            "turn-failed-model" => {
                include_str!("../../../schema/opencode-v2/2.0.16/turn-failed-model.json")
            }
            "turn-tool-failed" => {
                include_str!("../../../schema/opencode-v2/2.0.16/turn-tool-failed.json")
            }
            "model-list" => include_str!("../../../schema/opencode-v2/2.0.16/model-list.json"),
            "agent-list" => include_str!("../../../schema/opencode-v2/2.0.16/agent-list.json"),
            "command-list" => include_str!("../../../schema/opencode-v2/2.0.16/command-list.json"),
            _ => unreachable!("{name}"),
        };
        serde_json::from_str(text).unwrap()
    }

    /// Run a captured turn through the translator, as the session's own events.
    fn replay(name: &str, window: Option<u64>) -> Vec<Translated> {
        let events = captured(name);
        let events = events.as_array().unwrap();
        let session = events
            .iter()
            .find_map(|e| e.pointer("/data/sessionID").and_then(Value::as_str))
            .unwrap()
            .to_string();
        let mut stream = Stream::new(window);
        events
            .iter()
            .map(|event| stream.translate(event, &session))
            .collect()
    }

    fn events(translated: &[Translated]) -> Vec<&Event> {
        translated.iter().flat_map(|t| &t.events).collect()
    }

    /// A whole turn: the shell call runs to done under its command, the reply
    /// streams, the context is measured against the window, and the turn
    /// settles as succeeded — with nothing unknown left in the log.
    #[test]
    fn a_captured_turn_becomes_a_tool_a_reply_and_a_settle() {
        let turn = replay("turn-shell-tool", Some(1_050_000));
        let events = events(&turn);
        let reply: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Agent(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reply, "done");
        assert!(events.iter().any(|e| matches!(e,
            Event::ToolCall { title, status: "running", .. } if title == "shell · printf 'hi' > hello.txt")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCall { status: "done", .. }))
        );
        let last_usage = events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Usage(usage) => Some(usage),
                _ => None,
            })
            .unwrap();
        assert_eq!(last_usage.window, Some(1_050_000));
        assert!(last_usage.used.unwrap() > 7_000);
        assert!(
            events.iter().all(|e| !matches!(e, Event::Log(_))),
            "an event kind was not recognised: {:?}",
            events
                .iter()
                .filter_map(|e| match e {
                    Event::Log(line) => Some(line),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(turn.last().unwrap().settle, Some(Settle::Succeeded));
    }

    /// An edit that asks first names its tool call and carries the diff.
    #[test]
    fn a_permission_names_its_tool_and_shows_the_diff() {
        let ask = replay("turn-permission", None)
            .into_iter()
            .find_map(|t| t.permission)
            .unwrap();
        assert!(ask.id.starts_with("per_"));
        assert!(ask.tool_call_id.starts_with("call_"));
        assert_eq!(ask.title, "edit · hello.txt");
        assert!(ask.detail.contains("+hi"));
    }

    /// An interrupted turn settles as cancelled, with the interrupted tool
    /// failed; a failed one settles with the server's own reason.
    #[test]
    fn failures_and_interrupts_settle_as_such() {
        let interrupted = replay("turn-interrupt", None);
        assert!(events(&interrupted).iter().any(|e| matches!(
            e,
            Event::ToolCall {
                status: "failed",
                ..
            }
        )));
        assert_eq!(
            interrupted.last().unwrap().settle,
            Some(Settle::Interrupted)
        );
        let settle = replay("turn-failed-model", None)
            .into_iter()
            .find_map(|t| t.settle)
            .unwrap();
        assert_eq!(
            settle,
            Settle::Failed("Model unavailable: nope/missing-model".to_string())
        );
        let tool_failed = replay("turn-tool-failed", None);
        assert!(events(&tool_failed).iter().any(|e| matches!(e,
            Event::ToolCall { status: "failed", content, .. } if content.contains("File not found"))));
    }

    /// Another session's events are none of this one's business.
    #[test]
    fn other_sessions_are_ignored() {
        let mut stream = Stream::default();
        let event = json!({"type": "session.text.delta",
            "data": {"sessionID": "ses_other", "assistantMessageID": "m", "ordinal": 0, "delta": "x"}});
        assert!(stream.translate(&event, "ses_mine").events.is_empty());
        // A subagent's permission still has to reach the user.
        let asked = json!({"type": "permission.asked",
            "data": {"sessionID": "ses_child", "id": "per_1", "action": "edit"}});
        let ask = stream.translate(&asked, "ses_mine").permission.unwrap();
        assert_eq!(ask.session_id, "ses_child");
    }

    /// A block that streamed is not repeated by its `ended` event; one that did
    /// not stream is shown from it.
    #[test]
    fn an_ended_block_only_fills_in_what_did_not_stream() {
        let mut stream = Stream::default();
        let event = |kind: &str, field: &str| {
            json!({"type": kind, "data": {"sessionID": "s", "assistantMessageID": "m",
                "ordinal": 0, field: "hi"}})
        };
        stream.translate(&event("session.text.delta", "delta"), "s");
        assert!(
            stream
                .translate(&event("session.text.ended", "text"), "s")
                .events
                .is_empty()
        );
        let unstreamed = stream.translate(&event("session.text.ended", "text"), "s");
        assert!(matches!(unstreamed.events.as_slice(), [Event::Agent(text)] if text == "hi"));
    }

    /// The catalogue: models as `provider/id` with their own levels and
    /// window, the primary visible agents as modes, and the commands.
    #[test]
    fn the_catalogue_reads_models_modes_and_commands() {
        let models = model_entries(&captured("model-list"));
        let haiku = models.iter().find(|m| m.id.contains("haiku")).unwrap();
        assert_eq!(
            haiku.value(),
            "commandcode-anthropic/claude-haiku-4-5-20251001"
        );
        assert_eq!(haiku.variants, ["high", "max"]);
        assert_eq!(haiku.window, Some(200_000));

        let modes: Vec<String> = mode_catalog(&captured("agent-list"))
            .into_iter()
            .map(|m| m.value)
            .collect();
        assert_eq!(modes, ["build", "plan"]);

        let commands: Vec<String> = command_catalog(&captured("command-list"))
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(commands, ["init", "review"]);
    }

    /// Reasoning follows the model in force, and a switch keeps a level only
    /// when the new model has it.
    #[test]
    fn reasoning_is_the_current_models_own() {
        let catalog = Catalog {
            models: model_entries(&captured("model-list")),
            model: Some("commandcode-anthropic/claude-haiku-4-5-20251001".into()),
            ..Default::default()
        };
        let Event::SessionOptions { reasoning, .. } = catalog.options("opencode") else {
            unreachable!()
        };
        let levels: Vec<&str> = reasoning.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(levels, ["high", "max"]);
        assert_eq!(catalog.window(), Some(200_000));

        let haiku = "commandcode-anthropic/claude-haiku-4-5-20251001";
        assert_eq!(
            catalog.model_ref(haiku, Some("max")).unwrap()["variant"],
            "max"
        );
        assert!(
            catalog
                .model_ref(haiku, Some("low"))
                .unwrap()
                .get("variant")
                .is_none()
        );
        assert!(catalog.model_ref("nope/nothing", None).is_none());
    }

    #[test]
    fn a_prompt_names_a_command_only_when_it_is_one() {
        let commands = vec![CommandInfo {
            name: "review".into(),
            hint: None,
        }];
        assert_eq!(
            command_call("/review main", &commands),
            Some(("review", "main"))
        );
        assert_eq!(command_call("/review", &commands), Some(("review", "")));
        assert_eq!(command_call("/usr/bin", &commands), None);
        assert_eq!(command_call("review", &commands), None);
    }

    #[test]
    fn images_go_as_file_uris() {
        let image = crate::agent::ImageAttachment {
            path: "/tmp/a/shot.png".into(),
            mime: "image/png".into(),
        };
        assert_eq!(
            prompt_body("看图", &[image]),
            json!({"text": "看图", "files": [{"uri": "file:///tmp/a/shot.png", "name": "shot.png"}]})
        );
        assert_eq!(prompt_body("只有字", &[]), json!({"text": "只有字"}));
    }
}
