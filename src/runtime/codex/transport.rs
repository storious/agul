use std::collections::VecDeque;
use std::fmt;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::path::Path;
#[cfg(windows)]
use std::time::SystemTime;

use serde_json::{Value, json};

const STDERR_TAIL_LINES: usize = 8;
const STDERR_LINE_CHARS: usize = 240;

type StderrTail = Arc<Mutex<VecDeque<String>>>;

pub(crate) struct AppServer {
    child: Child,
    rpc: Option<Rpc<ChannelReader, BufWriter<ChildStdin>>>,
    stderr_tail: StderrTail,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl AppServer {
    pub(crate) fn start(command: Option<&str>) -> Result<Self, CodexError> {
        Self::start_inner(command, None, None)
    }

    pub(super) fn start_timeout(
        command: Option<&str>,
        reasoning_effort: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, CodexError> {
        Self::start_inner(command, Some(timeout), reasoning_effort)
    }

    fn start_inner(
        command: Option<&str>,
        timeout: Option<Duration>,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, CodexError> {
        let program = command
            .map(PathBuf::from)
            .unwrap_or_else(default_codex_command);
        let mut process = Command::new(&program);
        process.arg("app-server");
        if let Some(effort) = reasoning_effort.filter(|effort| !effort.trim().is_empty()) {
            process
                .arg("-c")
                .arg(format!("model_reasoning_effort={effort:?}"));
        }
        let mut child = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                CodexError::new(format!(
                    "could not start Codex app-server with {program:?}: {error}"
                ))
            })?;
        let stdin = take_pipe(&mut child, |child| child.stdin.take(), "stdin")?;
        let stdout = take_pipe(&mut child, |child| child.stdout.take(), "stdout")?;
        let stderr = take_pipe(&mut child, |child| child.stderr.take(), "stderr")?;

        let (reader, stdout_thread) = channel_reader(stdout);
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_thread = drain_stderr(stderr, Arc::clone(&stderr_tail));
        let mut rpc = Rpc::with_reader(reader, BufWriter::new(stdin));
        let initialized = rpc.call_with_timeout(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "agul",
                    "title": "Agul",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            timeout,
        );
        if let Err(error) = initialized {
            drop(rpc);
            stop_child(&mut child);
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(with_stderr(error, &stderr_tail));
        }
        if let Err(error) = rpc.notify("initialized", json!({})) {
            drop(rpc);
            stop_child(&mut child);
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(with_stderr(error, &stderr_tail));
        }
        Ok(Self {
            child,
            rpc: Some(rpc),
            stderr_tail,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        })
    }

    pub(super) fn call(&mut self, method: &str, params: Value) -> Result<Value, CodexError> {
        let result = self.rpc_mut().call(method, params);
        result.map_err(|error| with_stderr(error, &self.stderr_tail))
    }

    pub(super) fn call_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CodexError> {
        let result = self
            .rpc_mut()
            .call_with_timeout(method, params, Some(timeout));
        result.map_err(|error| with_stderr(error, &self.stderr_tail))
    }

    pub(super) fn next_message(&mut self) -> Result<Value, CodexError> {
        let result = self.rpc_mut().next_message();
        result.map_err(|error| with_stderr(error, &self.stderr_tail))
    }

    pub(super) fn next_message_timeout(&mut self, timeout: Duration) -> Result<Value, CodexError> {
        let result = self.rpc_mut().next_message_with_timeout(Some(timeout));
        result.map_err(|error| with_stderr(error, &self.stderr_tail))
    }

    fn rpc_mut(&mut self) -> &mut Rpc<ChannelReader, BufWriter<ChildStdin>> {
        self.rpc.as_mut().expect("Codex app-server is running")
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        // Closing stdin first lets app-server and any inherited pipe readers
        // terminate. Reader threads are detached so shutdown cannot block on a
        // descendant that kept stdout or stderr open.
        drop(self.rpc.take());
        stop_child(&mut self.child);
        drop(self.stdout_thread.take());
        drop(self.stderr_thread.take());
    }
}

fn take_pipe<T>(
    child: &mut Child,
    take: impl FnOnce(&mut Child) -> Option<T>,
    name: &str,
) -> Result<T, CodexError> {
    take(child).ok_or_else(|| {
        stop_child(child);
        CodexError::new(format!("Codex app-server {name} is unavailable"))
    })
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn default_codex_command() -> PathBuf {
    #[cfg(windows)]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
        && let Some(command) = desktop_codex_command(Path::new(&local_app_data))
    {
        return command;
    }
    PathBuf::from(if cfg!(windows) { "codex.cmd" } else { "codex" })
}

#[cfg(windows)]
fn desktop_codex_command(local_app_data: &Path) -> Option<PathBuf> {
    std::fs::read_dir(local_app_data.join("OpenAI/Codex/bin"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("codex.exe"))
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            (
                path.metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                path.clone(),
            )
        })
}

fn channel_reader(stdout: ChildStdout) -> (ChannelReader, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let thread = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(Err(CodexError::new(
                        "Codex app-server closed the connection",
                    )));
                    break;
                }
                Ok(_) if line.trim().is_empty() => {}
                Ok(_) => match serde_json::from_str(&line) {
                    Ok(message) => {
                        if sender.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(CodexError::new(format!(
                            "Codex app-server returned invalid JSON: {error}"
                        ))));
                        break;
                    }
                },
                Err(error) => {
                    let _ = sender.send(Err(CodexError::new(format!(
                        "could not read from Codex app-server: {error}"
                    ))));
                    break;
                }
            }
        }
    });
    (ChannelReader { receiver }, thread)
}

