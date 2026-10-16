//! The agentic tool loop and its dispatch seam.
//!
//! [`ToolRegistry`] maps a tool name to its [`ToolSpec`] and a synchronous
//! [`ToolHandler`]. [`ToolLoop`] wraps a [`Transport`]: while a reply's
//! `stop_reason` is `tool_use`, it executes each `tool_use` block through the
//! registry (serially, in call order) and sends the `tool_result` blocks back.
//! A configured round limit caps the loop; otherwise it continues until the
//! provider replies without requesting tools. `ToolLoop` is itself a
//! [`Transport`], so every driver (`ask`, `session`, `exchange`) shares it.
//!
//! Registered tools are [`ReadTool`] (`read`), [`WriteTool`] (`write`),
//! [`EditTool`] (`edit`), and [`BashTool`] (`bash`).

use std::cell::RefCell;

use crate::config::ContextBudget;
use crate::context::{
    TOOL_RESULT_TRUNCATION_MARKER, baseline_tokens, calibrated_tokens, cut_after_groups,
    historical_group_starts, project_tool_results, retained_group_count, summary_source,
    system_prompt_with_summary,
};
use crate::error::Result;
use crate::events::ToolStatus;
use crate::interrupt;
use crate::model::{AssistantReply, ContentBlock, Message, Role, StopReason, TokenUsage, ToolSpec};
use crate::transport::{StreamEvent, Transport, TransportCall};
use pretool::PreToolHook;

mod bash;
mod edit;
mod pretool;
mod process;
mod read;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::{ReadSet, ReadTool};
pub use write::WriteTool;

/// Formats the warning surfaced when a turn stops at a configured round limit.
pub fn tool_round_limit_warning(max_tool_rounds: usize) -> String {
    format!("warning: stopped after {max_tool_rounds} tool-use rounds; reply still requests tools")
}

/// Executes one tool call synchronously.
pub trait ToolHandler {
    /// Runs the tool on `input` (the call's JSON arguments). `Ok` is the
    /// tool's output; `Err` is reported back to the model with `is_error`.
    fn call(&self, input: &serde_json::Value) -> std::result::Result<String, String>;
}

/// The tools a [`ToolLoop`] advertises and can execute.
#[derive(Default)]
pub struct ToolRegistry {
    entries: Vec<(ToolSpec, Box<dyn ToolHandler>)>,
    pre_tool_hook: Option<PreToolHook>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handler` under `spec.name`.
    pub fn register(&mut self, spec: ToolSpec, handler: Box<dyn ToolHandler>) {
        self.entries.push((spec, handler));
    }

    pub(crate) fn with_pre_tool_hook(mut self, executable: impl Into<std::path::PathBuf>) -> Self {
        self.pre_tool_hook = Some(PreToolHook::new(executable.into()));
        self
    }

    /// The declarations to advertise on each provider request.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|(spec, _)| spec.clone()).collect()
    }

    /// Runs the handler named by a `tool_use` call and returns its
    /// `tool_result`. An unknown tool, failing handler, or hook denial yields
    /// an error result rather than aborting the turn.
    pub fn dispatch(&self, id: &str, name: &str, input: &serde_json::Value) -> ContentBlock {
        self.dispatch_with_status(id, name, input).0
    }

    fn dispatch_with_status(
        &self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> (ContentBlock, ToolStatus) {
        let result = match self
            .pre_tool_hook
            .as_ref()
            .map(|hook| hook.authorize(name, input))
        {
            Some(Err(message)) => Err((message, ToolStatus::Denied)),
            Some(Ok(())) | None => match self.entries.iter().find(|(spec, _)| spec.name == name) {
                Some((_, handler)) => handler
                    .call(input)
                    .map_err(|message| (message, ToolStatus::Failed)),
                None => Err((format!("unknown tool: {name}"), ToolStatus::Failed)),
            },
        };
        let (content, status) = match result {
            Ok(output) => (output, ToolStatus::Completed),
            Err((message, status)) => (message, status),
        };
        let is_error = (status != ToolStatus::Completed).then_some(true);
        (
            ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content,
                is_error,
            },
            status,
        )
    }
}

/// The result of one user turn run through a [`ToolLoop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// The final reply; its `usage` sums every provider call in the turn.
    pub reply: AssistantReply,
    /// The intermediate turns appended after the input history — each
    /// `tool_use` reply followed by its `tool_result` user turn — excluding
    /// the final reply.
    pub transcript: Vec<Message>,
    /// Whether the turn stopped at its configured round limit with the final
    /// reply still requesting tools (which were not executed).
    pub capped: bool,
}

/// An in-memory notification of one dispatched tool call's lifecycle.
///
/// Fired by [`ToolLoop::run_observed`]: one `Round` per dispatched tool round,
/// then per call a `Call` just before dispatch and one `Result` when it
/// finishes or is interrupted. The persisted trail form is
/// [`crate::events::ExchangeEvent::ToolRound`] /
/// [`crate::events::ExchangeEvent::ToolCall`] /
/// [`crate::events::ExchangeEvent::ToolResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEvent<'a> {
    /// A `tool_use` reply whose calls are about to be dispatched: its full
    /// content blocks, including signed thinking, in reply order.
    Round {
        /// The reply's content blocks.
        content: &'a [ContentBlock],
    },
    /// A `tool_use` call about to be dispatched.
    Call {
        /// Provider-assigned call id.
        id: &'a str,
        /// The called tool's name.
        name: &'a str,
        /// The call's JSON arguments.
        input: &'a serde_json::Value,
    },
    /// The terminal result of the call with the same `id`.
    Result {
        /// Provider-assigned call id, matching the `Call`.
        id: &'a str,
        /// The called tool's name.
        name: &'a str,
        /// The tool's output or the failure/denial message.
        output: &'a str,
        /// Whether the call completed, failed, or was denied by the hook.
        status: ToolStatus,
    },
}

/// An owned form of [`ToolEvent`] for fallible streaming callbacks.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedToolEvent {
    /// A tool-use reply in provider block order.
    Round { content: Vec<ContentBlock> },
    /// A tool call before dispatch.
    Call {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A completed, failed, denied, or interrupted tool call.
    Result {
        id: String,
        name: String,
        output: String,
        status: ToolStatus,
    },
}

impl OwnedToolEvent {
    fn from_borrowed(event: ToolEvent<'_>) -> Self {
        match event {
            ToolEvent::Round { content } => Self::Round {
                content: content.to_vec(),
            },
            ToolEvent::Call { id, name, input } => Self::Call {
                id: id.to_string(),
                name: name.to_string(),
                input: input.clone(),
            },
            ToolEvent::Result {
                id,
                name,
                output,
                status,
            } => Self::Result {
                id: id.to_string(),
                name: name.to_string(),
                output: output.to_string(),
                status,
            },
        }
    }

    /// Borrows the event in the form used by existing tool observers.
    pub fn as_tool_event(&self) -> ToolEvent<'_> {
        match self {
            Self::Round { content } => ToolEvent::Round { content },
            Self::Call { id, name, input } => ToolEvent::Call { id, name, input },
            Self::Result {
                id,
                name,
                output,
                status,
            } => ToolEvent::Result {
                id,
                name,
                output,
                status: *status,
            },
        }
    }
}

/// A callback receiving each [`ToolEvent`] as it happens.
pub type ToolObserver = Box<dyn FnMut(ToolEvent<'_>)>;

/// A [`Transport`] that iterates on `tool_use` replies, executing tools via a
/// [`ToolRegistry`].
pub struct ToolLoop<T: Transport> {
    transport: T,
    registry: ToolRegistry,
    max_tool_rounds: Option<usize>,
    observer: Option<RefCell<ToolObserver>>,
    context_budget: Option<ContextBudget>,
    context_state: RefCell<ContextState>,
}

#[derive(Debug, Clone, Default)]
struct ContextState {
    summary: Option<String>,
    calibration_factor: f64,
    dropped_groups: usize,
    history_turn_indices: Vec<u64>,
}

/// A compaction event emitted immediately before its retained provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompaction {
    /// First session turn represented by the retained message suffix.
    pub retained_from_turn_index: u64,
    /// Latest summary replacing the dropped history.
    pub summary: String,
}

