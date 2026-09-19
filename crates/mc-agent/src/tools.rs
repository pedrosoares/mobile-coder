//! The tool surface, and its dispatch into the sandbox.
//!
//! Deliberately small. `bash` alone would technically do, and on a phone screen
//! that is tempting - but dedicated file tools produce structured, reviewable
//! edits, which is the only thing that makes an approval UI viable on a handset.
//!
//! Definition order is fixed and must stay fixed: `tools` is rendered before
//! `system` and `messages`, so reordering this list invalidates the prompt cache
//! for every session.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use mc_sandbox::Sandbox;
use serde_json::{Value, json};

/// Default ceiling on `read_file`, so a stray `read_file` on a lockfile cannot
/// eat the context window.
const DEFAULT_READ_LIMIT: usize = 2000;

/// How long a command may run before it is killed, unless the call asks for
/// more. Long enough for a dependency install, short enough that a hung command
/// does not strand the agent.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Upper bound a call may ask for: a long build, not an unbounded wait.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Ceiling on what one tool result may add to the conversation.
///
/// Roughly 8k tokens. Everything the model reads is resent on every later turn,
/// so one `cat` of a lockfile would otherwise cost money on every request for
/// the rest of the session - and can end it outright by filling the context.
const MAX_TOOL_OUTPUT: usize = 30_000;

/// Trim a tool result to [`MAX_TOOL_OUTPUT`], keeping both ends.
///
/// The head carries the command's intent (a compiler's first errors); the tail
/// carries its conclusion (the summary line, the failing assertion). The middle
/// is what a person skims past, so that is what goes.
fn clamp_output(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_TOOL_OUTPUT {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_TOOL_OUTPUT * 2 / 3).collect();
    let tail: String = text
        .chars()
        .skip(count - MAX_TOOL_OUTPUT / 3)
        .collect::<String>();
    let dropped = count - head.chars().count() - tail.chars().count();
    format!("{head}\n\n… {dropped} characters trimmed from the middle …\n\n{tail}")
}

