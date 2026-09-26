//! Provider transport boundary.
//!
//! This module defines the seam between leg's typed model and a concrete
//! provider client. [`Transport`] is the stable boundary the CLI and tests
//! depend on; the [`claude`] submodule provides the first concrete
//! implementation (a Claude-compatible Messages client), and
//! [`http`] isolates the underlying HTTP execution so the request/response
//! logic can be tested without a network.

pub mod claude;
pub mod http;
pub mod retry;

use crate::error::Result;
use crate::model::{AssistantReply, Message, Prompt};

pub use retry::{RetryPolicy, RetryingHttpClient};

/// One provider event emitted while a response is streaming.
///
/// Message and content-block payloads retain their provider JSON shape so
/// callers can observe metadata beyond the fields used to assemble a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The provider began an assistant message.
    MessageStart {
        /// The provider's `message_start` payload.
        data: serde_json::Value,
    },
    /// The provider began one content block.
    ContentBlockStart {
        /// The block's index in the assistant reply.
        index: usize,
        /// The provider's initial content-block object.
        content_block: serde_json::Value,
    },
    /// The provider emitted a content-block delta.
    ContentBlockDelta {
        /// The block's index in the assistant reply.
        index: usize,
        /// The provider's delta object.
        delta: serde_json::Value,
    },
    /// The provider finished one content block.
    ContentBlockStop {
        /// The block's index in the assistant reply.
        index: usize,
    },
    /// The provider emitted message-level metadata.
    MessageDelta {
        /// The provider's `message_delta` payload.
        data: serde_json::Value,
    },
    /// The provider finished the assistant message.
    MessageStop,
    /// The provider sent an SSE ping.
    Ping,
    /// The provider sent a stream-level error event.
    Error {
        /// The provider's error type, when supplied.
        error_type: Option<String>,
        /// The provider's error message.
        message: String,
    },
    /// A valid but currently unrecognized provider event.
    Unknown {
        /// The SSE event name.
        name: String,
        /// The raw event data.
        data: String,
    },
}

/// A transport call's result and the number of provider attempts it used.
///
/// Attempts count provider-call invocations, including retries and calls from
/// earlier tool-loop rounds. Concrete clients report zero for local request
/// construction failures; the default counts one call for transports without
/// lower-level attempt metadata.
#[derive(Debug)]
pub struct TransportCall<T> {
    /// The reply or terminal error.
    pub result: Result<T>,
    /// Total provider attempts used for this call.
    pub attempts: u64,
}

impl<T> TransportCall<T> {
    /// Creates a transport call result with its attempt count.
    pub fn new(result: Result<T>, attempts: u64) -> Self {
        Self { result, attempts }
    }

    pub(crate) fn completed(result: Result<T>, attempts: u64) -> Self {
        Self::new(result, attempts)
    }
}

/// Sends a conversation and returns a single assembled reply.
///
/// Synchronous and single-call, with no tool execution: a reply's `tool_use`
/// blocks are returned to the caller, who may send `tool_result` blocks back
/// on a later call ([`crate::tools::ToolLoop`] does this). The streaming
/// methods additionally expose ordered provider events while assembling the
/// same final reply. Every call maps the full message history onto a provider
/// request, so a multi-turn session resends its accumulated turns.
pub trait Transport {
    /// Sends `messages` (the full conversation history, oldest first) and
    /// returns the assistant's reply to the latest turn.
    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply>;

    /// Sends `messages`, also reporting the total provider-attempt count.
    ///
    /// Implementations that do not expose lower-level metadata treat one
    /// transport call as one provider attempt. HTTP-backed clients report the
    /// cumulative attempts from their shared retrying HTTP client.
    fn send_conversation_with_attempts(
        &self,
        messages: &[Message],
    ) -> TransportCall<AssistantReply> {
        TransportCall::new(self.send_conversation(messages), 1)
    }

    /// Streams provider events while assembling the assistant reply.
    ///
    /// The default preserves compatibility with buffered transports: it
    /// returns their assembled reply without emitting events. Streaming
    /// implementations override this method.
    fn send_conversation_streaming_with_attempts(
        &self,
        messages: &[Message],
        _on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<AssistantReply> {
        self.send_conversation_with_attempts(messages)
    }

    /// [`Transport::send_conversation_streaming_with_attempts`] without
    /// attempt metadata.
    fn send_conversation_streaming(
        &self,
        messages: &[Message],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> Result<AssistantReply> {
        self.send_conversation_streaming_with_attempts(messages, on_event)
            .result
    }

    /// Sends a single user `prompt` and returns the assistant's reply.
    ///
    /// Wraps the prompt as a one-message user conversation and delegates to
    /// [`Transport::send_conversation`], so single-turn callers and tests are
    /// unchanged by the multi-turn primitive.
    fn send(&self, prompt: &Prompt) -> Result<AssistantReply> {
        self.send_conversation(std::slice::from_ref(&Message::user(prompt.text.as_str())))
    }

    /// [`Transport::send`] with attempt metadata.
    fn send_with_attempts(&self, prompt: &Prompt) -> TransportCall<AssistantReply> {
        self.send_conversation_with_attempts(std::slice::from_ref(&Message::user(
            prompt.text.as_str(),
        )))
    }
}

/// A shared reference to a transport is itself a transport, so a wrapper such
/// as [`crate::tools::ToolLoop`] can borrow one.
impl<T: Transport + ?Sized> Transport for &T {
    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
        (**self).send_conversation(messages)
    }

    fn send_conversation_with_attempts(
        &self,
        messages: &[Message],
    ) -> TransportCall<AssistantReply> {
        (**self).send_conversation_with_attempts(messages)
    }

    fn send_conversation_streaming_with_attempts(
        &self,
        messages: &[Message],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<AssistantReply> {
        (**self).send_conversation_streaming_with_attempts(messages, on_event)
    }
}
