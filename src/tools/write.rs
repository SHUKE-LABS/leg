//! The `write` tool: create a file, or replace one already read.
//!
//! Derived from `earendil-works/pi` @ `7fd564cbb78`
//! (`packages/coding-agent/src/core/tools/write.ts`), modified for leg: plain
//! cwd path resolution, and a read-before-write gate on existing files that
//! pi does not have.
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

use std::path::PathBuf;

use super::ToolHandler;
use super::read::{ReadSet, os_error, resolve};
use crate::model::ToolSpec;

const DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, \
overwrites if it does. Automatically creates parent directories. An existing file must be read \
with the read tool first; overwriting a file that has not been read fails.";

/// The `write` tool handler.
pub struct WriteTool {
    reads: ReadSet,
    base: Option<PathBuf>,
}

impl WriteTool {
    /// A handler resolving paths against the process cwd and permitting
    /// overwrites only of paths recorded in `reads`.
    pub fn new(reads: ReadSet) -> Self {
        Self { reads, base: None }
    }

    /// The `write` declaration advertised to the model.
    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            "write",
            DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        )
    }
}

impl ToolHandler for WriteTool {
    fn call(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = input["path"]
            .as_str()
            .ok_or("write: `path` must be a string")?;
        let content = input["content"]
            .as_str()
            .ok_or("write: `content` must be a string")?;
        let base = match &self.base {
            Some(base) => base.clone(),
            None => std::env::current_dir()
                .map_err(|err| format!("write: cannot determine working directory: {err}"))?,
        };
        let resolved = resolve(&base, path);
        if resolved.symlink_metadata().is_ok() && !self.reads.contains(&resolved) {
            return Err(format!(
                "cannot write {path}: file exists and has not been read; read it first"
            ));
        }
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {}", parent.display(), os_error(&err)))?;
        }
        std::fs::write(&resolved, content)
            .map_err(|err| format!("cannot write {}: {}", resolved.display(), os_error(&err)))?;
        Ok(format!("Successfully wrote to {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantReply, ContentBlock, Message, StopReason, TokenUsage};
    use crate::tools::tests::ScriptedTransport;
    use crate::tools::{ReadTool, ToolLoop, ToolRegistry};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh, empty directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leg-write-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `write` resolving against `dir` and a `read` sharing its read-set.
    fn tools_in(dir: &Path) -> (WriteTool, ReadTool) {
        let reads = ReadSet::new();
        let write = WriteTool {
            reads: reads.clone(),
            base: Some(dir.to_path_buf()),
        };
        (write, ReadTool::new(reads))
    }

    /// Reads `path` (absolute, so the read tool's cwd base is irrelevant).
    fn read(tool: &ReadTool, path: &Path, extra: serde_json::Value) -> Result<String, String> {
        let mut input = extra;
        input["path"] = path.to_str().unwrap().into();
        tool.call(&input)
    }

    fn write(tool: &WriteTool, path: &str, content: &str) -> Result<String, String> {
        tool.call(&serde_json::json!({"path": path, "content": content}))
    }

    #[test]
    fn spec_names_args_and_read_first_rule() {
        let spec = WriteTool::spec();
        assert_eq!(spec.name, "write");
        for arg in ["path", "content"] {
            assert_eq!(
                spec.input_schema["properties"][arg]["type"], "string",
                "{arg}"
            );
        }
        assert_eq!(
            spec.input_schema["required"],
            serde_json::json!(["path", "content"])
        );
        assert!(
            spec.description
                .contains("must be read with the read tool first")
        );
    }

    #[test]
    fn creates_a_missing_file() {
        let dir = temp_dir("create");
        let (tool, _) = tools_in(&dir);
        assert_eq!(
            write(&tool, "new.txt", "hello\n").unwrap(),
            "Successfully wrote to new.txt"
        );
        assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), b"hello\n");
    }

