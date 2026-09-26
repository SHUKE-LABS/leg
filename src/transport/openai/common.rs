use serde_json::Value;

use crate::config::{Credential, LegConfig};
use crate::error::{LegError, Result};
use crate::image_input::{MAX_IMAGE_REQUEST_BYTES, validate_message_image_limits};
use crate::model::{ContentBlock, ImageSource, Message, StopReason};

use super::super::http::UreqHttpClient;
use super::super::{RetryPolicy, RetryingHttpClient};

pub(crate) fn real_http(config: &LegConfig) -> RetryingHttpClient<UreqHttpClient> {
    let policy = RetryPolicy::new(config.max_retries, config.retry_base_delay);
    RetryingHttpClient::new(UreqHttpClient::new(config.timeout), policy)
}

pub(crate) fn endpoint(base_url: &str, resource: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/v1") {
        format!("{base_url}/{resource}")
    } else {
        format!("{base_url}/v1/{resource}")
    }
}

pub(crate) fn bearer_token(credential: &Credential) -> Result<&str> {
    match credential {
        Credential::Bearer(token) => Ok(token),
        _ => Err(LegError::Config(
            "OpenAI transports require OPENAI_API_KEY".to_string(),
        )),
    }
}

pub(crate) fn serialize_request(
    request: &Value,
    messages: &[Message],
    protocol: &str,
) -> Result<String> {
    validate_message_image_limits(messages)?;
    let body = serde_json::to_string(request)
        .map_err(|error| LegError::Transport(format!("failed to serialize request: {error}")))?;
    let has_images = messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    });
    if has_images && body.len() > MAX_IMAGE_REQUEST_BYTES {
        return Err(LegError::Usage(format!(
            "serialized image request exceeds the 32 MB {protocol} request limit"
        )));
    }
    Ok(body)
}

pub(crate) fn image_data_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
    }
}

pub(crate) fn api_error(status: u16, body: &str) -> LegError {
    let (error_type, message) = error_details(body);
    if status == 429 || is_rate_limit(error_type.as_deref()) {
        return LegError::RateLimited(error_message(error_type.as_deref(), message));
    }
    if status == 401 || is_auth_error(error_type.as_deref()) {
        return LegError::Auth(error_message(error_type.as_deref(), message));
    }
    match status {
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
    }
}

pub(crate) fn stream_error(data: &str) -> (Option<String>, String, LegError) {
    let (error_type, message) = error_details(data);
    let error = if is_rate_limit(error_type.as_deref()) {
        LegError::RateLimited(error_message(error_type.as_deref(), message.clone()))
    } else if is_auth_error(error_type.as_deref()) {
        LegError::Auth(error_message(error_type.as_deref(), message.clone()))
    } else {
        LegError::ProviderStream {
            error_type: error_type.clone(),
            message: message.clone(),
        }
    };
    (error_type, message, error)
}

fn error_details(body: &str) -> (Option<String>, String) {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let error = value
            .pointer("/response/error")
            .filter(|error| error.is_object())
            .or_else(|| value.get("error").filter(|error| error.is_object()))
            .unwrap_or(&value);
        let error_type = error
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| error.get("type").and_then(Value::as_str))
            .map(str::to_string);
        let message = error.get("message").and_then(Value::as_str);
        if let Some(message) = message {
            return (error_type, message.to_string());
        }
    }

    let message = if body.trim().is_empty() {
        "no response body".to_string()
    } else {
        body.trim().to_string()
    };
    (None, message)
}

fn is_rate_limit(error_type: Option<&str>) -> bool {
    error_type.is_some_and(|error_type| {
        let error_type = error_type.to_ascii_lowercase();
        error_type.contains("rate_limit") || error_type.contains("quota")
    })
}

fn is_auth_error(error_type: Option<&str>) -> bool {
    error_type.is_some_and(|error_type| {
        let error_type = error_type.to_ascii_lowercase();
        error_type.contains("auth") || error_type.contains("api_key")
    })
}

fn error_message(error_type: Option<&str>, message: String) -> String {
    match error_type {
        Some(error_type) => format!("{error_type}: {message}"),
        None => message,
    }
}

pub(crate) fn finish_reason(value: &str) -> StopReason {
    match value {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

pub(crate) fn parse_tool_arguments(arguments: &str) -> Result<Value> {
    let value = if arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str::<Value>(arguments).map_err(|error| {
            LegError::Decode(format!("malformed function-call arguments JSON: {error}"))
        })?
    };
    if !value.is_object() {
        return Err(LegError::Decode(
            "function-call arguments must be a JSON object".to_string(),
        ));
    }
    Ok(value)
}

pub(crate) fn tool_arguments(input: &Value) -> Result<String> {
    serde_json::to_string(input).map_err(|error| {
        LegError::Transport(format!("failed to serialize tool arguments: {error}"))
    })
}

pub(crate) fn reject_invalid_role_block(role: &str, block: &str) -> LegError {
    LegError::Decode(format!(
        "cannot send {block} content in an OpenAI {role} message"
    ))
}

pub(crate) fn reject_thinking_block() -> LegError {
    LegError::Usage(
        "OpenAI protocols cannot preserve provider-specific thinking blocks; resume this session with its original provider"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_openai_authentication_and_rate_limit_errors() {
        let (_, _, auth_error) = stream_error(
            r#"{"response":{"error":{"type":"invalid_request_error","code":"invalid_api_key","message":"bad key"}}}"#,
        );
        assert!(matches!(auth_error, LegError::Auth(_)));

        let rate_error = api_error(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        assert!(matches!(rate_error, LegError::RateLimited(_)));
    }
}
