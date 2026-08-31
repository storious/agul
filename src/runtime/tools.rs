use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::TurnCancellation;
use super::plugin::{
    PluginCallContext, PluginEvent, PluginExecutionError, PluginTerminal, PluginTool,
};
use super::process::{ProcessLimits, ProcessTree, run_process_cancellable};
use super::provider::{ToolCall, ToolDefinition};

const DEFAULT_READ_LINES: usize = 400;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
const MAX_RENDERED_OUTPUT_CHARS: usize = 200_000;
const MAX_SHELL_STDOUT_BYTES: usize = 200_000;
const MAX_SHELL_STDERR_BYTES: usize = 200_000;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read".to_string(),
            description: "Read a UTF-8 text file. Paths may be relative to the workspace or absolute.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1, "description": "First line, starting at 1."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum lines to return."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "write".to_string(),
            description: "Create or overwrite a file with the supplied content. Parent directories are created when needed.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "edit".to_string(),
            description: "Replace exact text in an existing UTF-8 file. By default the old text must occur exactly once.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "shell".to_string(),
            description: shell_description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "description": "Defaults to 120000."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
    ]
}

pub(crate) struct ToolSet {
    definitions: Vec<ToolDefinition>,
    plugins: HashMap<String, PluginTool>,
}

impl ToolSet {
    pub(crate) fn new(plugin_tools: &[PluginTool]) -> Result<Self, String> {
        let mut definitions = definitions();
        let mut names = definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect::<HashSet<_>>();
        let mut plugins = HashMap::new();
        for tool in plugin_tools {
            if !names.insert(tool.name.clone()) {
                return Err(format!("tool name is already registered: {}", tool.name));
            }
            definitions.push(tool.definition());
            plugins.insert(tool.name.clone(), tool.clone());
        }
        Ok(Self {
            definitions,
            plugins,
        })
    }

    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect()
    }

    pub(crate) fn describe_call(&self, call: &ToolCall) -> String {
        let Some(tool) = self.plugins.get(&call.name) else {
            return describe_call(call);
        };
        ["query", "url", "path", "command"]
            .into_iter()
            .find_map(|key| call.arguments.get(key).and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .map(|value| truncate(value, 100))
            .unwrap_or_else(|| tool.detail().to_string())
    }

    pub(crate) fn execute_cancellable<E>(
        &self,
        call: &ToolCall,
        context: &PluginCallContext<'_>,
        cancellation: &TurnCancellation,
        on_plugin_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<ToolExecution, E> {
        if cancellation.is_cancelled() {
            return Ok(cancelled_execution(call));
        }
        let Some(plugin) = self.plugins.get(&call.name) else {
            return Ok(execute_cancellable(call, context.workspace, cancellation));
        };
        let label = call.name.clone();
        let detail = self.describe_call(call);
        let plugin_execution =
            plugin.execute_cancellable(&call.arguments, context, cancellation, on_plugin_event);
        let execution = match plugin_execution {
            Ok(PluginTerminal::Success(content)) => ToolExecution {
                ok: true,
                label,
                detail,
                content: match content {
                    Value::String(content) => content,
                    content => serde_json::to_string(&content)
                        .expect("plugin result content is serializable"),
                },
            },
            Ok(PluginTerminal::Failure(error)) => ToolExecution {
                ok: false,
                label,
                detail,
                content: serde_json::to_string(&json!({"ok": false, "error": error}))
                    .expect("plugin error is serializable"),
            },
            Err(PluginExecutionError::Plugin(error)) => ToolExecution {
                ok: false,
                label,
                detail,
                content: serde_json::to_string(&json!({
                    "ok": false,
                    "error": {
                        "code": "plugin_runtime",
                        "message": error,
                        "retryable": false
                    }
                }))
                .expect("plugin error is serializable"),
            },
            Err(PluginExecutionError::Event(error)) => return Err(error),
        };
        Ok(execution)
    }
}

#[cfg(windows)]
fn shell_description() -> &'static str {
    "Run a PowerShell command in the workspace and return its exit code, stdout, and stderr. The working directory is already the workspace; use PowerShell syntax, not cmd.exe or POSIX shell syntax."
}

#[cfg(not(windows))]
fn shell_description() -> &'static str {
    "Run a /bin/sh command in the workspace and return its exit code, stdout, and stderr. The working directory is already the workspace; use POSIX shell syntax."
}