fn drain_stderr(stderr: ChildStderr, tail: StderrTail) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = truncate(&line, STDERR_LINE_CHARS);
            if line.trim().is_empty() {
                continue;
            }
            let Ok(mut tail) = tail.lock() else {
                break;
            };
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    })
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let head = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn with_stderr(error: CodexError, tail: &StderrTail) -> CodexError {
    let detail = tail
        .lock()
        .ok()
        .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join(" | "))
        .filter(|detail| !detail.is_empty());
    match detail {
        Some(detail) => error.with_detail(format_args!("Codex: {detail}")),
        None => error,
    }
}

trait MessageReader {
    fn read_message(&mut self, timeout: Option<Duration>) -> Result<Value, CodexError>;
}

#[cfg(test)]
struct BlockingReader<R> {
    inner: R,
}

#[cfg(test)]
impl<R: BufRead> MessageReader for BlockingReader<R> {
    fn read_message(&mut self, _timeout: Option<Duration>) -> Result<Value, CodexError> {
        loop {
            let mut line = String::new();
            let read = self.inner.read_line(&mut line).map_err(|error| {
                CodexError::new(format!("could not read from Codex app-server: {error}"))
            })?;
            if read == 0 {
                return Err(CodexError::new("Codex app-server closed the connection"));
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line).map_err(|error| {
                CodexError::new(format!("Codex app-server returned invalid JSON: {error}"))
            });
        }
    }
}

struct ChannelReader {
    receiver: Receiver<Result<Value, CodexError>>,
}

impl MessageReader for ChannelReader {
    fn read_message(&mut self, timeout: Option<Duration>) -> Result<Value, CodexError> {
        match timeout {
            Some(timeout) => self
                .receiver
                .recv_timeout(timeout)
                .map_err(|error| match error {
                    RecvTimeoutError::Timeout => {
                        CodexError::timeout("timed out waiting for Codex app-server")
                    }
                    RecvTimeoutError::Disconnected => {
                        CodexError::new("Codex app-server output reader stopped")
                    }
                })?,
            None => self
                .receiver
                .recv()
                .map_err(|_| CodexError::new("Codex app-server output reader stopped"))?,
        }
    }
}

struct Rpc<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
    pending: VecDeque<Value>,
}

#[cfg(test)]
impl<R: BufRead, W: Write> Rpc<BlockingReader<R>, W> {
    fn new(reader: R, writer: W) -> Self {
        Self::with_reader(BlockingReader { inner: reader }, writer)
    }
}

