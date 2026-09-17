//! The tool surface, and its dispatch into the sandbox.
//!
//! Deliberately small. `bash` alone would technically do, and on a phone screen
//! that is tempting - but dedicated file tools produce structured, reviewable
//! edits, which is the only thing that makes an approval UI viable on a handset.
//!
//! Definition order is fixed and must stay fixed: `tools` is rendered before
//! `system` and `messages`, so reordering this list invalidates the prompt cache
//! for every session.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use mc_sandbox::Sandbox;
use serde_json::{Value, json};

/// Default ceiling on `read_file`, so a stray `read_file` on a lockfile cannot
/// eat the context window.
const DEFAULT_READ_LIMIT: usize = 2000;

/// The tool definitions sent with every request.
///
/// The final entry carries `cache_control`, marking the end of the cacheable
/// prefix. Everything stable belongs above that breakpoint.
pub fn definitions() -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "bash",
            "description":
                "Run a shell command inside the project's Linux sandbox. Use for building, \
                 running tests, git, and package management. Output is captured and returned.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to run." },
                    "cwd": { "type": "string", "description": "Working directory. Defaults to the project root." }
                },
                "required": ["command"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "name": "read_file",
            "description":
                "Read a text file. Prefer this over `cat` so large files can be paged \
                 rather than dumped whole.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path":   { "type": "string" },
                    "offset": { "type": "integer", "description": "First line, 1-indexed." },
                    "limit":  { "type": "integer", "description": "How many lines to read." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "write_file",
            "description": "Write a file in full, creating it if needed. Overwrites existing content.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path":    { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "name": "edit_file",
            "description":
                "Replace an exact string in a file. `old` must appear exactly once, or the \
                 edit is refused - this is intentional, so an ambiguous edit fails loudly \
                 rather than changing the wrong line.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old":  { "type": "string", "description": "Exact text to replace. Must be unique in the file." },
                    "new":  { "type": "string", "description": "Replacement text." }
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "name": "search",
            "description":
                "Search file contents (regex) or find files by name, without spending a bash turn.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path":    { "type": "string", "description": "Directory to search. Defaults to the project root." },
                    "mode":    { "type": "string", "enum": ["content", "filename"] }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
    ];

    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
    tools
}

/// Outcome of a tool call. `is_error` is passed straight through to the API -
/// a failed tool must still return a `tool_result`, never a dropped block.
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false }
    }
    fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true }
    }
}

