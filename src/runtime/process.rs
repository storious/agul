use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::TurnCancellation;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::Stdio;

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_millis(100);
const WORKER_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(unix)]
const TERMINATOR_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputLimit {
    Error,
    HeadTail,
}

#[derive(Clone, Copy)]
pub(super) struct ProcessLimits {
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
    output_limit: OutputLimit,
}

impl ProcessLimits {
    pub(super) const fn new(timeout: Duration, stdout_bytes: usize, stderr_bytes: usize) -> Self {
        Self {
            timeout,
            stdout_bytes,
            stderr_bytes,
            output_limit: OutputLimit::Error,
        }
    }

    pub(super) const fn truncating(
        timeout: Duration,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Self {
        Self {
            timeout,
            stdout_bytes,
            stderr_bytes,
            output_limit: OutputLimit::HeadTail,
        }
    }

    pub(super) const fn timeout(self) -> Duration {
        self.timeout
    }
}

pub(super) struct ProcessOutput {
    pub(super) status: Option<ExitStatus>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) stdout_truncated: bool,
    pub(super) stderr_truncated: bool,
    pub(super) timed_out: bool,
}

enum ProcessEvent {
    Stdin(Result<(), String>),
    StdoutChunk(Vec<u8>),
    StdoutDone(Result<(), String>),
    Stderr(Result<(), String>),
}

enum Completion {
    Exited,
    TimedOut,
    Cancelled,
    Error(String),
}

pub(super) struct ProcessTree {
    #[cfg(unix)]
    process_group: Option<u32>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessTree {
    pub(super) fn prepare(_command: &mut Command) -> Result<Self, String> {
        #[cfg(unix)]
        {
            _command.process_group(0);
            Ok(Self {
                process_group: None,
            })
        }
        #[cfg(windows)]
        {
            windows_job::create().map(|job| Self { job })
        }
    }

    pub(super) fn assign(&mut self, child: &mut Child) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.process_group = Some(child.id());
            Ok(())
        }
        #[cfg(windows)]
        {
            if let Err(error) = windows_job::assign(&self.job, child) {
                bounded_kill_and_reap(child, Instant::now() + PROCESS_STOP_TIMEOUT);
                return Err(error);
            }
            Ok(())
        }
    }

    fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.process_group {
            let mut command = Command::new("kill");
            command.args(["-KILL", "--", &format!("-{pid}")]);
            run_terminator(command, TERMINATOR_TIMEOUT);
        }
        #[cfg(windows)]
        windows_job::terminate(&self.job);
    }

    fn preserve_descendants(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            Ok(())
        }
        #[cfg(windows)]
        {
            windows_job::set_kill_on_close(&self.job, false)
        }
    }
}

pub(super) fn run_process(
    child: &mut Child,
    tree: ProcessTree,
    input: Option<Vec<u8>>,
    limits: ProcessLimits,
) -> Result<ProcessOutput, String> {
    run_process_streaming(child, tree, input, limits, &mut |_| Ok(()))
}

pub(super) fn run_process_cancellable(
    child: &mut Child,
    tree: ProcessTree,
    input: Option<Vec<u8>>,
    limits: ProcessLimits,
    cancellation: &TurnCancellation,
) -> Result<ProcessOutput, String> {
    run_process_streaming_cancellable(child, tree, input, limits, cancellation, &mut |_| Ok(()))
}

