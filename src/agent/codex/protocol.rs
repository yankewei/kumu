//! The slice of the Codex `app-server` schema this backend reads.
//!
//! Hand-written, and pinned: the authoritative source for a given Codex version
//! is that version's own
//!
//! ```text
//! codex app-server generate-json-schema --out DIR
//! ```
//!
//! Every field here was checked against **codex-cli 0.153.4**. Re-run the
//! generator against a newer Codex before trusting these shapes; a snapshot of
//! that version's dump is vendored under `schema/codex-app-server/0.153.4/` so
//! the two can be compared without a Codex install.
//!
//! Two deliberate looseness rules, because app-server is a compatibility
//! surface between two independently versioned programs:
//!   - status fields are `String`, not enums, so an unfamiliar value degrades to
//!     "leave the row alone" instead of failing the whole notification;
//!   - unknown thread-item kinds parse into [`ThreadItem::Unknown`] rather than
//!     erroring, so a new Codex item type cannot break the stream.
//!
//! Unknown fields are ignored throughout (serde's default), which is what keeps
//! this slice viable across Codex releases.

use serde::Deserialize;
use serde_json::Value;

/// Result of `initialize`.
#[derive(Debug, Deserialize)]
pub(super) struct InitializeResult {
    /// The `${CODEX_HOME}` the server is using.
    #[serde(rename = "codexHome", default)]
    pub codex_home: String,
    /// The user agent the server presents upstream; useful in logs.
    #[serde(rename = "userAgent", default)]
    pub user_agent: String,
}

/// Result of `account/read`.
#[derive(Debug, Deserialize)]
pub(super) struct AccountResult {
    /// The signed-in account, or `null` when nobody is signed in.
    #[serde(default)]
    pub account: Option<Account>,
    /// Whether the active provider needs OpenAI credentials to run at all.
    #[serde(rename = "requiresOpenaiAuth", default)]
    pub requires_openai_auth: bool,
}

/// The account variants `account/read` reports. Only whether one exists is
/// used, so the plan and email fields are not modelled.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum Account {
    ApiKey,
    Chatgpt,
    AmazonBedrock,
    /// A mode this build does not model; still a signed-in account.
    #[serde(other)]
    Other,
}

/// Result of `model/list`.
#[derive(Debug, Deserialize)]
pub(super) struct ModelListResult {
    #[serde(default)]
    pub data: Vec<Model>,
}

/// One entry of `model/list`. Only the fields the picker needs are modelled.
#[derive(Debug, Deserialize)]
pub(super) struct Model {
    /// The value to send back as `model` on a turn.
    pub model: String,
    /// The label to show; falls back to the id when absent.
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    /// Models Codex hides from its own picker.
    #[serde(default)]
    pub hidden: bool,
    /// The model the agent recommends; used as the active value until a
    /// session reports its own.
    #[serde(rename = "isDefault", default)]
    pub is_default: bool,
    /// The effort the agent uses for this model when none is overridden.
    #[serde(rename = "defaultReasoningEffort", default)]
    pub default_reasoning_effort: Option<String>,
    /// The efforts this model advertises; the set differs per model, so this is
    /// the catalogue the reasoning picker reads.
    #[serde(rename = "supportedReasoningEfforts", default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

/// One effort a model advertises. The wire also carries a `description` of it,
/// which kumu does not show — the value is the label — so it is left unparsed.
#[derive(Debug, Deserialize)]
pub(super) struct ReasoningEffortOption {
    /// The value to send back as `effort` on a turn.
    #[serde(rename = "reasoningEffort", default)]
    pub reasoning_effort: String,
}

/// Result of `thread/start` or `thread/resume` (the same response shape).
#[derive(Debug, Deserialize)]
pub(super) struct ThreadStartResult {
    pub thread: ThreadSummary,
    /// The provider the thread runs against, for example `openai`.
    #[serde(rename = "modelProvider", default)]
    pub model_provider: String,
    /// The model the thread actually runs, once the agent has resolved the
    /// request. Empty when the agent reported none.
    #[serde(default)]
    pub model: String,
    /// The sandbox policy in force for the thread, as the agent resolved it.
    /// This echo is the only place kumu can learn the effective policy, so it is
    /// what the picker labels itself with.
    #[serde(default)]
    pub sandbox: Option<SandboxPolicy>,
    /// The reasoning effort in force for the thread, as the agent resolved it.
    /// Like the sandbox, this echo is the only place the effective value can be
    /// learned — `thread/start` itself cannot be asked to set one.
    #[serde(rename = "reasoningEffort", default)]
    pub reasoning_effort: Option<String>,
}

/// The sandbox policy a thread reports it is running under.
///
/// Only the tag is read: it tells the picker which value is in force. The
/// variant fields (writable roots, network access) are the agent's business.
/// Note that this is the *object* form — `thread/start` accepts the same
/// policies as kebab-case strings, and `turn/start` takes this form back.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum SandboxPolicy {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    /// The host already sandboxes the process; not a policy kumu offers.
    ExternalSandbox,
    /// A policy this build does not know.
    #[serde(other)]
    Other,
}