/// Run a command with the call's timeout, describing a kill in the result.
async fn run_command(
    sandbox: &Sandbox,
    command: &str,
    cwd: Option<&std::path::Path>,
    timeout: Duration,
    cancel: &mc_core::Cancel,
) -> ToolOutcome {
    match sandbox.run_cancellable(command, cwd, Some(timeout), cancel).await {
        Ok(_) if cancel.is_cancelled() => ToolOutcome::stopped(),
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
            if out.timed_out {
                // Say what happened and how to proceed: the model's next move
                // should be a shorter command, not the same one again.
                return ToolOutcome::err(format!(
                    "timed out after {}s and was killed. Any output before that:\n{}\n\n\
                     Run something shorter, or pass a larger timeout_seconds.",
                    timeout.as_secs(),
                    clamp_output(text.trim_end())
                ));
            }
            if text.is_empty() {
                text.push_str("(no output)");
            }
            let text = clamp_output(&text);
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

/// Start a background job and tell the model how to follow it.
fn start_job(sandbox: &Sandbox, command: &str, cwd: Option<&std::path::Path>) -> ToolOutcome {
    match sandbox.start_background(command, cwd) {
        Ok(id) => ToolOutcome::ok(format!(
            "started {id} in the background. Read it with job_output {{\"id\": \"{id}\"}}, \
             and stop it with job_kill when it is no longer needed."
        )),
        Err(e) => ToolOutcome::err(e.to_string()),
    }
}

/// A job's new output, with enough context to act on it.
fn read_job(id: &str) -> ToolOutcome {
    let Some(read) = mc_sandbox::jobs::read(id) else {
        return ToolOutcome::err(unknown_job(id));
    };
    let mut text = format!("{}\n", read.summary.describe());
    if read.dropped > 0 {
        // Say it, or the model will read a truncated log as the whole story.
        text.push_str(&format!(
            "… {} characters dropped from the start; this job prints more than is kept …\n",
            read.dropped
        ));
    }
    if read.output.is_empty() {
        text.push_str("(nothing new since the last read)");
    } else {
        text.push_str(&read.output);
    }
    // A job that has ended is still an ordinary result: what it printed is the
    // answer, and its exit code is in the first line.
    ToolOutcome::ok(clamp_output(&text))
}

fn list_jobs() -> ToolOutcome {
    let jobs = mc_sandbox::jobs::list();
    if jobs.is_empty() {
        return ToolOutcome::ok("no background jobs");
    }
    let lines: Vec<String> = jobs.iter().map(|job| job.describe()).collect();
    ToolOutcome::ok(lines.join("\n"))
}

/// Why an id might be unknown, since the usual reason is not a typo.
fn unknown_job(id: &str) -> String {
    format!(
        "no job {id}. Jobs live in the running app, so one started before the app restarted is \
         gone. Use job_output with no id to see what is running."
    )
}

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
                 running tests, git, and package management. Output is captured and returned. \
                 For anything that does not end on its own - a dev server, a watcher, \
                 `tail -f` - pass run_in_background instead of raising the timeout.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to run." },
                    "cwd": { "type": "string", "description": "Working directory. Defaults to the project root." },
                    "timeout_seconds": {
                        "type": "integer",
                        "description":
                            "Kill the command after this long. Defaults to 300. Raise it for a \
                             long build; the command is killed and reported if it overruns."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description":
                            "Start the command and return a job id immediately, instead of \
                             waiting for it. The job keeps running between turns, with no \
                             timeout. Read what it has printed with job_output, and end it \
                             with job_kill - always kill a job once it is no longer needed."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "job_output",
            "description":
                "Read what a background job has printed since the last read. Also reports \
                 whether it is still running. Call it with no id to list every job.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "A job id from bash run_in_background, e.g. job-1." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "job_kill",
            "description":
                "Stop a background job and everything it started. Use \"all\" to stop every job.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "A job id, or \"all\"." }
                },
                "required": ["id"],
                "additionalProperties": false
            }
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
    /// What a pending call gets once the user has stopped the turn.
    pub(crate) fn stopped() -> Self {
        Self { text: "Not run: the user stopped the turn.".into(), is_error: true }
    }

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
    execute_cancellable(sandbox, name, input, &mc_core::Cancel::new()).await
}