pub(super) fn run_process_streaming(
    child: &mut Child,
    tree: ProcessTree,
    input: Option<Vec<u8>>,
    limits: ProcessLimits,
    on_stdout: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<ProcessOutput, String> {
    run_process_streaming_inner(child, tree, input, limits, None, on_stdout)
}

pub(super) fn run_process_streaming_cancellable(
    child: &mut Child,
    tree: ProcessTree,
    input: Option<Vec<u8>>,
    limits: ProcessLimits,
    cancellation: &TurnCancellation,
    on_stdout: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<ProcessOutput, String> {
    run_process_streaming_inner(child, tree, input, limits, Some(cancellation), on_stdout)
}

fn run_process_streaming_inner(
    child: &mut Child,
    tree: ProcessTree,
    input: Option<Vec<u8>>,
    limits: ProcessLimits,
    cancellation: Option<&TurnCancellation>,
    on_stdout: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<ProcessOutput, String> {
    let stdin = match input {
        Some(input) => Some((
            child.stdin.take().ok_or_else(|| {
                stop_tree_and_reap(child, &tree);
                "stdin was unavailable".to_string()
            })?,
            input,
        )),
        None => None,
    };
    let stdout = child.stdout.take().ok_or_else(|| {
        stop_tree_and_reap(child, &tree);
        "stdout was unavailable".to_string()
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        stop_tree_and_reap(child, &tree);
        "stderr was unavailable".to_string()
    })?;

    let mut stdout_capture = OutputCapture::new(limits.stdout_bytes, limits.output_limit);
    let stderr_capture = Arc::new(Mutex::new(OutputCapture::new(
        limits.stderr_bytes,
        limits.output_limit,
    )));
    let (sender, receiver) = mpsc::sync_channel(16);
    let mut workers = Vec::with_capacity(3);
    let mut stdin_done = stdin.is_none();
    if let Some((stdin, input)) = stdin {
        let stdin_sender = sender.clone();
        workers.push(thread::spawn(move || {
            let mut stdin = stdin;
            let result = stdin
                .write_all(&input)
                .map_err(|error| format!("could not write stdin: {error}"));
            drop(stdin);
            let _ = stdin_sender.send(ProcessEvent::Stdin(result));
        }));
    }
    let stdout_sender = sender.clone();
    workers.push(thread::spawn(move || {
        capture_stdout(stdout, &stdout_sender);
    }));
    let stderr_worker_capture = Arc::clone(&stderr_capture);
    workers.push(thread::spawn(move || {
        let result = capture_output(stderr, &stderr_worker_capture, "stderr");
        let _ = sender.send(ProcessEvent::Stderr(result));
    }));

    let started = Instant::now();
    let mut status = None;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let completion = loop {
        if cancellation.is_some_and(TurnCancellation::is_cancelled) {
            break Completion::Cancelled;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => {
                    break Completion::Error(format!("could not query process: {error}"));
                }
            }
        }
        if stdin_done && stdout_done && stderr_done && status.is_some() {
            break Completion::Exited;
        }
        if started.elapsed() >= limits.timeout {
            break Completion::TimedOut;
        }

        let remaining = limits.timeout.saturating_sub(started.elapsed());
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(ProcessEvent::Stdin(Ok(()))) => stdin_done = true,
            Ok(ProcessEvent::Stdin(Err(error))) => break Completion::Error(error),
            Ok(ProcessEvent::StdoutChunk(bytes)) => {
                if cancellation.is_some_and(TurnCancellation::is_cancelled) {
                    break Completion::Cancelled;
                }
                let streamed = stdout_capture
                    .push(&bytes, "stdout")
                    .and_then(|()| on_stdout(&bytes));
                if cancellation.is_some_and(TurnCancellation::is_cancelled) {
                    break Completion::Cancelled;
                }
                if let Err(error) = streamed {
                    break Completion::Error(error);
                }
            }
            Ok(ProcessEvent::StdoutDone(Ok(()))) => stdout_done = true,
            Ok(ProcessEvent::StdoutDone(Err(error))) => break Completion::Error(error),
            Ok(ProcessEvent::Stderr(Ok(()))) => stderr_done = true,
            Ok(ProcessEvent::Stderr(Err(error))) => break Completion::Error(error),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if stdin_done && stdout_done && stderr_done {
                    thread::sleep(remaining.min(POLL_INTERVAL));
                } else {
                    break Completion::Error("worker ended without reporting a result".to_string());
                }
            }
        }
    };

    match &completion {
        Completion::Exited => {
            join_workers(workers);
            tree.preserve_descendants()?;
        }
        Completion::TimedOut | Completion::Cancelled | Completion::Error(_) => {
            stop_tree_and_reap(child, &tree);
            drain_workers(workers, WORKER_DRAIN_TIMEOUT);
        }
    }

    if matches!(completion, Completion::Cancelled) {
        return Err("cancelled".to_string());
    }
    if let Completion::Error(error) = completion {
        return Err(error);
    }
    let timed_out = matches!(completion, Completion::TimedOut);
    let stdout = stdout_capture.snapshot();
    let stderr = snapshot(&stderr_capture)?;
    Ok(ProcessOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        timed_out,
    })
}

fn capture_stdout(mut reader: impl Read, sender: &mpsc::SyncSender<ProcessEvent>) {
    let mut buffer = [0_u8; 8 * 1024];
    let result = loop {
        match reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(count) => {
                if sender
                    .send(ProcessEvent::StdoutChunk(buffer[..count].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => break Err(format!("could not read stdout: {error}")),
        }
    };
    let _ = sender.send(ProcessEvent::StdoutDone(result));
}

fn capture_output(
    mut reader: impl Read,
    capture: &Mutex<OutputCapture>,
    stream: &str,
) -> Result<(), String> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not read {stream}: {error}"))?;
        if count == 0 {
            return Ok(());
        }
        capture
            .lock()
            .map_err(|_| format!("could not retain {stream}"))?
            .push(&buffer[..count], stream)?;
    }
}