/// The identifying part of a thread.
#[derive(Debug, Deserialize)]
pub(super) struct ThreadSummary {
    pub id: String,
    /// The thread's user-facing title, when it already has one. A freshly
    /// started thread does not — `thread/start` accepts no name, so the title
    /// is generated as the conversation develops and arrives later as a
    /// `thread/name/updated` notification. A resumed thread does have one.
    #[serde(default)]
    pub name: Option<String>,
}

/// Result of `turn/start`.
#[derive(Debug, Deserialize)]
pub(super) struct TurnStartResult {
    pub turn: Turn,
}

/// A turn, as carried by `turn/start` responses and `turn/completed`.
#[derive(Debug, Deserialize)]
pub(super) struct Turn {
    /// The turn's id; Codex generates UUIDv7.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    /// Only populated when the turn failed.
    #[serde(default)]
    pub error: Option<TurnError>,
}

/// Why a turn failed.
#[derive(Debug, Deserialize)]
pub(super) struct TurnError {
    pub message: String,
}

/// Params of `item/started` and `item/completed`.
#[derive(Debug, Deserialize)]
pub(super) struct ItemNotification {
    pub item: ThreadItem,
}

/// Result of `skills/list`: the skills in reach of each working directory asked
/// about, one entry per directory.
#[derive(Debug, Deserialize)]
pub(super) struct SkillsListResult {
    #[serde(default)]
    pub data: Vec<SkillsListEntry>,
}

/// One working directory's skills. Its `cwd` and load `errors` are on the wire
/// and not modelled: kumu asks about one directory at a time, and a skill that
/// failed to load is simply not offered.
#[derive(Debug, Deserialize)]
pub(super) struct SkillsListEntry {
    #[serde(default)]
    pub skills: Vec<Skill>,
}

/// One skill. `path` is what a structured `skill` input names it by, so an
/// invocation cannot be mistaken for a same-named skill in another scope.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(super) struct Skill {
    pub name: String,
    pub path: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// `repo`, `user`, `system` or `admin`: whether the skill belongs to the
    /// working directory it was listed for or goes wherever Codex does.
    #[serde(default)]
    pub scope: String,
}

fn enabled_by_default() -> bool {
    true
}

/// Params of the streaming notifications, which all carry a chunk plus the item
/// it belongs to.
#[derive(Debug, Deserialize)]
pub(super) struct DeltaNotification {
    #[serde(rename = "itemId", default)]
    pub item_id: String,
    pub delta: String,
}

/// Params of `turn/completed`.
#[derive(Debug, Deserialize)]
pub(super) struct TurnCompletedNotification {
    pub turn: Turn,
}

/// Params of the `error` notification.
#[derive(Debug, Deserialize)]
pub(super) struct ErrorNotification {
    pub error: TurnError,
    /// Codex retries transient failures itself; only a final error is worth
    /// showing the user.
    #[serde(rename = "willRetry", default)]
    pub will_retry: bool,
}

/// Params of `thread/name/updated`: the thread's title as the agent now reports
/// it.
///
/// `threadId` is on the wire and deliberately not modelled, matching every
/// other notification here: kumu runs one thread per process, so there is
/// nothing to demultiplex.
#[derive(Debug, Deserialize)]
pub(super) struct ThreadNameUpdatedNotification {
    /// The new title, or `None` when the agent cleared it. `null` is a normal
    /// value for this field, and a cleared name is not a title to display.
    #[serde(rename = "threadName", default)]
    pub thread_name: Option<String>,
}