struct ContextCall<'a> {
    session_id: Option<&'a str>,
    turn_index: u64,
    on_compaction: Option<&'a mut dyn FnMut(ContextCompaction)>,
}

#[derive(Clone, Copy)]
enum SystemPromptMode<'a> {
    Configured,
    Explicit(Option<&'a str>),
}

impl<T: Transport> ToolLoop<T> {
    /// Wraps `transport`, executing tool calls through `registry`.
    ///
    /// When `max_tool_rounds` is `None`, tool-use rounds are unbounded.
    pub fn new(transport: T, registry: ToolRegistry, max_tool_rounds: Option<usize>) -> Self {
        Self {
            transport,
            registry,
            max_tool_rounds,
            observer: None,
            context_budget: None,
            context_state: RefCell::new(ContextState::default()),
        }
    }

    /// Enables the configured context-window guard for every provider call.
    pub fn with_context_budget(mut self, budget: Option<ContextBudget>) -> Self {
        self.context_budget = budget;
        self
    }

    /// Restores the latest session summary before driving a resumed session.
    pub fn set_context_summary(&self, summary: Option<String>) {
        self.context_state.borrow_mut().summary = summary;
    }

    /// Restores the turn indices corresponding to a resumed history suffix.
    pub fn set_context_history_turn_indices(&self, indices: Vec<u64>) {
        self.context_state.borrow_mut().history_turn_indices = indices;
    }

    /// Records a completed session turn for exact compaction boundaries.
    pub fn record_context_turn(&self, turn_index: u64) {
        self.context_state
            .borrow_mut()
            .history_turn_indices
            .push(turn_index);
    }

    /// The configured maximum tool-use rounds, or `None` when unbounded.
    pub fn max_tool_rounds(&self) -> Option<usize> {
        self.max_tool_rounds
    }

    /// Notifies `observer` of every tool call made when this loop is driven
    /// as a [`Transport`] (the participant-driven `ask` path).
    pub fn with_observer(mut self, observer: ToolObserver) -> Self {
        self.observer = Some(RefCell::new(observer));
        self
    }

    /// Runs one user turn: sends `history` and iterates while the reply
    /// requests tools, stopping at the configured limit if one is set.
    pub fn run(&self, history: &[Message]) -> Result<TurnOutcome> {
        self.run_observed_with_attempts(history, &mut |_| {}).result
    }

    /// [`ToolLoop::run`], notifying `observe` of each dispatched round, then
    /// each of its calls and their results, in call order. An interrupted call
    /// emits a failed result before the turn returns its interruption error. The
    /// capped reply's calls are not dispatched and emit nothing.
    pub fn run_observed(
        &self,
        history: &[Message],
        observe: &mut dyn FnMut(ToolEvent<'_>),
    ) -> Result<TurnOutcome> {
        self.run_observed_with_attempts(history, observe).result
    }

    /// [`ToolLoop::run_observed`] with the total provider-attempt count for
    /// the user turn, including attempts made before a terminal error.
    pub fn run_observed_with_attempts(
        &self,
        history: &[Message],
        observe: &mut dyn FnMut(ToolEvent<'_>),
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            observe,
            None,
            None,
            SystemPromptMode::Configured,
            None,
        )
    }

    /// [`ToolLoop::run_observed_with_attempts`] with a system-prompt snapshot
    /// that applies to the initial provider call and every tool continuation.
    pub fn run_observed_with_system_prompt_with_attempts(
        &self,
        history: &[Message],
        system_prompt: Option<&str>,
        observe: &mut dyn FnMut(ToolEvent<'_>),
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            observe,
            None,
            None,
            SystemPromptMode::Explicit(system_prompt),
            None,
        )
    }

    /// Runs one user turn while forwarding provider stream events to
    /// `on_stream_event`, in addition to the tool lifecycle notifications.
    pub fn run_streaming_observed_with_attempts(
        &self,
        history: &[Message],
        observe: &mut dyn FnMut(ToolEvent<'_>),
        on_stream_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            observe,
            Some(on_stream_event),
            None,
            SystemPromptMode::Configured,
            None,
        )
    }

    /// Streaming counterpart to
    /// [`ToolLoop::run_observed_with_system_prompt_with_attempts`].
    pub fn run_streaming_observed_with_system_prompt_with_attempts(
        &self,
        history: &[Message],
        system_prompt: Option<&str>,
        observe: &mut dyn FnMut(ToolEvent<'_>),
        on_stream_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            observe,
            Some(on_stream_event),
            None,
            SystemPromptMode::Explicit(system_prompt),
            None,
        )
    }

    /// Streaming counterpart that can stop the loop when observing a tool
    /// lifecycle event fails (for example, when the caller's output pipe
    /// closes). A failed observer prevents the next tool from being dispatched.
    pub fn run_streaming_observed_fallible_with_attempts(
        &self,
        history: &[Message],
        observe: &mut dyn FnMut(OwnedToolEvent) -> Result<()>,
        on_stream_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            &mut |_| {},
            Some(on_stream_event),
            Some(observe),
            SystemPromptMode::Configured,
            None,
        )
    }

