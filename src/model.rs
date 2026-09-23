//! Typed data structures for the prompt/reply and multi-turn session flows.
//!
//! [`Prompt`] and [`AssistantReply`] model the single-turn `ask` path. Multi-turn
//! sessions build on [`Message`] (a role-tagged turn) and [`Conversation`] (the
//! accumulated history that is resent with every request). A message's content
//! is an ordered list of [`ContentBlock`]s — `text`, `tool_use`, and
//! `tool_result` — so tool calls and their results round-trip through the
//! transport, the trail, and `--resume`. [`ToolSpec`] declares a tool the
//! provider may call and [`StopReason`] reports why a reply ended; executing
//! tools and iterating on `tool_use` stop reasons (the agent loop) and
//! streaming remain out of scope.

use serde::{Deserialize, Serialize};

/// The author of a single conversation turn.
///
/// Maps 1:1 onto the Messages API `role` field; [`Role::as_str`] is the wire
/// value the transport serializes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A turn authored by the user / calling agent.
    User,
    /// A turn authored by the assistant (a prior reply).
    Assistant,
}

impl Role {
    /// The Messages API wire value for this role.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One block of a message's content, mirroring a Messages API content block.
///
/// Serializes with the wire `type` tag (`text` / `tool_use` / `tool_result`),
/// so the same shape is sent to the provider, parsed from its replies, and
/// recorded on the trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text content.
        text: String,
    },
    /// A tool call requested by the assistant.
    ToolUse {
        /// Provider-assigned call id, echoed back by the matching result.
        id: String,
        /// The called tool's name.
        name: String,
        /// The call's arguments, as the JSON object the provider sent.
        input: serde_json::Value,
    },
    /// The result of a tool call, sent back on a user turn.
    ToolResult {
        /// The [`ContentBlock::ToolUse::id`] this result answers.
        tool_use_id: String,
        /// The tool's output text.
        content: String,
        /// Whether the tool call failed; omitted when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

impl ContentBlock {
    /// Creates a text block from anything string-like.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }
}

/// Concatenates the `text` blocks of `blocks` in order, ignoring tool blocks.
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Returns the lone text of `blocks` when they are exactly one text block.
///
/// This is the "text-only" shape: the transport sends it as a bare string and
/// the trail omits its `content` field, keeping text-only traffic byte-identical
/// to the pre-block wire format.
pub fn as_single_text(blocks: &[ContentBlock]) -> Option<&str> {
    match blocks {
        [ContentBlock::Text { text }] => Some(text),
        _ => None,
    }
}

/// A single role-tagged turn in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Who authored this turn.
    pub role: Role,
    /// The turn's content blocks, in order.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Creates a turn from explicit content blocks.
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    /// Creates a user turn holding one text block.
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentBlock::text(text)])
    }

    /// Creates an assistant turn holding one text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![ContentBlock::text(text)])
    }

    /// The turn's text blocks concatenated in order.
    pub fn text(&self) -> String {
        blocks_text(&self.content)
    }
}

/// An ordered, in-memory accumulation of conversation turns.
///
/// This is the unit-testable core of a multi-turn session: each turn is appended
/// in order, and [`Conversation::messages`] returns the full history that is
/// resent with every request. It deliberately holds no provider state — it is
/// pure data the transport reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    /// Creates an empty conversation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a turn.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Appends a user turn.
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(Message::user(content));
    }

    /// Appends an assistant turn.
    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.messages.push(Message::assistant(content));
    }

    /// Removes and returns the most recent turn, if any.
    ///
    /// Used to roll back a just-appended user turn when its request fails, so
    /// the history never holds two consecutive same-role turns (which the
    /// Messages API rejects).
    pub fn pop(&mut self) -> Option<Message> {
        self.messages.pop()
    }

    /// The full history in order, oldest turn first.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The number of accumulated turns.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether no turns have been accumulated yet.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// A single user prompt to send to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// The prompt text.
    pub text: String,
}

impl Prompt {
    /// Creates a prompt from anything string-like.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// A tool declaration advertised to the provider on each request.
///
/// Serializes directly to one entry of the Messages API `tools` array
/// (`{name, description, input_schema}`), where `input_schema` is a JSON Schema
/// object describing the tool's arguments. Declaring a tool only makes it
/// visible to the model; nothing here executes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    /// The tool name the model uses to call it.
    pub name: String,
    /// What the tool does, shown to the model.
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub input_schema: serde_json::Value,
}

impl ToolSpec {
    /// Creates a tool declaration.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Provider-reported token usage for a single call.
///
/// Each count is optional: a `2xx` response may omit the `usage` block (or a
/// field within it) entirely, in which case that count is `None` (unknown)
/// rather than an error. This is the token-accounting surface the exchange
/// trail records for cost/observability and the future budget governor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Input (prompt) tokens the provider billed, if reported.
    pub input_tokens: Option<u64>,
    /// Output (completion) tokens the provider billed, if reported.
    pub output_tokens: Option<u64>,
}

/// Why the provider stopped generating a reply.
///
/// Maps the Messages API `stop_reason` string; a value this client does not
/// know is kept verbatim in [`StopReason::Other`] rather than rejected. An
/// omitted `stop_reason` is modeled as `Option::None` (unknown), not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The reply hit the `max_tokens` limit and is truncated.
    MaxTokens,
    /// A custom stop sequence was generated.
    StopSequence,
    /// The model requested one or more tool calls.
    ToolUse,
    /// The provider paused a long-running turn.
    PauseTurn,
    /// The model declined to answer.
    Refusal,
    /// Any other provider value, preserved as sent.
    Other(String),
}

