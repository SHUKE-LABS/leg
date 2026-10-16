use crate::config::{LegConfig, Provider};
use crate::error::Result;
use crate::model::{AssistantReply, Message, ToolSpec};
use crate::transport::claude::ClaudeClient;
use crate::transport::http::UreqHttpClient;
use crate::transport::openai::chat_completions::OpenAiChatCompletionsClient;
use crate::transport::openai::responses::OpenAiResponsesClient;
use crate::transport::{RetryingHttpClient, StreamEvent, Transport, TransportCall};

type RuntimeHttp = RetryingHttpClient<UreqHttpClient>;

pub(crate) enum ProviderTransport {
    Anthropic(ClaudeClient<RuntimeHttp>),
    OpenAiChatCompletions(OpenAiChatCompletionsClient<RuntimeHttp>),
    OpenAiResponses(OpenAiResponsesClient<RuntimeHttp>),
}

impl ProviderTransport {
    pub(crate) fn from_config(config: LegConfig, tools: Vec<ToolSpec>) -> Self {
        match config.provider {
            Provider::Anthropic => {
                Self::Anthropic(ClaudeClient::from_config(config).with_tools(tools))
            }
            Provider::OpenAiChatCompletions => Self::OpenAiChatCompletions(
                OpenAiChatCompletionsClient::from_config(config).with_tools(tools),
            ),
            Provider::OpenAiResponses => {
                Self::OpenAiResponses(OpenAiResponsesClient::from_config(config).with_tools(tools))
            }
        }
    }
}

impl Transport for ProviderTransport {
    fn configured_system_prompt(&self) -> Option<&str> {
        match self {
            Self::Anthropic(client) => client.configured_system_prompt(),
            Self::OpenAiChatCompletions(client) => client.configured_system_prompt(),
            Self::OpenAiResponses(client) => client.configured_system_prompt(),
        }
    }

    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
        match self {
            Self::Anthropic(client) => client.send_conversation(messages),
            Self::OpenAiChatCompletions(client) => client.send_conversation(messages),
            Self::OpenAiResponses(client) => client.send_conversation(messages),
        }
    }

    fn send_conversation_with_attempts(
        &self,
        messages: &[Message],
    ) -> TransportCall<AssistantReply> {
        match self {
            Self::Anthropic(client) => client.send_conversation_with_attempts(messages),
            Self::OpenAiChatCompletions(client) => client.send_conversation_with_attempts(messages),
            Self::OpenAiResponses(client) => client.send_conversation_with_attempts(messages),
        }
    }

    fn send_conversation_with_system_prompt(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
    ) -> TransportCall<AssistantReply> {
        match self {
            Self::Anthropic(client) => {
                client.send_conversation_with_system_prompt(messages, system_prompt)
            }
            Self::OpenAiChatCompletions(client) => {
                client.send_conversation_with_system_prompt(messages, system_prompt)
            }
            Self::OpenAiResponses(client) => {
                client.send_conversation_with_system_prompt(messages, system_prompt)
            }
        }
    }

    fn estimate_input_bytes(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
        streaming: bool,
    ) -> Result<usize> {
        match self {
            Self::Anthropic(client) => {
                client.estimate_input_bytes(messages, system_prompt, streaming)
            }
            Self::OpenAiChatCompletions(client) => {
                client.estimate_input_bytes(messages, system_prompt, streaming)
            }
            Self::OpenAiResponses(client) => {
                client.estimate_input_bytes(messages, system_prompt, streaming)
            }
        }
    }

    fn estimate_summary_input_bytes(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
        max_output_tokens: u32,
    ) -> Result<usize> {
        match self {
            Self::Anthropic(client) => {
                client.estimate_summary_input_bytes(messages, system_prompt, max_output_tokens)
            }
            Self::OpenAiChatCompletions(client) => {
                client.estimate_summary_input_bytes(messages, system_prompt, max_output_tokens)
            }
            Self::OpenAiResponses(client) => {
                client.estimate_summary_input_bytes(messages, system_prompt, max_output_tokens)
            }
        }
    }

    fn send_context_summary(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
        max_output_tokens: u32,
    ) -> TransportCall<AssistantReply> {
        match self {
            Self::Anthropic(client) => {
                client.send_context_summary(messages, system_prompt, max_output_tokens)
            }
            Self::OpenAiChatCompletions(client) => {
                client.send_context_summary(messages, system_prompt, max_output_tokens)
            }
            Self::OpenAiResponses(client) => {
                client.send_context_summary(messages, system_prompt, max_output_tokens)
            }
        }
    }

    fn send_conversation_streaming_with_attempts(
        &self,
        messages: &[Message],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<AssistantReply> {
        match self {
            Self::Anthropic(client) => {
                client.send_conversation_streaming_with_attempts(messages, on_event)
            }
            Self::OpenAiChatCompletions(client) => {
                client.send_conversation_streaming_with_attempts(messages, on_event)
            }
            Self::OpenAiResponses(client) => {
                client.send_conversation_streaming_with_attempts(messages, on_event)
            }
        }
    }

    fn send_conversation_streaming_with_system_prompt(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<AssistantReply> {
        match self {
            Self::Anthropic(client) => client.send_conversation_streaming_with_system_prompt(
                messages,
                system_prompt,
                on_event,
            ),
            Self::OpenAiChatCompletions(client) => client
                .send_conversation_streaming_with_system_prompt(messages, system_prompt, on_event),
            Self::OpenAiResponses(client) => client.send_conversation_streaming_with_system_prompt(
                messages,
                system_prompt,
                on_event,
            ),
        }
    }
}