    /// Fallible-observer streaming counterpart with a system-prompt snapshot
    /// shared by the whole turn.
    pub fn run_streaming_observed_fallible_with_system_prompt_with_attempts(
        &self,
        history: &[Message],
        system_prompt: Option<&str>,
        observe: &mut dyn FnMut(OwnedToolEvent) -> Result<()>,
        on_stream_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            &mut |_| {},
            Some(on_stream_event),
            Some(observe),
            SystemPromptMode::Explicit(system_prompt),
            None,
        )
    }

    /// Runs a session turn with context compaction persistence before requests.
    pub fn run_observed_with_system_prompt_and_context_with_attempts(
        &self,
        history: &[Message],
        system_prompt: Option<&str>,
        session_id: &str,
        turn_index: u64,
        observe: &mut dyn FnMut(ToolEvent<'_>),
        on_compaction: &mut dyn FnMut(ContextCompaction),
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            observe,
            None,
            None,
            SystemPromptMode::Explicit(system_prompt),
            Some(ContextCall {
                session_id: Some(session_id),
                turn_index,
                on_compaction: Some(on_compaction),
            }),
        )
    }

    /// Streaming session counterpart with context compaction persistence.
    pub fn run_streaming_observed_with_system_prompt_and_context_with_attempts(
        &self,
        history: &[Message],
        system_prompt: Option<&str>,
        session_id: &str,
        turn_index: u64,
        observe: &mut dyn FnMut(ToolEvent<'_>),
        on_compaction: &mut dyn FnMut(ContextCompaction),
        on_stream_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            observe,
            Some(on_stream_event),
            None,
            SystemPromptMode::Explicit(system_prompt),
            Some(ContextCall {
                session_id: Some(session_id),
                turn_index,
                on_compaction: Some(on_compaction),
            }),
        )
    }

    /// Fallible streaming session counterpart with context compaction.
    pub fn run_streaming_observed_fallible_with_system_prompt_and_context_with_attempts(
        &self,
        history: &[Message],
        system_prompt: Option<&str>,
        session_id: &str,
        turn_index: u64,
        observe: &mut dyn FnMut(OwnedToolEvent) -> Result<()>,
        on_compaction: &mut dyn FnMut(ContextCompaction),
        on_stream_event: &mut dyn FnMut(StreamEvent) -> Result<()>,
    ) -> TransportCall<TurnOutcome> {
        self.run_observed_with_optional_stream(
            history,
            &mut |_| {},
            Some(on_stream_event),
            Some(observe),
            SystemPromptMode::Explicit(system_prompt),
            Some(ContextCall {
                session_id: Some(session_id),
                turn_index,
                on_compaction: Some(on_compaction),
            }),
        )
    }

    fn run_observed_with_optional_stream(
        &self,
        history: &[Message],
        observe: &mut dyn FnMut(ToolEvent<'_>),
        mut on_stream_event: Option<&mut dyn FnMut(StreamEvent) -> Result<()>>,
        mut observe_fallible: Option<&mut dyn FnMut(OwnedToolEvent) -> Result<()>>,
        system_prompt: SystemPromptMode<'_>,
        mut context_call: Option<ContextCall<'_>>,
    ) -> TransportCall<TurnOutcome> {
        let mut attempts = 0_u64;
        if let Err(error) = interrupt::check() {
            return TransportCall::completed(Err(error), attempts);
        }
        let mut context_state = self.context_state.borrow_mut();
        let mut messages = history.to_vec();
        let mut active_start = active_turn_start(&messages);
        if context_call.is_some()
            && context_state.dropped_groups > 0
            && let Some(cut) =
                cut_after_groups(&messages, active_start, context_state.dropped_groups)
        {
            messages.drain(..cut);
            active_start = active_start.saturating_sub(cut);
        }
        let mut transcript_start = messages.len();
        let call = self.provider_call(
            &mut messages,
            &mut active_start,
            &mut transcript_start,
            &mut context_state,
            system_prompt,
            &mut on_stream_event,
            &mut context_call,
        );
        attempts = attempts.saturating_add(call.attempts);
        if let Err(error) = interrupt::check() {
            return TransportCall::completed(Err(error), attempts);
        }
        let mut reply = match call.result {
            Ok(reply) => reply,
            Err(error) => return TransportCall::completed(Err(error), attempts),
        };
        let mut usage = reply.usage;
        let mut rounds = 0;

        while reply.stop_reason == Some(StopReason::ToolUse) {
            if let Err(error) = interrupt::check() {
                return TransportCall::completed(Err(error), attempts);
            }
            if let Some(max_tool_rounds) = self.max_tool_rounds
                && rounds >= max_tool_rounds
            {
                reply.usage = usage;
                return TransportCall::completed(
                    Ok(TurnOutcome {
                        reply,
                        transcript: messages.split_off(transcript_start),
                        capped: true,
                    }),
                    attempts,
                );
            }
            if let Err(error) = notify_tool_observers(
                observe,
                &mut observe_fallible,
                ToolEvent::Round {
                    content: &reply.content,
                },
            ) {
                return TransportCall::completed(Err(error), attempts);
            }
            let mut results = Vec::new();
            for block in &reply.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    if let Err(error) = interrupt::check() {
                        return TransportCall::completed(Err(error), attempts);
                    }
                    if let Err(error) = notify_tool_observers(
                        observe,
                        &mut observe_fallible,
                        ToolEvent::Call { id, name, input },
                    ) {
                        return TransportCall::completed(Err(error), attempts);
                    }
                    if let Some(error) = interrupt::error() {
                        if let Err(write_error) = observe_interrupted_result(
                            observe,
                            &mut observe_fallible,
                            id,
                            name,
                            &error,
                        ) {
                            return TransportCall::completed(Err(write_error), attempts);
                        }
                        return TransportCall::completed(Err(error), attempts);
                    }
                    let (result, status) = self.registry.dispatch_with_status(id, name, input);
                    if let Some(error) = interrupt::error() {
                        if let Err(write_error) = observe_interrupted_result(
                            observe,
                            &mut observe_fallible,
                            id,
                            name,
                            &error,
                        ) {
                            return TransportCall::completed(Err(write_error), attempts);
                        }
                        return TransportCall::completed(Err(error), attempts);
                    }
                    if let ContentBlock::ToolResult { content, .. } = &result
                        && let Err(error) = notify_tool_observers(
                            observe,
                            &mut observe_fallible,
                            ToolEvent::Result {
                                id,
                                name,
                                output: content,
                                status,
                            },
                        )
                    {
                        return TransportCall::completed(Err(error), attempts);
                    }
                    results.push(result);
                }
            }
            messages.push(Message::new(Role::Assistant, reply.content));
            messages.push(Message::new(Role::User, results));
            rounds = rounds.saturating_add(1);

            let call = self.provider_call(
                &mut messages,
                &mut active_start,
                &mut transcript_start,
                &mut context_state,
                system_prompt,
                &mut on_stream_event,
                &mut context_call,
            );
            attempts = attempts.saturating_add(call.attempts);
            if let Err(error) = interrupt::check() {
                return TransportCall::completed(Err(error), attempts);
            }
            reply = match call.result {
                Ok(reply) => reply,
                Err(error) => return TransportCall::completed(Err(error), attempts),
            };
            usage = add_usage(usage, reply.usage);
        }

        reply.usage = usage;
        TransportCall::completed(
            Ok(TurnOutcome {
                reply,
                transcript: messages.split_off(transcript_start),
                capped: false,
            }),
            attempts,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn provider_call(
        &self,
        messages: &mut Vec<Message>,
        active_start: &mut usize,
        transcript_start: &mut usize,
        state: &mut ContextState,
        system_prompt: SystemPromptMode<'_>,
        on_stream_event: &mut Option<&mut dyn FnMut(StreamEvent) -> Result<()>>,
        context_call: &mut Option<ContextCall<'_>>,
    ) -> TransportCall<AssistantReply> {
        let streaming = on_stream_event.is_some();
        let configured_mode = matches!(system_prompt, SystemPromptMode::Configured);
        let base_prompt = match system_prompt {
            SystemPromptMode::Configured => self.transport.configured_system_prompt(),
            SystemPromptMode::Explicit(prompt) => prompt,
        };
        let allow_compaction = context_call
            .as_ref()
            .is_some_and(|call| call.session_id.is_some());
        let turn_index = context_call.as_ref().map_or(0, |call| call.turn_index);
        let mut attempts = 0_u64;
        let mut prepared = match self.prepare_context(
            messages,
            *active_start,
            base_prompt,
            streaming,
            state,
            allow_compaction,
            false,
            configured_mode,
            turn_index,
            &mut attempts,
        ) {
            Ok(prepared) => prepared,
            Err(error) => return TransportCall::completed(Err(error), attempts),
        };
        self.apply_prepared_context(
            messages,
            active_start,
            transcript_start,
            state,
            &mut prepared,
            context_call,
        );
        let mut call = self.send_prepared(&prepared, on_stream_event);
        attempts = attempts.saturating_add(call.attempts);

        if self.context_budget.is_some()
            && call
                .result
                .as_ref()
                .err()
                .is_some_and(crate::error::LegError::is_context_window_rejection)
        {
            prepared = match self.prepare_context(
                messages,
                *active_start,
                base_prompt,
                streaming,
                state,
                allow_compaction,
                true,
                configured_mode,
                turn_index,
                &mut attempts,
            ) {
                Ok(prepared) => prepared,
                Err(error) => return TransportCall::completed(Err(error), attempts),
            };
            self.apply_prepared_context(
                messages,
                active_start,
                transcript_start,
                state,
                &mut prepared,
                context_call,
            );
            call = self.send_prepared(&prepared, on_stream_event);
            attempts = attempts.saturating_add(call.attempts);
            if call
                .result
                .as_ref()
                .err()
                .is_some_and(crate::error::LegError::is_context_window_rejection)
            {
                return TransportCall::completed(
                    Err(context_window_error(
                        "provider rejected the compacted request after one retry",
                    )),
                    attempts,
                );
            }
        }

        if let Ok(reply) = &call.result
            && let Some(reported) = reply.usage.input_tokens
            && prepared.baseline_tokens > 0
        {
            state.calibration_factor = state
                .calibration_factor
                .max((reported as f64 / prepared.baseline_tokens as f64).max(1.0));
        }
        TransportCall::completed(call.result, attempts)
    }

    fn prepare_context(
        &self,
        messages: &[Message],
        active_start: usize,
        base_prompt: Option<&str>,
        streaming: bool,
        state: &mut ContextState,
        allow_compaction: bool,
        force_compaction: bool,
        configured_mode: bool,
        turn_index: u64,
        attempts: &mut u64,
    ) -> Result<PreparedContext> {
        let projection = project_tool_results(messages);
        let original_prompt = system_prompt_with_summary(base_prompt, state.summary.as_deref());
        let Some(budget) = self.context_budget else {
            return Ok(PreparedContext {
                messages: projection,
                system_prompt: original_prompt,
                use_configured_prompt: configured_mode && state.summary.is_none(),
                baseline_tokens: 0,
                cut: 0,
                dropped_groups: 0,
                compaction: None,
            });
        };
        let base_bytes = self.transport.estimate_input_bytes(
            &projection,
            original_prompt.as_deref(),
            streaming,
        )?;
        let baseline = baseline_tokens(base_bytes);
        let usable = budget.input_tokens();
        if !force_compaction && calibrated_tokens(baseline, state.calibration_factor) <= usable {
            return Ok(PreparedContext {
                messages: projection,
                system_prompt: original_prompt,
                use_configured_prompt: configured_mode && state.summary.is_none(),
                baseline_tokens: baseline,
                cut: 0,
                dropped_groups: 0,
                compaction: None,
            });
        }
        if !allow_compaction {
            return Err(context_window_error(&format!(
                "projected input is {} tokens; usable budget is {usable}",
                calibrated_tokens(baseline, state.calibration_factor)
            )));
        }

        let starts = historical_group_starts(messages, active_start);
        if starts.is_empty() {
            return Err(context_window_error(
                "the active user turn and tool chain cannot fit within the usable context budget",
            ));
        }

        let mut chosen = None;
        for groups_to_drop in 1..=starts.len() {
            let cut = cut_after_groups(messages, active_start, groups_to_drop)
                .expect("a cut count within the historical group count is valid");
            let tail = project_tool_results(&messages[cut..]);
            let tail_baseline = baseline_tokens(self.transport.estimate_input_bytes(
                &tail,
                base_prompt,
                streaming,
            )?);
            let historical_end = active_start.saturating_sub(cut);
            let historical = if historical_end == 0 {
                0
            } else {
                baseline_tokens(self.transport.estimate_input_bytes(
                    &tail[..historical_end.min(tail.len())],
                    None,
                    streaming,
                )?)
            };
            if historical <= budget.keep_recent_tokens
                && calibrated_tokens(tail_baseline, state.calibration_factor) <= usable
            {
                chosen = Some((groups_to_drop, cut));
                break;
            }
        }
        let Some((mut groups_to_drop, mut cut)) = chosen else {
            return Err(context_window_error(
                "the active user turn and tool chain cannot fit after dropping complete historical turns",
            ));
        };

        let summary_max = budget.reserve_tokens.min(4096) as u32;
        loop {
            let prior_summary = state.summary.clone();
            let new_summary = self.generate_summary(
                prior_summary.as_deref(),
                &messages[..cut],
                summary_max,
                usable,
                &mut state.calibration_factor,
                attempts,
            )?;
            let system_prompt = system_prompt_with_summary(base_prompt, Some(&new_summary));
            let tail = project_tool_results(&messages[cut..]);
            let request_baseline = baseline_tokens(self.transport.estimate_input_bytes(
                &tail,
                system_prompt.as_deref(),
                streaming,
            )?);
            if calibrated_tokens(request_baseline, state.calibration_factor) <= usable {
                let retained = retained_group_count(messages, active_start, cut);
                let total_dropped = state.dropped_groups.saturating_add(groups_to_drop);
                let retained_from_turn_index = state
                    .history_turn_indices
                    .get(total_dropped)
                    .copied()
                    .unwrap_or_else(|| turn_index.saturating_sub(retained as u64));
                return Ok(PreparedContext {
                    messages: tail,
                    system_prompt,
                    use_configured_prompt: false,
                    baseline_tokens: request_baseline,
                    cut,
                    dropped_groups: groups_to_drop,
                    compaction: Some(ContextCompaction {
                        retained_from_turn_index,
                        summary: new_summary,
                    }),
                });
            }
            if groups_to_drop >= starts.len() {
                return Err(context_window_error(
                    "the active user turn, retained summary, and required provider framing exceed the usable context budget",
                ));
            }
            groups_to_drop += 1;
            cut = cut_after_groups(messages, active_start, groups_to_drop)
                .expect("the next complete historical group boundary exists");
        }
    }

    fn generate_summary(
        &self,
        prior_summary: Option<&str>,
        dropped_messages: &[Message],
        max_output_tokens: u32,
        usable_tokens: u64,
        calibration_factor: &mut f64,
        attempts: &mut u64,
    ) -> Result<String> {
        const SUMMARY_SYSTEM: &str = "Summarize earlier conversation context for the assistant. Preserve concrete facts, user intent, decisions, constraints, and unresolved work. Do not include private reasoning. Return only the summary.";

        let source = summary_source(None, dropped_messages);
        let chars = source.chars().collect::<Vec<_>>();
        let mut offset = 0;
        let mut summary = prior_summary.unwrap_or_default().to_string();
        while offset < chars.len() {
            let mut take = chars.len() - offset;
            let (messages, chunk_end, request_baseline) = loop {
                let end = (offset + take).min(chars.len());
                let chunk = chars[offset..end].iter().collect::<String>();
                let prompt = if summary.is_empty() {
                    format!("Conversation material:\n{chunk}")
                } else {
                    format!(
                        "Previous summary:\n{summary}\n\nAdditional conversation material:\n{chunk}"
                    )
                };
                let messages = [Message::user(prompt)];
                let bytes = self.transport.estimate_summary_input_bytes(
                    &messages,
                    Some(SUMMARY_SYSTEM),
                    max_output_tokens,
                )?;
                let baseline = baseline_tokens(bytes);
                if calibrated_tokens(baseline, *calibration_factor) <= usable_tokens {
                    break (messages, end, baseline);
                }
                if take <= 1 {
                    return Err(context_window_error(
                        "the internal summary prompt cannot fit within the usable context budget",
                    ));
                }
                take = (take / 2).max(1);
            };
            let call = self.transport.send_context_summary(
                &messages,
                Some(SUMMARY_SYSTEM),
                max_output_tokens,
            );
            *attempts = (*attempts).saturating_add(call.attempts);
            let reply = match call.result {
                Ok(reply) => reply,
                Err(error) if error.is_context_window_rejection() => {
                    return Err(context_window_error(
                        "the provider rejected the bounded internal summary request",
                    ));
                }
                Err(error) => return Err(error),
            };
            if let Some(reported) = reply.usage.input_tokens
                && request_baseline > 0
            {
                *calibration_factor =
                    (*calibration_factor).max((reported as f64 / request_baseline as f64).max(1.0));
            }
            if reply.text.trim().is_empty() {
                return Err(context_window_error(
                    "the provider returned an empty context summary",
                ));
            }
            summary = reply.text;
            offset = chunk_end;
        }
        if summary.is_empty() {
            return Err(context_window_error(
                "the provider returned an empty context summary",
            ));
        }
        Ok(summary)
    }

    fn apply_prepared_context(
        &self,
        messages: &mut Vec<Message>,
        active_start: &mut usize,
        transcript_start: &mut usize,
        state: &mut ContextState,
        prepared: &mut PreparedContext,
        context_call: &mut Option<ContextCall<'_>>,
    ) {
        if prepared.cut == 0 {
            return;
        }
        messages.drain(..prepared.cut);
        *active_start = active_start.saturating_sub(prepared.cut);
        *transcript_start = transcript_start.saturating_sub(prepared.cut);
        state.dropped_groups = state.dropped_groups.saturating_add(prepared.dropped_groups);
        if let Some(compaction) = prepared.compaction.take() {
            state.summary = Some(compaction.summary.clone());
            if let Some(callback) = context_call
                .as_mut()
                .and_then(|call| call.on_compaction.as_deref_mut())
            {
                callback(compaction);
            }
        }
    }

    fn send_prepared(
        &self,
        prepared: &PreparedContext,
        on_stream_event: &mut Option<&mut dyn FnMut(StreamEvent) -> Result<()>>,
    ) -> TransportCall<AssistantReply> {
        let use_configured = prepared.use_configured_prompt;
        match (use_configured, on_stream_event.as_mut()) {
            (true, Some(on_event)) => self
                .transport
                .send_conversation_streaming_with_attempts(&prepared.messages, *on_event),
            (true, None) => self
                .transport
                .send_conversation_with_attempts(&prepared.messages),
            (false, Some(on_event)) => self
                .transport
                .send_conversation_streaming_with_system_prompt(
                    &prepared.messages,
                    prepared.system_prompt.as_deref(),
                    *on_event,
                ),
            (false, None) => self.transport.send_conversation_with_system_prompt(
                &prepared.messages,
                prepared.system_prompt.as_deref(),
            ),
        }
    }
}

