//! A non-streaming Claude-compatible Messages client.
//!
//! [`ClaudeClient`] implements [`Transport`] against `POST /v1/messages`. It
//! sends a full conversation history (one or more role-tagged turns), plus any
//! declared [`ToolSpec`]s, and decodes one assistant reply. Message content is
//! sent and parsed as content blocks: `tool_use` blocks in a reply are decoded
//! alongside text, and `tool_result` blocks go out on a follow-up user turn.
//! Streaming and tool execution remain out of scope.
//! The request building and response parsing are pure functions so they can be
//! tested without a network via a fake [`HttpClient`].

use serde::{Deserialize, Serialize};

use crate::config::{Credential, LegConfig};
use crate::error::{LegError, Result};
use crate::model::{
    AssistantReply, ContentBlock, Message, StopReason, TokenUsage, ToolSpec, as_single_text,
};
use crate::transport::Transport;
use crate::transport::http::{HttpClient, UreqHttpClient};

/// The Messages API version pinned by this client.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A Claude-compatible Messages client over an arbitrary [`HttpClient`].
pub struct ClaudeClient<H: HttpClient> {
    config: LegConfig,
    http: H,
    tools: Vec<ToolSpec>,
}

impl ClaudeClient<UreqHttpClient> {
    /// Creates a client that talks to the provider over real HTTP, using the
    /// timeout from `config`.
    pub fn from_config(config: LegConfig) -> Self {
        let http = UreqHttpClient::new(config.timeout);
        Self::with_http(config, http)
    }
}

impl<H: HttpClient> ClaudeClient<H> {
    /// Creates a client over a caller-supplied [`HttpClient`].
    ///
    /// Used by tests to inject a fake transport; production code uses
    /// [`ClaudeClient::from_config`].
    pub fn with_http(config: LegConfig, http: H) -> Self {
        Self {
            config,
            http,
            tools: Vec::new(),
        }
    }

    /// Sets the static tool list advertised on every request.
    ///
    /// An empty list (the default) omits the request's `tools` key entirely.
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// The full Messages endpoint URL for the configured base URL.
    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'))
    }
}

impl<H: HttpClient> Transport for ClaudeClient<H> {
    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
        let body = build_request_body(
            &self.config.model,
            self.config.max_tokens,
            messages,
            self.config.system_prompt.as_deref(),
            &self.tools,
        )?;
        let url = self.endpoint();
        // `auth_value` is bound to this stack frame so the array of header
        // refs below can borrow from it. The OAuth case formats the bearer
        // token once per request; the API-key case clones the key (also
        // once per request). No heap allocation for the headers themselves.
        let (auth_name, auth_value) = auth_header(&self.config.credential);
        let headers = [
            (auth_name, auth_value.as_str()),
            ("anthropic-version", ANTHROPIC_VERSION),
            ("content-type", "application/json"),
        ];

        let response = self.http.post_json(&url, &headers, &body)?;
        parse_response(response.status, &response.body)
    }
}

/// Maps the resolved [`Credential`] onto the wire-level auth header pair.
///
/// The credential is read from the already-resolved config (no env lookup
/// happens per request) and converted into the matching name/value pair:
/// `ApiKey` -> `x-api-key`, `OAuth` -> `Authorization: Bearer <token>`.
///
/// Returns an owned value for the auth header so it can live on the caller's
/// stack frame and be borrowed into the `&[(&str, &str)]` slice that
/// `HttpClient::post_json` requires.
fn auth_header(credential: &Credential) -> (&'static str, String) {
    match credential {
        Credential::ApiKey(key) => ("x-api-key", key.clone()),
        Credential::OAuth(token) => ("Authorization", format!("Bearer {token}")),
    }
}

/// Serializes a Messages request body for `model` carrying `messages` in order.
///
/// Each turn's [`Role`](crate::model::Role) is emitted as its wire `role` value,
/// preserving order so multi-turn history reaches the provider intact. A turn
/// that is exactly one text block is sent as a bare `content` string; any other
/// turn (tool calls, tool results, multiple blocks) is sent as the block array.
/// When
/// `system_prompt` is `Some`, it is emitted as the request's `system` field;
/// `None` omits the field entirely. Likewise `tools` is emitted only when
/// non-empty, so a tool-less request is byte-identical to one built before tool
/// declarations existed.
fn build_request_body(
    model: &str,
    max_tokens: u32,
    messages: &[Message],
    system_prompt: Option<&str>,
    tools: &[ToolSpec],
) -> Result<String> {
    let request = MessagesRequest {
        model,
        max_tokens,
        system: system_prompt,
        messages: messages
            .iter()
            .map(|message| RequestMessage {
                role: message.role.as_str(),
                content: match as_single_text(&message.content) {
                    Some(text) => RequestContent::Text(text),
                    None => RequestContent::Blocks(&message.content),
                },
            })
            .collect(),
        tools,
    };
    serde_json::to_string(&request)
        .map_err(|err| LegError::Transport(format!("failed to serialize request: {err}")))
}

