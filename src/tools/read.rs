//! The `read` tool: a capped, windowed view of one UTF-8 text file.
//!
//! Derived from `earendil-works/pi` @ `7fd564cbb78`
//! (`packages/coding-agent/src/core/tools/read.ts` and `truncate.ts`),
//! modified for leg: text only, plain cwd path resolution, no `details`
//! channel, and an over-limit-line notice without a shell command.
//!
//! Copyright (c) 2025 Mario Zechner
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use super::ToolHandler;
use crate::model::ToolSpec;

/// The most file lines one `read` result may carry.
pub const MAX_LINES: usize = 2000;

/// The most UTF-8 bytes of file content one `read` result may carry.
pub const MAX_BYTES: usize = 50 * 1024;

const DESCRIPTION: &str = "Read the contents of a file. For text files, output is truncated to \
2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need \
the full file, continue with offset until complete.";

/// The absolute paths successfully read during this process.
///
/// A cloneable handle onto one shared set: `read` records into it and
/// `write` gates on it. It lives in memory for one run only.
#[derive(Debug, Clone, Default)]
pub struct ReadSet(Rc<RefCell<BTreeSet<PathBuf>>>);

impl ReadSet {
    /// Creates an empty read-set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `path` (a resolved absolute path) has been read.
    pub fn contains(&self, path: &Path) -> bool {
        self.0.borrow().contains(path)
    }

    fn record(&self, path: PathBuf) {
        self.0.borrow_mut().insert(path);
    }
}

/// Resolves `path` against `base`, normalising `.` and `..` lexically (no
/// `~` expansion, no symlink resolution).
pub(crate) fn resolve(base: &Path, path: &str) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// The `read` tool handler.
pub struct ReadTool {
    reads: ReadSet,
    base: Option<PathBuf>,
}

impl ReadTool {
    /// A handler resolving paths against the process cwd and recording
    /// successful reads into `reads`.
    pub fn new(reads: ReadSet) -> Self {
        Self { reads, base: None }
    }

    /// The `read` declaration advertised to the model.
    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            "read",
            DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read (relative or absolute)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        )
    }
}

impl ToolHandler for ReadTool {
    fn call(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = input["path"]
            .as_str()
            .ok_or("read: `path` must be a string")?;
        let offset = optional_count(input, "offset")?;
        let limit = optional_count(input, "limit")?;
        let base = match &self.base {
            Some(base) => base.clone(),
            None => std::env::current_dir()
                .map_err(|err| format!("read: cannot determine working directory: {err}"))?,
        };
        let resolved = resolve(&base, path);
        let bytes = std::fs::read(&resolved)
            .map_err(|err| format!("cannot read {}: {}", resolved.display(), os_error(&err)))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("cannot read {}: not valid UTF-8 text", resolved.display()))?;
        let output = read_window(&text, offset, limit)?;
        self.reads.record(resolved);
        Ok(output)
    }
}

/// An optional non-negative integer argument.
fn optional_count(input: &serde_json::Value, key: &str) -> Result<Option<u64>, String> {
    match &input[key] {
        serde_json::Value::Null => Ok(None),
        value => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("read: `{key}` must be a non-negative integer")),
    }
}

/// `err` prefixed with its errno name where one is known.
pub(crate) fn os_error(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => format!("ENOENT: {err}"),
        io::ErrorKind::PermissionDenied => format!("EACCES: {err}"),
        _ => err.to_string(),
    }
}

/// Selects the `offset`/`limit` window of `text`, caps it, and appends the
/// continuation notice when content was withheld.
///
/// `offset`/`limit` stay `u64` until bounded by the line count, so a huge
/// value cannot wrap on a 32-bit target.
fn read_window(text: &str, offset: Option<u64>, limit: Option<u64>) -> Result<String, String> {
    // Offset counting keeps a trailing empty element; cap counting (in
    // `truncate_head`) pops it. The two are deliberately not unified.
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total = all_lines.len();
    let start = offset.map_or(0, |o| o.saturating_sub(1));
    if start >= total as u64 {
        return Err(format!(
            "Offset {} is beyond end of file ({total} lines total)",
            offset.unwrap_or(0)
        ));
    }
    let end = limit.map_or(total, |l| {
        start.saturating_add(l).min(total as u64) as usize
    });
    let start = start as usize;
    let first_display = start + 1;
    let selected = all_lines[start..end].join("\n");

    let output = match truncate_head(&selected) {
        Truncation::FirstLineExceedsLimit => format!(
            "[Line {first_display} is {}, exceeds {} limit. Inspect the line in smaller chunks \
             with the bash tool.]",
            format_size(all_lines[start].len()),
            format_size(MAX_BYTES)
        ),
        Truncation::Truncated { content, lines, by } => {
            let last_display = first_display + lines - 1;
            let limit_note = match by {
                TruncatedBy::Lines => String::new(),
                TruncatedBy::Bytes => format!(" ({} limit)", format_size(MAX_BYTES)),
            };
            format!(
                "{content}\n\n[Showing lines {first_display}-{last_display} of \
                 {total}{limit_note}. Use offset={} to continue.]",
                last_display + 1
            )
        }
        Truncation::Whole if limit.is_some() && end < total => format!(
            "{selected}\n\n[{} more lines in file. Use offset={} to continue.]",
            total - end,
            end + 1
        ),
        Truncation::Whole => selected,
    };
    Ok(output)
}

