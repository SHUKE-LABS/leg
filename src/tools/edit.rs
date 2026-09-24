//! The `edit` tool: replace an exact string in a file, returning a diff.
//!
//! Derived from `openai/codex` @ `40eac3ce8a`
//! (`codex-rs/apply-patch/src/seek_sequence.rs` and
//! `codex-rs/apply-patch/src/file_update.rs`), modified for leg: exact-match
//! only (no whitespace-tolerant tiers), a single string replacement instead of
//! a patch, and a diff capped at [`MAX_DIFF_ROWS`] rows.
//!
//! Copyright 2025 OpenAI. Licensed under the Apache License, Version 2.0
//! (<http://www.apache.org/licenses/LICENSE-2.0>); a copy of the license and
//! the upstream NOTICE are in `THIRD_PARTY_LICENSES/openai-codex/`. Distributed
//! on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND.

use std::path::PathBuf;

use similar::TextDiff;

use super::ToolHandler;
use super::read::{os_error, resolve};
use crate::model::ToolSpec;

/// The most rows one `edit` diff may carry, truncation marker included.
pub const MAX_DIFF_ROWS: usize = 32;

const DESCRIPTION: &str = "Edit a file by replacing an exact string. oldString must match the \
file content exactly (no regex, no whitespace normalization) and must be unique unless \
replaceAll is true. Returns a unified diff of the change, capped at 32 rows.";

/// The `edit` tool handler.
#[derive(Default)]
pub struct EditTool {
    base: Option<PathBuf>,
}

impl EditTool {
    /// A handler resolving paths against the process cwd.
    pub fn new() -> Self {
        Self::default()
    }

    /// The `edit` declaration advertised to the model.
    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            "edit",
            DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit (relative or absolute)"
                    },
                    "oldString": {
                        "type": "string",
                        "description": "Exact text to replace"
                    },
                    "newString": {
                        "type": "string",
                        "description": "Text to replace it with"
                    },
                    "replaceAll": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring a unique match (default false)"
                    }
                },
                "required": ["path", "oldString", "newString"]
            }),
        )
    }
}

impl ToolHandler for EditTool {
    fn call(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = input["path"]
            .as_str()
            .ok_or("edit: `path` must be a string")?;
        let old_string = input["oldString"]
            .as_str()
            .ok_or("edit: `oldString` must be a string")?;
        let new_string = input["newString"]
            .as_str()
            .ok_or("edit: `newString` must be a string")?;
        let replace_all = match input.get("replaceAll") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or("edit: `replaceAll` must be a boolean")?,
        };
        if old_string.is_empty() {
            return Err("edit: `oldString` must not be empty".to_string());
        }
        if old_string == new_string {
            return Err("edit: `oldString` and `newString` are identical".to_string());
        }
        let base = match &self.base {
            Some(base) => base.clone(),
            None => std::env::current_dir()
                .map_err(|err| format!("edit: cannot determine working directory: {err}"))?,
        };
        let resolved = resolve(&base, path);
        let old = std::fs::read_to_string(&resolved)
            .map_err(|err| format!("cannot read {}: {}", resolved.display(), os_error(&err)))?;

        let count = if replace_all {
            old.matches(old_string).count()
        } else {
            match_starts(&old, old_string)
        };
        if count == 0 {
            return Err(format!("oldString not found in {path}"));
        }
        if count > 1 && !replace_all {
            return Err(format!(
                "oldString matches {count} times in {path}; add surrounding context to make \
                 it unique, or set replaceAll to true"
            ));
        }
        let new = if replace_all {
            old.replace(old_string, new_string)
        } else {
            old.replacen(old_string, new_string, 1)
        };
        std::fs::write(&resolved, &new)
            .map_err(|err| format!("cannot write {}: {}", resolved.display(), os_error(&err)))?;

        let noun = if count == 1 {
            "occurrence"
        } else {
            "occurrences"
        };
        Ok(format!(
            "Successfully replaced {count} {noun} in {path}.\n{}",
            capped_diff(path, &old, &new)
        ))
    }
}

/// How many positions `needle` occurs at in `haystack`, overlaps included,
/// so `aa` in `aaa` is ambiguous rather than unique.
fn match_starts(haystack: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        count += 1;
        let start = from + at;
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    count
}

