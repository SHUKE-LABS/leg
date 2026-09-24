//! The agentic tool loop and its dispatch seam.
//!
//! [`ToolRegistry`] maps a tool name to its [`ToolSpec`] and a synchronous
//! [`ToolHandler`]. [`ToolLoop`] wraps a [`Transport`]: while a reply's
//! `stop_reason` is `tool_use`, it executes each `tool_use` block through the
//! registry (serially, in call order) and sends the `tool_result` blocks back,
//! for at most [`MAX_TOOL_ROUNDS`] rounds per user turn. `ToolLoop` is itself a
//! [`Transport`], so every driver (`ask`, `session`, `exchange`) shares it.
//!
//! Registered tools: [`ReadTool`] (`read`), [`WriteTool`] (`write`) and
//! [`EditTool`] (`edit`), and [`BashTool`] (`bash`).

use std::cell::RefCell;

use crate::error::Result;
use crate::model::{AssistantReply, ContentBlock, Message, Role, StopReason, TokenUsage, ToolSpec};
use crate::transport::Transport;

mod bash;
mod edit;
mod read;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::{ReadSet, ReadTool};
pub use write::WriteTool;

/// The most tool-use rounds one user turn may run; the initial provider
/// request does not count.
pub const MAX_TOOL_ROUNDS: usize = 10;

/// The warning surfaced when a turn stops at [`MAX_TOOL_ROUNDS`].
pub const TOOL_ROUND_LIMIT_WARNING: &str =
    "warning: stopped after 10 tool-use rounds; reply still requests tools";

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

    /// The declarations to advertise on each provider request.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|(spec, _)| spec.clone()).collect()
    }

    /// Runs the handler named by a `tool_use` call and returns its
    /// `tool_result`. An unknown tool, or a failing handler, yields an
    /// `is_error` result rather than aborting the turn.
    pub fn dispatch(&self, id: &str, name: &str, input: &serde_json::Value) -> ContentBlock {
        let result = match self.entries.iter().find(|(spec, _)| spec.name == name) {
            Some((_, handler)) => handler.call(input),
            None => Err(format!("unknown tool: {name}")),
        };
        let (content, is_error) = match result {
            Ok(output) => (output, None),
            Err(message) => (message, Some(true)),
        };
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content,
            is_error,
        }
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
    /// Whether the turn stopped at [`MAX_TOOL_ROUNDS`] with the final reply
    /// still requesting tools (which were not executed).
    pub capped: bool,
}

/// An in-memory notification of one dispatched tool call's lifecycle.
///
/// Fired by [`ToolLoop::run_observed`]: one `Round` per dispatched tool round,
/// then per call a `Call` just before it is dispatched and exactly one
/// `Result` once it finishes. The persisted trail form is
/// [`crate::events::ExchangeEvent::ToolRound`] /
/// [`crate::events::ExchangeEvent::ToolCall`] /
/// [`crate::events::ExchangeEvent::ToolResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEvent<'a> {
    /// A `tool_use` reply whose calls are about to be dispatched: its full
    /// content blocks (any text plus the `tool_use` blocks), in reply order.
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
        /// The tool's output, or its error message when `is_error`.
        output: &'a str,
        /// Whether the call failed (unknown tool or handler error).
        is_error: bool,
    },
}

/// A callback receiving each [`ToolEvent`] as it happens.
pub type ToolObserver = Box<dyn FnMut(ToolEvent<'_>)>;

/// A [`Transport`] that iterates on `tool_use` replies, executing tools via a
/// [`ToolRegistry`].
pub struct ToolLoop<T: Transport> {
    transport: T,
    registry: ToolRegistry,
    observer: Option<RefCell<ToolObserver>>,
}

impl<T: Transport> ToolLoop<T> {
    /// Wraps `transport`, executing tool calls through `registry`.
    pub fn new(transport: T, registry: ToolRegistry) -> Self {
        Self {
            transport,
            registry,
            observer: None,
        }
    }

    /// Notifies `observer` of every tool call made when this loop is driven
    /// as a [`Transport`] (the participant-driven `ask` path).
    pub fn with_observer(mut self, observer: ToolObserver) -> Self {
        self.observer = Some(RefCell::new(observer));
        self
    }

    /// Runs one user turn: sends `history` and iterates while the reply
    /// requests tools, for at most [`MAX_TOOL_ROUNDS`] rounds.
    pub fn run(&self, history: &[Message]) -> Result<TurnOutcome> {
        self.run_observed(history, &mut |_| {})
    }

    /// [`ToolLoop::run`], notifying `observe` of each dispatched round, then
    /// each of its calls and their results, in call order. The capped reply's
    /// calls are not dispatched and emit nothing.
    pub fn run_observed(
        &self,
        history: &[Message],
        observe: &mut dyn FnMut(ToolEvent<'_>),
    ) -> Result<TurnOutcome> {
        let mut messages = history.to_vec();
        let mut reply = self.transport.send_conversation(&messages)?;
        let mut usage = reply.usage;
        let mut rounds = 0;

        while reply.stop_reason == Some(StopReason::ToolUse) {
            if rounds == MAX_TOOL_ROUNDS {
                reply.usage = usage;
                return Ok(TurnOutcome {
                    reply,
                    transcript: messages.split_off(history.len()),
                    capped: true,
                });
            }
            observe(ToolEvent::Round {
                content: &reply.content,
            });
            let mut results = Vec::new();
            for block in &reply.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    observe(ToolEvent::Call { id, name, input });
                    let result = self.registry.dispatch(id, name, input);
                    if let ContentBlock::ToolResult {
                        content, is_error, ..
                    } = &result
                    {
                        observe(ToolEvent::Result {
                            id,
                            name,
                            output: content,
                            is_error: is_error.unwrap_or(false),
                        });
                    }
                    results.push(result);
                }
            }
            messages.push(Message::new(Role::Assistant, reply.content));
            messages.push(Message::new(Role::User, results));
            rounds += 1;

            reply = self.transport.send_conversation(&messages)?;
            usage = add_usage(usage, reply.usage);
        }

        reply.usage = usage;
        Ok(TurnOutcome {
            reply,
            transcript: messages.split_off(history.len()),
            capped: false,
        })
    }
}