/// Maps an HTTP status and body onto an [`AssistantReply`] or [`LegError`].
///
/// 2xx responses are decoded into a reply; non-2xx statuses become the matching
/// explicit error variant, surfacing the provider's message and optional
/// error type rather than hiding the failure. A `rate_limit_error` is
/// rate-limited regardless of HTTP status.
fn parse_response(status: u16, body: &str) -> Result<AssistantReply> {
    if (200..300).contains(&status) {
        return parse_success(body);
    }

    let (error_type, message) = extract_error_details(body);
    if error_type.as_deref() == Some("rate_limit_error") || status == 429 {
        return Err(LegError::RateLimited(error_message_with_type(
            error_type.as_deref(),
            message,
        )));
    }

    Err(match status {
        401 => LegError::Auth(error_message_with_type(error_type.as_deref(), message)),
        500..=599 => LegError::Server {
            status,
            error_type,
            message,
        },
        _ => LegError::Api {
            status,
            error_type,
            message,
        },
    })
}

/// Decodes a successful Messages response into an [`AssistantReply`].
///
/// `text` and `tool_use` content blocks are kept in order; any other block
/// type (e.g. `thinking`) is skipped so a newer provider shape still decodes.
/// The provider's optional terminal reason is retained as a [`StopReason`]. A
/// body that fails to decode, a malformed `text`/`tool_use` block, or a reply
/// with neither assistant text nor a tool call is a [`LegError::Decode`] — the
/// client never returns a silently empty reply.
fn parse_success(body: &str) -> Result<AssistantReply> {
    let response: MessagesResponse = serde_json::from_str(body)
        .map_err(|err| LegError::Decode(format!("malformed Messages response: {err}")))?;

    let mut content = Vec::new();
    for block in response.content {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") | Some("tool_use") => {
                let block: ContentBlock = serde_json::from_value(block).map_err(|err| {
                    LegError::Decode(format!("malformed response content block: {err}"))
                })?;
                content.push(block);
            }
            _ => {}
        }
    }

    let has_tool_use = content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
    let has_text = content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text } if !text.is_empty()));
    if !has_text && !has_tool_use {
        return Err(LegError::Decode(
            "response contained no assistant text or tool call".to_string(),
        ));
    }

    // A missing `usage` block (or a missing field within it) is recorded as
    // `None`, not an error — usage is observability, never a decode failure.
    let usage = response
        .usage
        .map_or_else(TokenUsage::default, |u| TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });

    Ok(AssistantReply::from_blocks(
        content,
        usage,
        response.stop_reason.as_deref().map(StopReason::from_wire),
    ))
}

/// Pulls the provider error type and `error.message` out of a Claude error
/// body, falling back to the raw body (trimmed) when it is unparseable.
fn extract_error_details(body: &str) -> (Option<String>, String) {
    if let Ok(parsed) = serde_json::from_str::<ErrorResponse>(body) {
        return (parsed.error.error_type, parsed.error.message);
    }
    let trimmed = body.trim();
    let message = if trimmed.is_empty() {
        "no response body".to_string()
    } else {
        trimmed.to_string()
    };
    (None, message)
}

fn error_message_with_type(error_type: Option<&str>, message: String) -> String {
    match error_type {
        Some(error_type) => format!("{error_type}: {message}"),
        None => message,
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<RequestMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolSpec],
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: RequestContent<'a>,
}