impl StopReason {
    /// Parses a wire `stop_reason` value.
    pub fn from_wire(value: &str) -> Self {
        match value {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "tool_use" => StopReason::ToolUse,
            "pause_turn" => StopReason::PauseTurn,
            "refusal" => StopReason::Refusal,
            other => StopReason::Other(other.to_string()),
        }
    }

    /// The wire value for this stop reason.
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ToolUse => "tool_use",
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::Other(value) => value,
        }
    }
}

/// A single assistant reply returned by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantReply {
    /// The reply's text blocks concatenated in order; empty for a tool-only
    /// reply.
    pub text: String,
    /// The reply's content blocks, in order (text and `tool_use`).
    pub content: Vec<ContentBlock>,
    /// Provider-reported token usage for the call, when available.
    pub usage: TokenUsage,
    /// Provider-reported terminal reason, when available.
    pub stop_reason: Option<StopReason>,
}

impl AssistantReply {
    /// Creates a text reply from anything string-like, with no usage recorded.
    pub fn new(text: impl Into<String>) -> Self {
        Self::with_usage(text, TokenUsage::default())
    }

    /// Creates a text reply carrying the provider's reported token usage.
    pub fn with_usage(text: impl Into<String>, usage: TokenUsage) -> Self {
        Self::from_blocks(vec![ContentBlock::text(text)], usage, None)
    }

    /// Creates a reply from its content blocks, usage, and terminal reason;
    /// [`AssistantReply::text`] is derived from the text blocks.
    pub fn from_blocks(
        content: Vec<ContentBlock>,
        usage: TokenUsage,
        stop_reason: Option<StopReason>,
    ) -> Self {
        Self {
            text: blocks_text(&content),
            content,
            usage,
            stop_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_new_accepts_str_and_string() {
        assert_eq!(Prompt::new("hi"), Prompt::new(String::from("hi")));
        assert_eq!(Prompt::new("hi").text, "hi");
    }

    #[test]
    fn reply_new_stores_text() {
        assert_eq!(AssistantReply::new("ok").text, "ok");
        assert_eq!(AssistantReply::new("ok").stop_reason, None);
    }

    #[test]
    fn role_wire_values() {
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
    }

    #[test]
    fn message_constructors_tag_the_role() {
        assert_eq!(
            Message::user("hi"),
            Message::new(Role::User, vec![ContentBlock::text("hi")])
        );
        assert_eq!(
            Message::assistant("yo"),
            Message::new(Role::Assistant, vec![ContentBlock::text("yo")])
        );
    }

    #[test]
    fn content_blocks_serialize_with_wire_type_tags_and_round_trip() {
        let blocks = vec![
            ContentBlock::text("hi"),
            ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "lookup".to_string(),
                input: serde_json::json!({"q": "x"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "found".to_string(),
                is_error: None,
            },
        ];
        let json = serde_json::to_value(&blocks).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"},
            ])
        );
        let back: Vec<ContentBlock> = serde_json::from_value(json).unwrap();
        assert_eq!(back, blocks);
    }

    #[test]
    fn text_helpers_ignore_tool_blocks() {
        let tool = ContentBlock::ToolUse {
            id: "t".to_string(),
            name: "n".to_string(),
            input: serde_json::json!({}),
        };
        let blocks = vec![
            ContentBlock::text("a"),
            tool.clone(),
            ContentBlock::text("b"),
        ];
        assert_eq!(blocks_text(&blocks), "ab");
        assert_eq!(as_single_text(&blocks), None);
        assert_eq!(as_single_text(&[ContentBlock::text("a")]), Some("a"));
        assert_eq!(as_single_text(&[tool]), None);
    }

    #[test]
    fn stop_reason_round_trips_known_and_unknown_wire_values() {
        for wire in [
            "end_turn",
            "max_tokens",
            "stop_sequence",
            "tool_use",
            "pause_turn",
            "refusal",
            "something_new",
        ] {
            assert_eq!(StopReason::from_wire(wire).as_str(), wire);
        }
        assert_eq!(StopReason::from_wire("tool_use"), StopReason::ToolUse);
        assert_eq!(
            StopReason::from_wire("something_new"),
            StopReason::Other("something_new".to_string())
        );
    }

    #[test]
    fn conversation_starts_empty() {
        let convo = Conversation::new();
        assert!(convo.is_empty());
        assert_eq!(convo.len(), 0);
        assert_eq!(convo.messages(), &[]);
    }

    #[test]
    fn conversation_accumulates_turns_in_order() {
        let mut convo = Conversation::new();
        convo.push_user("a");
        convo.push_assistant("b");
        convo.push_user("c");

        assert_eq!(convo.len(), 3);
        assert!(!convo.is_empty());
        assert_eq!(
            convo.messages(),
            &[
                Message::user("a"),
                Message::assistant("b"),
                Message::user("c"),
            ]
        );
    }

    #[test]
    fn conversation_pop_removes_most_recent_turn() {
        let mut convo = Conversation::new();
        convo.push_user("a");
        convo.push_assistant("b");

        assert_eq!(convo.pop(), Some(Message::assistant("b")));
        assert_eq!(convo.messages(), &[Message::user("a")]);
        assert_eq!(convo.pop(), Some(Message::user("a")));
        assert_eq!(convo.pop(), None);
        assert!(convo.is_empty());
    }
}
