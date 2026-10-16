//! Context-window projection helpers shared by the tool loop and resume path.

use crate::model::{ContentBlock, Message, Role};

/// Marker appended after a projected tool result exceeds its source cap.
pub const TOOL_RESULT_TRUNCATION_MARKER: &str = "\n[tool result truncated by leg]";

/// Maximum source characters of a tool result sent to a provider or summarizer.
pub const TOOL_RESULT_SOURCE_LIMIT: usize = 2_000;

/// Conservative baseline from serialized request bytes to tokens.
pub(crate) fn baseline_tokens(serialized_bytes: usize) -> u64 {
    (serialized_bytes as u64).saturating_add(2) / 3
}

/// Applies a monotonic calibration factor to a baseline request estimate.
pub(crate) fn calibrated_tokens(baseline: u64, factor: f64) -> u64 {
    ((baseline as f64) * factor.max(1.0)).ceil() as u64
}

/// Clones the message projection, capping tool-result source text while
/// leaving the full messages available for append-only trail persistence.
pub(crate) fn project_tool_results(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|message| Message::new(message.role, project_blocks(&message.content)))
        .collect()
}

fn project_blocks(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: truncate_tool_result(content),
                is_error: *is_error,
            },
            other => other.clone(),
        })
        .collect()
}

/// Truncates by Unicode scalar values, then appends leg's exact marker.
pub(crate) fn truncate_tool_result(content: &str) -> String {
    let mut chars = content.chars();
    let prefix = chars
        .by_ref()
        .take(TOOL_RESULT_SOURCE_LIMIT)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}{TOOL_RESULT_TRUNCATION_MARKER}")
    } else {
        content.to_string()
    }
}

/// True for the user-side tool-result half of a provider tool exchange.
pub(crate) fn is_tool_result_message(message: &Message) -> bool {
    message.role == Role::User
        && !message.content.is_empty()
        && message
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Starts of complete historical user-turn groups before `active_start`.
pub(crate) fn historical_group_starts(messages: &[Message], active_start: usize) -> Vec<usize> {
    messages[..active_start.min(messages.len())]
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (message.role == Role::User && !is_tool_result_message(message)).then_some(index)
        })
        .collect()
}

/// Finds the first message after `groups_to_drop` complete user-turn groups.
pub(crate) fn cut_after_groups(
    messages: &[Message],
    active_start: usize,
    groups_to_drop: usize,
) -> Option<usize> {
    let starts = historical_group_starts(messages, active_start);
    if groups_to_drop == 0 {
        Some(0)
    } else if groups_to_drop < starts.len() {
        Some(starts[groups_to_drop])
    } else if groups_to_drop == starts.len() {
        Some(active_start.min(messages.len()))
    } else {
        None
    }
}

/// Number of complete user turns in the suffix preceding the active turn.
pub(crate) fn retained_group_count(messages: &[Message], active_start: usize, cut: usize) -> usize {
    historical_group_starts(messages, active_start)
        .into_iter()
        .filter(|start| *start >= cut)
        .count()
}

/// Adds a prior compaction summary as a system-context prefix.
pub(crate) fn system_prompt_with_summary(
    system_prompt: Option<&str>,
    summary: Option<&str>,
) -> Option<String> {
    match summary {
        Some(summary) if !summary.is_empty() => Some(match system_prompt {
            Some(system_prompt) if !system_prompt.is_empty() => {
                format!("Earlier conversation summary:\n{summary}\n\n{system_prompt}")
            }
            _ => format!("Earlier conversation summary:\n{summary}"),
        }),
        _ => system_prompt.map(str::to_string),
    }
}

/// Renders dropped history for a bounded internal summary request. Provider
/// reasoning and signatures are excluded; tool results use the same 2,000
/// character projection sent on ordinary provider requests.
pub(crate) fn summary_source(prior_summary: Option<&str>, messages: &[Message]) -> String {
    let mut output = String::new();
    if let Some(summary) = prior_summary.filter(|value| !value.is_empty()) {
        output.push_str("Previous summary:\n");
        output.push_str(summary);
        output.push_str("\n\nNewly dropped conversation:\n");
    }
    for message in messages {
        output.push_str(match message.role {
            Role::User => "User: ",
            Role::Assistant => "Assistant: ",
        });
        let mut first = true;
        for block in &message.content {
            let text = match block {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::ToolUse { name, input, .. } => {
                    Some(format!("[tool call {name}: {input}]"))
                }
                ContentBlock::ToolResult { content, .. } => {
                    Some(format!("[tool result: {}]", truncate_tool_result(content)))
                }
                ContentBlock::Image { .. } => Some("[image content omitted]".to_string()),
                ContentBlock::Thinking { .. } => None,
            };
            if let Some(text) = text {
                if !first {
                    output.push(' ');
                }
                output.push_str(&text);
                first = false;
            }
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_projection_caps_source_and_adds_exact_marker() {
        let source = format!("{}x", "a".repeat(TOOL_RESULT_SOURCE_LIMIT));
        let projected = truncate_tool_result(&source);
        assert_eq!(
            projected,
            format!("{}{TOOL_RESULT_TRUNCATION_MARKER}", "a".repeat(2_000))
        );
        assert_eq!(truncate_tool_result("short"), "short");
    }

    #[test]
    fn group_boundaries_keep_tool_result_chains_together() {
        let messages = vec![
            Message::user("old"),
            Message::assistant("reply"),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                }],
            ),
            Message::new(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "t".into(),
                    content: "ok".into(),
                    is_error: None,
                }],
            ),
            Message::assistant("done"),
            Message::user("active"),
        ];
        assert_eq!(historical_group_starts(&messages, 5), [0]);
        assert_eq!(cut_after_groups(&messages, 5, 1), Some(5));
    }

    #[test]
    fn summary_source_omits_thinking_and_caps_tool_results() {
        let messages = vec![Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Thinking {
                    thinking: "secret reasoning".into(),
                    signature: "signature".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t".into(),
                    content: "x".repeat(2_001),
                    is_error: None,
                },
            ],
        )];
        let source = summary_source(None, &messages);
        assert!(!source.contains("secret reasoning"));
        assert!(!source.contains("signature"));
        assert!(source.contains(TOOL_RESULT_TRUNCATION_MARKER));
    }
}