enum TruncatedBy {
    Lines,
    Bytes,
}

enum Truncation {
    /// The content fits both caps and is returned as-is.
    Whole,
    /// Whole lines were withheld; `content` is the leading `lines` lines.
    Truncated {
        content: String,
        lines: usize,
        by: TruncatedBy,
    },
    /// The first line alone exceeds [`MAX_BYTES`]; no content is returned.
    FirstLineExceedsLimit,
}

/// Keeps the leading whole lines of `content` within [`MAX_LINES`] and
/// [`MAX_BYTES`], charging each line after the first one extra byte for its
/// joining newline.
fn truncate_head(content: &str) -> Truncation {
    let mut lines: Vec<&str> = content.split('\n').collect();
    let mut budgeted = content.len();
    if content.ends_with('\n') {
        // A trailing file newline is neither a line nor a charged byte.
        lines.pop();
        budgeted -= 1;
    }
    if lines.len() <= MAX_LINES && budgeted <= MAX_BYTES {
        return Truncation::Whole;
    }
    if lines.first().is_some_and(|line| line.len() > MAX_BYTES) {
        return Truncation::FirstLineExceedsLimit;
    }
    let mut kept = 0;
    let mut bytes = 0;
    let mut by = TruncatedBy::Lines;
    for (i, line) in lines.iter().take(MAX_LINES).enumerate() {
        let cost = line.len() + usize::from(i > 0);
        if bytes + cost > MAX_BYTES {
            by = TruncatedBy::Bytes;
            break;
        }
        kept += 1;
        bytes += cost;
    }
    Truncation::Truncated {
        content: lines[..kept].join("\n"),
        lines: kept,
        by,
    }
}