/// A request turn's `content`: the Messages API accepts either a bare string
/// or an array of content blocks.
#[derive(Serialize)]
#[serde(untagged)]
enum RequestContent<'a> {
    Text(&'a str),
    Blocks(&'a [ContentBlock]),
}

#[derive(Deserialize)]
struct MessagesResponse {
    /// Raw blocks, filtered by `type` in [`parse_success`] so unknown block
    /// types are skipped rather than failing the decode.
    #[serde(default)]
    content: Vec<serde_json::Value>,
    #[serde(default)]
    usage: Option<UsageBlock>,
    #[serde(default)]
    /// Provider terminal state, such as `end_turn` or `max_tokens`.
    stop_reason: Option<String>,
}

/// The provider's `usage` object. Each count is optional so a partial or absent
/// block degrades to `None` per field rather than failing the decode.
#[derive(Deserialize)]
struct UsageBlock {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default, rename = "type")]
    error_type: Option<String>,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Credential, DEFAULT_MAX_TOKENS};
    use crate::events::{ExchangeMeta, Outcome};
    use crate::message::{MessageEnvelope, MessageKind};
    use crate::model::{Prompt, Role};
    use crate::participant::{LocalParticipant, Participant};
    use crate::tools::ToolLoop;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::Duration;

    /// A fake transport that records the last request and returns a canned
    /// response.
    struct FakeHttp {
        status: u16,
        body: String,
        last_url: RefCell<Option<String>>,
        last_headers: RefCell<Vec<(String, String)>>,
        last_body: RefCell<Option<String>>,
    }

    impl FakeHttp {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
                last_url: RefCell::new(None),
                last_headers: RefCell::new(Vec::new()),
                last_body: RefCell::new(None),
            }
        }
    }

    impl HttpClient for FakeHttp {
        fn post_json(
            &self,
            url: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> Result<crate::transport::http::HttpResponse> {
            *self.last_url.borrow_mut() = Some(url.to_string());
            *self.last_headers.borrow_mut() = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            *self.last_body.borrow_mut() = Some(body.to_string());
            Ok(crate::transport::http::HttpResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    struct ScriptedHttp {
        responses: RefCell<VecDeque<(u16, String)>>,
        calls: Rc<Cell<usize>>,
    }

    impl ScriptedHttp {
        fn new(responses: Vec<(u16, String)>, calls: Rc<Cell<usize>>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                calls,
            }
        }
    }

    impl HttpClient for ScriptedHttp {
        fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &str,
        ) -> Result<crate::transport::http::HttpResponse> {
            self.calls.set(self.calls.get() + 1);
            let Some((status, body)) = self.responses.borrow_mut().pop_front() else {
                return Err(LegError::Transport(
                    "scripted HTTP response queue exhausted".to_string(),
                ));
            };
            Ok(crate::transport::http::HttpResponse { status, body })
        }
    }

    fn config_with(base_url: &str, model: &str) -> LegConfig {
        config_with_credential(
            base_url,
            model,
            Credential::ApiKey("secret-key".to_string()),
        )
    }

    fn config_with_credential(base_url: &str, model: &str, credential: Credential) -> LegConfig {
        LegConfig {
            credential,
            base_url: base_url.to_string(),
            model: model.to_string(),
            timeout: Duration::from_secs(60),
            max_tokens: DEFAULT_MAX_TOKENS,
            max_tool_rounds: None,
            system_prompt: None,
        }
    }

    const SUCCESS_BODY: &str = r#"{
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello there"}],
        "stop_reason": "end_turn"
    }"#;

    #[test]
    fn extracts_assistant_text_from_valid_response() {
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, SUCCESS_BODY),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.text, "Hello there");
        assert_eq!(reply.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn decodes_max_tokens_stop_reason_without_rejecting_reply() {
        let body = r#"{
            "content": [{"type": "text", "text": "unfinished"}],
            "stop_reason": "max_tokens"
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );

        let reply = client.send(&Prompt::new("hi")).expect("should succeed");

        assert_eq!(reply.text, "unfinished");
        assert_eq!(reply.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn success_without_stop_reason_remains_backward_compatible() {
        let body = r#"{
            "content": [{"type": "text", "text": "complete enough"}]
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );

        let reply = client.send(&Prompt::new("hi")).expect("should succeed");

        assert_eq!(reply.text, "complete enough");
        assert_eq!(reply.stop_reason, None);
    }

    #[test]
    fn decodes_token_usage_from_response() {
        let body = r#"{
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 12, "output_tokens": 34}
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.usage.input_tokens, Some(12));
        assert_eq!(reply.usage.output_tokens, Some(34));
    }

    #[test]
    fn success_without_usage_block_records_absent_tokens() {
        // SUCCESS_BODY carries no `usage`: the reply still succeeds and usage is
        // recorded as unknown (None), never a decode error.
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, SUCCESS_BODY),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.text, "Hello there");
        assert_eq!(reply.usage.input_tokens, None);
        assert_eq!(reply.usage.output_tokens, None);
    }

    #[test]
    fn partial_usage_block_records_present_field_only() {
        let body = r#"{
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 7}
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.usage.input_tokens, Some(7));
        assert_eq!(reply.usage.output_tokens, None);
    }

    #[test]
    fn keeps_text_and_tool_use_blocks_in_order() {
        let body = r#"{
            "content": [
                {"type": "text", "text": "part one "},
                {"type": "tool_use", "id": "t1", "name": "x", "input": {}},
                {"type": "text", "text": "part two"}
            ]
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-opus-4-8"),
            FakeHttp::new(200, body),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.text, "part one part two");
        assert_eq!(
            reply.content,
            vec![
                ContentBlock::text("part one "),
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "x".to_string(),
                    input: serde_json::json!({}),
                },
                ContentBlock::text("part two"),
            ]
        );
    }

    #[test]
    fn skips_unknown_block_types() {
        let body = r#"{
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "s"},
                {"type": "text", "text": "answer"}
            ]
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.content, vec![ContentBlock::text("answer")]);
    }

    #[test]
    fn malformed_tool_use_block_is_decode_error() {
        let body = r#"{"content": [{"type": "tool_use", "name": "x"}]}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );
        assert!(matches!(
            client.send(&Prompt::new("hi")).unwrap_err(),
            LegError::Decode(_)
        ));
    }

    #[test]
    fn tool_result_follow_up_serializes_block_arrays() {
        let tool_use = ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"path": "a.txt"}),
        };
        let history = [
            Message::user("read a.txt"),
            Message::new(Role::Assistant, vec![tool_use]),
            Message::new(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".to_string(),
                    content: "hello".to_string(),
                    is_error: Some(false),
                }],
            ),
        ];
        let body = build_request_body("m", 16, &history, None, &[]).expect("serializes");
        assert_eq!(
            body,
            concat!(
                r#"{"model":"m","max_tokens":16,"messages":["#,
                r#"{"role":"user","content":"read a.txt"},"#,
                r#"{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a.txt"}}]},"#,
                r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"hello","is_error":false}]}"#,
                r#"]}"#,
            )
        );
    }

    #[test]
    fn request_uses_configured_endpoint_model_key_and_version() {
        let http = FakeHttp::new(200, SUCCESS_BODY);
        // Trailing slash on the base URL must not double up in the path.
        let client = ClaudeClient::with_http(
            config_with("https://proxy.example/", "claude-test-model"),
            http,
        );
        client
            .send(&Prompt::new("hello world"))
            .expect("should succeed");

        let FakeHttp {
            last_url,
            last_headers,
            last_body,
            ..
        } = &client.http;
        assert_eq!(
            last_url.borrow().as_deref(),
            Some("https://proxy.example/v1/messages")
        );

        let headers = last_headers.borrow();
        assert!(headers.contains(&("x-api-key".to_string(), "secret-key".to_string())));
        assert!(headers.contains(&(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string()
        )));

        let sent = last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        assert_eq!(value["model"], "claude-test-model");
        assert_eq!(value["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "hello world");
    }

    #[test]
    fn request_carries_configured_max_tokens() {
        let mut config = config_with("https://api.anthropic.com", "claude-sonnet-4-6");
        config.max_tokens = 4096;
        let client = ClaudeClient::with_http(config, FakeHttp::new(200, SUCCESS_BODY));
        client.send(&Prompt::new("hi")).expect("should succeed");

        let sent = client.http.last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        assert_eq!(value["max_tokens"], 4096);
    }

    #[test]
    fn send_conversation_serializes_full_history_in_order() {
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, SUCCESS_BODY),
        );
        let history = [
            Message::user("first"),
            Message::assistant("reply one"),
            Message::user("second"),
        ];
        client.send_conversation(&history).expect("should succeed");

        let sent = client.http.last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        let messages = value["messages"].as_array().expect("messages is an array");
        assert_eq!(messages.len(), 3, "the full history is sent, got: {value}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "first");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "reply one");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "second");
    }

    #[test]
    fn request_omits_system_field_when_system_prompt_is_none() {
        let http = FakeHttp::new(200, SUCCESS_BODY);
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            http,
        );
        client.send(&Prompt::new("hi")).expect("should succeed");

        let sent = client.http.last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        assert!(
            value.get("system").is_none(),
            "system field must be absent when system_prompt is None, got: {value}"
        );
    }

    #[test]
    fn request_includes_system_field_when_system_prompt_is_some() {
        let mut config = config_with("https://api.anthropic.com", "claude-sonnet-4-6");
        config.system_prompt = Some("You are a terse agent.".to_string());
        let client = ClaudeClient::with_http(config, FakeHttp::new(200, SUCCESS_BODY));
        client.send(&Prompt::new("hi")).expect("should succeed");

        let sent = client.http.last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        assert_eq!(value["system"], "You are a terse agent.");
    }

    #[test]
    fn text_only_request_body_is_byte_identical() {
        let body =
            build_request_body("m", 16, &[Message::user("hi")], None, &[]).expect("serializes");
        assert_eq!(
            body,
            r#"{"model":"m","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#
        );

        let body = build_request_body("m", 16, &[Message::user("hi")], Some("sys"), &[])
            .expect("serializes");
        assert_eq!(
            body,
            r#"{"model":"m","max_tokens":16,"system":"sys","messages":[{"role":"user","content":"hi"}]}"#
        );
    }

    #[test]
    fn request_includes_tools_when_configured() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, SUCCESS_BODY),
        )
        .with_tools(vec![ToolSpec::new("read", "Read a file.", schema.clone())]);
        client.send(&Prompt::new("hi")).expect("should succeed");

        let sent = client.http.last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        assert_eq!(
            value["tools"],
            serde_json::json!([{
                "name": "read",
                "description": "Read a file.",
                "input_schema": schema
            }])
        );
    }

    #[test]
    fn request_omits_tools_key_when_list_is_empty() {
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, SUCCESS_BODY),
        )
        .with_tools(Vec::new());
        client.send(&Prompt::new("hi")).expect("should succeed");

        let sent = client.http.last_body.borrow();
        let value: serde_json::Value =
            serde_json::from_str(sent.as_deref().unwrap()).expect("body is json");
        assert!(
            value.get("tools").is_none(),
            "tools key must be absent when no tools are declared, got: {value}"
        );
    }

    #[test]
    fn request_oauth_credential_emits_bearer_header_and_no_api_key() {
        let http = FakeHttp::new(200, SUCCESS_BODY);
        let client = ClaudeClient::with_http(
            config_with_credential(
                "https://api.anthropic.com",
                "claude-sonnet-4-6",
                Credential::OAuth("tok-123".to_string()),
            ),
            http,
        );
        client
            .send(&Prompt::new("hello world"))
            .expect("should succeed");

        let FakeHttp { last_headers, .. } = &client.http;
        let headers = last_headers.borrow();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer tok-123"),
            "expected `Authorization: Bearer tok-123` header, got: {headers:?}"
        );
        assert!(
            !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("x-api-key")),
            "OAuth credential must not emit an `x-api-key` header, got: {headers:?}"
        );
        // The other pinned headers still ride along.
        assert!(headers.contains(&(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string()
        )));
        assert!(headers.contains(&("content-type".to_string(), "application/json".to_string())));
    }

    #[test]
    fn unauthorized_maps_to_auth_error() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(401, body),
        );
        match client.send(&Prompt::new("hi")).unwrap_err() {
            LegError::Auth(msg) => {
                assert_eq!(msg, "authentication_error: invalid x-api-key")
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn too_many_requests_maps_to_rate_limited() {
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(429, body),
        );
        match client.send(&Prompt::new("hi")).unwrap_err() {
            LegError::RateLimited(msg) => {
                assert_eq!(msg, "rate_limit_error: slow down")
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_error_type_overrides_server_status() {
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(503, body),
        );
        let err = client.send(&Prompt::new("hi")).unwrap_err();
        assert_eq!(err.kind(), "rate_limited");
        assert!(err.to_string().contains("rate_limit_error"));
        match err {
            LegError::RateLimited(message) => {
                assert_eq!(message, "rate_limit_error: Error")
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn server_error_maps_to_server_variant_with_status() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(503, body),
        );
        match client.send(&Prompt::new("hi")).unwrap_err() {
            LegError::Server {
                status,
                error_type,
                message,
            } => {
                assert_eq!(status, 503);
                assert_eq!(error_type.as_deref(), Some("overloaded_error"));
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn api_error_type_on_server_status_stays_server_and_is_displayed() {
        let body = r#"{"type":"error","error":{"type":"api_error","message":"Error"}}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(500, body),
        );
        let err = client.send(&Prompt::new("hi")).unwrap_err();
        assert_eq!(err.kind(), "server");
        assert_eq!(
            err.to_string(),
            "provider server error (500, api_error): Error"
        );
        match err {
            LegError::Server {
                status,
                error_type,
                message,
            } => {
                assert_eq!(status, 500);
                assert_eq!(error_type.as_deref(), Some("api_error"));
                assert_eq!(message, "Error");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn other_status_maps_to_api_variant() {
        let body =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(400, body),
        );
        match client.send(&Prompt::new("hi")).unwrap_err() {
            LegError::Api {
                status,
                error_type,
                message,
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_type.as_deref(), Some("invalid_request_error"));
                assert_eq!(message, "bad model");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn later_round_bad_request_is_delivered_without_retry() {
        const TOOL_USE_BODY: &str = r#"{
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "echo",
                "input": {"text": "hi"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }"#;
        const ERROR_BODY: &str = r#"{
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "prompt is too long"
            }
        }"#;

        let http_calls = Rc::new(Cell::new(0));
        let tool_calls = Rc::new(Cell::new(0));
        let registry = crate::tools::tests::echo_registry(tool_calls.clone());
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            ScriptedHttp::new(
                vec![
                    (200, TOOL_USE_BODY.to_string()),
                    (400, ERROR_BODY.to_string()),
                ],
                http_calls.clone(),
            ),
        )
        .with_tools(registry.specs());
        let transport = ToolLoop::new(client, registry, None);
        let participant = LocalParticipant::new(
            transport,
            ExchangeMeta {
                model: "claude-sonnet-4-6".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
            },
        );
        let request = MessageEnvelope::new(
            "m-1",
            "c-1",
            "user",
            "assistant",
            MessageKind::Request,
            "hello",
            1_700_000_000_000,
        );

        let response = participant.respond(&request);

        assert_eq!(response.kind, MessageKind::Error);
        assert_eq!(
            response.body,
            "provider error (400, invalid_request_error): prompt is too long"
        );
        assert_eq!(tool_calls.get(), 1);
        assert_eq!(http_calls.get(), 2);
        match &response
            .exchange
            .expect("wrapped exchange")
            .exchange
            .outcome
        {
            Outcome::Error { kind, message, .. } => {
                assert_eq!(kind, "api");
                assert_eq!(
                    message,
                    "provider error (400, invalid_request_error): prompt is too long"
                );
            }
            other => panic!("expected delivered error outcome, got {other:?}"),
        }
    }

    #[test]
    fn error_body_without_json_falls_back_to_raw_text() {
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(502, "  upstream timeout  "),
        );
        match client.send(&Prompt::new("hi")).unwrap_err() {
            LegError::Server {
                status,
                error_type,
                message,
            } => {
                assert_eq!(status, 502);
                assert_eq!(error_type, None);
                assert_eq!(message, "upstream timeout");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn malformed_success_body_is_decode_error() {
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, "not json"),
        );
        assert!(matches!(
            client.send(&Prompt::new("hi")).unwrap_err(),
            LegError::Decode(_)
        ));
    }

    #[test]
    fn tool_only_reply_parses_with_tool_use_stop_reason() {
        let body = r#"{
            "content": [{"type": "tool_use", "id": "t1", "name": "x", "input": {"a": 1}}],
            "stop_reason": "tool_use"
        }"#;
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FakeHttp::new(200, body),
        );
        let reply = client.send(&Prompt::new("hi")).expect("should succeed");
        assert_eq!(reply.text, "");
        assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(
            reply.content,
            vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "x".to_string(),
                input: serde_json::json!({"a": 1}),
            }]
        );
    }

    #[test]
    fn success_with_no_text_or_tool_use_is_decode_error() {
        for body in [
            r#"{"content": []}"#,
            r#"{"content": [{"type": "text", "text": ""}]}"#,
            r#"{"content": [{"type": "thinking", "thinking": "hmm"}]}"#,
        ] {
            let client = ClaudeClient::with_http(
                config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
                FakeHttp::new(200, body),
            );
            assert!(
                matches!(
                    client.send(&Prompt::new("hi")).unwrap_err(),
                    LegError::Decode(_)
                ),
                "expected Decode for {body}"
            );
        }
    }

    /// A fake transport that always returns a transport-level error.
    struct FailingHttp;

    impl HttpClient for FailingHttp {
        fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &str,
        ) -> Result<crate::transport::http::HttpResponse> {
            Err(LegError::Transport("connection timed out".to_string()))
        }
    }

    #[test]
    fn timeout_transport_error() {
        let client = ClaudeClient::with_http(
            config_with("https://api.anthropic.com", "claude-sonnet-4-6"),
            FailingHttp,
        );
        match client.send(&Prompt::new("hi")).unwrap_err() {
            LegError::Transport(msg) => assert!(msg.contains("timed out")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }
}
