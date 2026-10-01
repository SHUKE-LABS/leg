use std::collections::BTreeMap;

use leg_ui_client::StreamEvent;
use serde_json::Value;

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
    input: String,
    status: String,
    result: Option<String>,
}

#[derive(Clone, Debug)]
enum RoundPart {
    Text(u64),
    Tool(String),
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
    prompt: String,
    rounds: BTreeMap<u64, TranscriptRound>,
    assistant_sanitizers: BTreeMap<(u64, u64), TerminalSanitizer>,
    authoritative_final: Option<(u64, String)>,
}

impl TranscriptTurn {
    pub fn new(prompt: &str) -> Self {
        Self {
            prompt: terminal_safe_text(prompt),
            rounds: BTreeMap::new(),
            assistant_sanitizers: BTreeMap::new(),
            authoritative_final: None,
        }
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
                    let input = block.get("input").map(compact_summary).unwrap_or_default();
                    round.tools.entry(id.to_string()).or_insert(ToolActivity {
                        name,
                        input,
                        status: "running".to_string(),
                        result: None,
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
                    input: compact_summary(input),
                    status: "running".to_string(),
                    result: None,
                }
            });
        activity.name = terminal_safe_text(tool_name);
        activity.input = compact_summary(input);
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
                    input: String::new(),
                    status: terminal_safe_text(status),
                    result: None,
                }
            });
        activity.name = terminal_safe_text(tool_name);
        activity.status = terminal_safe_text(status);
        activity.result = Some(compact_summary(output));
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

    fn finish_round_sanitizers(&mut self, round_index: u64) {
        for ((index, _), sanitizer) in &mut self.assistant_sanitizers {
            if *index == round_index {
                sanitizer.finish();
            }
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("You: {}", self.prompt)];
        for (round_index, round) in &self.rounds {
            let final_text = self
                .authoritative_final
                .as_ref()
                .filter(|(index, _)| index == round_index)
                .map(|(_, text)| text.as_str());
            if let Some(parts) = &round.parts {
                let mut rendered_final = false;
                for part in parts {
                    match part {
                        RoundPart::Text(index) => {
                            if let Some(text) = final_text {
                                if !rendered_final {
                                    push_assistant(&mut lines, text);
                                    rendered_final = true;
                                }
                            } else if let Some(block) = round.text_blocks.get(index) {
                                push_assistant(&mut lines, block.text());
                            }
                        }
                        RoundPart::Tool(id) => {
                            if let Some(tool) = round.tools.get(id) {
                                lines.push(tool.render());
                            }
                        }
                    }
                }
                if let Some(text) = final_text
                    && !rendered_final
                {
                    push_assistant(&mut lines, text);
                }
                for (index, block) in &round.text_blocks {
                    if !parts.iter().any(
                        |part| matches!(part, RoundPart::Text(part_index) if part_index == index),
                    ) && final_text.is_none()
                    {
                        push_assistant(&mut lines, block.text());
                    }
                }
                for id in &round.tool_order {
                    if !parts
                        .iter()
                        .any(|part| matches!(part, RoundPart::Tool(part_id) if part_id == id))
                        && let Some(tool) = round.tools.get(id)
                    {
                        lines.push(tool.render());
                    }
                }
            } else if let Some(text) = final_text {
                push_assistant(&mut lines, text);
                for id in &round.tool_order {
                    if let Some(tool) = round.tools.get(id) {
                        lines.push(tool.render());
                    }
                }
            } else {
                for block in round.text_blocks.values() {
                    push_assistant(&mut lines, block.text());
                }
                for id in &round.tool_order {
                    if let Some(tool) = round.tools.get(id) {
                        lines.push(tool.render());
                    }
                }
            }
        }
        lines
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
        if let Some(result) = &self.result {
            format!("Tool {}: {} — {result}", self.status, self.name)
        } else if self.input.is_empty() {
            format!("Tool running: {}", self.name)
        } else {
            format!("Tool running: {} — {}", self.name, self.input)
        }
    }
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

fn push_assistant(lines: &mut Vec<String>, text: &str) {
    if !text.is_empty() {
        lines.push(format!("Assistant: {text}"));
    }
}

fn compact_summary(value: &Value) -> String {
    let raw = value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string());
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

#[cfg(test)]
mod tests {
    use leg_ui_client::StreamEvent;
    use serde_json::{Value, json};

    use super::TranscriptTurn;

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
        let first_tool = rendered.find("Tool completed: read").unwrap();
        let second_text = rendered.find("Second round text.").unwrap();
        let second_tool = rendered.find("Tool completed: lookup").unwrap();
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
        assert!(rendered.contains("Tool completed: read"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("secret title"));
        assert!(rendered.contains("more characters"));
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