struct OutputCapture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    limit: usize,
    head_limit: usize,
    mode: OutputLimit,
    total: usize,
}

impl OutputCapture {
    fn new(limit: usize, mode: OutputLimit) -> Self {
        let head_limit = match mode {
            OutputLimit::Error => limit,
            OutputLimit::HeadTail => limit.saturating_sub(limit / 3),
        };
        Self {
            head: Vec::with_capacity(head_limit.min(8 * 1024)),
            tail: VecDeque::with_capacity(limit.saturating_sub(head_limit).min(8 * 1024)),
            limit,
            head_limit,
            mode,
            total: 0,
        }
    }

    fn push(&mut self, bytes: &[u8], stream: &str) -> Result<(), String> {
        self.total = self.total.saturating_add(bytes.len());
        if self.mode == OutputLimit::Error {
            let available = self.limit.saturating_sub(self.head.len());
            self.head
                .extend_from_slice(&bytes[..bytes.len().min(available)]);
            return if bytes.len() > available {
                Err(format!("{stream} exceeded {} bytes", self.limit))
            } else {
                Ok(())
            };
        }

        let head_available = self.head_limit.saturating_sub(self.head.len());
        let head_count = bytes.len().min(head_available);
        self.head.extend_from_slice(&bytes[..head_count]);
        self.push_tail(&bytes[head_count..]);
        Ok(())
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        let tail_limit = self.limit.saturating_sub(self.head_limit);
        if tail_limit == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= tail_limit {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - tail_limit..]);
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(tail_limit);
        self.tail.drain(..overflow);
        self.tail.extend(bytes);
    }

    fn snapshot(&self) -> CapturedOutput {
        let truncated = self.total > self.limit;
        const MARKER: &[u8] = b"\n... output truncated ...\n";
        let marker_bytes = usize::from(truncated) * MARKER.len();
        let mut bytes = Vec::with_capacity(self.head.len() + marker_bytes + self.tail.len());
        bytes.extend_from_slice(&self.head);
        if truncated {
            bytes.extend_from_slice(MARKER);
        }
        bytes.extend(self.tail.iter().copied());
        CapturedOutput { bytes, truncated }
    }
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn snapshot(capture: &Mutex<OutputCapture>) -> Result<CapturedOutput, String> {
    capture
        .lock()
        .map_err(|_| "could not retain process output".to_string())
        .map(|capture| capture.snapshot())
}

fn stop_tree_and_reap(child: &mut Child, tree: &ProcessTree) {
    tree.terminate();
    bounded_kill_and_reap(child, Instant::now() + PROCESS_STOP_TIMEOUT);
}

fn bounded_kill_and_reap(child: &mut Child, deadline: Instant) {
    let _ = child.kill();
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(POLL_INTERVAL),
        }
    }
}

#[cfg(test)]
fn bounded_wait_or_kill(child: &mut Child, deadline: Instant) {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(POLL_INTERVAL),
        }
    }
    bounded_kill_and_reap(child, Instant::now() + PROCESS_STOP_TIMEOUT);
}

#[cfg(unix)]
fn run_terminator(mut command: Command, timeout: Duration) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut helper) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match helper.try_wait() {
            Ok(Some(_)) | Err(_) => return false,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                bounded_kill_and_reap(&mut helper, Instant::now() + PROCESS_STOP_TIMEOUT);
                return true;
            }
        }
    }
}

