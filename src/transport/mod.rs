//! Provider transport boundary.
//!
//! This module defines the seam between leg's typed model and a concrete
//! provider client. [`Transport`] is the stable boundary the CLI and tests
//! depend on; the [`claude`] submodule provides the first concrete
//! implementation (a non-streaming Claude-compatible Messages client), and
//! [`http`] isolates the underlying HTTP execution so the request/response
//! logic can be tested without a network.

pub mod claude;
pub mod http;
pub mod retry;

use crate::error::Result;
use crate::model::{AssistantReply, Message, Prompt};

pub use retry::{RetryPolicy, RetryingHttpClient};

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

/// Sends a conversation and returns a single reply.
///
/// Intentionally synchronous and single-call: no streaming, and no tool
/// execution — a reply's `tool_use` blocks are returned to the caller, who
/// may send `tool_result` blocks back on a later call
/// ([`crate::tools::ToolLoop`] does this). The primitive is
/// [`Transport::send_conversation`], which maps the full message history onto a
/// provider request — so a multi-turn session resends its accumulated turns on
/// every call. [`Transport::send`] is a single-turn convenience wrapping one
/// user prompt, provided so the `ask` path needs no separate implementation.
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
}