pub(crate) struct ToolExecution {
    pub(crate) ok: bool,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) content: String,
}

#[cfg(test)]
fn execute(call: &ToolCall, workspace: &Path) -> ToolExecution {
    execute_cancellable(call, workspace, &TurnCancellation::default())
}

pub(crate) fn execute_cancellable(
    call: &ToolCall,
    workspace: &Path,
    cancellation: &TurnCancellation,
) -> ToolExecution {
    execute_inner(call, workspace, cancellation)
}

fn execute_inner(
    call: &ToolCall,
    workspace: &Path,
    cancellation: &TurnCancellation,
) -> ToolExecution {
    let label = call.name.clone();
    let detail = describe_call(call);
    let result = if cancellation.is_cancelled() {
        Err("cancelled".to_string())
    } else {
        match call.name.as_str() {
            "read" => {
                parse::<ReadArgs>(&call.arguments).and_then(|args| read_file(workspace, args))
            }
            "write" => {
                parse::<WriteArgs>(&call.arguments).and_then(|args| write_file(workspace, args))
            }
            "edit" => {
                parse::<EditArgs>(&call.arguments).and_then(|args| edit_file(workspace, args))
            }
            "shell" => parse::<ShellArgs>(&call.arguments)
                .and_then(|args| run_shell_cancellable(workspace, args, cancellation)),
            other => Err(format!("unknown tool: {other}")),
        }
    };
    match result {
        Ok(value) => {
            let ok = value
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            ToolExecution {
                ok,
                label,
                detail,
                content: serde_json::to_string(&json!({"ok": ok, "result": value}))
                    .expect("tool result is serializable"),
            }
        }
        Err(error) => ToolExecution {
            ok: false,
            label,
            detail,
            content: serde_json::to_string(&json!({"ok": false, "error": error}))
                .expect("tool error is serializable"),
        },
    }
}

fn cancelled_execution(call: &ToolCall) -> ToolExecution {
    ToolExecution {
        ok: false,
        label: call.name.clone(),
        detail: describe_call(call),
        content: serde_json::to_string(&json!({"ok": false, "error": "cancelled"}))
            .expect("tool error is serializable"),
    }
}

pub(crate) fn describe_call(call: &ToolCall) -> String {
    let key = if call.name == "shell" {
        "command"
    } else {
        "path"
    };
    let value = call
        .arguments
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    truncate(value, 100)
}