/// As [`execute`], with a token that kills a running command.
pub async fn execute_cancellable(
    sandbox: &Sandbox,
    name: &str,
    input: &Value,
    cancel: &mc_core::Cancel,
) -> ToolOutcome {
    match name {
        "bash" => {
            let Some(command) = field(input, "command") else {
                return ToolOutcome::err("bash requires a `command`");
            };
            let cwd = field(input, "cwd").map(std::path::Path::new);
            if input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false) {
                return start_job(sandbox, command, cwd);
            }
            let seconds = input
                .get("timeout_seconds")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS);
            run_command(sandbox, command, cwd, Duration::from_secs(seconds), cancel).await
        }

        "job_output" => match field(input, "id") {
            Some(id) => read_job(id),
            // No id: the listing, which is also how a resumed session finds out
            // what is still running.
            None => list_jobs(),
        },

        "job_kill" => {
            let Some(id) = field(input, "id") else {
                return ToolOutcome::err("job_kill requires an `id` (or \"all\")");
            };
            if id == "all" {
                let killed = mc_sandbox::jobs::kill_all();
                return ToolOutcome::ok(format!("killed {killed} job(s)"));
            }
            match mc_sandbox::jobs::kill(id) {
                Some(job) => ToolOutcome::ok(format!("{id} {}", job.status.describe())),
                None => ToolOutcome::err(unknown_job(id)),
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
            match sandbox
                .run_with_timeout(&cmd, None, Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS)))
                .await
            {
                Ok(out) if out.ok() => ToolOutcome::ok(clamp_output(&out.stdout)),
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
            match sandbox
                .run_with_timeout(&cmd, None, Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS)))
                .await
            {
                // grep exits 1 on "no matches", which is not an error worth
                // surfacing to the model as a failure.
                Ok(out) if out.ok() => ToolOutcome::ok(clamp_output(&out.stdout)),
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
        // Reordering invalidates the prompt cache for every live session. Adding
        // one does too, once - which is why the job tools sit beside `bash`,
        // where they belong, rather than being appended to dodge a cost that
        // any change to this list pays anyway.
        let names: Vec<_> = definitions()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            ["bash", "job_output", "job_kill", "read_file", "write_file", "edit_file", "search"]
        );
    }

    #[test]
    fn short_output_is_passed_through_untouched() {
        assert_eq!(clamp_output("all good"), "all good");
    }

    #[test]
    fn huge_output_keeps_both_ends_and_says_what_was_dropped() {
        let text = format!("HEAD{}TAIL", "x".repeat(MAX_TOOL_OUTPUT * 2));
        let clamped = clamp_output(&text);
        assert!(clamped.starts_with("HEAD"), "keeps the start");
        assert!(clamped.ends_with("TAIL"), "keeps the end");
        assert!(clamped.contains("characters trimmed from the middle"));
        assert!(
            clamped.chars().count() < MAX_TOOL_OUTPUT + 200,
            "stays near the ceiling, got {}",
            clamped.chars().count()
        );
    }

    #[tokio::test]
    async fn a_background_job_is_started_read_and_killed_through_the_tools() {
        let sandbox = Sandbox::host();
        let started = execute(
            &sandbox,
            "bash",
            &json!({
                // Prints something that does not appear in the command itself,
                // so a read cannot pass on the echo of its own summary line.
                "command": "echo $((6 * 7)); sleep 60",
                "run_in_background": true,
            }),
        )
        .await;
        assert!(!started.is_error, "got {}", started.text);
        // The model has to be able to find the id in what it is handed.
        let id = started
            .text
            .split_whitespace()
            .find(|word| word.starts_with("job-"))
            .expect("the result names the job")
            .to_string();

        tokio::time::sleep(Duration::from_millis(400)).await;
        let output = execute(&sandbox, "job_output", &json!({ "id": id })).await;
        assert!(!output.is_error, "got {}", output.text);
        assert!(output.text.contains("42"), "got {}", output.text);
        assert!(output.text.contains("running"), "says where it stands: {}", output.text);

        // Read again with nothing new: not an error, and it says so rather than
        // handing back the same lines.
        let again = execute(&sandbox, "job_output", &json!({ "id": id })).await;
        assert!(!again.is_error);
        assert!(!again.text.contains("42"), "re-read its output: {}", again.text);
        assert!(again.text.contains("nothing new"), "got {}", again.text);

        // Listing needs no id, which is how a resumed session finds its jobs.
        let listed = execute(&sandbox, "job_output", &json!({})).await;
        assert!(listed.text.contains(&id), "got {}", listed.text);

        let killed = execute(&sandbox, "job_kill", &json!({ "id": id })).await;
        assert!(!killed.is_error, "got {}", killed.text);
        assert!(killed.text.contains("killed"), "got {}", killed.text);

        let after = execute(&sandbox, "job_output", &json!({ "id": id })).await;
        assert!(after.text.contains("killed"), "got {}", after.text);
    }

    #[tokio::test]
    async fn an_unknown_job_explains_itself_instead_of_failing_blankly() {
        let out = execute(&Sandbox::host(), "job_output", &json!({ "id": "job-9999" })).await;
        assert!(out.is_error);
        assert!(out.text.contains("app restarted"), "got {}", out.text);
    }

    #[tokio::test]
    async fn a_hanging_command_is_killed_and_the_model_is_told_how_to_proceed() {
        let out = execute(
            &Sandbox::host(),
            "bash",
            &json!({ "command": "echo starting; sleep 30", "timeout_seconds": 1 }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("timed out after 1s"), "got {}", out.text);
        assert!(out.text.contains("starting"), "keeps output from before the kill");
        assert!(out.text.contains("timeout_seconds"), "tells the model the way out");
    }

    #[tokio::test]
    async fn a_tool_result_cannot_flood_the_conversation() {
        let out = execute(
            &Sandbox::host(),
            "bash",
            &json!({ "command": "yes hello | head -200000" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.chars().count() < MAX_TOOL_OUTPUT + 200);
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
