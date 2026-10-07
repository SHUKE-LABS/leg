use std::collections::BTreeMap;

use leg_ui_client::StreamEvent;
use leg_ui_client::{TrailOutcome, TrailTurn};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sanitize::{TerminalSanitizer, terminal_safe_text};

const MAX_TOOL_SUMMARY_CHARS: usize = 240;

#[derive(Clone, Debug, Default)]
struct TextBlock {
    streamed: String,
    fallback: String,
}

#[derive(Clone, Debug)]
struct ToolActivity {
    name: String,
    input: Value,
    status: String,
    result: Option<Value>,
    error: Option<String>,
    timestamp_ms: Option<u64>,
    result_timestamp_ms: Option<u64>,
}

#[derive(Clone, Debug)]
enum RoundPart {
    Text(u64),
    Tool(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TranscriptSourceId {
    Prompt,
    Assistant {
        round_index: u64,
        block_index: u64,
    },
    AuthoritativeFinal {
        round_index: u64,
    },
    Tool {
        round_index: u64,
        tool_use_id: String,
    },
    Outcome,
    Failure,
    Warning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptBlockKind {
    Prompt,
    Assistant,
    Tool,
    Outcome,
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct TranscriptBlock {
    pub source_id: TranscriptSourceId,
    pub kind: TranscriptBlockKind,
    /// Sanitized source or tool placeholder text, before visual formatting.
    pub text: String,
}

#[derive(Clone, Debug, Default)]
struct TranscriptRound {
    text_blocks: BTreeMap<u64, TextBlock>,
    parts: Option<Vec<RoundPart>>,
    tools: BTreeMap<String, ToolActivity>,
    tool_order: Vec<String>,
}

/// One submitted prompt and its streamed assistant/tool history.
#[derive(Clone, Debug)]
pub struct TranscriptTurn {
    turn_index: Option<u64>,
    prompt: String,
    timestamp_ms: Option<u64>,
    outcome_timestamp_ms: Option<u64>,
    outcome: Option<String>,
    failure: Option<String>,
    completion_warning: Option<String>,
    rounds: BTreeMap<u64, TranscriptRound>,
    assistant_sanitizers: BTreeMap<(u64, u64), TerminalSanitizer>,
    authoritative_final: Option<(u64, String)>,
}

impl TranscriptTurn {
    pub fn new(prompt: &str) -> Self {
        Self {
            turn_index: None,
            prompt: terminal_safe_text(prompt),
            timestamp_ms: Some(now_ms()),
            outcome_timestamp_ms: None,
            outcome: None,
            failure: None,
            completion_warning: None,
            rounds: BTreeMap::new(),
            assistant_sanitizers: BTreeMap::new(),
            authoritative_final: None,
        }
    }

    pub fn from_trail(turn: &TrailTurn) -> Self {
        let mut transcript = Self::new(&turn.prompt);
        transcript.turn_index = Some(turn.turn_index);
        transcript.timestamp_ms = turn.timestamp_ms;
        transcript.outcome_timestamp_ms = turn.outcome_timestamp_ms;
        transcript.outcome = Some(
            match turn.outcome {
                TrailOutcome::Succeeded => "succeeded",
                TrailOutcome::Failed => "failed",
                TrailOutcome::Interrupted => "interrupted",
                TrailOutcome::Incomplete => "incomplete",
            }
            .to_string(),
        );
        transcript.failure = turn.failure_message.as_deref().map(terminal_safe_text);
        if turn.stop_reason.as_deref() == Some("max_tokens") {
            transcript.completion_warning = Some("Reply truncated at max tokens.".to_string());
        }

        for (round_index, content) in turn.tool_rounds.iter().enumerate() {
            transcript.observe(&StreamEvent::ToolRound {
                seq: round_index as u64,
                round_index: round_index as u64,
                content: content.clone(),
            });
        }
        for tool in &turn.tools {
            let round_index = transcript
                .rounds
                .iter()
                .find_map(|(round_index, round)| {
                    round
                        .tools
                        .contains_key(&tool.tool_use_id)
                        .then_some(*round_index)
                })
                .unwrap_or(0);
            let round = transcript.rounds.entry(round_index).or_default();
            let activity = round
                .tools
                .entry(tool.tool_use_id.clone())
                .or_insert(ToolActivity {
                    name: terminal_safe_text(&tool.tool_name),
                    input: tool.input.clone(),
                    status: "pending".to_string(),
                    result: None,
                    error: None,
                    timestamp_ms: tool.timestamp_ms,
                    result_timestamp_ms: None,
                });
            activity.name = terminal_safe_text(&tool.tool_name);
            activity.input = tool.input.clone();
            activity.timestamp_ms = tool.timestamp_ms;
            if let Some(result) = &tool.result {
                activity.status = terminal_safe_text(&result.status);
                activity.result = result
                    .result
                    .as_ref()
                    .map(|text| Value::String(text.clone()));
                activity.error = result.error.as_deref().map(terminal_safe_text);
                activity.result_timestamp_ms = result.timestamp_ms;
            } else {
                activity.status = match turn.outcome {
                    TrailOutcome::Interrupted => "interrupted",
                    TrailOutcome::Incomplete => "running",
                    _ => "missing_result",
                }
                .to_string();
            }
            ensure_tool_part(round, &tool.tool_use_id);
        }
        if let Some(reply) = turn.reply.as_deref() {
            transcript.reconcile_final(&json!({"kind":"response","body":reply}));
        }
        transcript
    }

    #[cfg(test)]
    pub fn searchable_text(&self) -> String {
        self.searchable_sources()
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Complete sanitized content grouped by the source row that can display
    /// or inspect it. Tool summaries remain compact in the transcript, while
    /// their full arguments and results remain searchable from the tool row.
    pub fn searchable_sources(&self) -> Vec<(TranscriptSourceId, String)> {
        let mut sources = Vec::new();
        for block in self.source_blocks() {
            match &block.source_id {
                TranscriptSourceId::Tool {
                    round_index,
                    tool_use_id,
                } => {
                    if let Some(tool) = self
                        .rounds
                        .get(round_index)
                        .and_then(|round| round.tools.get(tool_use_id))
                    {
                        let mut text = format!(
                            "{}\n{}\n{}",
                            tool.name,
                            terminal_safe_text(&value_text(&tool.input)),
                            terminal_safe_text(&tool.status),
                        );
                        if let Some(result) = &tool.result {
                            text.push('\n');
                            text.push_str(&terminal_safe_text(&value_text(result)));
                        }
                        if let Some(error) = &tool.error {
                            text.push('\n');
                            text.push_str(&terminal_safe_text(error));
                        }
                        sources.push((block.source_id, text));
                    }
                }
                _ if !block.text.is_empty() => sources.push((block.source_id, block.text)),
                _ => {}
            }
        }
        sources
    }

    /// Ordered semantic transcript content. Stable source IDs let the reader
    /// keep a position when rows are rewrapped or provisional text reconciles.
    pub fn source_blocks(&self) -> Vec<TranscriptBlock> {
        let mut blocks = vec![TranscriptBlock {
            source_id: TranscriptSourceId::Prompt,
            kind: TranscriptBlockKind::Prompt,
            text: self.prompt.clone(),
        }];

        for (round_index, round) in &self.rounds {
            let final_text = self
                .authoritative_final
                .as_ref()
                .filter(|(index, _)| index == round_index)
                .map(|(_, text)| text.as_str());
            let final_source = round
                .parts
                .as_ref()
                .and_then(|parts| {
                    parts.iter().find_map(|part| match part {
                        RoundPart::Text(index) => Some(TranscriptSourceId::Assistant {
                            round_index: *round_index,
                            block_index: *index,
                        }),
                        RoundPart::Tool(_) => None,
                    })
                })
                .or_else(|| {
                    round
                        .text_blocks
                        .keys()
                        .next()
                        .map(|index| TranscriptSourceId::Assistant {
                            round_index: *round_index,
                            block_index: *index,
                        })
                })
                .unwrap_or(TranscriptSourceId::AuthoritativeFinal {
                    round_index: *round_index,
                });
            let mut rendered_final = false;

            if let Some(parts) = &round.parts {
                for part in parts {
                    match part {
                        RoundPart::Text(index) => {
                            if let Some(text) = final_text {
                                if !rendered_final && !text.is_empty() {
                                    blocks.push(TranscriptBlock {
                                        source_id: final_source.clone(),
                                        kind: TranscriptBlockKind::Assistant,
                                        text: text.to_string(),
                                    });
                                    rendered_final = true;
                                }
                            } else if let Some(block) = round.text_blocks.get(index)
                                && !block.text().is_empty()
                            {
                                blocks.push(TranscriptBlock {
                                    source_id: TranscriptSourceId::Assistant {
                                        round_index: *round_index,
                                        block_index: *index,
                                    },
                                    kind: TranscriptBlockKind::Assistant,
                                    text: block.text().to_string(),
                                });
                            }
                        }
                        RoundPart::Tool(id) => {
                            if let Some(tool) = round.tools.get(id) {
                                blocks.push(TranscriptBlock {
                                    source_id: TranscriptSourceId::Tool {
                                        round_index: *round_index,
                                        tool_use_id: id.clone(),
                                    },
                                    kind: TranscriptBlockKind::Tool,
                                    text: tool.render(),
                                });
                            }
                        }
                    }
                }
                if let Some(text) = final_text
                    && !rendered_final
                    && !text.is_empty()
                {
                    blocks.push(TranscriptBlock {
                        source_id: final_source.clone(),
                        kind: TranscriptBlockKind::Assistant,
                        text: text.to_string(),
                    });
                }
                for (index, block) in &round.text_blocks {
                    if !parts.iter().any(
                        |part| matches!(part, RoundPart::Text(part_index) if part_index == index),
                    ) && final_text.is_none()
                        && !block.text().is_empty()
                    {
                        blocks.push(TranscriptBlock {
                            source_id: TranscriptSourceId::Assistant {
                                round_index: *round_index,
                                block_index: *index,
                            },
                            kind: TranscriptBlockKind::Assistant,
                            text: block.text().to_string(),
                        });
                    }
                }
                for id in &round.tool_order {
                    if !parts
                        .iter()
                        .any(|part| matches!(part, RoundPart::Tool(part_id) if part_id == id))
                        && let Some(tool) = round.tools.get(id)
                    {
                        blocks.push(TranscriptBlock {
                            source_id: TranscriptSourceId::Tool {
                                round_index: *round_index,
                                tool_use_id: id.clone(),
                            },
                            kind: TranscriptBlockKind::Tool,
                            text: tool.render(),
                        });
                    }
                }
            } else if let Some(text) = final_text {
                if !text.is_empty() {
                    blocks.push(TranscriptBlock {
                        source_id: final_source,
                        kind: TranscriptBlockKind::Assistant,
                        text: text.to_string(),
                    });
                }
                for id in &round.tool_order {
                    if let Some(tool) = round.tools.get(id) {
                        blocks.push(TranscriptBlock {
                            source_id: TranscriptSourceId::Tool {
                                round_index: *round_index,
                                tool_use_id: id.clone(),
                            },
                            kind: TranscriptBlockKind::Tool,
                            text: tool.render(),
                        });
                    }
                }
            } else {
                for (index, block) in &round.text_blocks {
                    if !block.text().is_empty() {
                        blocks.push(TranscriptBlock {
                            source_id: TranscriptSourceId::Assistant {
                                round_index: *round_index,
                                block_index: *index,
                            },
                            kind: TranscriptBlockKind::Assistant,
                            text: block.text().to_string(),
                        });
                    }
                }
                for id in &round.tool_order {
                    if let Some(tool) = round.tools.get(id) {
                        blocks.push(TranscriptBlock {
                            source_id: TranscriptSourceId::Tool {
                                round_index: *round_index,
                                tool_use_id: id.clone(),
                            },
                            kind: TranscriptBlockKind::Tool,
                            text: tool.render(),
                        });
                    }
                }
            }
        }

        if let Some((round_index, text)) = &self.authoritative_final
            && !self.rounds.contains_key(round_index)
            && !text.is_empty()
        {
            blocks.push(TranscriptBlock {
                source_id: TranscriptSourceId::AuthoritativeFinal {
                    round_index: *round_index,
                },
                kind: TranscriptBlockKind::Assistant,
                text: text.clone(),
            });
        }
        if let Some(outcome) = &self.outcome {
            blocks.push(TranscriptBlock {
                source_id: TranscriptSourceId::Outcome,
                kind: TranscriptBlockKind::Outcome,
                text: outcome.clone(),
            });
        }
        if let Some(failure) = &self.failure {
            blocks.push(TranscriptBlock {
                source_id: TranscriptSourceId::Failure,
                kind: TranscriptBlockKind::Error,
                text: failure.clone(),
            });
        }
        if let Some(warning) = &self.completion_warning {
            blocks.push(TranscriptBlock {
                source_id: TranscriptSourceId::Warning,
                kind: TranscriptBlockKind::Warning,
                text: warning.clone(),
            });
        }
        blocks
    }

    pub fn detail_fields(&self) -> Vec<(String, String)> {
        let mut fields = vec![("Prompt".to_string(), terminal_safe_text(&self.prompt))];
        if let Some(timestamp) = self.timestamp_ms {
            fields.push(("Turn started (Unix ms)".to_string(), timestamp.to_string()));
        }
        if let Some(timestamp) = self.outcome_timestamp_ms {
            fields.push(("Turn ended (Unix ms)".to_string(), timestamp.to_string()));
        }
        if let Some(warning) = &self.completion_warning {
            fields.push((
                "Completion warning".to_string(),
                terminal_safe_text(warning),
            ));
        }
        let mut final_rendered = false;
        for (round_index, round) in &self.rounds {
            if let Some((final_index, final_text)) = &self.authoritative_final
                && final_index == round_index
            {
                fields.push((
                    "Assistant reply".to_string(),
                    terminal_safe_text(final_text),
                ));
                final_rendered = true;
            } else {
                for block in round.text_blocks.values() {
                    let text = block.text();
                    if !text.is_empty() {
                        fields.push(("Assistant text".to_string(), terminal_safe_text(text)));
                    }
                }
            }
            for id in &round.tool_order {
                if let Some(tool) = round.tools.get(id) {
                    let name = terminal_safe_text(&tool.name);
                    fields.push((
                        terminal_safe_text(&format!("Tool {name} · id {id} · status")),
                        terminal_safe_text(&tool.display_status()),
                    ));
                    let argument_label = tool.timestamp_ms.map_or_else(
                        || format!("Tool {name} · input"),
                        |timestamp| format!("Tool {name} · input (Unix ms {timestamp})"),
                    );
                    fields.push((argument_label, terminal_safe_text(&value_text(&tool.input))));
                    if let Some(error) = &tool.error {
                        fields.push((format!("Tool {name} · error"), terminal_safe_text(error)));
                    } else if let Some(result) = &tool.result {
                        let result_label = tool.result_timestamp_ms.map_or_else(
                            || format!("Tool {name} · result"),
                            |timestamp| format!("Tool {name} · result (Unix ms {timestamp})"),
                        );
                        let result_text = value_text(result);
                        if tool.name == "bash"
                            && let Some(decoded) = decoded_bash_result(result)
                        {
                            fields.push((
                                format!("Tool {name} · stdout"),
                                terminal_safe_text(&decoded.stdout),
                            ));
                            fields.push((
                                format!("Tool {name} · stderr"),
                                terminal_safe_text(&decoded.stderr),
                            ));
                            fields.push((
                                format!("Tool {name} · exit_code"),
                                decoded.exit_code.to_string(),
                            ));
                            fields.push((
                                format!("Tool {name} · status"),
                                terminal_safe_text(&decoded.status),
                            ));
                            if let Some(bytes) = decoded.stdout_omitted_bytes {
                                fields.push((
                                    format!("Tool {name} · stdout_omitted_bytes"),
                                    bytes.to_string(),
                                ));
                            }
                            if let Some(bytes) = decoded.stderr_omitted_bytes {
                                fields.push((
                                    format!("Tool {name} · stderr_omitted_bytes"),
                                    bytes.to_string(),
                                ));
                            }
                            fields.push((
                                format!("Tool {name} · result (literal)"),
                                terminal_safe_text(&result_text),
                            ));
                        } else {
                            let label =
                                if tool.name == "edit" && contains_unified_diff(&result_text) {
                                    format!("Tool {name} · diff")
                                } else {
                                    result_label
                                };
                            fields.push((label, terminal_safe_text(&result_text)));
                        }
                    } else {
                        fields.push((
                            format!("Tool {name} · outcome"),
                            if tool.status == "interrupted" {
                                "Call was interrupted before a result was recorded.".to_string()
                            } else {
                                "No result record was written for this call.".to_string()
                            },
                        ));
                    }
                }
            }
        }
        if !final_rendered && let Some((_, final_text)) = &self.authoritative_final {
            fields.push((
                "Assistant reply".to_string(),
                terminal_safe_text(final_text),
            ));
        }
        fields
    }

    pub fn tool_detail_field_index(&self, round_index: u64, tool_use_id: &str) -> Option<usize> {
        let tool = self.rounds.get(&round_index)?.tools.get(tool_use_id)?;
        let status_label =
            terminal_safe_text(&format!("Tool {} · id {tool_use_id} · status", tool.name));
        self.detail_fields()
            .iter()
            .position(|(label, _)| label == &status_label)
    }

    pub fn export_value(&self, fallback_index: u64) -> Value {
        let tools = self
            .rounds
            .values()
            .flat_map(|round| {
                round.tool_order.iter().filter_map(|id| {
                    round.tools.get(id).map(|tool| {
                        json!({
                            "id": id,
                            "name": tool.name,
                            "arguments": tool.input,
                            "status": tool.status,
                            "timestamp_ms": tool.timestamp_ms,
                            "result_timestamp_ms": tool.result_timestamp_ms,
                            "result": tool.result,
                            "error": tool.error,
                        })
                    })
                })
            })
            .collect::<Vec<_>>();
        let reply = self
            .authoritative_final
            .as_ref()
            .map(|(_, text)| text.clone());
        json!({
            "turn_index": self.turn_index.unwrap_or(fallback_index),
            "timestamp_ms": self.timestamp_ms,
            "outcome_timestamp_ms": self.outcome_timestamp_ms,
            "prompt": self.prompt,
            "reply": reply,
            "outcome": self.outcome,
            "failure": self.failure,
            "completion_warning": self.completion_warning,
            "tools": tools,
        })
    }

    /// Adds a visible event. `false` means the event did not add renderable
    /// transcript content (for example, an incomplete escape sequence).
    pub fn observe(&mut self, event: &StreamEvent) -> bool {
        match event {
            StreamEvent::TextDelta {
                round_index,
                block_index,
                text,
                ..
            } => {
                let safe = self
                    .assistant_sanitizers
                    .entry((*round_index, *block_index))
                    .or_default()
                    .push(text);
                if safe.is_empty() {
                    return false;
                }
                let round = self.rounds.entry(*round_index).or_default();
                let block = round.text_blocks.entry(*block_index).or_default();
                block.streamed.push_str(&safe);
                true
            }
            StreamEvent::ToolRound {
                round_index,
                content,
                ..
            } => self.observe_tool_round(*round_index, content),
            StreamEvent::ToolCall {
                round_index,
                tool_use_id,
                tool_name,
                input,
                ..
            } => self.observe_tool_call(*round_index, tool_use_id, tool_name, input),
            StreamEvent::ToolResult {
                round_index,
                tool_use_id,
                tool_name,
                status,
                output,
                ..
            } => self.observe_tool_result(*round_index, tool_use_id, tool_name, status, output),
            _ => false,
        }
    }

    fn observe_tool_round(&mut self, round_index: u64, content: &Value) -> bool {
        let Some(blocks) = content.as_array() else {
            return false;
        };
        self.finish_round_sanitizers(round_index);
        let round = self.rounds.entry(round_index).or_default();
        let mut parts = Vec::new();
        let mut tool_order = Vec::new();
        for (block_index, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let index = block_index as u64;
                    let fallback = block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(terminal_safe_text)
                        .unwrap_or_default();
                    round.text_blocks.entry(index).or_default().fallback = fallback;
                    parts.push(RoundPart::Text(index));
                }
                Some("tool_use") => {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .map(terminal_safe_text)
                        .unwrap_or_else(|| "tool".to_string());
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    round.tools.entry(id.to_string()).or_insert(ToolActivity {
                        name,
                        input,
                        status: "pending".to_string(),
                        result: None,
                        error: None,
                        timestamp_ms: Some(now_ms()),
                        result_timestamp_ms: None,
                    });
                    tool_order.push(id.to_string());
                    parts.push(RoundPart::Tool(id.to_string()));
                }
                _ => {}
            }
        }
        let has_visible_parts = !parts.is_empty();
        round.parts = Some(parts);
        round.tool_order = tool_order;
        has_visible_parts
    }

    fn observe_tool_call(
        &mut self,
        round_index: u64,
        tool_use_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> bool {
        let round = self.rounds.entry(round_index).or_default();
        let activity = round
            .tools
            .entry(tool_use_id.to_string())
            .or_insert_with(|| {
                round.tool_order.push(tool_use_id.to_string());
                ToolActivity {
                    name: terminal_safe_text(tool_name),
                    input: input.clone(),
                    status: "running".to_string(),
                    result: None,
                    error: None,
                    timestamp_ms: Some(now_ms()),
                    result_timestamp_ms: None,
                }
            });
        activity.name = terminal_safe_text(tool_name);
        activity.input = input.clone();
        activity.timestamp_ms = Some(now_ms());
        if activity.result.is_none() && activity.error.is_none() {
            activity.status = "running".to_string();
        }
        ensure_tool_part(round, tool_use_id);
        true
    }

    fn observe_tool_result(
        &mut self,
        round_index: u64,
        tool_use_id: &str,
        tool_name: &str,
        status: &str,
        output: &Value,
    ) -> bool {
        let round = self.rounds.entry(round_index).or_default();
        let activity = round
            .tools
            .entry(tool_use_id.to_string())
            .or_insert_with(|| {
                round.tool_order.push(tool_use_id.to_string());
                ToolActivity {
                    name: terminal_safe_text(tool_name),
                    input: Value::Null,
                    status: terminal_safe_text(status),
                    result: None,
                    error: None,
                    timestamp_ms: Some(now_ms()),
                    result_timestamp_ms: None,
                }
            });
        activity.name = terminal_safe_text(tool_name);
        activity.status = terminal_safe_text(status);
        if matches!(status, "failed" | "denied") {
            activity.error = Some(value_text(output));
            activity.result = None;
        } else {
            activity.result = Some(output.clone());
            activity.error = None;
        }
        activity.result_timestamp_ms = Some(now_ms());
        ensure_tool_part(round, tool_use_id);
        true
    }

    /// Replaces provisional text from the final provider round with the
    /// correlated terminal response while preserving earlier tool rounds.
    pub fn reconcile_final(&mut self, response: &Value) {
        self.finish_stream();
        let Some(body) = response
            .get("body")
            .and_then(Value::as_str)
            .filter(|body| !body.is_empty())
            .or_else(|| {
                response
                    .pointer("/exchange/exchange/outcome/reply")
                    .and_then(Value::as_str)
                    .filter(|reply| !reply.is_empty())
            })
        else {
            return;
        };
        let round_index = self.rounds.keys().next_back().copied().unwrap_or(0);
        self.authoritative_final = Some((round_index, terminal_safe_text(body)));
    }

    pub fn finish_stream(&mut self) {
        for sanitizer in self.assistant_sanitizers.values_mut() {
            sanitizer.finish();
        }
    }

    pub fn set_outcome(&mut self, outcome: &str, failure: Option<&str>) {
        self.outcome = Some(terminal_safe_text(outcome));
        self.failure = failure.map(terminal_safe_text);
        self.outcome_timestamp_ms = Some(now_ms());
        for round in self.rounds.values_mut() {
            for tool in round.tools.values_mut() {
                if tool.result.is_none()
                    && tool.error.is_none()
                    && matches!(tool.status.as_str(), "pending" | "running")
                {
                    tool.status = if outcome == "interrupted" {
                        "interrupted".to_string()
                    } else {
                        "missing_result".to_string()
                    };
                }
            }
        }
    }

    pub fn set_completion_warning(&mut self, warning: &str) {
        self.completion_warning = Some(terminal_safe_text(warning));
    }

    pub fn set_turn_index(&mut self, turn_index: u64) {
        self.turn_index = Some(turn_index);
    }

    pub fn turn_index(&self) -> Option<u64> {
        self.turn_index
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self.outcome.as_deref(),
            Some("failed" | "interrupted" | "incomplete")
        )
    }

    fn finish_round_sanitizers(&mut self, round_index: u64) {
        for ((index, _), sanitizer) in &mut self.assistant_sanitizers {
            if *index == round_index {
                sanitizer.finish();
            }
        }
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        self.source_blocks()
            .into_iter()
            .map(|block| match block.kind {
                TranscriptBlockKind::Prompt => {
                    format!("You: {}", compact_display(&block.text, 4_000))
                }
                TranscriptBlockKind::Assistant => {
                    format!("Assistant: {}", compact_display(&block.text, 4_000))
                }
                TranscriptBlockKind::Tool => block.text,
                TranscriptBlockKind::Outcome => format!("Turn {}", block.text),
                TranscriptBlockKind::Error => format!(
                    "Turn detail: {}",
                    compact_summary(&Value::String(block.text))
                ),
                TranscriptBlockKind::Warning => format!("Warning: {}", block.text),
            })
            .collect()
    }
}

impl TextBlock {
    fn text(&self) -> &str {
        if self.streamed.is_empty() {
            &self.fallback
        } else {
            &self.streamed
        }
    }
}

impl ToolActivity {
    fn render(&self) -> String {
        let state = self.display_status();
        let summary = match self.name.as_str() {
            "read" => self
                .input_string("path")
                .map(|path| {
                    let offset = self.input.get("offset").and_then(Value::as_u64);
                    let limit = self.input.get("limit").and_then(Value::as_u64);
                    match (offset, limit) {
                        (Some(offset), Some(limit)) => {
                            format!("{path} · offset {offset} · limit {limit}")
                        }
                        (Some(offset), None) => format!("{path} · offset {offset}"),
                        (None, Some(limit)) => format!("{path} · limit {limit}"),
                        (None, None) => path.to_string(),
                    }
                })
                .unwrap_or_else(|| self.generic_input_summary()),
            "write" | "edit" => self
                .input_string("path")
                .map(str::to_string)
                .unwrap_or_else(|| self.generic_input_summary()),
            "bash" => self
                .input_string("description")
                .filter(|description| !description.trim().is_empty())
                .or_else(|| self.input_string("command"))
                .map(str::to_string)
                .unwrap_or_else(|| self.generic_input_summary()),
            _ => self
                .first_string_input()
                .map(str::to_string)
                .unwrap_or_else(|| "tool call".to_string()),
        };
        format!(
            "{} · {} · {}",
            one_line(&terminal_safe_text(&self.name)),
            one_line(&state),
            one_line(&terminal_safe_text(&summary)),
        )
    }

    fn display_status(&self) -> String {
        if self.status == "completed"
            && self.name == "bash"
            && let Some(result) = self.result.as_ref().and_then(decoded_bash_result)
        {
            if result.status == "timed_out" {
                return format!("timed out (exit {})", result.exit_code);
            }
            if result.exit_code != 0 {
                return format!("exit {}", result.exit_code);
            }
        }
        match self.status.as_str() {
            "missing_result" => "missing result".to_string(),
            status => status.replace('_', " "),
        }
    }

    fn input_string(&self, key: &str) -> Option<&str> {
        self.input.get(key).and_then(Value::as_str)
    }

    fn first_string_input(&self) -> Option<&str> {
        self.input
            .as_str()
            .or_else(|| self.input.as_object()?.values().find_map(Value::as_str))
    }

    fn generic_input_summary(&self) -> String {
        if self.input.is_null() {
            "tool call".to_string()
        } else {
            compact_summary(&self.input)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BashResultEnvelope {
    status: String,
    exit_code: i64,
    stdout: String,
    stderr: String,
    stdout_omitted_bytes: Option<u64>,
    stderr_omitted_bytes: Option<u64>,
}

fn decoded_bash_result(value: &Value) -> Option<BashResultEnvelope> {
    let parsed = match value {
        Value::Object(_) => None,
        Value::String(text) => Some(serde_json::from_str::<Value>(text).ok()?),
        _ => return None,
    };
    let envelope = parsed.as_ref().unwrap_or(value);
    let object = envelope.as_object()?;
    let status = object.get("status")?.as_str()?;
    if !matches!(status, "exited" | "timed_out") {
        return None;
    }
    let omitted_bytes = |key: &str| -> Option<Option<u64>> {
        match object.get(key) {
            None => Some(None),
            Some(value) => value.as_u64().map(Some),
        }
    };
    let stdout_omitted_bytes = omitted_bytes("stdout_omitted_bytes")?;
    let stderr_omitted_bytes = omitted_bytes("stderr_omitted_bytes")?;
    Some(BashResultEnvelope {
        status: status.to_string(),
        exit_code: object.get("exit_code")?.as_i64()?,
        stdout: object.get("stdout")?.as_str()?.to_string(),
        stderr: object.get("stderr")?.as_str()?.to_string(),
        stdout_omitted_bytes,
        stderr_omitted_bytes,
    })
}

fn contains_unified_diff(text: &str) -> bool {
    let mut found_old_header = false;
    let mut found_new_header = false;
    for line in text.lines() {
        if line.starts_with("--- ") {
            found_old_header = true;
        } else if found_old_header && line.starts_with("+++ ") {
            found_new_header = true;
        } else if found_old_header && found_new_header && line.starts_with("@@") {
            return true;
        }
    }
    false
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ensure_tool_part(round: &mut TranscriptRound, id: &str) {
    let Some(parts) = round.parts.as_mut() else {
        return;
    };
    if !parts
        .iter()
        .any(|part| matches!(part, RoundPart::Tool(existing) if existing == id))
    {
        parts.push(RoundPart::Tool(id.to_string()));
    }
}

#[cfg(test)]
fn compact_display(text: &str, limit: usize) -> String {
    let mut characters = text.chars();
    let prefix = characters.by_ref().take(limit).collect::<String>();
    let omitted = characters.count();
    if omitted == 0 {
        prefix
    } else {
        format!(
            "{prefix}\n[… {omitted} characters hidden here; inspect the turn for the full text.]"
        )
    }
}

fn compact_summary(value: &Value) -> String {
    let raw = value_text(value);
    let safe = terminal_safe_text(&raw);
    let mut characters = safe.chars();
    let prefix = characters
        .by_ref()
        .take(MAX_TOOL_SUMMARY_CHARS)
        .collect::<String>();
    let omitted = characters.count();
    if omitted == 0 {
        prefix
    } else {
        format!("{prefix}… [{omitted} more characters]")
    }
}

fn value_text(value: &Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_else(|| {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use leg_ui_client::{StreamEvent, TrailTurn};
    use serde_json::{Value, json};

    use super::{TranscriptSourceId, TranscriptTurn};

    #[test]
    fn reconciles_multiple_tool_rounds_without_repeating_final_text() {
        let mut transcript = TranscriptTurn::new("run tools");
        transcript.observe(&text(0, 0, "First round text."));
        transcript.observe(&tool_round(
            0,
            json!([
                {"type":"text","text":"First round text."},
                {"type":"tool_use","id":"tool-1","name":"read","input":{"path":"one"}}
            ]),
        ));
        transcript.observe(&tool_call(0, "tool-1", "read", json!({"path":"one"})));
        transcript.observe(&tool_result(0, "tool-1", "read", json!("first result")));

        transcript.observe(&text(1, 0, "Second round text."));
        transcript.observe(&tool_round(
            1,
            json!([
                {"type":"text","text":"Second round text."},
                {"type":"tool_use","id":"tool-2","name":"lookup","input":{"q":"two"}}
            ]),
        ));
        transcript.observe(&tool_call(1, "tool-2", "lookup", json!({"q":"two"})));
        transcript.observe(&tool_result(1, "tool-2", "lookup", json!("second result")));

        transcript.observe(&text(2, 0, "provisional final"));
        transcript.reconcile_final(&json!({"kind":"response","body":"Authoritative final."}));
        let rendered = transcript.lines().join("\n");

        assert_eq!(rendered.matches("First round text.").count(), 1);
        assert_eq!(rendered.matches("Second round text.").count(), 1);
        assert_eq!(rendered.matches("Authoritative final.").count(), 1);
        assert!(!rendered.contains("provisional final"));
        let first_tool = rendered.find("read · completed · one").unwrap();
        let second_text = rendered.find("Second round text.").unwrap();
        let second_tool = rendered.find("lookup · completed · two").unwrap();
        let final_text = rendered.find("Authoritative final.").unwrap();
        assert!(rendered.find("First round text.").unwrap() < first_tool);
        assert!(first_tool < second_text && second_text < second_tool && second_tool < final_text);
    }

    #[test]
    fn fallback_tool_round_text_and_large_output_are_safe_and_compact() {
        let mut transcript = TranscriptTurn::new("inspect");
        transcript.observe(&tool_round(
            0,
            json!([
                {"type":"text","text":"before tool"},
                {"type":"tool_use","id":"tool","name":"\x1b]2;name\x07read","input":{"path":"x"}}
            ]),
        ));
        transcript.observe(&tool_call(0, "tool", "read", json!({"path":"x"})));
        transcript.observe(&tool_result(
            0,
            "tool",
            "read",
            Value::String("x".repeat(2_000)),
        ));
        let rendered = transcript.lines().join("\n");

        assert!(rendered.contains("Assistant: before tool"));
        assert!(rendered.contains("read · completed · x"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("secret title"));
        assert!(!rendered.contains(&"x".repeat(1_000)));
        assert!(rendered.len() < 600);
    }

    #[test]
    fn unfinished_control_sequence_in_one_block_does_not_hide_another_block() {
        let mut transcript = TranscriptTurn::new("show both blocks");
        transcript.observe(&text(0, 0, "first block\u{1b}[31;"));
        transcript.observe(&text(0, 1, "second block remains readable"));
        transcript.finish_stream();
        let rendered = transcript.lines().join("\n");

        assert!(rendered.contains("Assistant: first block"));
        assert!(rendered.contains("Assistant: second block remains readable"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn trail_projection_pairs_tools_preserves_timestamps_and_sanitizes_details() {
        let trail: TrailTurn = serde_json::from_value(json!({
            "turn_index": 12,
            "prompt": "inspect history",
            "timestamp_ms": 100,
            "outcome": "succeeded",
            "outcome_timestamp_ms": 200,
            "tool_rounds": [[
                {"type":"tool_use","id":"done","name":"read","input":{"path":"safe"}},
                {"type":"tool_use","id":"denied","name":"bash","input":{"command":"no"}},
                {"type":"tool_use","id":"pending","name":"write","input":{"path":"wait"}}
            ]],
            "tools": [
                {"tool_use_id":"done","tool_name":"read","input":{"path":"safe"},"timestamp_ms":110,
                    "result":{"status":"completed","timestamp_ms":120,"result":"literal\u{001b}]0;title\u{0007} output"}},
                {"tool_use_id":"denied","tool_name":"bash","input":{"command":"no"},"timestamp_ms":130,
                    "result":{"status":"denied","timestamp_ms":140,"error":"denied"}},
                {"tool_use_id":"pending","tool_name":"write","input":{"path":"wait"},"timestamp_ms":150,"result":null}
            ]
        }))
        .expect("trail turn deserializes");
        let transcript = TranscriptTurn::from_trail(&trail);
        let fields = transcript.detail_fields();
        let rendered = fields
            .iter()
            .map(|(label, value)| format!("{label}: {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Turn started (Unix ms): 100"));
        assert!(rendered.contains("Unix ms 120"));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("denied"));
        assert!(rendered.contains("missing result"));
        assert!(rendered.contains("No result record was written"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(transcript.searchable_text().contains("literal output"));
        assert!(!transcript.retryable());
    }

    #[test]
    fn incomplete_tool_call_is_shown_as_interrupted_and_export_omits_catalog_data() {
        let trail: TrailTurn = serde_json::from_value(json!({
            "turn_index": 3,
            "prompt": "TRIAL-INTERRUPTED",
            "timestamp_ms": 1000,
            "outcome": "interrupted",
            "outcome_timestamp_ms": 1010,
            "tool_rounds": [[{"type":"tool_use","id":"call","name":"bash","input":{"command":"sleep"}}]],
            "tools": [{"tool_use_id":"call","tool_name":"bash","input":{"command":"sleep"},"timestamp_ms":1005,"result":null}],
            "private_catalog_field": "must not be exported",
            "authorization_key": "inherited secret"
        }))
        .expect("trail turn deserializes");
        let transcript = TranscriptTurn::from_trail(&trail);
        assert!(transcript.retryable());
        assert!(
            transcript
                .detail_fields()
                .iter()
                .any(|(label, value)| { label.contains("status") && value == "interrupted" })
        );
        let export = transcript.export_value(99);
        assert_eq!(export["turn_index"], 3);
        assert!(export.get("cwd").is_none());
        assert!(export.get("display").is_none());
        assert!(export.get("private_catalog_field").is_none());
        assert!(export.get("authorization_key").is_none());
    }

    #[test]
    fn long_tool_result_stays_complete_in_inspector_and_compact_in_transcript() {
        let output = "Ω".repeat(6_000);
        let trail: TrailTurn = serde_json::from_value(json!({
            "turn_index": 0,
            "prompt": "large output",
            "outcome": "succeeded",
            "tool_rounds": [[{"type":"tool_use","id":"call","name":"bash","input":{"command":"large"}}]],
            "tools": [{"tool_use_id":"call","tool_name":"bash","input":{"command":"large"},
                "result":{"status":"completed","result":output}}]
        }))
        .expect("trail turn deserializes");
        let transcript = TranscriptTurn::from_trail(&trail);
        let detail = transcript
            .detail_fields()
            .into_iter()
            .find(|(label, _)| label.contains("result"))
            .map(|(_, value)| value)
            .expect("result field");
        assert_eq!(detail, output);
        assert!(
            transcript
                .lines()
                .join("\n")
                .contains("bash · completed · large")
        );
        assert!(transcript.searchable_text().contains(&output));
    }

    #[test]
    fn tool_rows_prioritize_readable_inputs_and_hide_call_ids() {
        let mut transcript = TranscriptTurn::new("run calls");
        transcript.observe(&tool_round(
            0,
            json!([
                {"type":"tool_use","id":"same-a","name":"read","input":{"path":"src/main.rs","offset":12,"limit":8}},
                {"type":"tool_use","id":"same-b","name":"write","input":{"path":"out.txt","content":"written"}},
                {"type":"tool_use","id":"same-c","name":"edit","input":{"path":"edit.txt","oldString":"old","newString":"new"}},
                {"type":"tool_use","id":"same-d","name":"bash","input":{"description":"  inspect files  ","command":"find ."}},
                {"type":"tool_use","id":"same-e","name":"lookup","input":{"query":"find this"}}
            ]),
        ));
        let rows = transcript
            .source_blocks()
            .into_iter()
            .filter(|block| matches!(&block.source_id, TranscriptSourceId::Tool { .. }))
            .map(|block| block.text)
            .collect::<Vec<_>>();
        assert!(rows[0].contains("read · pending · src/main.rs · offset 12 · limit 8"));
        assert!(rows[1].contains("write · pending · out.txt"));
        assert!(rows[2].contains("edit · pending · edit.txt"));
        assert!(rows[3].contains("bash · pending · inspect files"));
        assert!(rows[4].contains("lookup · pending · find this"));
        assert!(!rows.iter().any(|row| row.contains("same-")));
    }

    #[test]
    fn bash_envelopes_decode_outputs_and_keep_the_literal_result() {
        let envelope = json!({
            "wall_time_seconds": 0.25,
            "status": "exited",
            "exit_code": 7,
            "stdout": "out\n... [3 bytes omitted]",
            "stderr": "err",
            "stdout_omitted_bytes": 3,
            "stderr_omitted_bytes": 0
        });
        let serialized = envelope.to_string();
        let decoded = super::decoded_bash_result(&Value::String(serialized.clone()))
            .expect("core returns a JSON string envelope");
        assert_eq!(decoded.status, "exited");
        assert_eq!(decoded.exit_code, 7);
        assert_eq!(decoded.stdout_omitted_bytes, Some(3));

        let mut transcript = TranscriptTurn::new("run bash");
        transcript.observe(&tool_call(
            0,
            "bash-id",
            "bash",
            json!({"command":"false","description":"check exit status"}),
        ));
        transcript.observe(&tool_result(
            0,
            "bash-id",
            "bash",
            Value::String(serialized.clone()),
        ));
        let block = transcript
            .source_blocks()
            .into_iter()
            .find(|block| matches!(&block.source_id, TranscriptSourceId::Tool { .. }))
            .expect("tool row");
        assert!(block.text.contains("bash · exit 7 · check exit status"));
        let fields = transcript.detail_fields();
        assert!(fields.iter().any(|(label, value)| {
            label.ends_with("stdout") && value == "out\n... [3 bytes omitted]"
        }));
        assert!(
            fields
                .iter()
                .any(|(label, value)| { label.ends_with("stderr") && value == "err" })
        );
        assert!(
            fields
                .iter()
                .any(|(label, value)| { label.ends_with("stdout_omitted_bytes") && value == "3" })
        );
        assert!(
            fields.iter().any(|(label, value)| {
                label.ends_with("result (literal)") && value == &serialized
            })
        );
        assert_eq!(transcript.tool_detail_field_index(0, "bash-id"), Some(2));

        let object_envelope = super::decoded_bash_result(&envelope)
            .expect("JSON-object fixture envelope also decodes");
        assert_eq!(object_envelope.exit_code, 7);
    }

    #[test]
    fn bash_timeouts_and_unknown_envelopes_are_not_rendered_as_success() {
        let timeout = json!({
            "wall_time_seconds": 1.0,
            "status": "timed_out",
            "exit_code": 124,
            "stdout": "partial",
            "stderr": "",
            "stdout_omitted_bytes": 0,
            "stderr_omitted_bytes": 0,
        });
        let mut timed_out = TranscriptTurn::new("timeout");
        timed_out.observe(&tool_call(0, "timeout", "bash", json!({"command":"sleep"})));
        timed_out.observe(&tool_result(
            0,
            "timeout",
            "bash",
            Value::String(timeout.to_string()),
        ));
        assert!(
            timed_out
                .lines()
                .join("\n")
                .contains("bash · timed out (exit 124) · sleep")
        );

        let signal = json!({
            "wall_time_seconds": 0.01,
            "status": "exited",
            "exit_code": 143,
            "stdout": "",
            "stderr": "",
            "stdout_omitted_bytes": 0,
            "stderr_omitted_bytes": 0,
        });
        let mut signaled = TranscriptTurn::new("signal");
        signaled.observe(&tool_call(
            0,
            "signal",
            "bash",
            json!({"command":"kill -TERM $$"}),
        ));
        signaled.observe(&tool_result(
            0,
            "signal",
            "bash",
            Value::String(signal.to_string()),
        ));
        assert!(
            signaled
                .lines()
                .join("\n")
                .contains("bash · exit 143 · kill -TERM $$")
        );

        let unknown = Value::String(
            r#"{"status":"future","stdout":"keep me","stderr":"","exit_code":0}"#.to_string(),
        );
        assert!(super::decoded_bash_result(&unknown).is_none());
        let mut fallback = TranscriptTurn::new("unknown result");
        fallback.observe(&tool_call(
            0,
            "future",
            "bash",
            json!({"command":"inspect"}),
        ));
        fallback.observe(&tool_result(0, "future", "bash", unknown.clone()));
        let fields = fallback.detail_fields();
        assert!(!fields.iter().any(|(label, _)| label.ends_with("stdout")));
        assert!(fields.iter().any(|(label, value)| {
            label.starts_with("Tool bash · result") && value == unknown.as_str().unwrap()
        }));
    }

    #[test]
    fn edit_diff_stays_literal_and_missing_results_are_distinct() {
        let supplied_diff = "Successfully replaced 1 occurrence.\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n... [diff truncated: 4 more lines]";
        let mut transcript = TranscriptTurn::new("edit file");
        transcript.observe(&tool_call(0, "edit", "edit", json!({"path":"file"})));
        transcript.observe(&tool_result(
            0,
            "edit",
            "edit",
            Value::String(supplied_diff.to_string()),
        ));
        assert!(transcript.detail_fields().iter().any(|(label, value)| {
            label.starts_with("Tool edit · diff") && value == supplied_diff
        }));

        let mut no_patch = TranscriptTurn::new("edit without patch");
        no_patch.observe(&tool_call(0, "no-patch", "edit", json!({"path":"file"})));
        no_patch.observe(&tool_result(
            0,
            "no-patch",
            "edit",
            Value::String("edit result without a supplied patch".to_string()),
        ));
        assert!(no_patch.detail_fields().iter().any(|(label, value)| {
            label.starts_with("Tool edit · result")
                && value == "edit result without a supplied patch"
        }));

        let mut missing = TranscriptTurn::new("incomplete");
        missing.observe(&tool_call(0, "lost", "write", json!({"path":"file"})));
        missing.set_outcome("succeeded", None);
        let row = missing
            .source_blocks()
            .into_iter()
            .find(|block| matches!(&block.source_id, TranscriptSourceId::Tool { .. }))
            .expect("tool row");
        assert!(row.text.contains("write · missing result · file"));
        assert!(missing.detail_fields().iter().any(|(label, value)| {
            label.ends_with("outcome") && value.contains("No result record")
        }));

        let mut denied = TranscriptTurn::new("denied");
        denied.observe(&tool_call(0, "denied", "bash", json!({"command":"no"})));
        denied.observe(&StreamEvent::ToolResult {
            seq: 1,
            round_index: 0,
            tool_use_id: "denied".to_string(),
            tool_name: "bash".to_string(),
            status: "denied".to_string(),
            output: Value::String("hook denied".to_string()),
        });
        assert!(denied.lines().join("\n").contains("bash · denied · no"));
        assert!(
            denied
                .detail_fields()
                .iter()
                .any(|(label, value)| { label.ends_with("error") && value == "hook denied" })
        );
    }

    #[test]
    fn final_reconciliation_keeps_a_stable_source_id_and_searches_complete_text() {
        let mut transcript = TranscriptTurn::new("prompt");
        transcript.observe(&text(0, 0, "provisional text"));
        transcript.observe(&tool_round(
            0,
            json!([
                {"type":"text","text":"provisional text"},
                {"type":"tool_use","id":"call-identity","name":"read","input":{"path":"x"}}
            ]),
        ));
        transcript.reconcile_final(&json!({
            "kind":"response",
            "body":"authoritative text\nsecond line"
        }));

        let blocks = transcript.source_blocks();
        let final_block = blocks
            .iter()
            .find(|block| block.text.starts_with("authoritative text"))
            .expect("authoritative reply remains visible");
        assert_eq!(
            final_block.source_id,
            TranscriptSourceId::Assistant {
                round_index: 0,
                block_index: 0,
            }
        );
        assert!(blocks.iter().any(|block| {
            block.source_id
                == TranscriptSourceId::Tool {
                    round_index: 0,
                    tool_use_id: "call-identity".to_string(),
                }
        }));
        let searchable = transcript.searchable_text();
        assert!(searchable.contains("authoritative text\nsecond line"));
        assert!(!searchable.contains("provisional text"));
    }

    #[test]
    fn unknown_supported_schema_event_does_not_erase_known_transcript_content() {
        let mut transcript = TranscriptTurn::new("prompt");
        transcript.observe(&text(0, 0, "known assistant content"));
        assert!(!transcript.observe(&StreamEvent::Unknown {
            seq: 1,
            event: "future_event".to_string(),
            record: json!({"new_field":"future value"}),
        }));
        assert!(
            transcript
                .source_blocks()
                .iter()
                .any(|block| block.text == "known assistant content")
        );
    }

    fn text(round_index: u64, block_index: u64, text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            seq: 0,
            round_index,
            block_index,
            text: text.to_string(),
        }
    }

    fn tool_round(round_index: u64, content: Value) -> StreamEvent {
        StreamEvent::ToolRound {
            seq: 0,
            round_index,
            content,
        }
    }

    fn tool_call(round_index: u64, id: &str, name: &str, input: Value) -> StreamEvent {
        StreamEvent::ToolCall {
            seq: 0,
            round_index,
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            input,
        }
    }

    fn tool_result(round_index: u64, id: &str, name: &str, output: Value) -> StreamEvent {
        StreamEvent::ToolResult {
            seq: 0,
            round_index,
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            status: "completed".to_string(),
            output,
        }
    }
}