impl<R: MessageReader, W: Write> Rpc<R, W> {
    fn with_reader(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, CodexError> {
        self.call_with_timeout(method, params, None)
    }

    fn call_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, CodexError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"method": method, "id": id, "params": params}))?;
        let deadline = deadline(timeout)?;
        loop {
            let message = self.reader.read_message(remaining(deadline)?)?;
            if self.reject_server_request(&message)? {
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                self.pending.push_back(message);
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string());
                return Err(CodexError::new(format!("{method}: {detail}")));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| CodexError::new(format!("{method}: response has no result")));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), CodexError> {
        self.send(&json!({"method": method, "params": params}))
    }

    fn next_message(&mut self) -> Result<Value, CodexError> {
        self.next_message_with_timeout(None)
    }

    fn next_message_with_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Value, CodexError> {
        let deadline = deadline(timeout)?;
        loop {
            let message = if let Some(message) = self.pending.pop_front() {
                message
            } else {
                self.reader.read_message(remaining(deadline)?)?
            };
            if self.reject_server_request(&message)? {
                continue;
            }
            return Ok(message);
        }
    }

    fn reject_server_request(&mut self, message: &Value) -> Result<bool, CodexError> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        let Some(id) = message.get("id") else {
            return Ok(false);
        };
        self.send(&json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Agul does not support server request: {method}")
            }
        }))?;
        Ok(true)
    }

    fn send(&mut self, value: &Value) -> Result<(), CodexError> {
        serde_json::to_writer(&mut self.writer, value)
            .map_err(|error| CodexError::new(format!("could not encode Codex request: {error}")))?;
        self.writer
            .write_all(b"\n")
            .and_then(|_| self.writer.flush())
            .map_err(|error| {
                CodexError::new(format!("could not write to Codex app-server: {error}"))
            })
    }
}

fn remaining(deadline: Option<Instant>) -> Result<Option<Duration>, CodexError> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(CodexError::timeout(
            "timed out waiting for Codex app-server",
        ))
    } else {
        Ok(Some(remaining))
    }
}

fn deadline(timeout: Option<Duration>) -> Result<Option<Instant>, CodexError> {
    timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| CodexError::new("Codex timeout is too large"))
        })
        .transpose()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexErrorKind {
    Other,
    Timeout,
}

#[derive(Debug)]
pub(crate) struct CodexError {
    message: String,
    kind: CodexErrorKind,
}

impl CodexError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: CodexErrorKind::Other,
        }
    }

    pub(super) fn timeout(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: CodexErrorKind::Timeout,
        }
    }

    pub(super) fn is_timeout(&self) -> bool {
        self.kind == CodexErrorKind::Timeout
    }

    fn with_detail(self, detail: impl fmt::Display) -> Self {
        Self {
            message: format!("{} · {detail}", self.message),
            kind: self.kind,
        }
    }
}