/// Params of `item/commandExecution/requestApproval`.
#[derive(Debug, Deserialize)]
pub(super) struct CommandApprovalParams {
    #[serde(rename = "itemId", default)]
    pub item_id: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Params of `item/fileChange/requestApproval`. The request itself describes no
/// change — only the item id — so the diff comes from the item we saw start.
#[derive(Debug, Deserialize)]
pub(super) struct FileChangeApprovalParams {
    #[serde(rename = "itemId", default)]
    pub item_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    /// Set when the agent wants write access to a whole root.
    #[serde(rename = "grantRoot", default)]
    pub grant_root: Option<String>,
}

/// Codex's best-effort description of what a shell command will do. A single
/// command can contain more than one action, such as reading a file and then
/// searching its output.
#[derive(Debug, Deserialize)]
pub(super) struct CommandAction {
    #[serde(rename = "type")]
    pub kind: String,
}

/// One unit of work inside a turn. Only the variants kumu renders are modelled;
/// the tag decides which, and anything else becomes [`ThreadItem::Unknown`].
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum ThreadItem {
    AgentMessage {
        #[serde(default)]
        id: String,
        /// The whole message. Normally it has already streamed as deltas; a
        /// review's findings arrive only here.
        #[serde(default)]
        text: String,
    },
    Reasoning {
        #[serde(default)]
        id: String,
    },
    CommandExecution {
        #[serde(default)]
        id: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        status: String,
        /// The command's output, aggregated by the agent. The streaming
        /// `outputDelta` notifications carry the same text in pieces.
        #[serde(rename = "aggregatedOutput", default)]
        aggregated_output: Option<String>,
        /// Best-effort action names for friendly display.
        #[serde(rename = "commandActions", default)]
        command_actions: Vec<CommandAction>,
    },
    FileChange {
        #[serde(default)]
        id: String,
        #[serde(default)]
        changes: Vec<FileUpdateChange>,
        #[serde(default)]
        status: String,
    },
    McpToolCall {
        #[serde(default)]
        id: String,
        #[serde(default)]
        server: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        error: Option<McpToolCallError>,
        #[serde(default)]
        result: Option<McpToolCallResult>,
    },
    WebSearch {
        #[serde(default)]
        id: String,
        #[serde(default)]
        query: String,
    },
    Plan {
        #[serde(default)]
        id: String,
    },
    /// The conversation history being summarised, from `thread/compact/start`
    /// or Codex's own automatic compaction.
    ContextCompaction {
        #[serde(default)]
        id: String,
    },
    #[serde(other)]
    Unknown,
}

impl ThreadItem {
    /// The item's id; empty for a kind this build does not model.
    pub(super) fn id(&self) -> &str {
        match self {
            Self::AgentMessage { id, .. }
            | Self::Reasoning { id }
            | Self::CommandExecution { id, .. }
            | Self::FileChange { id, .. }
            | Self::McpToolCall { id, .. }
            | Self::WebSearch { id, .. }
            | Self::Plan { id }
            | Self::ContextCompaction { id } => id,
            Self::Unknown => "",
        }
    }
}

/// One proposed file modification: the path plus its unified diff.
#[derive(Debug, Deserialize)]
pub(super) struct FileUpdateChange {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub diff: String,
}

/// A failed MCP tool call.
#[derive(Debug, Deserialize)]
pub(super) struct McpToolCallError {
    #[serde(default)]
    pub message: String,
}

/// A successful MCP tool call. The content items are MCP's own shapes, so they
/// stay opaque here and are rendered on the way out.
#[derive(Debug, Deserialize)]
pub(super) struct McpToolCallResult {
    #[serde(default)]
    pub content: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag decides the variant, and a kind this build has never seen must
    /// parse rather than break the notification stream.
    #[test]
    fn unknown_item_kinds_do_not_fail_the_parse() {
        let item: ThreadItem =
            serde_json::from_str(r#"{"type":"somethingNew","id":"i1"}"#).unwrap();
        assert!(matches!(item, ThreadItem::Unknown));
        assert_eq!(item.id(), "");
    }

    /// Item fields are camelCase on the wire and are renamed accordingly.
    #[test]
    fn command_execution_reads_camel_case_fields() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"type":"commandExecution","id":"c1","command":"ls","cwd":"/tmp",
                "status":"inProgress","aggregatedOutput":"a\nb","commandActions":[]}"#,
        )
        .unwrap();
        match item {
            ThreadItem::CommandExecution {
                command,
                status,
                aggregated_output,
                ..
            } => {
                assert_eq!(command, "ls");
                assert_eq!(status, "inProgress");
                assert_eq!(aggregated_output.as_deref(), Some("a\nb"));
            }
            other => panic!("expected commandExecution, got {other:?}"),
        }
    }

    /// A status value this build does not know must survive as-is; the mapper,
    /// not the parser, decides what to do with it.
    #[test]
    fn an_unknown_status_still_parses() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"type":"fileChange","id":"f1","changes":[],"status":"somethingNew"}"#,
        )
        .unwrap();
        match item {
            ThreadItem::FileChange { status, .. } => assert_eq!(status, "somethingNew"),
            other => panic!("expected fileChange, got {other:?}"),
        }
    }

    /// `account/read` reports an unauthenticated caller without failing.
    #[test]
    fn account_read_parses_the_signed_out_case() {
        let result: AccountResult =
            serde_json::from_str(r#"{"account":null,"requiresOpenaiAuth":true}"#).unwrap();
        assert!(result.account.is_none());
        assert!(result.requires_openai_auth);
    }

    /// A signed-in ChatGPT account is recognised, and its plan fields (which
    /// this build does not use) do not have to be modelled.
    #[test]
    fn account_read_parses_a_chatgpt_account() {
        let result: AccountResult = serde_json::from_str(
            r#"{"account":{"type":"chatgpt","email":"a@b.c","planType":"pro"},
                "requiresOpenaiAuth":true}"#,
        )
        .unwrap();
        assert!(matches!(result.account, Some(Account::Chatgpt)));
    }

    /// `model/list` carries the value to send back and the label to show.
    #[test]
    fn model_list_reads_the_picker_fields() {
        let result: ModelListResult = serde_json::from_str(
            r#"{"data":[{"id":"gpt-5.6-sol","model":"gpt-5.6-sol",
                "displayName":"GPT-5.6-Sol","description":"d","hidden":false,
                "isDefault":true,"defaultReasoningEffort":"low",
                "supportedReasoningEfforts":[
                    {"reasoningEffort":"low","description":"Fast responses with lighter reasoning"},
                    {"reasoningEffort":"high","description":"Greater reasoning depth"}]}],
                "nextCursor":null}"#,
        )
        .unwrap();
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].model, "gpt-5.6-sol");
        assert_eq!(result.data[0].display_name, "GPT-5.6-Sol");
        assert!(result.data[0].is_default);
        assert_eq!(
            result.data[0].default_reasoning_effort.as_deref(),
            Some("low")
        );
        assert_eq!(
            result.data[0]
                .supported_reasoning_efforts
                .iter()
                .map(|effort| effort.reasoning_effort.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"],
            "the agent's own order is kept, and the description is not parsed"
        );
    }

    /// An error Codex intends to retry is still well-formed; the mapper decides
    /// whether to surface it.
    #[test]
    fn error_notification_reads_will_retry() {
        let notification: ErrorNotification = serde_json::from_str(
            r#"{"error":{"message":"boom"},"threadId":"t1","turnId":"u1","willRetry":true}"#,
        )
        .unwrap();
        assert_eq!(notification.error.message, "boom");
        assert!(notification.will_retry);
    }

    /// The title the agent reports for a thread, and the two ways it can say
    /// there is none: `null`, or the field left out entirely. Both must read as
    /// absent rather than failing the parse, since a cleared name is a normal
    /// state and not an error.
    #[test]
    fn thread_name_updated_reads_the_title_or_its_absence() {
        let named: ThreadNameUpdatedNotification =
            serde_json::from_str(r#"{"threadId":"t1","threadName":"排查网络质量问题"}"#).unwrap();
        assert_eq!(named.thread_name.as_deref(), Some("排查网络质量问题"));

        let cleared: ThreadNameUpdatedNotification =
            serde_json::from_str(r#"{"threadId":"t1","threadName":null}"#).unwrap();
        assert!(cleared.thread_name.is_none());

        let omitted: ThreadNameUpdatedNotification =
            serde_json::from_str(r#"{"threadId":"t1"}"#).unwrap();
        assert!(omitted.thread_name.is_none());
    }

    /// A thread carries its title when it has one. A freshly started thread
    /// omits the field, which must parse as absent rather than break the
    /// handshake.
    #[test]
    fn thread_start_reads_the_threads_name_when_present() {
        let named: ThreadStartResult =
            serde_json::from_str(r#"{"thread":{"id":"t1","name":"排查网络质量问题"}}"#).unwrap();
        assert_eq!(named.thread.id, "t1");
        assert_eq!(named.thread.name.as_deref(), Some("排查网络质量问题"));

        let fresh: ThreadStartResult =
            serde_json::from_str(r#"{"thread":{"id":"t2"},"model":"gpt-5.6-sol"}"#).unwrap();
        assert_eq!(fresh.thread.id, "t2");
        assert!(fresh.thread.name.is_none());
    }
}