impl<T: Transport> Transport for ToolLoop<T> {
    /// Runs the tool loop and returns only the final reply, warning on stderr
    /// when the turn hit [`MAX_TOOL_ROUNDS`].
    fn send_conversation(&self, messages: &[Message]) -> Result<AssistantReply> {
        let outcome = match &self.observer {
            Some(observer) => {
                self.run_observed(messages, &mut |event| (observer.borrow_mut())(event))?
            }
            None => self.run(messages)?,
        };
        if outcome.capped {
            eprintln!("{TOOL_ROUND_LIMIT_WARNING}");
        }
        Ok(outcome.reply)
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
        let tool_loop = ToolLoop::new(transport, echo_registry(count.clone()));

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
    fn run_observed_reports_each_call_then_its_result_in_order() {
        let count = std::rc::Rc::new(Cell::new(0));
        let mut first = tool_use_reply("toolu_1", "echo");
        first.content.push(ContentBlock::ToolUse {
            id: "toolu_2".to_string(),
            name: "missing".to_string(),
            input: serde_json::json!({}),
        });
        let transport = ScriptedTransport::new(vec![first, AssistantReply::new("done")]);
        let tool_loop = ToolLoop::new(transport, echo_registry(count));

        let mut seen = Vec::new();
        tool_loop
            .run_observed(&[Message::user("go")], &mut |event| {
                seen.push(match event {
                    ToolEvent::Round { content } => format!("round {}", content.len()),
                    ToolEvent::Call { id, name, .. } => format!("call {id} {name}"),
                    ToolEvent::Result {
                        id,
                        output,
                        is_error,
                        ..
                    } => format!("result {id} {is_error} {output}"),
                })
            })
            .unwrap();

        assert_eq!(
            seen,
            [
                "round 3",
                "call toolu_1 echo",
                "result toolu_1 false echo: hi",
                "call toolu_2 missing",
                "result toolu_2 true unknown tool: missing",
            ]
        );
    }

    #[test]
    fn loop_without_tool_use_makes_a_single_call() {
        let transport = ScriptedTransport::new(vec![AssistantReply::new("hi")]);
        let tool_loop = ToolLoop::new(transport, ToolRegistry::new());

        let outcome = tool_loop.run(&[Message::user("go")]).unwrap();

        assert_eq!(outcome.reply, AssistantReply::new("hi"));
        assert!(outcome.transcript.is_empty());
        assert_eq!(tool_loop.transport.calls.borrow().len(), 1);
    }

    #[test]
    fn loop_stops_after_max_rounds_without_another_request() {
        let count = std::rc::Rc::new(Cell::new(0));
        let replies = (0..=MAX_TOOL_ROUNDS)
            .map(|i| tool_use_reply(&format!("toolu_{i}"), "echo"))
            .collect();
        let tool_loop = ToolLoop::new(
            ScriptedTransport::new(replies),
            echo_registry(count.clone()),
        );

        let outcome = tool_loop.run(&[Message::user("go")]).unwrap();

        assert!(outcome.capped);
        assert_eq!(
            count.get(),
            MAX_TOOL_ROUNDS,
            "the capped reply's tools are not run"
        );
        assert_eq!(
            tool_loop.transport.calls.borrow().len(),
            MAX_TOOL_ROUNDS + 1,
            "the initial request plus one per round"
        );
        assert_eq!(outcome.transcript.len(), 2 * MAX_TOOL_ROUNDS);
        assert_eq!(outcome.reply.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(outcome.reply.usage.input_tokens, Some(11));
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
