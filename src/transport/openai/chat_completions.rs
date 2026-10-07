//! OpenAI-compatible Chat Completions transport.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::config::LegConfig;
use crate::error::{LegError, Result};
use crate::model::{
    AssistantReply, ContentBlock, ImageSource, Message, Role, StopReason, TokenUsage, ToolSpec,
};
use crate::transport::http::{HttpClient, UreqHttpClient};
use crate::transport::sse::{SseDecoder, SseEvent};
use crate::transport::{RetryingHttpClient, StreamEvent, Transport, TransportCall};

use super::common;

/// A transport for `POST /v1/chat/completions`.
pub struct OpenAiChatCompletionsClient<H: HttpClient> {
    config: LegConfig,
    http: H,
    tools: Vec<ToolSpec>,
}

impl OpenAiChatCompletionsClient<RetryingHttpClient<UreqHttpClient>> {
    /// Creates a client that uses the configured timeout and retry policy.
    pub fn from_config(config: LegConfig) -> Self {
        let http = common::real_http(&config);
        Self::with_http(config, http)
    }
}

impl<H: HttpClient> OpenAiChatCompletionsClient<H> {
    /// Creates a client over a caller-supplied HTTP implementation.
    pub fn with_http(config: LegConfig, http: H) -> Self {
        Self {
            config,
            http,
            tools: Vec::new(),
        }
    }

    /// Sets the tools advertised on every request.
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }
}

impl<H: HttpClient> Transport for OpenAiChatCompletionsClient<H> {
    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
        self.send_conversation_with_attempts(messages).result
    }

    fn send_conversation_with_attempts(
        &self,
        messages: &[Message],
    ) -> TransportCall<AssistantReply> {
        self.send_conversation_streaming_with_attempts(messages, &mut |_| Ok(()))
    }

    fn send_conversation_streaming_with_attempts(
        &self,
        messages: &[Message],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<AssistantReply> {
        let body = match build_request_body(&self.config, messages, &self.tools) {
            Ok(body) => body,
            Err(error) => return TransportCall::completed(Err(error), 0),
        };
        let token = match common::bearer_token(&self.config.credential) {
            Ok(token) => token,
            Err(error) => return TransportCall::completed(Err(error), 0),
        };
        let auth_value = format!("Bearer {token}");
        let headers = [
            ("Authorization", auth_value.as_str()),
            ("content-type", "application/json"),
        ];
        let url = common::endpoint(&self.config.base_url, "chat/completions");

        let mut decoder = SseDecoder::default();
        let mut assembler = ChatAssembler {
            max_tokens: self.config.max_tokens,
            ..ChatAssembler::default()
        };
        let call = {
            let mut accept_event = |event| assembler.accept(event, on_event);
            self.http
                .post_json_streaming(&url, &headers, &body, &mut |chunk| {
                    decoder.push(chunk, &mut accept_event)
                })
        };
        let attempts = call.attempts;
        let result = match call.result {
            Err(error) => Err(error),
            Ok(response) if (200..300).contains(&response.status) => {
                let result = {
                    let mut accept_event = |event| assembler.accept(event, on_event);
                    decoder.finish(&mut accept_event)
                };
                result.and_then(|()| assembler.finish())
            }
            Ok(response) => Err(common::api_error(response.status, &response.body)),
        };
        TransportCall::new(result, attempts)
    }
}

fn build_request_body(
    config: &LegConfig,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Result<String> {
    let mut api_messages = Vec::new();
    if let Some(system_prompt) = &config.system_prompt {
        api_messages.push(json!({"role": "system", "content": system_prompt}));
    }
    api_messages.extend(chat_messages(messages)?);

    let mut request = json!({
        "model": config.model,
        "messages": api_messages,
        "max_tokens": config.max_tokens,
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    if !tools.is_empty() {
        request["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema
                        }
                    })
                })
                .collect(),
        );
    }

    common::serialize_request(&request, messages, "Chat Completions")
}