fn drain_workers(workers: Vec<JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while workers.iter().any(|worker| !worker.is_finished()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    for worker in workers {
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
}

fn join_workers(workers: Vec<JoinHandle<()>>) {
    for worker in workers {
        let _ = worker.join();
    }
}

#[cfg(windows)]
mod windows_job {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;
    use std::ptr::null;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub(super) fn create() -> Result<OwnedHandle, String> {
        let raw_job = unsafe { CreateJobObjectW(null(), null()) };
        if raw_job.is_null() {
            return Err(format!(
                "could not create Windows job object: {}",
                std::io::Error::last_os_error()
            ));
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        set_kill_on_close(&job, true)?;
        Ok(job)
    }

    pub(super) fn set_kill_on_close(job: &OwnedHandle, enabled: bool) -> Result<(), String> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if enabled {
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let action = if enabled { "enable" } else { "disable" };
            return Err(format!(
                "could not {action} Windows job kill-on-close: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub(super) fn assign(job: &OwnedHandle, child: &Child) -> Result<(), String> {
        let assigned = unsafe {
            AssignProcessToJobObject(
                job.as_raw_handle() as HANDLE,
                child.as_raw_handle() as HANDLE,
            )
        };
        if assigned == 0 {
            Err(format!(
                "could not assign process to Windows job object: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn terminate(job: &OwnedHandle) {
        unsafe {
            TerminateJobObject(job.as_raw_handle() as HANDLE, 1);
        }
    }
}

#[cfg(test)]
pub(crate) struct FixtureProcess(u32);

#[cfg(test)]
static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn process_test_lock() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
impl FixtureProcess {
    pub(crate) fn new(pid: u32) -> Self {
        Self(pid)
    }

    pub(crate) fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while fixture_process_is_running(self.0) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        !fixture_process_is_running(self.0)
    }
}

#[cfg(test)]
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if !fixture_process_is_running(self.0) {
            return;
        }
        #[cfg(windows)]
        let mut cleanup = {
            let mut command = Command::new("taskkill");
            command.args(["/PID", &self.0.to_string(), "/T", "/F"]);
            command
        };
        #[cfg(unix)]
        let mut cleanup = {
            let mut command = Command::new("kill");
            command.args(["-KILL", "--", &self.0.to_string()]);
            command
        };
        cleanup
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Ok(mut child) = cleanup.spawn() {
            bounded_wait_or_kill(&mut child, Instant::now() + Duration::from_secs(1));
        }
    }
}

#[cfg(all(test, windows))]
fn fixture_process_is_running(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist should run");
    let pid = pid.to_string();
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.split(',')
            .nth(1)
            .is_some_and(|field| field.trim_matches('"') == pid)
    })
}

#[cfg(all(test, unix))]
fn fixture_process_is_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent_process() -> (Child, ProcessTree) {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "/bin/sleep 30"]);
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut tree = ProcessTree::prepare(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        tree.assign(&mut child).unwrap();
        (child, tree)
    }

    fn stdout_then_wait_process() -> (Child, ProcessTree) {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::Out.WriteLine('progress'); Start-Sleep -Seconds 30",
            ]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf 'progress\\n'; /bin/sleep 30"]);
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut tree = ProcessTree::prepare(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        tree.assign(&mut child).unwrap();
        (child, tree)
    }

    #[test]
    fn cancellation_stops_a_silent_process_within_250_ms() {
        let _serial = process_test_lock();
        let (mut child, tree) = silent_process();
        let cancellation = TurnCancellation::default();
        let worker_cancellation = cancellation.clone();
        let cancelled_at = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            worker_cancellation.cancel();
            Instant::now()
        });

        let error = run_process_cancellable(
            &mut child,
            tree,
            None,
            ProcessLimits::new(Duration::from_secs(30), 128, 128),
            &cancellation,
        )
        .err()
        .expect("silent process should be cancelled");
        let cancelled_at = cancelled_at.join().unwrap();

        assert_eq!(error, "cancelled");
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(250),
            "cancellation took {:?}",
            cancelled_at.elapsed()
        );
        assert!(child.try_wait().unwrap().is_some(), "child was not reaped");
    }

    #[test]
    fn cancellation_during_stdout_callback_wins_over_callback_error() {
        let _serial = process_test_lock();
        let (mut child, tree) = stdout_then_wait_process();
        let cancellation = TurnCancellation::default();
        let callback_cancellation = cancellation.clone();
        let mut callbacks = 0;

        let outcome = run_process_streaming_cancellable(
            &mut child,
            tree,
            None,
            ProcessLimits::new(Duration::from_secs(30), 128, 128),
            &cancellation,
            &mut |_| {
                callbacks += 1;
                callback_cancellation.cancel();
                Err("callback error after stop".to_string())
            },
        );
        let error = match outcome {
            Err(error) => error,
            Ok(_) => panic!("cancellation should stop the process"),
        };

        assert_eq!(error, "cancelled");
        assert_eq!(callbacks, 1);
        assert!(child.try_wait().unwrap().is_some(), "child was not reaped");
    }

    #[test]
    fn truncating_capture_keeps_the_head_and_tail() {
        let mut capture = OutputCapture::new(9, OutputLimit::HeadTail);
        capture.push(b"HEAD--middle--END", "stdout").unwrap();
        let output = capture.snapshot();
        assert!(output.truncated);
        assert!(output.bytes.starts_with(b"HEAD--"));
        assert!(output.bytes.ends_with(b"END"));
        assert!(
            String::from_utf8_lossy(&output.bytes).contains("output truncated"),
            "{}",
            String::from_utf8_lossy(&output.bytes)
        );
    }

    #[test]
    fn strict_capture_rejects_overflow() {
        let mut capture = OutputCapture::new(4, OutputLimit::Error);
        let error = capture.push(b"12345", "stdout").unwrap_err();
        assert_eq!(error, "stdout exceeded 4 bytes");
    }
}