struct PreparedContext {
    messages: Vec<Message>,
    system_prompt: Option<String>,
    use_configured_prompt: bool,
    baseline_tokens: u64,
    cut: usize,
    dropped_groups: usize,
    compaction: Option<ContextCompaction>,
}

fn active_turn_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (message.role == Role::User && !crate::context::is_tool_result_message(message))
                .then_some(index)
        })
        .unwrap_or_else(|| messages.len().saturating_sub(1))
}

fn context_window_error(message: &str) -> crate::error::LegError {
    crate::error::LegError::ContextWindow(message.to_string())
}

fn observe_interrupted_result(
    observe: &mut dyn FnMut(ToolEvent<'_>),
    observe_fallible: &mut Option<&mut dyn FnMut(OwnedToolEvent) -> Result<()>>,
    id: &str,
    name: &str,
    error: &crate::error::LegError,
) -> Result<()> {
    let output = error.to_string();
    notify_tool_observers(
        observe,
        observe_fallible,
        ToolEvent::Result {
            id,
            name,
            output: &output,
            status: ToolStatus::Failed,
        },
    )
}

fn notify_tool_observers(
    observe: &mut dyn FnMut(ToolEvent<'_>),
    observe_fallible: &mut Option<&mut dyn FnMut(OwnedToolEvent) -> Result<()>>,
    event: ToolEvent<'_>,
) -> Result<()> {
    observe(event);
    if let Some(observer) = observe_fallible.as_mut() {
        (**observer)(OwnedToolEvent::from_borrowed(event))
    } else {
        Ok(())
    }
}

impl<T: Transport> Transport for ToolLoop<T> {
    /// Runs the tool loop and returns only the final reply, warning on stderr
    /// when the turn hit its configured round limit.
    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
        self.send_conversation_with_attempts(messages).result
    }

    fn send_conversation_with_attempts(
        &self,
        messages: &[Message],
    ) -> TransportCall<AssistantReply> {
        let call = match &self.observer {
            Some(observer) => self
                .run_observed_with_attempts(messages, &mut |event| (observer.borrow_mut())(event)),
            None => self.run_observed_with_attempts(messages, &mut |_| {}),
        };
        let attempts = call.attempts;
        let result = call.result.map(|outcome| {
            if outcome.capped {
                let max_tool_rounds = self
                    .max_tool_rounds
                    .expect("a capped turn has a configured round limit");
                eprintln!("{}", tool_round_limit_warning(max_tool_rounds));
            }
            outcome.reply
        });
        TransportCall::completed(result, attempts)
    }
}

/// Sums two usages per count; a count stays `None` only when both are.
fn add_usage(a: TokenUsage, b: TokenUsage) -> TokenUsage {
    let add = |x: Option<u64>, y: Option<u64>| match (x, y) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
    };
    TokenUsage {
        input_tokens: add(a.input_tokens, b.input_tokens),
        output_tokens: add(a.output_tokens, b.output_tokens),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    /// Answers from a queue of canned replies and records every call's history.
    pub(crate) struct ScriptedTransport {
        pub(crate) replies: RefCell<VecDeque<AssistantReply>>,
        pub(crate) calls: RefCell<Vec<Vec<Message>>>,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct CapturedContextRequest {
        pub(crate) messages: Vec<Message>,
        pub(crate) system_prompt: Option<String>,
        pub(crate) estimated_bytes: usize,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct CapturedSummaryRequest {
        pub(crate) messages: Vec<Message>,
        pub(crate) system_prompt: Option<String>,
        pub(crate) max_output_tokens: u32,
    }

    /// A provider fake that records actual and internal-summary projections,
    /// including a fixed tool declaration and wire-framing cost in estimates.
    pub(crate) struct BudgetTransport {
        pub(crate) replies: RefCell<VecDeque<Result<AssistantReply>>>,
        pub(crate) requests: RefCell<Vec<CapturedContextRequest>>,
        pub(crate) summaries: RefCell<Vec<CapturedSummaryRequest>>,
        pub(crate) summary_replies: RefCell<VecDeque<Result<AssistantReply>>>,
    }

    impl BudgetTransport {
        pub(crate) fn new(replies: Vec<Result<AssistantReply>>) -> Self {
            Self {
                replies: RefCell::new(replies.into()),
                requests: RefCell::new(Vec::new()),
                summaries: RefCell::new(Vec::new()),
                summary_replies: RefCell::new(VecDeque::new()),
            }
        }

        fn input_bytes(
            messages: &[Message],
            system_prompt: Option<&str>,
            streaming: bool,
            include_tools: bool,
            output_tokens: u32,
        ) -> usize {
            let messages = messages
                .iter()
                .map(|message| {
                    serde_json::json!({
                        "role": message.role.as_str(),
                        "content": message.content,
                    })
                })
                .collect::<Vec<_>>();
            let tools = if include_tools {
                serde_json::json!([{
                    "name": "read",
                    "description": "Read a UTF-8 text file",
                    "parameters": {"type":"object", "properties":{"path":{"type":"string"}}}
                }])
            } else {
                serde_json::json!([])
            };
            serde_json::to_vec(&serde_json::json!({
                "protocol": "fake-provider/v1",
                "model": "budget-test",
                "max_tokens": output_tokens,
                "stream": streaming,
                "system": system_prompt,
                "tools": tools,
                "messages": messages,
            }))
            .expect("serializes fake provider request")
            .len()
        }
    }

    impl Transport for BudgetTransport {
        fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
            self.send_conversation_with_system_prompt(messages, None)
                .result
        }

        fn send_conversation_with_system_prompt(
            &self,
            messages: &[Message],
            system_prompt: Option<&str>,
        ) -> TransportCall<AssistantReply> {
            let bytes = Self::input_bytes(messages, system_prompt, false, true, 32_000);
            self.requests.borrow_mut().push(CapturedContextRequest {
                messages: messages.to_vec(),
                system_prompt: system_prompt.map(str::to_string),
                estimated_bytes: bytes,
            });
            TransportCall::new(
                self.replies
                    .borrow_mut()
                    .pop_front()
                    .expect("test queues enough provider replies"),
                1,
            )
        }

        fn estimate_input_bytes(
            &self,
            messages: &[Message],
            system_prompt: Option<&str>,
            streaming: bool,
        ) -> Result<usize> {
            Ok(Self::input_bytes(
                messages,
                system_prompt,
                streaming,
                true,
                32_000,
            ))
        }

        fn estimate_summary_input_bytes(
            &self,
            messages: &[Message],
            system_prompt: Option<&str>,
            max_output_tokens: u32,
        ) -> Result<usize> {
            Ok(Self::input_bytes(
                messages,
                system_prompt,
                false,
                false,
                max_output_tokens,
            ))
        }

        fn send_context_summary(
            &self,
            messages: &[Message],
            system_prompt: Option<&str>,
            max_output_tokens: u32,
        ) -> TransportCall<AssistantReply> {
            self.summaries.borrow_mut().push(CapturedSummaryRequest {
                messages: messages.to_vec(),
                system_prompt: system_prompt.map(str::to_string),
                max_output_tokens,
            });
            let result = self
                .summary_replies
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(AssistantReply::new("summary of earlier turns")));
            TransportCall::new(result, 1)
        }
    }

    struct LargeResultTool(String);

    impl ToolHandler for LargeResultTool {
        fn call(&self, _input: &serde_json::Value) -> std::result::Result<String, String> {
            Ok(self.0.clone())
        }
    }

    pub(crate) fn large_result_registry(result: String) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(
            ToolSpec::new(
                "read",
                "Read a file.",
                serde_json::json!({"type":"object", "properties":{"path":{"type":"string"}}}),
            ),
            Box::new(LargeResultTool(result)),
        );
        registry
    }

    pub(crate) fn context_budget(window: u64, reserve: u64, keep_recent: u64) -> ContextBudget {
        ContextBudget {
            window_tokens: window,
            reserve_tokens: reserve,
            keep_recent_tokens: keep_recent,
        }
    }

    impl ScriptedTransport {
        pub(crate) fn new(replies: Vec<AssistantReply>) -> Self {
            Self {
                replies: RefCell::new(replies.into()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for ScriptedTransport {
        fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
            self.calls.borrow_mut().push(messages.to_vec());
            Ok(self
                .replies
                .borrow_mut()
                .pop_front()
                .expect("test queued enough replies for every expected call"))
        }
    }

    /// A stub handler that echoes its `text` argument and counts calls.
    pub(crate) struct EchoTool(pub(crate) std::rc::Rc<Cell<usize>>);

    impl ToolHandler for EchoTool {
        fn call(&self, input: &serde_json::Value) -> std::result::Result<String, String> {
            self.0.set(self.0.get() + 1);
            Ok(format!("echo: {}", input["text"].as_str().unwrap_or("")))
        }
    }

    pub(crate) fn echo_registry(count: std::rc::Rc<Cell<usize>>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(
            ToolSpec::new("echo", "Echo text.", serde_json::json!({"type": "object"})),
            Box::new(EchoTool(count)),
        );
        registry
    }

    pub(crate) fn tool_use_reply(id: &str, name: &str) -> AssistantReply {
        AssistantReply::from_blocks(
            vec![
                ContentBlock::text("calling"),
                ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: serde_json::json!({"text": "hi"}),
                },
            ],
            TokenUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
            },
            Some(StopReason::ToolUse),
        )
    }

    #[test]
    fn context_compacts_before_tool_continuation_and_preserves_full_tool_result() {
        let large_result = "x".repeat(2_501);
        let transport = BudgetTransport::new(vec![
            Ok(tool_use_reply("tool-1", "read")),
            Ok(AssistantReply::new("done")),
        ]);
        let tool_loop = ToolLoop::new(transport, large_result_registry(large_result.clone()), None)
            .with_context_budget(Some(context_budget(1_200, 200, 400)));
        let old_tool_result = "y".repeat(2_501);
        let history = vec![
            Message::user("old context ".repeat(10)),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "old-tool".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"old.txt"}),
                }],
            ),
            Message::new(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "old-tool".into(),
                    content: old_tool_result,
                    is_error: None,
                }],
            ),
            Message::assistant("old answer"),
            Message::user("read the file"),
        ];
        let mut compactions = Vec::new();
        let mut observed_results = Vec::new();
        let outcome = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &history,
            Some("project instructions"),
            "sess-budget",
            1,
            &mut |event| {
                if let ToolEvent::Result { output, .. } = event {
                    observed_results.push(output.to_string());
                }
            },
            &mut |event| compactions.push(event),
        );
        let outcome = outcome.result.expect("turn fits after compaction");

        let captured = tool_loop.transport.requests.borrow();
        assert_eq!(captured.len(), 2);
        let usable = 1_000;
        assert!(
            captured
                .iter()
                .all(|request| { baseline_tokens(request.estimated_bytes) <= usable })
        );
        assert_eq!(
            captured[0].system_prompt.as_deref(),
            Some("project instructions")
        );
        assert!(
            captured[1]
                .system_prompt
                .as_deref()
                .unwrap()
                .contains("Earlier conversation summary:")
        );
        assert!(
            !captured[1]
                .messages
                .iter()
                .any(|message| message.text().contains("old context"))
        );

        let sent_result = captured[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .expect("continuation carries the tool result");
        assert_eq!(
            sent_result,
            &format!("{}{TOOL_RESULT_TRUNCATION_MARKER}", "x".repeat(2_000))
        );
        let old_sent_result = captured[0]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .expect("old result is projected too");
        assert_eq!(
            old_sent_result,
            &format!("{}{TOOL_RESULT_TRUNCATION_MARKER}", "y".repeat(2_000))
        );
        let tool_use_ids = captured[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_use_ids, ["tool-1"]);
        assert_eq!(observed_results, [large_result.clone()]);
        assert!(outcome.transcript.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content, .. } if content == &large_result
                )
            })
        }));
        assert_eq!(compactions.len(), 1);
        assert_eq!(compactions[0].retained_from_turn_index, 1);

        let summaries = tool_loop.transport.summaries.borrow();
        assert!(!summaries.is_empty());
        assert!(
            summaries
                .iter()
                .all(|summary| summary.max_output_tokens == 200)
        );
        assert!(summaries.iter().all(|summary| summary.messages.len() == 1));
        assert!(
            summaries
                .iter()
                .all(|summary| summary.system_prompt.is_some())
        );
        let summary_source = summaries
            .iter()
            .flat_map(|summary| summary.messages.iter())
            .map(Message::text)
            .collect::<String>();
        assert!(summary_source.contains(TOOL_RESULT_TRUNCATION_MARKER));
        // The source contributes 2,000 y characters; the marker itself has
        // one more y in "by".
        assert_eq!(summary_source.matches('y').count(), 2_001);
    }

    #[test]
    fn reported_usage_calibrates_the_next_session_projection() {
        let old_history = vec![
            Message::user("old facts ".repeat(55)),
            Message::assistant("acknowledged"),
            Message::user("first current turn"),
        ];
        let first_bytes = BudgetTransport::input_bytes(&old_history, None, false, true, 32_000);
        let reported = baseline_tokens(first_bytes).saturating_mul(4);
        let first_reply = AssistantReply::from_blocks(
            vec![ContentBlock::text("first done")],
            TokenUsage {
                input_tokens: Some(reported),
                output_tokens: Some(1),
            },
            Some(StopReason::EndTurn),
        );
        let transport = BudgetTransport::new(vec![
            Ok(first_reply),
            Ok(AssistantReply::new("second done")),
        ]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(1_200, 200, 200)));
        let mut compactions = Vec::new();

        tool_loop
            .run_observed_with_system_prompt_and_context_with_attempts(
                &old_history,
                None,
                "sess-calibrated",
                1,
                &mut |_| {},
                &mut |event| compactions.push(event),
            )
            .result
            .expect("first request fits baseline budget");

        let mut next_history = old_history;
        next_history.push(Message::assistant("first done"));
        next_history.push(Message::user("second current turn"));
        let raw_next_estimate = baseline_tokens(BudgetTransport::input_bytes(
            &next_history,
            None,
            false,
            true,
            32_000,
        ));
        assert!(raw_next_estimate < 1_000, "baseline: {raw_next_estimate}");
        assert!(raw_next_estimate.saturating_mul(4) > 1_000);

        tool_loop
            .run_observed_with_system_prompt_and_context_with_attempts(
                &next_history,
                None,
                "sess-calibrated",
                2,
                &mut |_| {},
                &mut |event| compactions.push(event),
            )
            .result
            .expect("usage-calibrated projection compacts before dispatch");

        assert_eq!(compactions.len(), 1);
        let requests = tool_loop.transport.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[1]
                .messages
                .iter()
                .any(|message| message.text().contains("old facts"))
        );
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .filter(|message| message.text().contains("second current turn"))
                .count(),
            1
        );
        assert!(baseline_tokens(requests[1].estimated_bytes) < 1_000);
    }

    #[test]
    fn internal_summary_usage_calibrates_later_projections() {
        let transport = BudgetTransport::new(Vec::new());
        transport
            .summary_replies
            .borrow_mut()
            .push_back(Ok(AssistantReply::from_blocks(
                vec![ContentBlock::text("summary")],
                TokenUsage {
                    input_tokens: Some(10_000),
                    output_tokens: Some(1),
                },
                Some(StopReason::EndTurn),
            )));
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None);
        let mut factor = 1.0;
        let mut attempts = 0;

        assert_eq!(
            tool_loop
                .generate_summary(
                    None,
                    &[Message::user("known facts")],
                    200,
                    100_000,
                    &mut factor,
                    &mut attempts,
                )
                .expect("summary succeeds"),
            "summary"
        );
        assert_eq!(attempts, 1);
        assert!(factor > 1.0);
    }

    #[test]
    fn resumed_session_sends_the_restored_summary_with_only_the_retained_suffix() {
        let transport = BudgetTransport::new(vec![Ok(AssistantReply::new("done"))]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(10_000, 1_000, 2_000)));
        tool_loop.set_context_summary(Some("restored facts and constraints".to_string()));
        tool_loop.set_context_history_turn_indices(vec![7]);
        let history = vec![
            Message::user("retained turn"),
            Message::assistant("retained answer"),
            Message::user("current turn"),
        ];

        tool_loop
            .run_observed_with_system_prompt_and_context_with_attempts(
                &history,
                Some("base instructions"),
                "sess-resumed",
                8,
                &mut |_| {},
                &mut |_| {},
            )
            .result
            .expect("resumed request fits");

        let requests = tool_loop.transport.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .system_prompt
                .as_deref()
                .unwrap()
                .starts_with("Earlier conversation summary:\nrestored facts and constraints")
        );
        assert!(
            requests[0]
                .system_prompt
                .as_deref()
                .unwrap()
                .contains("base instructions")
        );
        assert_eq!(requests[0].messages, history);
    }

    #[test]
    fn resumed_session_compacts_again_before_next_turn_and_advances_boundary() {
        let transport = BudgetTransport::new(vec![Ok(AssistantReply::new("done"))]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(10_000, 1_000, 2_000)));
        tool_loop.set_context_summary(Some("summary from the previous compaction".to_string()));
        tool_loop.set_context_history_turn_indices(vec![7]);
        let history = vec![
            Message::user("retained historical context ".repeat(1_000)),
            Message::assistant("historical answer ".repeat(1_000)),
            Message::user("current turn"),
        ];
        let mut compactions = Vec::new();

        tool_loop
            .run_observed_with_system_prompt_and_context_with_attempts(
                &history,
                None,
                "sess-resumed-compact",
                8,
                &mut |_| {},
                &mut |event| compactions.push(event),
            )
            .result
            .expect("resumed history compacts before dispatch");

        assert_eq!(compactions.len(), 1);
        assert_eq!(compactions[0].retained_from_turn_index, 8);
        assert!(compactions[0].summary.contains("summary of earlier turns"));
        let requests = tool_loop.transport.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages, [Message::user("current turn")]);
        assert!(
            requests[0]
                .system_prompt
                .as_deref()
                .unwrap()
                .starts_with("Earlier conversation summary:\nsummary of earlier turns")
        );
        assert!(
            tool_loop.transport.summaries.borrow()[0].messages[0]
                .text()
                .contains("Previous summary:\nsummary from the previous compaction")
        );
    }

    #[test]
    fn compaction_keeps_multiple_tool_rounds_as_valid_groups() {
        let transport = BudgetTransport::new(vec![
            Ok(tool_use_reply("tool-1", "read")),
            Ok(tool_use_reply("tool-2", "read")),
            Ok(AssistantReply::new("done")),
        ]);
        let tool_loop = ToolLoop::new(
            transport,
            large_result_registry("small result".to_string()),
            None,
        )
        .with_context_budget(Some(context_budget(1_200, 200, 400)));
        let history = vec![
            Message::user("discarded history ".repeat(300)),
            Message::assistant("old reply"),
            Message::user("active request"),
        ];
        let mut compactions = Vec::new();
        tool_loop
            .run_observed_with_system_prompt_and_context_with_attempts(
                &history,
                None,
                "sess-rounds",
                1,
                &mut |_| {},
                &mut |event| compactions.push(event),
            )
            .result
            .expect("all tool rounds stay within the budget");

        let requests = tool_loop.transport.requests.borrow();
        assert_eq!(requests.len(), 3);
        assert_eq!(compactions.len(), 1);
        for request in requests.iter().skip(1) {
            let tool_use_ids = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let result_ids = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(tool_use_ids, result_ids);
        }
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["tool-1"]
        );
        assert_eq!(
            requests[2]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["tool-1", "tool-2"]
        );
    }

    #[test]
    fn provider_context_rejection_compacts_and_retries_without_duplicating_user_turn() {
        let rejection = crate::error::LegError::Api {
            status: 400,
            error_type: Some("invalid_request_error".to_string()),
            message: "maximum context length exceeded".to_string(),
        };
        let transport =
            BudgetTransport::new(vec![Err(rejection), Ok(AssistantReply::new("retried"))]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(10_000, 1_000, 2_000)));
        let history = vec![
            Message::user("old turn"),
            Message::assistant("old answer"),
            Message::user("current turn"),
        ];
        let mut compactions = Vec::new();
        let call = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &history,
            None,
            "sess-retry",
            1,
            &mut |_| {},
            &mut |event| compactions.push(event),
        );

        assert_eq!(
            call.result.expect("one compact retry succeeds").reply.text,
            "retried"
        );
        let requests = tool_loop.transport.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request
                .messages
                .iter()
                .filter(|message| message.text().contains("current turn"))
                .count()
                == 1
        }));
        assert_eq!(compactions.len(), 1);
    }

    #[test]
    fn second_provider_context_rejection_returns_context_window_error() {
        let rejection = || crate::error::LegError::Api {
            status: 400,
            error_type: Some("invalid_request_error".to_string()),
            message: "maximum context length exceeded".to_string(),
        };
        let transport = BudgetTransport::new(vec![Err(rejection()), Err(rejection())]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(10_000, 1_000, 2_000)));
        let history = vec![
            Message::user("old turn"),
            Message::assistant("old answer"),
            Message::user("current turn"),
        ];
        let mut compactions = Vec::new();
        let call = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &history,
            None,
            "sess-second-rejection",
            1,
            &mut |_| {},
            &mut |event| compactions.push(event),
        );

        let error = call.result.unwrap_err();
        assert_eq!(error.kind(), "context_window");
        assert_eq!(call.attempts, 3);
        assert_eq!(tool_loop.transport.requests.borrow().len(), 2);
        assert_eq!(compactions.len(), 1);
    }

    #[test]
    fn failed_compaction_preparation_preserves_rejected_request_attempts() {
        let rejection = crate::error::LegError::Api {
            status: 400,
            error_type: Some("invalid_request_error".to_string()),
            message: "maximum context length exceeded".to_string(),
        };
        let transport = BudgetTransport::new(vec![Err(rejection)]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(10_000, 1_000, 2_000)));
        let call = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &[Message::user("current turn")],
            None,
            "sess-uncompactable-rejection",
            0,
            &mut |_| {},
            &mut |_| {},
        );

        assert_eq!(call.result.unwrap_err().kind(), "context_window");
        assert_eq!(call.attempts, 1);
        assert_eq!(tool_loop.transport.requests.borrow().len(), 1);
    }

    #[test]
    fn failed_internal_summary_preserves_summary_provider_attempts() {
        let transport = BudgetTransport::new(vec![]);
        transport
            .summary_replies
            .borrow_mut()
            .push_back(Err(crate::error::LegError::Api {
                status: 400,
                error_type: Some("invalid_request_error".to_string()),
                message: "maximum context length exceeded".to_string(),
            }));
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(1_200, 200, 400)));
        let history = vec![
            Message::user("old context ".repeat(300)),
            Message::assistant("old answer"),
            Message::user("current turn"),
        ];
        let call = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &history,
            None,
            "sess-summary-rejection",
            1,
            &mut |_| {},
            &mut |_| {},
        );

        assert_eq!(call.result.unwrap_err().kind(), "context_window");
        assert_eq!(call.attempts, 1);
        assert!(tool_loop.transport.requests.borrow().is_empty());
        assert_eq!(tool_loop.transport.summaries.borrow().len(), 1);
    }

    #[test]
    fn active_turn_that_cannot_fit_fails_before_provider_dispatch() {
        let transport = BudgetTransport::new(Vec::new());
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None)
            .with_context_budget(Some(context_budget(100, 20, 30)));
        let history = vec![Message::user("large active turn ".repeat(200))];
        let call = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &history,
            None,
            "sess-unfit",
            0,
            &mut |_| {},
            &mut |_| {},
        );

        let error = call.result.unwrap_err();
        assert_eq!(error.kind(), "context_window");
        assert!(tool_loop.transport.requests.borrow().is_empty());
    }

    #[test]
    fn active_tool_chain_that_cannot_fit_fails_before_continuation_dispatch() {
        let result = "x".repeat(2_501);
        let transport = BudgetTransport::new(vec![Ok(tool_use_reply("tool-1", "read"))]);
        let tool_loop = ToolLoop::new(transport, large_result_registry(result.clone()), None)
            .with_context_budget(Some(context_budget(500, 200, 100)));
        let mut observed_results = Vec::new();
        let call = tool_loop.run_observed_with_system_prompt_and_context_with_attempts(
            &[Message::user("read file")],
            None,
            "sess-unfit-tool-chain",
            0,
            &mut |event| {
                if let ToolEvent::Result { output, .. } = event {
                    observed_results.push(output.to_string());
                }
            },
            &mut |_| {},
        );

        assert_eq!(call.result.unwrap_err().kind(), "context_window");
        assert_eq!(call.attempts, 1);
        assert_eq!(observed_results, [result]);
        let requests = tool_loop.transport.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        }));
    }

    #[test]
    fn loop_executes_registered_handler_and_feeds_back_tool_result() {
        let count = std::rc::Rc::new(Cell::new(0));
        let transport = ScriptedTransport::new(vec![
            tool_use_reply("toolu_1", "echo"),
            AssistantReply::from_blocks(
                vec![ContentBlock::text("done")],
                TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: None,
                },
                Some(StopReason::EndTurn),
            ),
        ]);
        let tool_loop = ToolLoop::new(transport, echo_registry(count.clone()), None);

        let outcome = tool_loop.run(&[Message::user("go")]).unwrap();

        assert_eq!(outcome.reply.text, "done");
        assert!(!outcome.capped);
        assert_eq!(count.get(), 1);
        assert_eq!(
            outcome.reply.usage,
            TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(2),
            },
            "usage sums every call in the turn"
        );
        let result = Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "echo: hi".to_string(),
                is_error: None,
            }],
        );
        let expected_transcript = vec![
            Message::new(Role::Assistant, tool_use_reply("toolu_1", "echo").content),
            result,
        ];
        assert_eq!(outcome.transcript, expected_transcript);

        let calls = tool_loop.transport.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec![Message::user("go")]);
        assert_eq!(calls[1][1..], expected_transcript[..]);
    }

    #[test]
    fn registry_without_pre_tool_hook_preserves_dispatch() {
        let count = std::rc::Rc::new(Cell::new(0));
        let registry = echo_registry(count.clone());

        let result = registry.dispatch("toolu_1", "echo", &serde_json::json!({"text": "hi"}));

        assert_eq!(count.get(), 1);
        assert_eq!(
            result,
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "echo: hi".to_string(),
                is_error: None,
            }
        );
    }

    #[test]
    fn run_observed_reports_each_call_then_its_result_in_order() {
        let count = std::rc::Rc::new(Cell::new(0));
        let mut first = tool_use_reply("toolu_1", "echo");
        first.content.push(ContentBlock::ToolUse {
            id: "toolu_2".to_string(),
            name: "missing".to_string(),
            input: serde_json::json!({}),
        });
        let transport = ScriptedTransport::new(vec![first, AssistantReply::new("done")]);
        let tool_loop = ToolLoop::new(transport, echo_registry(count), None);

        let mut seen = Vec::new();
        tool_loop
            .run_observed(&[Message::user("go")], &mut |event| {
                seen.push(match event {
                    ToolEvent::Round { content } => format!("round {}", content.len()),
                    ToolEvent::Call { id, name, .. } => format!("call {id} {name}"),
                    ToolEvent::Result {
                        id, output, status, ..
                    } => format!("result {id} {status:?} {output}"),
                })
            })
            .unwrap();

        assert_eq!(
            seen,
            [
                "round 3",
                "call toolu_1 echo",
                "result toolu_1 Completed echo: hi",
                "call toolu_2 missing",
                "result toolu_2 Failed unknown tool: missing",
            ]
        );
    }

    #[test]
    fn fallible_stream_observer_stops_dispatch_after_a_result_write_failure() {
        let count = std::rc::Rc::new(Cell::new(0));
        let mut first = tool_use_reply("toolu_1", "echo");
        first.content.push(ContentBlock::ToolUse {
            id: "toolu_2".to_string(),
            name: "echo".to_string(),
            input: serde_json::json!({"text": "again"}),
        });
        let transport = ScriptedTransport::new(vec![first, AssistantReply::new("done")]);
        let tool_loop = ToolLoop::new(transport, echo_registry(count.clone()), None);

        let mut observe = |event| match event {
            OwnedToolEvent::Result { id, .. } if id == "toolu_1" => {
                Err(crate::error::LegError::Io("closed output".to_string()))
            }
            _ => Ok(()),
        };
        let call = tool_loop.run_streaming_observed_fallible_with_attempts(
            &[Message::user("go")],
            &mut observe,
            &mut |_| Ok(()),
        );

        assert!(matches!(call.result, Err(crate::error::LegError::Io(_))));
        assert_eq!(count.get(), 1, "the second call must not be dispatched");
        assert_eq!(tool_loop.transport.calls.borrow().len(), 1);
    }

    #[test]
    fn loop_without_tool_use_makes_a_single_call() {
        let transport = ScriptedTransport::new(vec![AssistantReply::new("hi")]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new(), None);

        let outcome = tool_loop.run(&[Message::user("go")]).unwrap();

        assert_eq!(outcome.reply, AssistantReply::new("hi"));
        assert!(outcome.transcript.is_empty());
        assert_eq!(tool_loop.transport.calls.borrow().len(), 1);
    }

    #[test]
    fn loop_runs_more_than_fifty_rounds_when_uncapped() {
        const ROUNDS: usize = 50;
        let count = std::rc::Rc::new(Cell::new(0));
        let mut replies: Vec<_> = (0..ROUNDS)
            .map(|i| tool_use_reply(&format!("toolu_{i}"), "echo"))
            .collect();
        replies.push(AssistantReply::new("done"));
        let tool_loop = ToolLoop::new(
            ScriptedTransport::new(replies),
            echo_registry(count.clone()),
            None,
        );

        let outcome = tool_loop.run(&[Message::user("go")]).unwrap();

        assert_eq!(outcome.reply.text, "done");
        assert!(!outcome.capped);
        assert_eq!(count.get(), ROUNDS);
        assert_eq!(tool_loop.transport.calls.borrow().len(), ROUNDS + 1);
        assert_eq!(outcome.transcript.len(), 2 * ROUNDS);
    }

    #[test]
    fn loop_stops_after_configured_round_limit_without_another_request() {
        const MAX_ROUNDS: usize = 3;
        let count = std::rc::Rc::new(Cell::new(0));
        let replies = (0..=MAX_ROUNDS)
            .map(|i| tool_use_reply(&format!("toolu_{i}"), "echo"))
            .collect();
        let tool_loop = ToolLoop::new(
            ScriptedTransport::new(replies),
            echo_registry(count.clone()),
            Some(MAX_ROUNDS),
        );

        let outcome = tool_loop.run(&[Message::user("go")]).unwrap();

        assert!(outcome.capped);
        assert_eq!(
            count.get(),
            MAX_ROUNDS,
            "the capped reply's tools are not run"
        );
        assert_eq!(
            tool_loop.transport.calls.borrow().len(),
            MAX_ROUNDS + 1,
            "the initial request plus one per round"
        );
        assert_eq!(outcome.transcript.len(), 2 * MAX_ROUNDS);
        assert_eq!(outcome.reply.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(
            outcome.reply.usage.input_tokens,
            Some((MAX_ROUNDS + 1) as u64)
        );
    }

    #[test]
    fn dispatch_reports_unknown_tool_and_handler_error_as_is_error() {
        struct Failing;
        impl ToolHandler for Failing {
            fn call(&self, _input: &serde_json::Value) -> std::result::Result<String, String> {
                Err("boom".to_string())
            }
        }
        let mut registry = ToolRegistry::new();
        registry.register(
            ToolSpec::new(
                "fail",
                "Always fails.",
                serde_json::json!({"type": "object"}),
            ),
            Box::new(Failing),
        );

        assert_eq!(
            registry.dispatch("t1", "missing", &serde_json::json!({})),
            ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "unknown tool: missing".to_string(),
                is_error: Some(true),
            }
        );
        assert_eq!(
            registry.dispatch("t2", "fail", &serde_json::json!({})),
            ContentBlock::ToolResult {
                tool_use_id: "t2".to_string(),
                content: "boom".to_string(),
                is_error: Some(true),
            }
        );
        assert_eq!(registry.specs()[0].name, "fail");
    }
}