impl fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexError {}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[cfg(windows)]
    #[test]
    fn desktop_command_discovery_uses_the_latest_bundled_runtime() {
        let local = tempfile::tempdir().unwrap();
        let older = local.path().join("OpenAI/Codex/bin/a/codex.exe");
        let newer = local.path().join("OpenAI/Codex/bin/z/codex.exe");
        std::fs::create_dir_all(older.parent().unwrap()).unwrap();
        std::fs::create_dir_all(newer.parent().unwrap()).unwrap();
        std::fs::write(&older, b"older").unwrap();
        std::fs::write(&newer, b"newer").unwrap();

        assert_eq!(desktop_codex_command(local.path()), Some(newer));
    }

    #[test]
    fn rpc_keeps_notifications_that_arrive_before_a_response() {
        let input = concat!(
            "{\"method\":\"account/updated\",\"params\":{\"authMode\":\"chatgpt\"}}\n",
            "{\"id\":1,\"result\":{\"account\":{\"type\":\"chatgpt\"}}}\n"
        );
        let mut rpc = Rpc::new(Cursor::new(input.as_bytes()), Vec::new());

        let account = rpc.call("account/read", json!({})).unwrap();

        assert_eq!(account["account"]["type"], "chatgpt");
        assert_eq!(rpc.next_message().unwrap()["method"], "account/updated");
        assert_eq!(
            String::from_utf8(rpc.writer).unwrap(),
            "{\"id\":1,\"method\":\"account/read\",\"params\":{}}\n"
        );
    }

    #[test]
    fn rpc_surfaces_server_error_messages() {
        let input = "{\"id\":1,\"error\":{\"code\":-1,\"message\":\"not logged in\"}}\n";
        let mut rpc = Rpc::new(Cursor::new(input.as_bytes()), Vec::new());

        let error = rpc.call("account/usage/read", json!({})).unwrap_err();

        assert_eq!(error.to_string(), "account/usage/read: not logged in");
    }

    #[test]
    fn rpc_rejects_server_requests_and_keeps_the_stream_in_sync() {
        let input = concat!(
            "{\"id\":77,\"method\":\"item/tool/requestUserInput\",\"params\":{}}\n",
            "{\"id\":78,\"method\":\"item/commandExecution/requestApproval\",\"params\":{}}\n",
            "{\"id\":1,\"result\":{}}\n"
        );
        let mut rpc = Rpc::new(Cursor::new(input.as_bytes()), Vec::new());

        let response = rpc.call("turn/start", json!({})).unwrap();

        assert_eq!(response, json!({}));
        assert_eq!(
            String::from_utf8(rpc.writer).unwrap(),
            concat!(
                "{\"id\":1,\"method\":\"turn/start\",\"params\":{}}\n",
                "{\"error\":{\"code\":-32601,\"message\":\"Agul does not support server request: item/tool/requestUserInput\"},\"id\":77}\n",
                "{\"error\":{\"code\":-32601,\"message\":\"Agul does not support server request: item/commandExecution/requestApproval\"},\"id\":78}\n"
            )
        );
    }

    #[test]
    fn channel_reader_enforces_a_real_timeout() {
        let (_sender, receiver) = mpsc::channel();
        let mut rpc = Rpc::with_reader(ChannelReader { receiver }, Vec::new());

        let error = rpc
            .call_with_timeout("turn/start", json!({}), Some(Duration::from_millis(1)))
            .unwrap_err();

        assert_eq!(error.to_string(), "timed out waiting for Codex app-server");
        assert!(error.is_timeout());
    }

    #[test]
    fn rpc_interrupt_drains_queued_turn_messages_before_the_next_turn() {
        let input = concat!(
            "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",\"turn\":{\"id\":\"turn-1\",\"status\":\"interrupted\"}}}\n",
            "{\"id\":1,\"result\":{}}\n",
            "{\"id\":2,\"result\":{\"turn\":{\"id\":\"turn-2\"}}}\n"
        );
        let mut rpc = Rpc::new(Cursor::new(input.as_bytes()), Vec::new());

        rpc.call(
            "turn/interrupt",
            json!({"threadId": "thread-1", "turnId": "turn-1"}),
        )
        .unwrap();
        assert_eq!(
            rpc.next_message().unwrap()["params"]["turn"]["status"],
            "interrupted"
        );
        assert_eq!(
            rpc.call("turn/start", json!({"threadId": "thread-1"}))
                .unwrap()["turn"]["id"],
            "turn-2"
        );
        assert_eq!(
            String::from_utf8(rpc.writer).unwrap(),
            concat!(
                "{\"id\":1,\"method\":\"turn/interrupt\",\"params\":{\"threadId\":\"thread-1\",\"turnId\":\"turn-1\"}}\n",
                "{\"id\":2,\"method\":\"turn/start\",\"params\":{\"threadId\":\"thread-1\"}}\n"
            )
        );
    }

    #[test]
    fn server_errors_that_mention_timeout_are_not_transport_timeouts() {
        let input = "{\"id\":1,\"error\":{\"message\":\"operation timed out upstream\"}}\n";
        let mut rpc = Rpc::new(Cursor::new(input.as_bytes()), Vec::new());

        let error = rpc
            .call_with_timeout("turn/start", json!({}), Some(Duration::from_secs(1)))
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "turn/start: operation timed out upstream"
        );
        assert!(!error.is_timeout());
    }

    #[test]
    fn stderr_tail_is_short_and_only_attached_to_errors() {
        let tail = Arc::new(Mutex::new(VecDeque::from([
            "first warning".to_string(),
            "second warning".to_string(),
        ])));

        assert_eq!(
            with_stderr(CodexError::new("failed"), &tail).to_string(),
            "failed · Codex: first warning | second warning"
        );
    }
}