    #[test]
    fn creates_nested_missing_parent_directories() {
        let dir = temp_dir("nested");
        let (tool, _) = tools_in(&dir);
        assert_eq!(
            write(&tool, "a/b/c/deep.txt", "x").unwrap(),
            "Successfully wrote to a/b/c/deep.txt"
        );
        assert_eq!(std::fs::read(dir.join("a/b/c/deep.txt")).unwrap(), b"x");
    }

    #[test]
    fn overwrites_a_file_read_earlier() {
        let dir = temp_dir("overwrite");
        let file = dir.join("f.txt");
        std::fs::write(&file, "old").unwrap();
        let (tool, reader) = tools_in(&dir);
        read(&reader, &file, serde_json::json!({})).unwrap();

        assert_eq!(
            write(&tool, "f.txt", "new").unwrap(),
            "Successfully wrote to f.txt"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
    }

    #[test]
    fn rejects_overwrite_of_an_unread_file() {
        let dir = temp_dir("unread");
        let file = dir.join("f.txt");
        std::fs::write(&file, "keep me").unwrap();
        let (tool, _) = tools_in(&dir);
        let mut registry = ToolRegistry::new();
        registry.register(WriteTool::spec(), Box::new(tool));

        let result = registry.dispatch(
            "toolu_1",
            "write",
            &serde_json::json!({"path": "f.txt", "content": "clobbered"}),
        );

        let ContentBlock::ToolResult {
            content, is_error, ..
        } = result
        else {
            panic!("expected a tool_result");
        };
        assert_eq!(is_error, Some(true));
        assert!(content.contains("f.txt"), "{content}");
        assert!(content.contains("read it first"), "{content}");
        assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    }

    #[test]
    fn read_set_is_keyed_by_resolved_absolute_path() {
        let dir = temp_dir("keyed");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let (tool, reader) = tools_in(&dir);
        read(&reader, &a, serde_json::json!({})).unwrap();

        for path in ["a.txt", "./a.txt", a.to_str().unwrap()] {
            assert_eq!(
                write(&tool, path, path).unwrap(),
                format!("Successfully wrote to {path}")
            );
            assert_eq!(std::fs::read_to_string(&a).unwrap(), path);
        }
        assert!(write(&tool, "b.txt", "nope").is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
    }

    #[test]
    fn failed_read_does_not_permit_overwrite() {
        let dir = temp_dir("failed-read");
        let file = dir.join("f.txt");
        std::fs::write(&file, "one line").unwrap();
        let (tool, reader) = tools_in(&dir);
        assert!(read(&reader, &file, serde_json::json!({"offset": 9})).is_err());

        let err = write(&tool, "f.txt", "clobbered").unwrap_err();
        assert!(err.contains("read it first"), "{err}");
        assert_eq!(std::fs::read(&file).unwrap(), b"one line");
    }

    #[test]
    fn loop_turn_writes_a_file() {
        let dir = temp_dir("loop");
        let file = dir.join("out/script.sh");
        let path = file.to_str().unwrap().to_string();
        let transport = ScriptedTransport::new(vec![
            AssistantReply::from_blocks(
                vec![ContentBlock::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "write".to_string(),
                    input: serde_json::json!({"path": path, "content": "#!/bin/sh\necho hi\n"}),
                }],
                TokenUsage::default(),
                Some(StopReason::ToolUse),
            ),
            AssistantReply::new("done"),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(WriteTool::spec(), Box::new(WriteTool::new(ReadSet::new())));
        let tool_loop = ToolLoop::new(transport, registry, None);

        tool_loop.run(&[Message::user("go")]).unwrap();

        assert_eq!(std::fs::read(&file).unwrap(), b"#!/bin/sh\necho hi\n");
        let calls = tool_loop.transport.calls.borrow();
        assert_eq!(
            calls[1].last().unwrap().content,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: format!("Successfully wrote to {path}"),
                is_error: None,
            }]
        );
    }
}
