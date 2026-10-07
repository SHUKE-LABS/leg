//! OpenAI-compatible Responses transport.

use std::collections::{BTreeMap, BTreeSet};

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

/// A transport for `POST /v1/responses`.
pub struct OpenAiResponsesClient<H: HttpClient> {
    config: LegConfig,
    http: H,
    tools: Vec<ToolSpec>,
}

impl OpenAiResponsesClient<RetryingHttpClient<UreqHttpClient>> {
    /// Creates a client that uses the configured timeout and retry policy.
    pub fn from_config(config: LegConfig) -> Self {
        let http = common::real_http(&config);
        Self::with_http(config, http)
    }
}

impl<H: HttpClient> OpenAiResponsesClient<H> {
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

impl<H: HttpClient> Transport for OpenAiResponsesClient<H> {
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
        let url = common::endpoint(&self.config.base_url, "responses");

        let mut decoder = SseDecoder::default();
        let mut assembler = ResponsesAssembler {
            max_tokens: self.config.max_tokens,
            ..ResponsesAssembler::default()
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
    let mut request = json!({
        "model": config.model,
        "input": responses_input(messages)?,
        "max_output_tokens": config.max_tokens,
        "stream": true
    });
    if let Some(instructions) = &config.system_prompt {
        request["instructions"] = Value::String(instructions.clone());
    }
    if !tools.is_empty() {
        request["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema
                    })
                })
                .collect(),
        );
    }
    common::serialize_request(&request, messages, "Responses")
}

fn responses_input(messages: &[Message]) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut content = Vec::new();
                let mut tool_results = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            content.push(json!({"type": "input_text", "text": text}));
                        }
                        ContentBlock::Image {
                            source: source @ ImageSource::Base64 { .. },
                        } => {
                            content.push(json!({
                                "type": "input_image",
                                "image_url": common::image_data_url(source)
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => tool_results.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content
                        })),
                        ContentBlock::Thinking { .. } => {
                            return Err(common::reject_thinking_block());
                        }
                        ContentBlock::ToolUse { .. } => {
                            return Err(common::reject_invalid_role_block("user", "tool call"));
                        }
                    }
                }
                if !content.is_empty() {
                    output.push(json!({"role": "user", "content": content}));
                }
                output.extend(tool_results);
            }
            Role::Assistant => {
                let mut content = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            content.push(json!({"type": "output_text", "text": text}));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": common::tool_arguments(input)?
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
                if !content.is_empty() {
                    output.push(json!({"role": "assistant", "content": content}));
                }
                output.extend(tool_calls);
            }
        }
    }
    Ok(output)
}

#[derive(Default)]
struct ResponsesAssembler {
    started: bool,
    stopped: bool,
    next_index: usize,
    text_indexes: BTreeMap<(usize, usize), usize>,
    tool_indexes: BTreeMap<String, usize>,
    open_blocks: BTreeSet<usize>,
    reply: Option<AssistantReply>,
    max_tokens: u32,
}

impl ResponsesAssembler {
    fn accept(
        &mut self,
        event: SseEvent,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        if event.name == "ping" {
            return on_event(StreamEvent::Ping);
        }
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(data) => data,
            Err(_) if event.name == "error" => {
                return self.fail(&event.data, on_event);
            }
            Err(error) => {
                return Err(LegError::Decode(format!(
                    "malformed Responses SSE event: {error}"
                )));
            }
        };
        let event_type = data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(&event.name);
        if event_type == "error" || event.name == "error" {
            return self.fail(&event.data, on_event);
        }
        if event_type == "response.failed" {
            return self.fail(&event.data, on_event);
        }
        if matches!(event_type, "response.completed" | "response.incomplete") {
            return self.complete(&data, on_event);
        }