/// The unified diff (context radius 1) of `old` to `new`, cut to at most
/// [`MAX_DIFF_ROWS`] rows with a marker naming the rows dropped.
fn capped_diff(path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(1)
        .header(path, path)
        .to_string();
    let rows: Vec<&str> = diff.lines().collect();
    if rows.len() <= MAX_DIFF_ROWS {
        return rows.join("\n");
    }
    let kept = MAX_DIFF_ROWS - 1;
    let mut out = rows[..kept].join("\n");
    out.push_str(&format!(
        "\n... [diff truncated: {} more lines]",
        rows.len() - kept
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantReply, ContentBlock, Message, StopReason, TokenUsage};
    use crate::tools::tests::ScriptedTransport;
    use crate::tools::{ToolLoop, ToolRegistry};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh directory under the system temp dir holding `f.txt` = `contents`.
    fn file_with(name: &str, contents: &str) -> (PathBuf, PathBuf) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leg-edit-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, contents).unwrap();
        (dir, file)
    }

    fn tool_in(dir: &Path) -> EditTool {
        EditTool {
            base: Some(dir.to_path_buf()),
        }
    }

    fn edit(dir: &Path, input: serde_json::Value) -> Result<String, String> {
        let mut input = input;
        input["path"] = "f.txt".into();
        tool_in(dir).call(&input)
    }

    #[test]
    fn spec_names_args() {
        let spec = EditTool::spec();
        assert_eq!(spec.name, "edit");
        for arg in ["path", "oldString", "newString"] {
            assert_eq!(
                spec.input_schema["properties"][arg]["type"], "string",
                "{arg}"
            );
        }
        assert_eq!(
            spec.input_schema["properties"]["replaceAll"]["type"],
            "boolean"
        );
        assert_eq!(
            spec.input_schema["required"],
            serde_json::json!(["path", "oldString", "newString"])
        );
    }

    #[test]
    fn replaces_a_unique_exact_match() {
        let (dir, file) = file_with("exact", "alpha\nbeta\ngamma\n");
        let out = edit(
            &dir,
            serde_json::json!({"oldString": "beta", "newString": "BETA"}),
        )
        .unwrap();
        assert!(
            out.starts_with("Successfully replaced 1 occurrence in f.txt.\n"),
            "{out}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let (dir, file) = file_with("all", "x = 1;\ny = x;\nz = x;\n");
        let out = edit(
            &dir,
            serde_json::json!({"oldString": "x", "newString": "w", "replaceAll": true}),
        )
        .unwrap();
        assert!(
            out.starts_with("Successfully replaced 3 occurrences in f.txt.\n"),
            "{out}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"w = 1;\ny = w;\nz = w;\n");
    }

    #[test]
    fn no_match_is_an_error_and_leaves_the_file() {
        let (dir, file) = file_with("none", "alpha\n");
        let err = edit(
            &dir,
            serde_json::json!({"oldString": "omega", "newString": "x"}),
        )
        .unwrap_err();
        assert_eq!(err, "oldString not found in f.txt");
        assert_eq!(std::fs::read(&file).unwrap(), b"alpha\n");
    }

    #[test]
    fn multiple_matches_without_replace_all_is_an_error() {
        let (dir, file) = file_with("many", "dup\ndup\n");
        for input in [
            serde_json::json!({"oldString": "dup", "newString": "x"}),
            serde_json::json!({"oldString": "dup", "newString": "x", "replaceAll": false}),
        ] {
            let err = edit(&dir, input).unwrap_err();
            assert!(err.contains("matches 2 times"), "{err}");
            assert!(err.contains("replaceAll"), "{err}");
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"dup\ndup\n");
    }

    #[test]
    fn overlapping_matches_are_ambiguous() {
        let (dir, file) = file_with("overlap", "aaa\n");
        let err = edit(
            &dir,
            serde_json::json!({"oldString": "aa", "newString": "b"}),
        )
        .unwrap_err();
        assert!(err.contains("matches 2 times"), "{err}");
        assert_eq!(std::fs::read(&file).unwrap(), b"aaa\n");
    }

    #[test]
    fn only_the_exact_match_counts() {
        // A trailing-space variant is neither a second match nor a fallback.
        let (dir, file) = file_with("priority", "foo \nfoo\n");
        edit(
            &dir,
            serde_json::json!({"oldString": "foo\n", "newString": "bar\n"}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"foo \nbar\n");

        let (dir, file) = file_with("no-fallback", "  indented\n");
        let err = edit(
            &dir,
            serde_json::json!({"oldString": "indented \n", "newString": "x"}),
        )
        .unwrap_err();
        assert_eq!(err, "oldString not found in f.txt");
        assert_eq!(std::fs::read(&file).unwrap(), b"  indented\n");
    }

    #[test]
    fn non_boolean_replace_all_is_rejected() {
        // A unique match, so a flag misread as `false` would edit the file.
        let (dir, file) = file_with("bad-flag", "dup\n");
        for flag in [serde_json::Value::Null, serde_json::json!("true")] {
            let err = edit(
                &dir,
                serde_json::json!({"oldString": "dup", "newString": "x", "replaceAll": flag}),
            )
            .unwrap_err();
            assert_eq!(err, "edit: `replaceAll` must be a boolean");
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"dup\n");
    }

    #[test]
    fn empty_or_identical_strings_are_rejected() {
        let (dir, file) = file_with("degenerate", "abc\n");
        assert!(edit(&dir, serde_json::json!({"oldString": "", "newString": "x"})).is_err());
        assert!(
            edit(
                &dir,
                serde_json::json!({"oldString": "b", "newString": "b"})
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"abc\n");
    }

    #[test]
    fn returns_a_unified_diff_with_one_line_of_context() {
        let (dir, _) = file_with("diff", "1\n2\n3\n4\n5\n");
        let out = edit(
            &dir,
            serde_json::json!({"oldString": "3\n", "newString": "three\n"}),
        )
        .unwrap();
        assert_eq!(
            out,
            "Successfully replaced 1 occurrence in f.txt.\n\
             --- f.txt\n\
             +++ f.txt\n\
             @@ -2,3 +2,3 @@\n\
             \x202\n\
             -3\n\
             +three\n\
             \x204"
        );
    }

    #[test]
    fn long_diff_is_capped_with_a_truncation_marker() {
        let old: String = (0..50).map(|n| format!("line {n}\n")).collect();
        let (dir, file) = file_with("long", &old);
        let out = edit(
            &dir,
            serde_json::json!({"oldString": "line", "newString": "row", "replaceAll": true}),
        )
        .unwrap();
        let diff = out.split_once('\n').unwrap().1;
        let rows: Vec<&str> = diff.lines().collect();
        // Uncapped: 2 header + 1 hunk header + 50 removed + 50 added = 103 rows.
        assert_eq!(rows.len(), MAX_DIFF_ROWS);
        assert_eq!(rows[..3], ["--- f.txt", "+++ f.txt", "@@ -1,50 +1,50 @@"]);
        assert_eq!(rows[31], "... [diff truncated: 72 more lines]");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            old.replace("line", "row")
        );
    }

    #[test]
    fn diff_at_the_cap_is_not_truncated() {
        // 2 header + 1 hunk header + 14 removed + 14 added + 1 trailing
        // context = 32 rows exactly.
        let old: String = (0..14)
            .map(|n| format!("a{n}\n"))
            .chain(["end\n".into()])
            .collect();
        let (dir, _) = file_with("at-cap", &old);
        let out = edit(
            &dir,
            serde_json::json!({"oldString": "a", "newString": "b", "replaceAll": true}),
        )
        .unwrap();
        let diff = out.split_once('\n').unwrap().1;
        assert_eq!(diff.lines().count(), MAX_DIFF_ROWS);
        assert!(!diff.contains("diff truncated"), "{diff}");
    }

    #[test]
    fn loop_turn_edits_a_file() {
        let (_, file) = file_with("loop", "fn main() {\n    println!(\"hi\");\n}\n");
        let path = file.to_str().unwrap().to_string();
        let transport = ScriptedTransport::new(vec![
            AssistantReply::from_blocks(
                vec![ContentBlock::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "edit".to_string(),
                    input: serde_json::json!({
                        "path": path,
                        "oldString": "\"hi\"",
                        "newString": "\"bye\""
                    }),
                }],
                TokenUsage::default(),
                Some(StopReason::ToolUse),
            ),
            AssistantReply::new("done"),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(EditTool::spec(), Box::new(EditTool::new()));
        let tool_loop = ToolLoop::new(transport, registry);

        tool_loop.run(&[Message::user("go")]).unwrap();

        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"fn main() {\n    println!(\"bye\");\n}\n"
        );
        let calls = tool_loop.transport.calls.borrow();
        assert_eq!(
            calls[1].last().unwrap().content,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: format!(
                    "Successfully replaced 1 occurrence in {path}.\n\
                     --- {path}\n\
                     +++ {path}\n\
                     @@ -1,3 +1,3 @@\n\
                     \x20fn main() {{\n\
                     -    println!(\"hi\");\n\
                     +    println!(\"bye\");\n\
                     \x20}}"
                ),
                is_error: None,
            }]
        );
    }
}