fn parse<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("invalid arguments: {error}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn read_file(workspace: &Path, args: ReadArgs) -> Result<Value, String> {
    let path = resolve_path(workspace, &args.path);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let offset = args.offset.unwrap_or(1);
    if offset == 0 {
        return Err("offset starts at 1".to_string());
    }
    let limit = args.limit.unwrap_or(DEFAULT_READ_LINES);
    if limit == 0 {
        return Err("limit must be greater than zero".to_string());
    }
    let lines = text.lines().collect::<Vec<_>>();
    let start = offset.saturating_sub(1).min(lines.len());
    let end = start.saturating_add(limit).min(lines.len());
    let mut content = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        content.push_str(&format!("{:>6} | {line}\n", start + index + 1));
    }
    Ok(json!({
        "path": path.display().to_string(),
        "content": content,
        "start_line": start + 1,
        "end_line": end,
        "total_lines": lines.len()
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

fn write_file(workspace: &Path, args: WriteArgs) -> Result<Value, String> {
    let path = resolve_path(workspace, &args.path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(&path, args.content.as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": args.content.len()
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

fn as_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn has_only_crlf_line_endings(text: &str) -> bool {
    text.contains("\r\n") && text.split("\r\n").all(|part| !part.contains('\n'))
}

fn edit_file(workspace: &Path, args: EditArgs) -> Result<Value, String> {
    if args.old_text.is_empty() {
        return Err("old_text must not be empty".to_string());
    }
    let path = resolve_path(workspace, &args.path);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    // `read` renders LF line endings. Preserve an exact match first, then adapt
    // a copied multiline snippet to a CRLF file without rewriting its style.
    let mut old_text = args.old_text;
    let mut new_text = args.new_text;
    let mut matches = text.matches(&old_text).count();
    if matches == 0 && old_text.contains('\n') && text.contains("\r\n") {
        let crlf_old = as_crlf(&old_text);
        matches = text.matches(&crlf_old).count();
        if matches > 0 {
            old_text = crlf_old;
        }
    }
    if matches == 0 {
        return Err("old_text was not found".to_string());
    }
    if new_text.contains('\n') && (old_text.contains("\r\n") || has_only_crlf_line_endings(&text)) {
        new_text = as_crlf(&new_text);
    }
    if !args.replace_all && matches != 1 {
        return Err(format!(
            "old_text occurs {matches} times; provide more context or set replace_all"
        ));
    }
    let updated = if args.replace_all {
        text.replace(&old_text, &new_text)
    } else {
        text.replacen(&old_text, &new_text, 1)
    };
    let mut file =
        fs::File::create(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(updated.as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(json!({
        "path": path.display().to_string(),
        "replacements": if args.replace_all { matches } else { 1 },
        "bytes": updated.len()
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    command: String,
    timeout_ms: Option<u64>,
}

fn run_shell_cancellable(
    workspace: &Path,
    args: ShellArgs,
    cancellation: &TurnCancellation,
) -> Result<Value, String> {
    if cancellation.is_cancelled() {
        return Err("cancelled".to_string());
    }
    if args.command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS));
    let limits = ProcessLimits::truncating(timeout, MAX_SHELL_STDOUT_BYTES, MAX_SHELL_STDERR_BYTES);
    run_shell_with_limits_cancellable(workspace, args, limits, cancellation)
}

#[cfg(test)]
fn run_shell_with_limits(
    workspace: &Path,
    args: ShellArgs,
    limits: ProcessLimits,
) -> Result<Value, String> {
    run_shell_with_limits_cancellable(workspace, args, limits, &TurnCancellation::default())
}

fn run_shell_with_limits_cancellable(
    workspace: &Path,
    args: ShellArgs,
    limits: ProcessLimits,
    cancellation: &TurnCancellation,
) -> Result<Value, String> {
    let started = Instant::now();
    let (mut child, tree) = spawn_shell(workspace, &args.command)?;
    let output = run_process_cancellable(&mut child, tree, None, limits, cancellation)
        .map_err(|error| format!("command {error}"))?;
    let truncated = output.stdout_truncated || output.stderr_truncated;
    let stdout = truncate(
        &String::from_utf8_lossy(&output.stdout),
        MAX_RENDERED_OUTPUT_CHARS,
    );
    let stderr = truncate(
        &String::from_utf8_lossy(&output.stderr),
        MAX_RENDERED_OUTPUT_CHARS,
    );
    if output.timed_out {
        return Ok(json!({
            "command": args.command,
            "exit_code": null,
            "success": false,
            "timed_out": true,
            "truncated": truncated,
            "elapsed_ms": started.elapsed().as_millis(),
            "stdout": stdout,
            "stderr": stderr,
            "error": format!("timed out after {} ms", limits.timeout().as_millis())
        }));
    }
    let status = output
        .status
        .ok_or_else(|| "command ended without an exit status".to_string())?;
    Ok(json!({
        "command": args.command,
        "exit_code": status.code(),
        "success": status.success(),
        "timed_out": false,
        "truncated": truncated,
        "elapsed_ms": started.elapsed().as_millis(),
        "stdout": stdout,
        "stderr": stderr
    }))
}

#[cfg(windows)]
fn spawn_shell(
    workspace: &Path,
    source: &str,
) -> Result<(std::process::Child, ProcessTree), String> {
    let configure = |program: &str| -> Result<_, String> {
        let mut command = Command::new(program);
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                source,
            ])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let tree = ProcessTree::prepare(&mut command)?;
        Ok((command, tree))
    };
    let (mut command, mut tree) = configure("pwsh.exe")?;
    match command.spawn() {
        Ok(mut child) => {
            tree.assign(&mut child)?;
            Ok((child, tree))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (mut command, mut tree) = configure("powershell.exe")?;
            let mut child = command
                .spawn()
                .map_err(|error| format!("could not start PowerShell: {error}"))?;
            tree.assign(&mut child)?;
            Ok((child, tree))
        }
        Err(error) => Err(format!("could not start PowerShell: {error}")),
    }
}

#[cfg(unix)]
fn spawn_shell(
    workspace: &Path,
    source: &str,
) -> Result<(std::process::Child, ProcessTree), String> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-lc", source])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut tree = ProcessTree::prepare(&mut command)?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start shell: {error}"))?;
    tree.assign(&mut child)?;
    Ok((child, tree))
}

fn resolve_path(workspace: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let head = max_chars * 2 / 3;
    let tail = max_chars - head;
    let start = value.chars().take(head).collect::<String>();
    let end = value
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}\n... {count} characters total ...\n{end}")
}

#[cfg(test)]
mod tests {
    use super::super::process::{FixtureProcess, process_test_lock};
    use super::*;

    #[test]
    fn exposes_only_four_general_tools() {
        assert_eq!(
            definitions()
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "write", "edit", "shell"]
        );
        let shell = definitions()
            .into_iter()
            .find(|definition| definition.name == "shell")
            .unwrap();
        #[cfg(windows)]
        assert!(shell.description.contains("PowerShell"));
        #[cfg(not(windows))]
        assert!(shell.description.contains("/bin/sh"));
    }

    #[test]
    fn write_read_and_edit_form_a_direct_loop() {
        let root = tempfile::tempdir().unwrap();
        let write = ToolCall {
            id: "1".to_string(),
            name: "write".to_string(),
            arguments: json!({"path": "src/note.txt", "content": "old\n"}),
        };
        assert!(execute(&write, root.path()).ok);
        let edit = ToolCall {
            id: "2".to_string(),
            name: "edit".to_string(),
            arguments: json!({"path": "src/note.txt", "old_text": "old", "new_text": "new"}),
        };
        assert!(execute(&edit, root.path()).ok);
        assert_eq!(
            fs::read_to_string(root.path().join("src/note.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn edit_accepts_read_style_lf_multiline_snippets_in_crlf_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("note.txt");
        let original = "alpha\r\nbeta\r\n";
        fs::write(&path, original).unwrap();

        let read = read_file(
            root.path(),
            ReadArgs {
                path: "note.txt".to_string(),
                offset: None,
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(read["content"], "     1 | alpha\n     2 | beta\n");

        let edit = |old_text: &str, new_text: &str, replace_all| EditArgs {
            path: "note.txt".to_string(),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
            replace_all,
        };
        let result = edit_file(root.path(), edit("alpha\nbeta", "alpha\nchanged", false)).unwrap();
        assert_eq!(result["replacements"], 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\r\nchanged\r\n");

        fs::write(&path, original).unwrap();
        edit_file(root.path(), edit("alpha", "alpha\ninserted", false)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "alpha\r\ninserted\r\nbeta\r\n"
        );

        fs::write(&path, original.repeat(2)).unwrap();
        assert_eq!(
            edit_file(root.path(), edit("alpha\nbeta", "alpha\nchanged", false)).unwrap_err(),
            "old_text occurs 2 times; provide more context or set replace_all"
        );
        let result = edit_file(root.path(), edit("alpha\nbeta", "alpha\nchanged", true)).unwrap();
        assert_eq!(result["replacements"], 2);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "alpha\r\nchanged\r\nalpha\r\nchanged\r\n"
        );
    }

    #[test]
    fn edit_prefers_exact_lf_match_in_mixed_line_ending_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("note.txt");
        fs::write(&path, "alpha\nmiddle\nbeta\r\nomega\r\n").unwrap();
        edit_file(
            root.path(),
            EditArgs {
                path: "note.txt".to_string(),
                old_text: "alpha\nmiddle\nbeta".to_string(),
                new_text: "alpha\nmiddle-new\nbeta".to_string(),
                replace_all: false,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "alpha\nmiddle-new\nbeta\r\nomega\r\n"
        );
    }

    #[test]
    fn a_nonzero_shell_exit_is_visible_as_a_failed_tool_result() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            name: "shell".to_string(),
            arguments: json!({"command": "exit 7"}),
        };
        let result = execute(&call, root.path());
        assert!(!result.ok);
        assert!(result.content.contains("\"success\":false"));
    }

    #[test]
    fn shell_truncates_high_output_and_keeps_the_head_and_tail() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let result = run_shell_with_limits(
            root.path(),
            ShellArgs {
                command: high_output_command().to_string(),
                timeout_ms: None,
            },
            ProcessLimits::truncating(Duration::from_secs(2), 128, 128),
        )
        .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["truncated"], true);
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("HEAD"), "{stdout}");
        assert!(stdout.ends_with("TAIL"), "{stdout}");
        assert!(stdout.contains("output truncated"), "{stdout}");
    }

    #[test]
    fn shell_timeout_returns_a_failed_result_promptly() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = run_shell_with_limits(
            root.path(),
            ShellArgs {
                command: sleeping_command().to_string(),
                timeout_ms: None,
            },
            ProcessLimits::truncating(Duration::from_millis(750), 128, 128),
        )
        .unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["stdout"], "before-timeout");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn shell_timeout_is_bounded_when_a_child_inherits_the_pipes() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let outcome = run_shell_with_limits(
            root.path(),
            ShellArgs {
                command: descendant_command().to_string(),
                timeout_ms: None,
            },
            ProcessLimits::truncating(Duration::from_millis(500), 128, 128),
        );
        let child_pid = fs::read_to_string(root.path().join("shell-child-started"))
            .unwrap_or_else(|error| panic!("{error}; shell outcome: {outcome:?}"))
            .parse::<u32>()
            .unwrap();
        let fixture = FixtureProcess::new(child_pid);
        let result = outcome.unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["timed_out"], true);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            fixture.wait_for_exit(Duration::from_secs(4)),
            "fixture child {child_pid} survived"
        );
    }

    #[test]
    fn shell_cancellation_stops_a_silent_process_tree_within_250_ms() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("shell-child-started");
        let cancellation = TurnCancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker_marker = marker.clone();
        let canceller = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            while !worker_marker.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let process_started = worker_marker.exists();
            worker_cancellation.cancel();
            (process_started, Instant::now())
        });

        let outcome = run_shell_with_limits_cancellable(
            root.path(),
            ShellArgs {
                command: descendant_command().to_string(),
                timeout_ms: None,
            },
            ProcessLimits::truncating(Duration::from_secs(30), 128, 128),
            &cancellation,
        );
        let (process_started, cancelled_at) = canceller.join().unwrap();
        let latency = cancelled_at.elapsed();
        let error = outcome.unwrap_err();

        assert!(process_started, "silent fixture process did not start");
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            latency < Duration::from_millis(250),
            "shell cancellation took {latency:?}"
        );
        let child_pid = fs::read_to_string(&marker).unwrap().parse::<u32>().unwrap();
        let fixture = FixtureProcess::new(child_pid);
        assert!(
            fixture.wait_for_exit(Duration::from_secs(4)),
            "fixture child {child_pid} survived cancellation"
        );
    }

    #[test]
    fn shell_timeout_is_bounded_after_the_parent_exits_with_inherited_pipes() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let outcome = run_shell_with_limits(
            root.path(),
            ShellArgs {
                command: exited_parent_command().to_string(),
                timeout_ms: None,
            },
            ProcessLimits::truncating(Duration::from_millis(1_000), 128, 128),
        );
        let child_pid = fs::read_to_string(root.path().join("shell-child-started"))
            .unwrap_or_else(|error| panic!("{error}; shell outcome: {outcome:?}"))
            .parse::<u32>()
            .unwrap();
        let fixture = FixtureProcess::new(child_pid);
        let result = outcome.unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["timed_out"], true);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            fixture.wait_for_exit(Duration::from_secs(4)),
            "fixture child {child_pid} survived"
        );
    }

    #[test]
    fn completed_shell_leaves_a_detached_child_running() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let outcome = run_shell_with_limits(
            root.path(),
            ShellArgs {
                command: detached_descendant_command().to_string(),
                timeout_ms: None,
            },
            ProcessLimits::truncating(Duration::from_secs(3), 128, 128),
        );
        let pid_path = root.path().join("detached-child-pid");
        let pid_deadline = Instant::now() + Duration::from_secs(4);
        while !pid_path.exists() && Instant::now() < pid_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let child_pid = fs::read_to_string(&pid_path)
            .unwrap_or_else(|error| panic!("{error}; shell outcome: {outcome:?}"))
            .parse::<u32>()
            .unwrap();
        let fixture = FixtureProcess::new(child_pid);
        let result = outcome.unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["timed_out"], false, "{result}");

        fs::write(root.path().join("detached-child-release"), b"go").unwrap();
        let survived = root.path().join("detached-child-survived");
        let deadline = Instant::now() + Duration::from_secs(4);
        while !survived.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            survived.exists(),
            "detached fixture child {child_pid} was killed after its parent completed"
        );
        assert!(
            fixture.wait_for_exit(Duration::from_secs(4)),
            "detached fixture child {child_pid} did not stop on its own"
        );
    }

    #[cfg(windows)]
    fn high_output_command() -> &'static str {
        "[Console]::Out.Write('HEAD' + ('x' * 4096) + 'TAIL')"
    }

    #[cfg(unix)]
    fn high_output_command() -> &'static str {
        "printf HEAD; i=0; while [ $i -lt 256 ]; do printf '%s' 'xxxxxxxxxxxxxxxx'; i=$((i + 1)); done; printf TAIL"
    }

    #[cfg(windows)]
    fn sleeping_command() -> &'static str {
        "[Console]::Out.Write('before-timeout'); Start-Sleep -Seconds 30"
    }

    #[cfg(unix)]
    fn sleeping_command() -> &'static str {
        "printf before-timeout; /bin/sleep 30"
    }

    #[cfg(windows)]
    fn descendant_command() -> &'static str {
        "[IO.File]::WriteAllText('shell-child.ps1', 'Start-Sleep -Seconds 30'); $child = Start-Process powershell.exe -ArgumentList '-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File','shell-child.ps1' -NoNewWindow -PassThru; [IO.File]::WriteAllText('shell-child-started', $child.Id); $child.WaitForExit()"
    }

    #[cfg(unix)]
    fn descendant_command() -> &'static str {
        "/bin/sleep 30 & child=$!; printf '%s' \"$child\" > shell-child-started; wait \"$child\""
    }

    #[cfg(windows)]
    fn exited_parent_command() -> &'static str {
        "[IO.File]::WriteAllText('shell-child.ps1', \"[IO.File]::WriteAllText('shell-child-started', `$PID); Start-Sleep -Seconds 30\"); & cmd.exe /d /c 'start \"\" /b powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File shell-child.ps1'; exit 0"
    }

    #[cfg(unix)]
    fn exited_parent_command() -> &'static str {
        "/bin/sleep 30 & child=$!; printf '%s' \"$child\" > shell-child-started; exit 0"
    }

    #[cfg(windows)]
    fn detached_descendant_command() -> &'static str {
        "[IO.File]::WriteAllText('detached-child.ps1', \"[IO.File]::WriteAllText((Join-Path `$PSScriptRoot 'detached-child-pid'), `$PID); while (-not (Test-Path (Join-Path `$PSScriptRoot 'detached-child-release'))) { Start-Sleep -Milliseconds 50 }; [IO.File]::WriteAllText((Join-Path `$PSScriptRoot 'detached-child-survived'), 'yes'); Start-Sleep -Milliseconds 500\"); Start-Process powershell.exe -ArgumentList '-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File','detached-child.ps1' -WorkingDirectory (Get-Location) -WindowStyle Hidden; exit 0"
    }

    #[cfg(unix)]
    fn detached_descendant_command() -> &'static str {
        "(while [ ! -f detached-child-release ]; do sleep 0.05; done; printf yes > detached-child-survived; sleep 0.5) </dev/null >/dev/null 2>&1 & child=$!; printf '%s' \"$child\" > detached-child-pid; exit 0"
    }

    #[test]
    fn plugin_detail_prefers_a_useful_argument() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("plugin.json"),
            r#"{
                "format":"agul/plugin/v2",
                "name":"search",
                "version":"1.0.0",
                "command":["not-run"],
                "tools":[{
                    "name":"web_search",
                    "description":"Search the web",
                    "parameters":{"type":"object"}
                }]
            }"#,
        )
        .unwrap();
        let plugins = super::super::plugin::discover(Some(root.path())).unwrap();
        let tools = ToolSet::new(&plugins.tools).unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            name: "web_search".to_string(),
            arguments: json!({"query": "Agul plugin protocol"}),
        };

        assert_eq!(tools.describe_call(&call), "Agul plugin protocol");

        let open = ToolCall {
            id: "2".to_string(),
            name: "web_search".to_string(),
            arguments: json!({"url": "https://example.com/source"}),
        };
        assert_eq!(tools.describe_call(&open), "https://example.com/source");
    }
}