        self.ensure_started(&data, on_event)?;
        match event_type {
            "response.created" | "response.in_progress" => {}
            "response.output_item.added" => {
                self.start_function_call(&data, on_event)?;
            }
            "response.content_part.added" => {
                self.start_text_block(&data, on_event)?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let index = self.ensure_text_block(&data, on_event)?;
                let delta = data
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LegError::Decode(format!("{event_type} event omitted delta")))?;
                if delta.is_empty() {
                    return Ok(());
                }
                on_event(StreamEvent::ContentBlockDelta {
                    index,
                    delta: data.clone(),
                })?;
            }
            "response.function_call_arguments.delta" => {
                let index = self.ensure_tool_block(&data, on_event)?;
                on_event(StreamEvent::ContentBlockDelta {
                    index,
                    delta: data.clone(),
                })?;
            }
            "response.content_part.done" => {
                if let Some(index) = self.text_index(&data) {
                    self.close_block(index, on_event)?;
                }
            }
            "response.output_item.done" => {
                if let Some(index) = self.tool_index(&data) {
                    self.close_block(index, on_event)?;
                }
            }
            _ => {
                on_event(StreamEvent::Unknown {
                    name: event_type.to_string(),
                    data: event.data,
                })?;
            }
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
                "Responses event arrived after the terminal response event".to_string(),
            ));
        }
        if !self.started {
            self.started = true;
            on_event(StreamEvent::MessageStart { data: data.clone() })?;
        }
        Ok(())
    }

    fn start_function_call(
        &mut self,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        let Some(item) = data.get("item") else {
            return Err(LegError::Decode(
                "response.output_item.added omitted item".to_string(),
            ));
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Ok(());
        }
        let key = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.get("call_id").and_then(Value::as_str))
            .ok_or_else(|| LegError::Decode("Responses function call omitted item id".to_string()))?
            .to_string();
        if self.tool_indexes.contains_key(&key) {
            return Err(LegError::Decode(format!(
                "duplicate Responses function call item {key:?}"
            )));
        }
        let index = self.allocate_block();
        self.tool_indexes.insert(key, index);
        let mut content_block = json!({"type": "tool_use"});
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            content_block["id"] = Value::String(call_id.to_string());
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            content_block["name"] = Value::String(name.to_string());
        }
        on_event(StreamEvent::ContentBlockStart {
            index,
            content_block,
        })
    }

    fn start_text_block(
        &mut self,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        let part = data.get("part").unwrap_or(&Value::Null);
        if part.get("type").and_then(Value::as_str) != Some("output_text") {
            return Ok(());
        }
        let (output, content) = output_content_index(data)?;
        if self.text_indexes.contains_key(&(output, content)) {
            return Err(LegError::Decode(
                "duplicate Responses output text content part".to_string(),
            ));
        }
        let index = self.allocate_block();
        self.text_indexes.insert((output, content), index);
        on_event(StreamEvent::ContentBlockStart {
            index,
            content_block: part.clone(),
        })
    }

    fn ensure_text_block(
        &mut self,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<usize> {
        let key = output_content_index(data)?;
        if let Some(index) = self.text_indexes.get(&key) {
            return Ok(*index);
        }
        let index = self.allocate_block();
        self.text_indexes.insert(key, index);
        on_event(StreamEvent::ContentBlockStart {
            index,
            content_block: json!({"type": "output_text", "text": ""}),
        })?;
        Ok(index)
    }

    fn ensure_tool_block(
        &mut self,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<usize> {
        let key = data
            .get("item_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LegError::Decode(
                    "response.function_call_arguments.delta omitted item_id".to_string(),
                )
            })?
            .to_string();
        if let Some(index) = self.tool_indexes.get(&key) {
            return Ok(*index);
        }
        let index = self.allocate_block();
        self.tool_indexes.insert(key, index);
        on_event(StreamEvent::ContentBlockStart {
            index,
            content_block: json!({"type": "tool_use"}),
        })?;
        Ok(index)
    }

    fn text_index(&self, data: &Value) -> Option<usize> {
        let key = output_content_index(data).ok()?;
        self.text_indexes.get(&key).copied()
    }

    fn tool_index(&self, data: &Value) -> Option<usize> {
        let item = data.get("item")?;
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return None;
        }
        let key = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.get("call_id").and_then(Value::as_str))?;
        self.tool_indexes.get(key).copied()
    }

    fn allocate_block(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        self.open_blocks.insert(index);
        index
    }

    fn close_block(
        &mut self,
        index: usize,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        if self.open_blocks.remove(&index) {
            on_event(StreamEvent::ContentBlockStop { index })?;
        }
        Ok(())
    }

    fn complete(
        &mut self,
        data: &Value,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<()> {
        self.ensure_started(data, on_event)?;
        let response = data.get("response").ok_or_else(|| {
            LegError::Decode("terminal Responses event omitted response".to_string())
        })?;
        let reply = parse_response(response, self.max_tokens)?;
        for index in std::mem::take(&mut self.open_blocks) {
            on_event(StreamEvent::ContentBlockStop { index })?;
        }
        on_event(StreamEvent::MessageDelta { data: data.clone() })?;
        on_event(StreamEvent::MessageStop)?;
        self.reply = Some(reply);
        self.stopped = true;
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

    fn finish(self) -> Result<AssistantReply> {
        if !self.stopped {
            return Err(LegError::Decode(
                "Responses stream ended before response.completed".to_string(),
            ));
        }
        self.reply.ok_or_else(|| {
            LegError::Decode("Responses stream ended without an assembled reply".to_string())
        })
    }
}

fn output_content_index(data: &Value) -> Result<(usize, usize)> {
    let output = data
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| LegError::Decode("Responses event omitted output_index".to_string()))?;
    let content = data
        .get("content_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| LegError::Decode("Responses event omitted content_index".to_string()))?;
    Ok((output, content))
}

fn parse_response(response: &Value, max_tokens: u32) -> Result<AssistantReply> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LegError::Decode("Responses body omitted output items".to_string()))?;
    let mut content = Vec::new();
    let mut refusal = false;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                let text =
                                    part.get("text").and_then(Value::as_str).ok_or_else(|| {
                                        LegError::Decode(
                                            "Responses output_text omitted text".to_string(),
                                        )
                                    })?;
                                content.push(ContentBlock::text(text));
                            }
                            Some("refusal") => {
                                let text = part.get("refusal").and_then(Value::as_str).ok_or_else(
                                    || {
                                        LegError::Decode(
                                            "Responses refusal omitted text".to_string(),
                                        )
                                    },
                                )?;
                                refusal = true;
                                content.push(ContentBlock::text(text));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("function_call") => {
                let id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                    LegError::Decode("Responses function call omitted call_id".to_string())
                })?;
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                    LegError::Decode("Responses function call omitted name".to_string())
                })?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        LegError::Decode("Responses function call omitted arguments".to_string())
                    })?;
                content.push(ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: common::parse_tool_arguments(arguments)?,
                });
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
    if !has_tool_use && !has_text {
        if response.get("status").and_then(Value::as_str) == Some("incomplete")
            && let Some(stop_reason) = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            && stop_reason == "max_output_tokens"
        {
            return Err(LegError::token_limit_reply(max_tokens, stop_reason));
        }
        return Err(LegError::Decode(
            "response contained no assistant text or tool call".to_string(),
        ));
    }

    let usage = response
        .get("usage")
        .map_or_else(TokenUsage::default, |usage| TokenUsage {
            input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        });
    let stop_reason = if has_tool_use {
        Some(StopReason::ToolUse)
    } else if refusal {
        Some(StopReason::Refusal)
    } else if response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        == Some("max_output_tokens")
    {
        Some(StopReason::MaxTokens)
    } else {
        Some(StopReason::EndTurn)
    };
    Ok(AssistantReply::from_blocks(content, usage, stop_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Credential, DEFAULT_MAX_TOKENS, Provider};
    use crate::error::Result as LegResult;
    use crate::transport::http::{HttpClient, HttpResponse};
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
        ) -> LegResult<HttpResponse> {
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
            provider: Provider::OpenAiResponses,
            credential: Credential::Bearer("secret".to_string()),
            base_url: "https://api.openai.com/v1".to_string(),
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

    fn event(name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    fn tool_response() -> Value {
        json!({
            "id":"resp_1",
            "status":"completed",
            "output":[{
                "type":"function_call",
                "id":"fc_1",
                "call_id":"call_1",
                "name":"read",
                "arguments":"{\"path\":\"notes.txt\"}"
            }],
            "usage":{"input_tokens":4,"output_tokens":2}
        })
    }

    #[test]
    fn assembles_function_call_events_and_maps_responses_history() {
        let response = tool_response();
        let body = [
            event(
                "response.created",
                json!({"type":"response.created","response":{"id":"resp_1"}}),
            ),
            event(
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{
                    "type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""
                }}),
            ),
            event(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"path\":\"notes.txt\"}"}),
            ),
            event(
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":0,"item":{
                    "type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"notes.txt\"}"
                }}),
            ),
            event(
                "response.completed",
                json!({"type":"response.completed","response":response}),
            ),
        ]
        .join("");
        let http = FakeHttp {
            body,
            request: RefCell::new(None),
        };
        let tool = ToolSpec::new(
            "read",
            "read a file",
            json!({"type":"object","properties":{"path":{"type":"string"}}}),
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
        let client = OpenAiResponsesClient::with_http(config(), http).with_tools(vec![tool]);
        let reply = client
            .send_conversation(&history)
            .expect("function-call response parses");
        assert_eq!(
            reply.content,
            [ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: json!({"path":"notes.txt"}),
            }]
        );
        assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(reply.usage.input_tokens, Some(4));

        let request = client.http.request.borrow();
        let (url, headers, body) = request.as_ref().expect("request captured");
        assert_eq!(url, "https://api.openai.com/v1/responses");
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer secret")
        );
        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "be concise");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
    }

    #[test]
    fn streams_output_text_and_maps_user_images() {
        let response = json!({
            "status":"completed",
            "output":[{
                "type":"message",
                "role":"assistant",
                "content":[{"type":"output_text","text":"hello"}]
            }],
            "usage":{"input_tokens":2,"output_tokens":1}
        });
        let body = [
            event(
                "response.created",
                json!({"type":"response.created","response":{"id":"resp_2"}}),
            ),
            event(
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{
                    "type":"message","id":"msg_1","role":"assistant","content":[]
                }}),
            ),
            event(
                "response.content_part.added",
                json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{
                    "type":"output_text","text":""
                }}),
            ),
            event(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hello"}),
            ),
            event(
                "response.completed",
                json!({"type":"response.completed","response":response}),
            ),
        ]
        .join("");
        let http = FakeHttp {
            body,
            request: RefCell::new(None),
        };
        let client = OpenAiResponsesClient::with_http(config(), http);
        let image = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/jpeg".to_string(),
                data: "aGVsbG8=".to_string(),
            },
        };
        let mut streamed = String::new();
        let reply = client
            .send_conversation_streaming_with_attempts(
                &[Message::new(
                    Role::User,
                    vec![ContentBlock::text("describe"), image],
                )],
                &mut |event| {
                    if let StreamEvent::ContentBlockDelta { delta, .. } = event
                        && delta.get("type").and_then(Value::as_str)
                            == Some("response.output_text.delta")
                    {
                        streamed.push_str(delta.get("delta").and_then(Value::as_str).unwrap());
                    }
                    Ok(())
                },
            )
            .result
            .expect("text response parses");
        assert_eq!(streamed, "hello");
        assert_eq!(reply.text, "hello");

        let request = client.http.request.borrow();
        let (_, _, body) = request.as_ref().expect("request captured");
        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/jpeg;base64,aGVsbG8="
        );
    }

    #[test]
    fn incomplete_output_token_limit_without_content_has_dedicated_error() {
        let response = json!({
            "status":"incomplete",
            "output":[],
            "incomplete_details":{"reason":"max_output_tokens"}
        });
        let body = event(
            "response.incomplete",
            json!({"type":"response.incomplete","response":response}),
        );
        let http = FakeHttp {
            body,
            request: RefCell::new(None),
        };
        let mut config = config();
        config.max_tokens = 1024;
        let client = OpenAiResponsesClient::with_http(config, http);

        let error = client
            .send_conversation(&[Message::user("think")])
            .unwrap_err();

        assert!(matches!(
            &error,
            LegError::TokenLimit {
                max_tokens: 1024,
                stop_reason,
            } if stop_reason == "max_output_tokens"
        ));
        assert_eq!(error.stop_reason(), Some("max_output_tokens"));
    }

    #[test]
    fn rejects_provider_specific_thinking_blocks_in_history() {
        let error = responses_input(&[Message::new(
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