fn chat_messages(messages: &[Message]) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut text = String::new();
                let mut parts = Vec::new();
                let mut has_text = false;
                let mut has_image = false;
                let mut tool_results = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: value } => {
                            has_text = true;
                            text.push_str(value);
                            parts.push(json!({"type": "text", "text": value}));
                        }
                        ContentBlock::Image {
                            source: source @ ImageSource::Base64 { .. },
                        } => {
                            has_image = true;
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": common::image_data_url(source)}
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content
                        })),
                        ContentBlock::Thinking { .. } => {
                            return Err(common::reject_thinking_block());
                        }
                        ContentBlock::ToolUse { .. } => {
                            return Err(common::reject_invalid_role_block("user", "tool call"));
                        }
                    }
                }
                if has_text || has_image {
                    let content = if has_image {
                        Value::Array(parts)
                    } else {
                        Value::String(text)
                    };
                    output.push(json!({"role": "user", "content": content}));
                }
                output.extend(tool_results);
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: value } => text.push_str(value),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": common::tool_arguments(input)?
                                }
                            }));
                        }
                        ContentBlock::Thinking { .. } => {
                            return Err(common::reject_thinking_block());
                        }
                        ContentBlock::Image { .. } => {
                            return Err(common::reject_invalid_role_block("assistant", "image"));
                        }
                        ContentBlock::ToolResult { .. } => {
                            return Err(common::reject_invalid_role_block(
                                "assistant",
                                "tool result",
                            ));
                        }
                    }
                }
                if !text.is_empty() || !tool_calls.is_empty() {
                    let content = if text.is_empty() && !tool_calls.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    };
                    let mut item = json!({"role": "assistant", "content": content});
                    if !tool_calls.is_empty() {
                        item["tool_calls"] = Value::Array(tool_calls);
                    }
                    output.push(item);
                }
            }
        }
    }
    Ok(output)
}

#[derive(Default)]
struct ChatAssembler {
    started: bool,
    stopped: bool,
    next_index: usize,
    text_index: Option<usize>,
    text: String,
    refusal: bool,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    block_order: Vec<BlockOrder>,
    open_blocks: Vec<usize>,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
    provider_stop_reason: Option<String>,
    max_tokens: u32,
}

struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
    block_index: usize,
}

#[derive(Clone, Copy)]
enum BlockOrder {
    Text,
    Tool(usize),
}