/// Renders a byte count as `B`, `KB` or `MB` with one decimal.
fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantReply, ContentBlock, Message, StopReason, TokenUsage};
    use crate::tools::tests::ScriptedTransport;
    use crate::tools::{ToolLoop, ToolRegistry};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh, empty directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leg-read-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool_in(dir: &Path) -> (ReadTool, ReadSet) {
        let reads = ReadSet::new();
        let tool = ReadTool {
            reads: reads.clone(),
            base: Some(dir.to_path_buf()),
        };
        (tool, reads)
    }

    /// Writes `contents` to a file in a fresh directory and reads it by a
    /// relative path.
    fn read(contents: &str, input: serde_json::Value) -> Result<String, String> {
        let dir = temp_dir("file");
        std::fs::write(dir.join("f.txt"), contents).unwrap();
        let mut input = input;
        input["path"] = "f.txt".into();
        tool_in(&dir).0.call(&input)
    }

    /// Splits a result into its file content and its notice, if any.
    fn split_notice(output: &str) -> (&str, Option<&str>) {
        match output.rfind("\n\n[") {
            Some(at) if output.ends_with(']') => (&output[..at], Some(&output[at + 2..])),
            _ => (output, None),
        }
    }

    fn assert_within_caps(output: &str) {
        let (content, _) = split_notice(output);
        assert!(content.split('\n').count() <= MAX_LINES);
        assert!(content.len() <= MAX_BYTES);
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("L{i}\n")).collect()
    }

    #[test]
    fn spec_names_args_caps_and_continuation() {
        let spec = ReadTool::spec();
        assert_eq!(spec.name, "read");
        for arg in ["path", "offset", "limit"] {
            assert!(spec.input_schema["properties"][arg].is_object(), "{arg}");
        }
        assert_eq!(spec.input_schema["required"], serde_json::json!(["path"]));
        assert!(spec.description.contains("2000 lines or 50KB"));
        assert!(spec.description.contains("continue with offset"));
    }

    #[test]
    fn fits_within_limits_returns_whole_file_without_notice() {
        assert_eq!(
            read("a\nb\nc\n", serde_json::json!({})).unwrap(),
            "a\nb\nc\n"
        );
        assert_eq!(read("a\nb\nc", serde_json::json!({})).unwrap(), "a\nb\nc");
    }

    #[test]
    fn offset_alone_reads_to_end_of_file() {
        let out = read("a\nb\nc\n", serde_json::json!({"offset": 2})).unwrap();
        assert_eq!(out, "b\nc\n");
    }

    #[test]
    fn limit_alone_stops_early_with_more_lines_notice() {
        let out = read("1\n2\n3\n4\n5", serde_json::json!({"limit": 2})).unwrap();
        assert_eq!(
            out,
            "1\n2\n\n[3 more lines in file. Use offset=3 to continue.]"
        );
    }

    #[test]
    fn offset_and_limit_select_a_window() {
        let file = "1\n2\n3\n4\n5";
        let out = read(file, serde_json::json!({"offset": 2, "limit": 2})).unwrap();
        assert_eq!(
            out,
            "2\n3\n\n[2 more lines in file. Use offset=4 to continue.]"
        );
        let out = read(file, serde_json::json!({"offset": 4, "limit": 5})).unwrap();
        assert_eq!(out, "4\n5", "a window reaching EOF carries no notice");
    }

    #[test]
    fn offset_boundary_counts_the_trailing_newline_element() {
        let with = "a\nb\nc\n";
        assert_eq!(read(with, serde_json::json!({"offset": 4})).unwrap(), "");
        assert_eq!(
            read(with, serde_json::json!({"offset": 5})).unwrap_err(),
            "Offset 5 is beyond end of file (4 lines total)"
        );
        assert_eq!(
            read(with, serde_json::json!({"limit": 1})).unwrap(),
            "a\n\n[3 more lines in file. Use offset=2 to continue.]"
        );

        let without = "a\nb\nc";
        assert_eq!(
            read(without, serde_json::json!({"offset": 3})).unwrap(),
            "c"
        );
        assert_eq!(
            read(without, serde_json::json!({"offset": 4})).unwrap_err(),
            "Offset 4 is beyond end of file (3 lines total)"
        );
        assert_eq!(
            read(without, serde_json::json!({"limit": 1})).unwrap(),
            "a\n\n[2 more lines in file. Use offset=2 to continue.]"
        );
    }

    #[test]
    fn huge_offset_and_limit_do_not_wrap() {
        // 2^32 + 1 would narrow to 1 as a 32-bit `usize`.
        let huge = (1u64 << 32) + 1;
        assert_eq!(
            read("a\nb", serde_json::json!({"offset": huge})).unwrap_err(),
            format!("Offset {huge} is beyond end of file (2 lines total)")
        );
        assert_eq!(
            read("a\nb", serde_json::json!({"offset": u64::MAX})).unwrap_err(),
            format!("Offset {} is beyond end of file (2 lines total)", u64::MAX)
        );
        assert_eq!(
            read("a\nb", serde_json::json!({"limit": huge})).unwrap(),
            "a\nb"
        );
        assert_eq!(
            read("a\nb", serde_json::json!({"offset": 2, "limit": u64::MAX})).unwrap(),
            "b"
        );
    }

    #[test]
    fn line_cap_reached_first() {
        let out = read(&numbered(3000), serde_json::json!({})).unwrap();
        assert_within_caps(&out);
        let (content, notice) = split_notice(&out);
        assert_eq!(content.split('\n').count(), MAX_LINES);
        assert!(content.ends_with("L2000"));
        assert_eq!(
            notice,
            Some("[Showing lines 1-2000 of 3001. Use offset=2001 to continue.]")
        );
    }

    #[test]
    fn byte_cap_reached_first_drops_the_cut_line_whole() {
        // 100 UTF-8 bytes (50 chars) per line: the first line costs 100,
        // each later one 101, so 506 lines fit in 51 200 bytes.
        let line = "é".repeat(50);
        let file = vec![line.as_str(); 1000].join("\n");
        let out = read(&file, serde_json::json!({})).unwrap();
        assert_within_caps(&out);
        let (content, notice) = split_notice(&out);
        assert_eq!(content.split('\n').count(), 506);
        assert!(content.split('\n').all(|l| l == line), "no line is split");
        assert_eq!(
            notice,
            Some("[Showing lines 1-506 of 1000 (50.0KB limit). Use offset=507 to continue.]")
        );
    }

    #[test]
    fn limit_never_lifts_the_cap() {
        let out = read(&numbered(3000), serde_json::json!({"limit": 2500})).unwrap();
        assert_within_caps(&out);
        let (_, notice) = split_notice(&out);
        assert_eq!(
            notice,
            Some("[Showing lines 1-2000 of 3001. Use offset=2001 to continue.]")
        );
    }

    #[test]
    fn trailing_newline_is_not_charged_to_the_byte_cap() {
        let file = format!("{}\n", "x".repeat(MAX_BYTES));
        assert_eq!(read(&file, serde_json::json!({})).unwrap(), file);
    }

    #[test]
    fn single_line_over_byte_cap_returns_no_file_bytes() {
        let long = "y".repeat(60_000);
        let out = read(
            &format!("short\n{long}\nend"),
            serde_json::json!({"offset": 2}),
        )
        .unwrap();
        assert_eq!(
            out,
            "[Line 2 is 58.6KB, exceeds 50.0KB limit. Inspect the line in smaller chunks with \
             the bash tool.]"
        );
    }

    #[test]
    fn over_limit_notice_has_no_shell_command_or_path() {
        // Windows forbids `"` and `|` in file names.
        let name = if cfg!(windows) {
            "a b;$(touch x)'`&"
        } else {
            "a b;$(touch x)'\"`|&"
        };
        let dir = temp_dir("meta").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big file $HOME.txt");
        std::fs::write(&file, "z".repeat(60_000)).unwrap();
        let (tool, _) = tool_in(&dir);

        let out = tool
            .call(&serde_json::json!({"path": file.to_str().unwrap()}))
            .unwrap();

        assert!(out.starts_with("[Line 1 is 58.6KB"));
        assert!(!out.contains('z'));
        for needle in ["sed", "head", "big file", "$HOME", "a b;"] {
            assert!(!out.contains(needle), "{needle} in {out}");
        }
    }

    #[test]
    fn missing_file_is_an_error_naming_path_and_enoent() {
        let dir = temp_dir("missing");
        let (tool, reads) = tool_in(&dir);
        let err = tool
            .call(&serde_json::json!({"path": "nope.txt"}))
            .unwrap_err();
        assert!(
            err.contains(&dir.join("nope.txt").display().to_string()),
            "{err}"
        );
        assert!(err.contains("ENOENT"), "{err}");
        assert!(reads.0.borrow().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unreadable");
        let file = dir.join("secret.txt");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&file).is_ok() {
            return; // running as root: the platform does not deny the read
        }
        let (tool, reads) = tool_in(&dir);
        let err = tool
            .call(&serde_json::json!({"path": "secret.txt"}))
            .unwrap_err();
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("EACCES"), "{err}");
        assert!(reads.0.borrow().is_empty());
    }

    #[test]
    fn equivalent_paths_record_one_read_set_entry_and_failures_none() {
        let dir = temp_dir("readset");
        let abs = dir.join("a.txt");
        std::fs::write(&abs, "hi").unwrap();
        let (tool, reads) = tool_in(&dir);

        for path in ["a.txt", "./a.txt", "sub/../a.txt", abs.to_str().unwrap()] {
            assert_eq!(tool.call(&serde_json::json!({"path": path})).unwrap(), "hi");
        }
        assert!(tool.call(&serde_json::json!({"path": "b.txt"})).is_err());
        assert!(
            tool.call(&serde_json::json!({"path": "a.txt", "offset": 9}))
                .is_err()
        );

        assert_eq!(*reads.0.borrow(), BTreeSet::from([abs.clone()]));
        assert!(reads.contains(&abs));
    }

    #[test]
    fn loop_turn_receives_file_content_as_tool_result() {
        let dir = temp_dir("loop");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "line one\nline two\n").unwrap();
        let transport = ScriptedTransport::new(vec![
            AssistantReply::from_blocks(
                vec![ContentBlock::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": file.to_str().unwrap()}),
                }],
                TokenUsage::default(),
                Some(StopReason::ToolUse),
            ),
            AssistantReply::new("done"),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(ReadTool::spec(), Box::new(ReadTool::new(ReadSet::new())));
        let tool_loop = ToolLoop::new(transport, registry);

        tool_loop.run(&[Message::user("go")]).unwrap();

        let calls = tool_loop.transport.calls.borrow();
        assert_eq!(
            calls[1].last().unwrap().content,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "line one\nline two\n".to_string(),
                is_error: None,
            }]
        );
    }
}