/// Single-quote a string for `/bin/sh`.
///
/// Everything reaching the shell is model-authored, so this is the boundary that
/// stops a filename with a quote in it becoming a command.
fn shq(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

fn field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

pub async fn execute(sandbox: &Sandbox, name: &str, input: &Value) -> ToolOutcome {
    match name {
        "bash" => {
            let Some(command) = field(input, "command") else {
                return ToolOutcome::err("bash requires a `command`");
            };
            let cwd = field(input, "cwd").map(std::path::Path::new);
            match sandbox.run(command, cwd).await {
                Ok(out) => {
                    let mut text = String::new();
                    if !out.stdout.is_empty() {
                        text.push_str(&out.stdout);
                    }
                    if !out.stderr.is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&out.stderr);
                    }
                    if text.is_empty() {
                        text.push_str("(no output)");
                    }
                    if out.ok() {
                        ToolOutcome::ok(text)
                    } else {
                        ToolOutcome::err(format!(
                            "exit status {}\n{text}",
                            out.status.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
                        ))
                    }
                }
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }

        "read_file" => {
            let Some(path) = field(input, "path") else {
                return ToolOutcome::err("read_file requires a `path`");
            };
            let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1);
            let limit = input
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_READ_LIMIT as u64);
            let end = offset + limit - 1;
            let cmd = format!("sed -n {},{}p -- {}", offset, end, shq(path));
            match sandbox.run(&cmd, None).await {
                Ok(out) if out.ok() => ToolOutcome::ok(out.stdout),
                Ok(out) => ToolOutcome::err(format!("cannot read {path}: {}", out.stderr.trim())),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }

        "write_file" => {
            let (Some(path), Some(content)) = (field(input, "path"), field(input, "content")) else {
                return ToolOutcome::err("write_file requires `path` and `content`");
            };
            // Round-trip through base64 so arbitrary bytes - quotes, newlines,
            // non-UTF8 escapes - survive the shell without quoting heroics.
            let encoded = B64.encode(content.as_bytes());
            let cmd = format!(
                "mkdir -p -- \"$(dirname -- {p})\" && printf %s {b} | base64 -d > {p}",
                p = shq(path),
                b = shq(&encoded),
            );
            match sandbox.run(&cmd, None).await {
                Ok(out) if out.ok() => {
                    ToolOutcome::ok(format!("wrote {} bytes to {path}", content.len()))
                }
                Ok(out) => ToolOutcome::err(format!("cannot write {path}: {}", out.stderr.trim())),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }

        "edit_file" => {
            let (Some(path), Some(old), Some(new)) =
                (field(input, "path"), field(input, "old"), field(input, "new"))
            else {
                return ToolOutcome::err("edit_file requires `path`, `old` and `new`");
            };

            let read = format!("base64 -w0 -- {} 2>/dev/null || base64 -- {}", shq(path), shq(path));
            let current = match sandbox.run(&read, None).await {
                Ok(out) if out.ok() => {
                    let raw = out.stdout.replace(['\n', '\r'], "");
                    match B64.decode(raw).ok().and_then(|b| String::from_utf8(b).ok()) {
                        Some(text) => text,
                        None => return ToolOutcome::err(format!("{path} is not valid UTF-8")),
                    }
                }
                Ok(out) => return ToolOutcome::err(format!("cannot read {path}: {}", out.stderr.trim())),
                Err(e) => return ToolOutcome::err(e.to_string()),
            };

            match current.matches(old).count() {
                0 => return ToolOutcome::err(format!("`old` does not appear in {path}")),
                1 => {}
                n => {
                    return ToolOutcome::err(format!(
                        "`old` appears {n} times in {path}; include more surrounding context so \
                         the match is unique"
                    ));
                }
            }

            let updated = current.replacen(old, new, 1);
            let encoded = B64.encode(updated.as_bytes());
            let cmd = format!("printf %s {b} | base64 -d > {p}", b = shq(&encoded), p = shq(path));
            match sandbox.run(&cmd, None).await {
                Ok(out) if out.ok() => ToolOutcome::ok(format!("edited {path}")),
                Ok(out) => ToolOutcome::err(format!("cannot write {path}: {}", out.stderr.trim())),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }

        "search" => {
            let Some(pattern) = field(input, "pattern") else {
                return ToolOutcome::err("search requires a `pattern`");
            };
            let path = field(input, "path").unwrap_or(".");
            let cmd = match field(input, "mode").unwrap_or("content") {
                "filename" => format!("find {} -name {} -type f", shq(path), shq(pattern)),
                _ => format!("grep -rnI -e {} -- {}", shq(pattern), shq(path)),
            };
            match sandbox.run(&cmd, None).await {
                // grep exits 1 on "no matches", which is not an error worth
                // surfacing to the model as a failure.
                Ok(out) if out.ok() => ToolOutcome::ok(out.stdout),
                Ok(out) if out.status == Some(1) => ToolOutcome::ok("no matches"),
                Ok(out) => ToolOutcome::err(out.stderr),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }

        other => ToolOutcome::err(format!("unknown tool `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_neutralises_embedded_quotes() {
        assert_eq!(shq("plain"), "'plain'");
        assert_eq!(shq("it's"), r"'it'\''s'");
        // The classic injection attempt becomes inert data.
        assert_eq!(shq("'; rm -rf /; '"), r"''\''; rm -rf /; '\'''");
    }

    #[test]
    fn the_cache_breakpoint_sits_on_the_last_tool() {
        let tools = definitions();
        assert!(tools.last().unwrap().get("cache_control").is_some());
        assert_eq!(
            tools.iter().filter(|t| t.get("cache_control").is_some()).count(),
            1,
            "exactly one breakpoint, or the prefix is being split needlessly"
        );
    }

    #[test]
    fn tool_order_is_stable() {
        // Reordering invalidates the prompt cache for every live session.
        let names: Vec<_> = definitions()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["bash", "read_file", "write_file", "edit_file", "search"]);
    }

    #[tokio::test]
    async fn write_then_read_round_trips_through_the_host_sandbox() {
        let dir = std::env::temp_dir().join("mc-agent-tool-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hello.txt");
        let sandbox = Sandbox::host();

        let wrote = execute(
            &sandbox,
            "write_file",
            &json!({ "path": path.to_str().unwrap(), "content": "line one\nit's \"quoted\"\n" }),
        )
        .await;
        assert!(!wrote.is_error, "{}", wrote.text);

        let read = execute(&sandbox, "read_file", &json!({ "path": path.to_str().unwrap() })).await;
        assert!(!read.is_error, "{}", read.text);
        assert!(read.text.contains(r#"it's "quoted""#), "got: {}", read.text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_refuses_an_ambiguous_match() {
        let dir = std::env::temp_dir().join("mc-agent-edit-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dup.txt");
        std::fs::write(&path, "x\nx\n").unwrap();

        let out = execute(
            &Sandbox::host(),
            "edit_file",
            &json!({ "path": path.to_str().unwrap(), "old": "x", "new": "y" }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("appears 2 times"), "got: {}", out.text);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