impl ChatAssembler {
    fn accept(
        &mut self,
        event: SseEvent,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        if event.name == "ping" {
            return on_event(StreamEvent::Ping);
        }
        if event.name == "error" {
            return self.fail(&event.data, on_event);
        }
        if event.name != "message" {
            return on_event(StreamEvent::Unknown {
                name: event.name,
                data: event.data,
            });
        }
        if event.data == "[DONE]" {
            return self.stop(on_event);
        }

        let data: Value = serde_json::from_str(&event.data).map_err(|error| {
            LegError::Decode(format!("malformed Chat Completions SSE event: {error}"))
        })?;
        if data.get("error").is_some() {
            return self.fail(&event.data, on_event);
        }
        self.ensure_started(&data, on_event)?;
        self.merge_usage(data.get("usage"))?;

        let choices = data
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                LegError::Decode("Chat Completions chunk omitted choices".to_string())
            })?;
        if let Some(choice) = choices.first() {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                self.accept_text(text, &data, on_event)?;
            }
            if let Some(text) = delta.get("refusal").and_then(Value::as_str) {
                self.refusal = true;
                self.accept_text(text, &data, on_event)?;
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.accept_tool_delta(call, &data, on_event)?;
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.provider_stop_reason = Some(reason.to_string());
                self.stop_reason = Some(if self.refusal && reason == "stop" {
                    StopReason::Refusal
                } else {
                    common::finish_reason(reason)
                });
                on_event(StreamEvent::MessageDelta { data: data.clone() })?;
            }
        } else if data.get("usage").is_some() {
            on_event(StreamEvent::MessageDelta { data: data.clone() })?;
        }
        Ok(())
    }

    fn ensure_started(
        &mut self,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        if self.stopped {
            return Err(LegError::Decode(
                "Chat Completions event arrived after [DONE]".to_string(),
            ));
        }
        if !self.started {
            self.started = true;
            on_event(StreamEvent::MessageStart { data: data.clone() })?;
        }
        Ok(())
    }

    fn accept_text(
        &mut self,
        text: &str,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        let index = match self.text_index {
            Some(index) => index,
            None => {
                let index = self.next_index;
                self.next_index += 1;
                self.text_index = Some(index);
                self.block_order.push(BlockOrder::Text);
                self.open_blocks.push(index);
                on_event(StreamEvent::ContentBlockStart {
                    index,
                    content_block: json!({"type": "text", "text": ""}),
                })?;
                index
            }
        };
        self.text.push_str(text);
        on_event(StreamEvent::ContentBlockDelta {
            index,
            delta: data.clone(),
        })
    }

    fn accept_tool_delta(
        &mut self,
        delta: &Value,
        event_data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        let tool_index = delta
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| {
                LegError::Decode("Chat Completions tool delta omitted index".to_string())
            })?;
        if !self.tool_calls.contains_key(&tool_index) {
            let block_index = self.next_index;
            self.next_index += 1;
            self.tool_calls.insert(
                tool_index,
                PartialToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    block_index,
                },
            );
            self.block_order.push(BlockOrder::Tool(tool_index));
            self.open_blocks.push(block_index);
            on_event(StreamEvent::ContentBlockStart {
                index: block_index,
                content_block: json!({"type": "tool_use"}),
            })?;
        }

        let tool = self
            .tool_calls
            .get_mut(&tool_index)
            .expect("tool call inserted above");
        if let Some(id) = delta.get("id").and_then(Value::as_str) {
            tool.id.push_str(id);
        }
        if let Some(function) = delta.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                tool.name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                tool.arguments.push_str(arguments);
            }
        }
        on_event(StreamEvent::ContentBlockDelta {
            index: tool.block_index,
            delta: event_data.clone(),
        })
    }

    fn merge_usage(&mut self, value: Option<&Value>) -> Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        self.usage.input_tokens = value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .or(self.usage.input_tokens);
        self.usage.output_tokens = value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .or(self.usage.output_tokens);
        Ok(())
    }

    fn fail(
        &mut self,
        data: &str,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        let (error_type, message, error) = common::stream_error(data);
        on_event(StreamEvent::Error {
            error_type,
            message,
        })?;
        Err(error)
    }

    fn stop(&mut self, on_event: &mut dyn FnMut(StreamEvent) -> Result<()>) -> Result<()> {
        if !self.started {
            return Err(LegError::Decode(
                "Chat Completions stream ended before its first chunk".to_string(),
            ));
        }
        if self.stopped {
            return Err(LegError::Decode(
                "Chat Completions stream contained duplicate [DONE]".to_string(),
            ));
        }
        if self.stop_reason.is_none() {
            return Err(LegError::Decode(
                "Chat Completions stream ended without a finish_reason".to_string(),
            ));
        }
        for index in self.open_blocks.drain(..) {
            on_event(StreamEvent::ContentBlockStop { index })?;
        }
        self.stopped = true;
        on_event(StreamEvent::MessageStop)
    }

    fn finish(self) -> Result<AssistantReply> {
        if !self.stopped {
            return Err(LegError::Decode(
                "Chat Completions stream ended before [DONE]".to_string(),
            ));
        }

        let mut content = Vec::new();
        for block in self.block_order {
            match block {
                BlockOrder::Text if !self.text.is_empty() => {
                    content.push(ContentBlock::text(self.text.clone()));
                }
                BlockOrder::Text => {}
                BlockOrder::Tool(index) => {
                    let tool = self
                        .tool_calls
                        .get(&index)
                        .expect("ordered tool call exists");
                    if tool.id.is_empty() || tool.name.is_empty() {
                        return Err(LegError::Decode(
                            "Chat Completions tool call omitted id or function name".to_string(),
                        ));
                    }
                    content.push(ContentBlock::ToolUse {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        input: common::parse_tool_arguments(&tool.arguments)?,
                    });
                }
            }
        }
        if content.is_empty() {
            if let Some(reason) = self.provider_stop_reason.as_deref()
                && reason == "length"
            {
                return Err(LegError::token_limit_reply(self.max_tokens, reason));
            }
            return Err(LegError::Decode(
                "response contained no assistant text or tool call".to_string(),
            ));
        }
        let stop_reason = if self.tool_calls.is_empty() {
            self.stop_reason
        } else {
            Some(StopReason::ToolUse)
        };
        Ok(AssistantReply::from_blocks(
            content,
            self.usage,
            stop_reason,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Credential, DEFAULT_MAX_TOKENS, Provider};
    use crate::error::Result as HttpResult;
    use crate::transport::http::HttpResponse;
    use std::cell::RefCell;
    use std::time::Duration;

    type CapturedRequest = (String, Vec<(String, String)>, String);

    struct FakeHttp {
        body: String,
        request: RefCell<Option<CapturedRequest>>,
    }

    impl HttpClient for FakeHttp {
        fn post_json(
            &self,
            url: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> HttpResult<HttpResponse> {
            *self.request.borrow_mut() = Some((
                url.to_string(),
                headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
                body.to_string(),
            ));
            Ok(HttpResponse {
                status: 200,
                body: self.body.clone(),
                retry_after: None,
            })
        }
    }

    fn config() -> LegConfig {
        LegConfig {
            provider: Provider::OpenAiChatCompletions,
            credential: Credential::Bearer("secret".to_string()),
            base_url: "https://api.openai.com".to_string(),
            model: "test-model".to_string(),
            timeout: Duration::from_secs(5),
            bash_timeout_secs: crate::config::DEFAULT_BASH_TIMEOUT_SECS,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_retries: 0,
            retry_base_delay: Duration::ZERO,
            max_tool_rounds: None,
            system_prompt: Some("be concise".to_string()),
            pre_tool_hook: None,
        }
    }

    fn stream(chunks: &[Value]) -> String {
        chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n"
    }

    #[test]
    fn streams_text_usage_and_openai_auth() {
        let http = FakeHttp {
            body: stream(&[
                json!({"choices":[{"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}),
            ]),
            request: RefCell::new(None),
        };
        let client = OpenAiChatCompletionsClient::with_http(config(), http);
        let mut text = String::new();
        let call = client.send_conversation_streaming_with_attempts(
            &[Message::user("hi")],
            &mut |event| {
                if let StreamEvent::ContentBlockDelta { delta, .. } = event
                    && let Some(value) = delta
                        .pointer("/choices/0/delta/content")
                        .and_then(Value::as_str)
                {
                    text.push_str(value);
                }
                Ok(())
            },
        );
        let reply = call.result.expect("stream parses");
        assert_eq!(text, "hello");
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.usage.input_tokens, Some(3));
        assert_eq!(reply.usage.output_tokens, Some(1));

        let request = client.http.request.borrow();
        let (url, headers, body) = request.as_ref().expect("request captured");
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer secret")
        );
        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn length_finish_without_content_has_dedicated_error() {
        let http = FakeHttp {
            body: stream(&[json!({"choices":[{"delta":{},"finish_reason":"length"}]})]),
            request: RefCell::new(None),
        };
        let mut config = config();
        config.max_tokens = 1024;
        let client = OpenAiChatCompletionsClient::with_http(config, http);

        let error = client
            .send_conversation(&[Message::user("think")])
            .unwrap_err();

        assert!(matches!(
            &error,
            LegError::TokenLimit {
                max_tokens: 1024,
                stop_reason,
            } if stop_reason == "length"
        ));
        assert_eq!(error.kind(), "decode");
        assert_eq!(error.stop_reason(), Some("length"));
        assert!(error.to_string().contains("raise LEG_MAX_TOKENS"));
    }

    #[test]
    fn incrementally_assembles_function_arguments_and_serializes_history() {
        let http = FakeHttp {
            body: stream(&[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"notes.txt\"}"}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ]),
            request: RefCell::new(None),
        };
        let tool = ToolSpec::new(
            "read",
            "read a file",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        );
        let history = [
            Message::user("inspect"),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "old_call".to_string(),
                    name: "read".to_string(),
                    input: json!({"path":"old.txt"}),
                }],
            ),
            Message::new(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "old_call".to_string(),
                    content: "old file".to_string(),
                    is_error: None,
                }],
            ),
        ];
        let client = OpenAiChatCompletionsClient::with_http(config(), http).with_tools(vec![tool]);
        let reply = client
            .send_conversation(&history)
            .expect("tool reply parses");
        assert_eq!(
            reply.content,
            [ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: json!({"path":"notes.txt"}),
            }]
        );
        assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));

        let request = client.http.request.borrow();
        let (_, _, body) = request.as_ref().expect("request captured");
        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "old_call");
        assert!(body["messages"][2]["content"].is_null());
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"old.txt\"}"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "old_call");
        assert_eq!(body["messages"][3]["content"], "old file");
    }

    #[test]
    fn maps_user_images_to_chat_completions_image_url_parts() {
        let http = FakeHttp {
            body: stream(&[
                json!({"choices":[{"delta":{"content":"seen"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ]),
            request: RefCell::new(None),
        };
        let image = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            },
        };
        let client = OpenAiChatCompletionsClient::with_http(config(), http);
        client
            .send_conversation(&[Message::new(
                Role::User,
                vec![ContentBlock::text("what is this?"), image],
            )])
            .expect("image reply parses");

        let request = client.http.request.borrow();
        let (_, _, body) = request.as_ref().expect("request captured");
        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn rejects_provider_specific_thinking_blocks_in_history() {
        let error = chat_messages(&[Message::new(
            Role::Assistant,
            vec![ContentBlock::Thinking {
                thinking: "hidden reasoning".to_string(),
                signature: "signature".to_string(),
            }],
        )])
        .unwrap_err();

        assert!(matches!(error, LegError::Usage(_)));
        assert!(error.to_string().contains("original provider"));
    }
}
