use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(test)]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AGUL_PLUGIN_FORMAT;
use super::TurnCancellation;
use super::process::{
    ProcessLimits, ProcessTree, run_process_streaming, run_process_streaming_cancellable,
};
use super::provider::ToolDefinition;

const DEFAULT_PLUGIN_TIMEOUT_SECONDS: u64 = 30;
const MAX_PLUGIN_STDOUT_BYTES: usize = 200_000;
const MAX_PLUGIN_STDERR_BYTES: usize = 16_384;
const MAX_PROGRESS_PREVIEW_CHARS: usize = 160;

#[derive(Clone, Debug, Default)]
pub(crate) struct Plugins {
    pub(crate) tools: Vec<PluginTool>,
    pub(crate) commands: Vec<PluginCommand>,
    pub(crate) capabilities: Vec<PluginCapability>,
}

#[derive(Clone, Debug)]
struct PluginProcess {
    plugin: String,
    root: PathBuf,
    program: OsString,
    arguments: Vec<String>,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginTool {
    process: PluginProcess,
    pub(crate) name: String,
    description: String,
    parameters: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginCommand {
    process: PluginProcess,
    pub(crate) name: String,
    pub(crate) description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PluginCapability {
    pub(crate) plugin: String,
    pub(crate) name: String,
}

pub(crate) struct PluginCallContext<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) workspace: &'a Path,
    pub(crate) launch_path: Option<&'a Path>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolProgress {
    pub(crate) call_id: String,
    pub(crate) seq: u64,
    pub(crate) task_id: Option<String>,
    pub(crate) stage: String,
    pub(crate) preview: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelatedSession {
    pub(crate) call_id: String,
    pub(crate) seq: u64,
    pub(crate) relation: String,
    pub(crate) session_id: String,
    pub(crate) delegation_id: String,
    pub(crate) task_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PluginEvent {
    ToolProgress(ToolProgress),
    RelatedSession(RelatedSession),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PluginTerminal {
    Success(Value),
    Failure(PluginFailure),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginFailure {
    pub(crate) code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<String>,
    pub(crate) message: String,
    pub(crate) retryable: bool,
}

#[derive(Debug)]
pub(crate) enum PluginExecutionError<E> {
    Plugin(String),
    Event(E),
}

impl PluginTool {
    pub(crate) fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    pub(crate) fn detail(&self) -> &str {
        &self.process.plugin
    }

    #[cfg(test)]
    pub(crate) fn execute<E>(
        &self,
        arguments: &Value,
        context: &PluginCallContext<'_>,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        self.execute_with_limits(
            arguments,
            context,
            ProcessLimits::new(
                self.process.timeout,
                MAX_PLUGIN_STDOUT_BYTES,
                MAX_PLUGIN_STDERR_BYTES,
            ),
            on_event,
        )
    }

    pub(crate) fn execute_cancellable<E>(
        &self,
        arguments: &Value,
        context: &PluginCallContext<'_>,
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        self.execute_with_limits_cancellable(
            arguments,
            context,
            ProcessLimits::new(
                self.process.timeout,
                MAX_PLUGIN_STDOUT_BYTES,
                MAX_PLUGIN_STDERR_BYTES,
            ),
            cancellation,
            on_event,
        )
    }

    #[cfg(test)]
    fn execute_with_limits<E>(
        &self,
        arguments: &Value,
        context: &PluginCallContext<'_>,
        limits: ProcessLimits,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        self.execute_with_limits_inner(arguments, context, limits, None, on_event)
    }

    fn execute_with_limits_cancellable<E>(
        &self,
        arguments: &Value,
        context: &PluginCallContext<'_>,
        limits: ProcessLimits,
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        self.execute_with_limits_inner(arguments, context, limits, Some(cancellation), on_event)
    }

    fn execute_with_limits_inner<E>(
        &self,
        arguments: &Value,
        context: &PluginCallContext<'_>,
        limits: ProcessLimits,
        cancellation: Option<&TurnCancellation>,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        self.process.invoke_inner(
            json!({
                "tool": self.name,
                "arguments": arguments,
                "context": WireContext::from(context),
            }),
            context,
            limits,
            cancellation,
            on_event,
        )
    }
}

impl PluginCommand {
    pub(crate) fn detail(&self) -> &str {
        &self.process.plugin
    }

    pub(crate) fn execute_cancellable<E>(
        &self,
        arguments: &str,
        context: &PluginCallContext<'_>,
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        self.process.invoke_inner(
            json!({
                "command": self.name,
                "arguments": arguments,
                "context": WireContext::from(context),
            }),
            context,
            ProcessLimits::new(
                self.process.timeout,
                MAX_PLUGIN_STDOUT_BYTES,
                MAX_PLUGIN_STDERR_BYTES,
            ),
            Some(cancellation),
            on_event,
        )
    }
}

impl PluginProcess {
    fn invoke_inner<E>(
        &self,
        request: Value,
        context: &PluginCallContext<'_>,
        limits: ProcessLimits,
        cancellation: Option<&TurnCancellation>,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), E>,
    ) -> Result<PluginTerminal, PluginExecutionError<E>> {
        if cancellation.is_some_and(TurnCancellation::is_cancelled) {
            return Err(PluginExecutionError::Plugin(format!(
                "plugin {} cancelled",
                self.plugin
            )));
        }
        validate_context(context).map_err(|error| {
            PluginExecutionError::Plugin(format!("plugin {} {error}", self.plugin))
        })?;
        let mut input = serde_json::to_vec(&request).map_err(|error| {
            PluginExecutionError::Plugin(format!(
                "could not encode plugin {} input: {error}",
                self.plugin
            ))
        })?;
        input.push(b'\n');

        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut tree = ProcessTree::prepare(&mut command).map_err(|error| {
            PluginExecutionError::Plugin(format!(
                "could not prepare plugin {}: {error}",
                self.plugin
            ))
        })?;
        let mut child = command.spawn().map_err(|error| {
            PluginExecutionError::Plugin(format!("could not start plugin {}: {error}", self.plugin))
        })?;
        tree.assign(&mut child).map_err(|error| {
            PluginExecutionError::Plugin(format!("could not start plugin {}: {error}", self.plugin))
        })?;

        let mut parser = OutputParser::new(context.call_id);
        let mut event_error = None;
        let output = {
            let mut handle_stdout = |bytes: &[u8]| {
                parser.push(bytes, &mut |event| match on_event(event) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        event_error = Some(error);
                        Err("plugin event handler failed".to_string())
                    }
                })
            };
            match cancellation {
                Some(cancellation) => run_process_streaming_cancellable(
                    &mut child,
                    tree,
                    Some(input),
                    limits,
                    cancellation,
                    &mut handle_stdout,
                ),
                None => {
                    run_process_streaming(&mut child, tree, Some(input), limits, &mut handle_stdout)
                }
            }
        };
        if let Some(error) = event_error {
            return Err(PluginExecutionError::Event(error));
        }
        let output = output.map_err(|error| {
            PluginExecutionError::Plugin(format!("plugin {} {error}", self.plugin))
        })?;
        if output.timed_out {
            return Err(PluginExecutionError::Plugin(format!(
                "plugin {} timed out after {} ms",
                self.plugin,
                limits.timeout().as_millis()
            )));
        }
        let status = output.status.ok_or_else(|| {
            PluginExecutionError::Plugin(format!(
                "plugin {} ended without an exit status",
                self.plugin
            ))
        })?;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let suffix = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            return Err(PluginExecutionError::Plugin(format!(
                "plugin {} exited with {}{suffix}",
                self.plugin, status
            )));
        }
        let finish = parser.finish(&mut |event| match on_event(event) {
            Ok(()) => Ok(()),
            Err(error) => {
                event_error = Some(error);
                Err("plugin event handler failed".to_string())
            }
        });
        if let Some(error) = event_error {
            return Err(PluginExecutionError::Event(error));
        }
        finish.map_err(|error| {
            PluginExecutionError::Plugin(format!("plugin {} {error}", self.plugin))
        })?;
        parser.terminal.ok_or_else(|| {
            PluginExecutionError::Plugin(format!(
                "plugin {} ended without a result event",
                self.plugin
            ))
        })
    }
}

fn validate_context(context: &PluginCallContext<'_>) -> Result<(), String> {
    if context.call_id.trim().is_empty() {
        return Err("call_id must not be empty".to_string());
    }
    if context.session_id.trim().is_empty() {
        return Err("session_id must not be empty".to_string());
    }
    if !context.workspace.is_absolute() {
        return Err("workspace must be absolute".to_string());
    }
    if context
        .launch_path
        .is_some_and(|launch_path| !launch_path.is_absolute())
    {
        return Err("launch_path must be absolute when present".to_string());
    }
    Ok(())
}

#[derive(Serialize)]
struct WireContext<'a> {
    call_id: &'a str,
    session_id: &'a str,
    workspace: &'a Path,
    launch_path: Option<&'a Path>,
}

impl<'a> From<&PluginCallContext<'a>> for WireContext<'a> {
    fn from(context: &PluginCallContext<'a>) -> Self {
        Self {
            call_id: context.call_id,
            session_id: context.session_id,
            workspace: context.workspace,
            launch_path: context.launch_path,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginManifest {
    format: String,
    name: String,
    version: String,
    command: Vec<String>,
    timeout_seconds: Option<u64>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    commands: Vec<ManifestCommand>,
    #[serde(default)]
    tools: Vec<ManifestTool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCommand {
    name: String,
    description: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestTool {
    name: String,
    description: String,
    parameters: Value,
}

pub(crate) fn discover(root: Option<&Path>) -> Result<Plugins, PluginError> {
    let Some(root) = root else {
        return Ok(Plugins::default());
    };
    if !root.is_dir() {
        return Err(PluginError::new(format!(
            "plugin directory does not exist: {}",
            root.display()
        )));
    }

    let direct_manifest = root.join("plugin.json");
    let has_direct_manifest = match fs::metadata(&direct_manifest) {
        Ok(metadata) => metadata.is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(PluginError::new(format!(
                "could not inspect {}: {error}",
                direct_manifest.display()
            )));
        }
    };
    let mut manifests = if has_direct_manifest {
        vec![direct_manifest]
    } else {
        let mut manifests = Vec::new();
        let entries = fs::read_dir(root).map_err(|error| {
            PluginError::new(format!("could not list {}: {error}", root.display()))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                PluginError::new(format!(
                    "could not inspect an entry in {}: {error}",
                    root.display()
                ))
            })?;
            let entry_path = entry.path();
            let metadata = fs::metadata(&entry_path).map_err(|error| {
                PluginError::new(format!(
                    "could not inspect {}: {error}",
                    entry_path.display()
                ))
            })?;
            if metadata.is_dir() {
                let manifest = entry_path.join("plugin.json");
                match fs::metadata(&manifest) {
                    Ok(metadata) if metadata.is_file() => manifests.push(manifest),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(PluginError::new(format!(
                            "could not inspect {}: {error}",
                            manifest.display()
                        )));
                    }
                }
            }
        }
        manifests
    };
    if manifests.is_empty() {
        return Err(PluginError::new(format!(
            "plugin directory contains no plugin.json manifests: {}",
            root.display()
        )));
    }
    manifests.sort();

    let mut plugins = Plugins::default();
    let mut command_names = HashSet::new();
    for path in manifests {
        let discovered = read_manifest(&path)?;
        for command in &discovered.commands {
            if !command_names.insert(command.name.clone()) {
                return Err(PluginError::new(format!(
                    "plugin command {} is declared more than once",
                    command.name
                )));
            }
        }
        plugins.tools.extend(discovered.tools);
        plugins.commands.extend(discovered.commands);
        plugins.capabilities.extend(discovered.capabilities);
    }
    Ok(plugins)
}

fn read_manifest(path: &Path) -> Result<Plugins, PluginError> {
    let bytes = fs::read(path)
        .map_err(|error| PluginError::new(format!("could not read {}: {error}", path.display())))?;
    let manifest: PluginManifest = serde_json::from_slice(&bytes).map_err(|error| {
        PluginError::new(format!("could not parse {}: {error}", path.display()))
    })?;
    if manifest.format != AGUL_PLUGIN_FORMAT {
        return Err(PluginError::new(format!(
            "{} format must be {AGUL_PLUGIN_FORMAT}",
            path.display()
        )));
    }
    if manifest.name.trim().is_empty() {
        return Err(PluginError::new(format!(
            "{} plugin name must not be empty",
            path.display()
        )));
    }
    if manifest.version.trim().is_empty() {
        return Err(PluginError::new(format!(
            "{} plugin version must not be empty",
            path.display()
        )));
    }
    if manifest.tools.is_empty() && manifest.commands.is_empty() {
        return Err(PluginError::new(format!(
            "{} plugin must declare at least one tool or command",
            path.display()
        )));
    }
    if manifest
        .command
        .first()
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err(PluginError::new(format!(
            "{} plugin command must not be empty",
            path.display()
        )));
    }
    let timeout_seconds = manifest
        .timeout_seconds
        .unwrap_or(DEFAULT_PLUGIN_TIMEOUT_SECONDS);
    if timeout_seconds == 0 {
        return Err(PluginError::new(format!(
            "{} plugin timeout_seconds must be greater than zero",
            path.display()
        )));
    }
    let timeout = Duration::from_secs(timeout_seconds);
    let root =
        fs::canonicalize(path.parent().unwrap_or_else(|| Path::new("."))).map_err(|error| {
            PluginError::new(format!(
                "could not resolve plugin root for {}: {error}",
                path.display()
            ))
        })?;
    let program = resolve_program(&root, &manifest.command[0])
        .map_err(|error| PluginError::new(format!("{} plugin command: {error}", path.display())))?;
    let arguments = manifest.command[1..].to_vec();
    let process = PluginProcess {
        plugin: format!("{}@{}", manifest.name, manifest.version),
        root,
        program,
        arguments,
        timeout,
    };
    let mut capability_names = HashSet::new();
    let capabilities = manifest
        .capabilities
        .into_iter()
        .map(|capability| {
            if !valid_capability(&capability) {
                return Err(PluginError::new(format!(
                    "{} capability {:?} must use a namespace/name/vN identifier",
                    path.display(),
                    capability
                )));
            }
            if !capability_names.insert(capability.clone()) {
                return Err(PluginError::new(format!(
                    "{} declares capability {} more than once",
                    path.display(),
                    capability
                )));
            }
            Ok(PluginCapability {
                plugin: process.plugin.clone(),
                name: capability,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut command_names = HashSet::new();
    let commands = manifest
        .commands
        .into_iter()
        .map(|command| {
            validate_named_description(
                path,
                "command",
                &command.name,
                &command.description,
                &mut command_names,
            )?;
            Ok(PluginCommand {
                process: process.clone(),
                name: command.name,
                description: command.description,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut tool_names = HashSet::new();
    let tools = manifest
        .tools
        .into_iter()
        .map(|tool| {
            validate_named_description(
                path,
                "tool",
                &tool.name,
                &tool.description,
                &mut tool_names,
            )?;
            if !tool.parameters.is_object() {
                return Err(PluginError::new(format!(
                    "{} tool {} parameters must be a JSON Schema object",
                    path.display(),
                    tool.name
                )));
            }
            Ok(PluginTool {
                process: process.clone(),
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Plugins {
        tools,
        commands,
        capabilities,
    })
}

fn validate_named_description(
    path: &Path,
    kind: &str,
    name: &str,
    description: &str,
    names: &mut HashSet<String>,
) -> Result<(), PluginError> {
    if !valid_name(name) {
        return Err(PluginError::new(format!(
            "{} {kind} name {:?} must match [A-Za-z0-9_-]{{1,64}}",
            path.display(),
            name
        )));
    }
    if !names.insert(name.to_string()) {
        return Err(PluginError::new(format!(
            "{} declares {kind} {name} more than once",
            path.display()
        )));
    }
    if description.trim().is_empty() {
        return Err(PluginError::new(format!(
            "{} {kind} {name} description must not be empty",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_program(root: &Path, program: &str) -> Result<OsString, String> {
    let path = Path::new(program);
    if !path.is_absolute() && path.components().count() == 1 {
        return Ok(OsString::from(program));
    }
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    fs::canonicalize(&candidate)
        .map(PathBuf::into_os_string)
        .map_err(|error| format!("could not resolve {}: {error}", candidate.display()))
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_capability(capability: &str) -> bool {
    let mut parts = capability.split('/');
    let namespace = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    !namespace.is_empty()
        && !name.is_empty()
        && version.strip_prefix('v').is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts.next().is_none()
        && namespace
            .bytes()
            .chain(name.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

struct OutputParser {
    call_id: String,
    next_seq: u64,
    pending: Vec<u8>,
    terminal: Option<PluginTerminal>,
}

impl OutputParser {
    fn new(call_id: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            next_seq: 1,
            pending: Vec::new(),
            terminal: None,
        }
    }

    fn push(
        &mut self,
        bytes: &[u8],
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        self.pending.extend_from_slice(bytes);
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=index).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.parse_line(&line, on_event)?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.parse_line(&line, on_event)?;
        }
        Ok(())
    }

    fn parse_line(
        &mut self,
        line: &[u8],
        on_event: &mut dyn FnMut(PluginEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        if self.terminal.is_some() {
            return Err("returned an event after the result event".to_string());
        }
        let event: WireEvent = serde_json::from_slice(line)
            .map_err(|error| format!("returned invalid NDJSON: {error}"))?;
        let (call_id, seq) = event.identity();
        if call_id != self.call_id {
            return Err(format!(
                "returned call_id {call_id:?}; expected {:?}",
                self.call_id
            ));
        }
        if seq != self.next_seq {
            return Err(format!("returned seq {seq}; expected {}", self.next_seq));
        }
        self.next_seq = self.next_seq.saturating_add(1);
        match event {
            WireEvent::Progress {
                call_id,
                seq,
                task_id,
                stage,
                preview,
            } => {
                require_text("progress stage", &stage)?;
                require_text("progress preview", &preview)?;
                if let Some(task_id) = task_id.as_deref() {
                    require_text("progress task_id", task_id)?;
                }
                on_event(PluginEvent::ToolProgress(ToolProgress {
                    call_id,
                    seq,
                    task_id,
                    stage,
                    preview: preview.chars().take(MAX_PROGRESS_PREVIEW_CHARS).collect(),
                }))?;
            }
            WireEvent::Session {
                call_id,
                seq,
                relation,
                session_id,
                delegation_id,
                task_id,
            } => {
                if relation != "delegated" {
                    return Err(format!(
                        "returned unsupported session relation {relation:?}"
                    ));
                }
                require_text("session_id", &session_id)?;
                require_text("delegation_id", &delegation_id)?;
                require_text("task_id", &task_id)?;
                on_event(PluginEvent::RelatedSession(RelatedSession {
                    call_id,
                    seq,
                    relation,
                    session_id,
                    delegation_id,
                    task_id,
                }))?;
            }
            WireEvent::Result {
                ok: true,
                content: Some(content),
                error: None,
                ..
            } => self.terminal = Some(PluginTerminal::Success(content)),
            WireEvent::Result {
                ok: false,
                content: None,
                error: Some(error),
                ..
            } => {
                require_text("error code", &error.code)?;
                require_text("error message", &error.message)?;
                if let Some(stage) = error.stage.as_deref() {
                    require_text("error stage", stage)?;
                }
                self.terminal = Some(PluginTerminal::Failure(error));
            }
            WireEvent::Result { ok, .. } => {
                let expected = if ok { "content" } else { "error" };
                return Err(format!(
                    "returned an invalid result event: ok={ok} requires exactly {expected}"
                ));
            }
        }
        Ok(())
    }
}

fn require_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("returned an empty {field}"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireEvent {
    Progress {
        call_id: String,
        seq: u64,
        #[serde(default)]
        task_id: Option<String>,
        stage: String,
        preview: String,
    },
    Session {
        call_id: String,
        seq: u64,
        relation: String,
        session_id: String,
        delegation_id: String,
        task_id: String,
    },
    Result {
        call_id: String,
        seq: u64,
        ok: bool,
        #[serde(default)]
        content: Option<Value>,
        #[serde(default)]
        error: Option<PluginFailure>,
    },
}

impl WireEvent {
    fn identity(&self) -> (&str, u64) {
        match self {
            Self::Progress { call_id, seq, .. }
            | Self::Session { call_id, seq, .. }
            | Self::Result { call_id, seq, .. } => (call_id, *seq),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PluginError(String);

impl PluginError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PluginError {}

#[cfg(test)]
pub(crate) fn process_tree_command_fixture() -> (Vec<String>, &'static str, &'static str) {
    #[cfg(windows)]
    {
        (
            vec![
                "powershell.exe".to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                "plugin.ps1".to_string(),
            ],
            "plugin.ps1",
            "$null = [Console]::In.ReadToEnd()\n[IO.File]::WriteAllText('child.ps1', \"Start-Sleep -Seconds 30\")\n$child = Start-Process powershell.exe -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', 'child.ps1') -NoNewWindow -PassThru\n[IO.File]::WriteAllText('child-started', [string]$child.Id)\nexit 0\n",
        )
    }
    #[cfg(unix)]
    {
        (
            vec!["/bin/sh".to_string(), "plugin.sh".to_string()],
            "plugin.sh",
            "#!/bin/sh\ncat >/dev/null\n/bin/sleep 30 &\nchild=$!\nprintf '%s' \"$child\" > child-started\nexit 0\n",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::process::{FixtureProcess, process_test_lock};
    use super::*;

    fn command_and_scripts() -> (
        Vec<String>,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        let (command, script_name, process_tree_script) = process_tree_command_fixture();
        #[cfg(windows)]
        {
            (
                command,
                script_name,
                "$payload = [Console]::In.ReadToEnd()\n[IO.File]::WriteAllText((Join-Path (Get-Location) 'invocation.json'), $payload)\n[Console]::Out.WriteLine('{\"type\":\"progress\",\"call_id\":\"call-1\",\"seq\":1,\"task_id\":\"scan\",\"stage\":\"thinking\",\"preview\":\"locating\"}')\n[Console]::Out.WriteLine('{\"type\":\"session\",\"call_id\":\"call-1\",\"seq\":2,\"relation\":\"delegated\",\"session_id\":\"child-1\",\"delegation_id\":\"delegation-1\",\"task_id\":\"scan\"}')\n[Console]::Out.WriteLine('{\"type\":\"result\",\"call_id\":\"call-1\",\"seq\":3,\"ok\":true,\"content\":\"plugin-ok\"}')\n",
                "$null = [Console]::In.ReadToEnd()\n[Console]::Error.Write('plugin-broke')\nexit 7\n",
                "while ($true) {}\n",
                "$null = [Console]::In.ReadToEnd()\n[Console]::Out.Write('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')\n",
                process_tree_script,
            )
        }
        #[cfg(unix)]
        {
            (
                command,
                script_name,
                "#!/bin/sh\npayload=$(cat)\nprintf '%s' \"$payload\" > invocation.json\nprintf '%s\\n' '{\"type\":\"progress\",\"call_id\":\"call-1\",\"seq\":1,\"task_id\":\"scan\",\"stage\":\"thinking\",\"preview\":\"locating\"}'\nprintf '%s\\n' '{\"type\":\"session\",\"call_id\":\"call-1\",\"seq\":2,\"relation\":\"delegated\",\"session_id\":\"child-1\",\"delegation_id\":\"delegation-1\",\"task_id\":\"scan\"}'\nprintf '%s\\n' '{\"type\":\"result\",\"call_id\":\"call-1\",\"seq\":3,\"ok\":true,\"content\":\"plugin-ok\"}'\n",
                "#!/bin/sh\ncat >/dev/null\nprintf '%s' 'plugin-broke' >&2\nexit 7\n",
                "#!/bin/sh\nwhile :; do :; done\n",
                "#!/bin/sh\ncat >/dev/null\nprintf '%s' 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'\n",
                process_tree_script,
            )
        }
    }

    #[cfg(windows)]
    fn silent_cancellation_script() -> &'static str {
        "$null = [Console]::In.ReadToEnd()\n[IO.File]::WriteAllText('plugin-started', 'yes')\nStart-Sleep -Seconds 30\n"
    }

    #[cfg(unix)]
    fn silent_cancellation_script() -> &'static str {
        "#!/bin/sh\ncat >/dev/null\nprintf yes > plugin-started\n/bin/sleep 30\n"
    }

    fn write_test_plugin(
        plugin: &Path,
        command: &[String],
        script_name: &str,
        script: &str,
        tool_name: &str,
    ) -> PluginTool {
        fs::create_dir_all(plugin).unwrap();
        fs::write(plugin.join(script_name), script).unwrap();
        fs::write(
            plugin.join("plugin.json"),
            serde_json::to_vec(&json!({
                "format": AGUL_PLUGIN_FORMAT,
                "name": "echo",
                "version": "1.0.0",
                "command": command,
                "tools": [{
                    "name": tool_name,
                    "description": "Echo text",
                    "parameters": {"type": "object"}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        discover(Some(plugin)).unwrap().tools.remove(0)
    }

    fn write_test_command_plugin(
        plugin: &Path,
        command: &[String],
        script_name: &str,
        script: &str,
        command_name: &str,
    ) -> PluginCommand {
        fs::create_dir_all(plugin).unwrap();
        fs::write(plugin.join(script_name), script).unwrap();
        fs::write(
            plugin.join("plugin.json"),
            serde_json::to_vec(&json!({
                "format": AGUL_PLUGIN_FORMAT,
                "name": "echo",
                "version": "1.0.0",
                "command": command,
                "commands": [{
                    "name": command_name,
                    "description": "Run a command"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        discover(Some(plugin)).unwrap().commands.remove(0)
    }

    fn call_context(workspace: &Path) -> PluginCallContext<'_> {
        PluginCallContext {
            call_id: "call-1",
            session_id: "session-1",
            workspace,
            launch_path: None,
        }
    }

    #[cfg(unix)]
    fn link_plugin_directory(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn link_plugin_directory(target: &Path, link: &Path) {
        let status = Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "could not create plugin directory link");
    }

    fn plugin_error<E>(error: PluginExecutionError<E>) -> String {
        match error {
            PluginExecutionError::Plugin(error) => error,
            PluginExecutionError::Event(_) => panic!("unexpected event-handler error"),
        }
    }

    #[test]
    fn discovers_a_collection_and_rejects_the_wrong_format() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("plugins/echo");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            plugin.join("plugin.json"),
            r#"{
                "format":"agul/plugin/v2",
                "name":"echo",
                "version":"1.0.0",
                "command":["echo-plugin"],
                "tools":[{
                    "name":"echo_text",
                    "description":"Echo text",
                    "parameters":{"type":"object"}
                }]
            }"#,
        )
        .unwrap();

        let plugins = discover(Some(&root.path().join("plugins"))).unwrap();
        assert_eq!(plugins.tools.len(), 1);
        assert_eq!(plugins.tools[0].name, "echo_text");
        assert_eq!(plugins.tools[0].detail(), "echo@1.0.0");

        fs::write(
            plugin.join("plugin.json"),
            r#"{
                "format":"agul/plugin/v1",
                "name":"echo",
                "version":"1.0.0",
                "command":["echo-plugin"],
                "tools":[]
            }"#,
        )
        .unwrap();
        let error = discover(Some(&root.path().join("plugins"))).unwrap_err();
        assert!(error.to_string().contains("format must be agul/plugin/v2"));
    }

    #[test]
    fn rejects_an_explicit_plugin_directory_without_manifests() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        fs::create_dir_all(plugins.join("missing-manifest")).unwrap();

        let error = discover(Some(&plugins)).unwrap_err().to_string();

        assert!(
            error.contains("plugin directory contains no plugin.json manifests"),
            "{error}"
        );
        assert!(error.contains(&plugins.display().to_string()), "{error}");
    }

    #[test]
    fn discovers_plugins_through_directory_links() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("plugin-target");
        let plugins = root.path().join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let (command, script_name, success_script, _, _, _, _) = command_and_scripts();
        write_test_plugin(
            &target,
            &command,
            script_name,
            success_script,
            "linked_tool",
        );
        link_plugin_directory(&target, &plugins.join("linked"));

        let discovered = discover(Some(&plugins)).unwrap();

        assert_eq!(discovered.tools.len(), 1);
        assert_eq!(discovered.tools[0].name, "linked_tool");
    }

    #[test]
    fn uses_the_manifest_timeout_and_rejects_zero() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("echo");
        fs::create_dir_all(&plugin).unwrap();
        let manifest = |timeout_seconds: Option<u64>| {
            let mut value = json!({
                "format": AGUL_PLUGIN_FORMAT,
                "name": "echo",
                "version": "1.0.0",
                "command": ["echo-plugin"],
                "tools": [{
                    "name": "echo_text",
                    "description": "Echo text",
                    "parameters": {"type": "object"}
                }]
            });
            if let Some(timeout_seconds) = timeout_seconds {
                value["timeout_seconds"] = timeout_seconds.into();
            }
            serde_json::to_vec(&value).unwrap()
        };

        fs::write(plugin.join("plugin.json"), manifest(None)).unwrap();
        let tool = discover(Some(&plugin)).unwrap().tools.remove(0);
        assert_eq!(
            tool.process.timeout,
            Duration::from_secs(DEFAULT_PLUGIN_TIMEOUT_SECONDS)
        );

        fs::write(plugin.join("plugin.json"), manifest(Some(300))).unwrap();
        let tool = discover(Some(&plugin)).unwrap().tools.remove(0);
        assert_eq!(tool.process.timeout, Duration::from_secs(300));

        fs::write(plugin.join("plugin.json"), manifest(Some(0))).unwrap();
        let error = discover(Some(&plugin)).unwrap_err().to_string();
        assert!(
            error.contains("timeout_seconds must be greater than zero"),
            "{error}"
        );
    }

    #[test]
    fn invokes_one_process_with_one_json_request() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("echo");
        fs::create_dir_all(&plugin).unwrap();
        let (command, script_name, script, failure_script, _, _, _) = command_and_scripts();
        let tool = write_test_plugin(&plugin, &command, script_name, script, "echo_text");
        let context = call_context(&plugin);
        let mut events = Vec::new();
        assert_eq!(
            tool.execute(&json!({"text": "hello"}), &context, &mut |event| {
                events.push(event);
                Ok::<(), ()>(())
            })
            .unwrap(),
            PluginTerminal::Success(json!("plugin-ok"))
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], PluginEvent::ToolProgress(_)));
        assert!(matches!(events[1], PluginEvent::RelatedSession(_)));
        let invocation: Value =
            serde_json::from_slice(&fs::read(plugin.join("invocation.json")).unwrap()).unwrap();
        assert_eq!(
            invocation,
            json!({
                "tool": "echo_text",
                "arguments": {"text": "hello"},
                "context": {
                    "call_id": "call-1",
                    "session_id": "session-1",
                    "workspace": plugin,
                    "launch_path": null
                }
            })
        );

        fs::write(plugin.join(script_name), failure_script).unwrap();
        let error = plugin_error(
            tool.execute(&json!({"text": "again"}), &context, &mut |_| {
                Ok::<(), ()>(())
            })
            .unwrap_err(),
        );
        assert!(error.contains("exited with"));
        assert!(error.contains("plugin-broke"));
    }

    #[test]
    fn times_out_while_stdin_is_blocked_and_reaps_the_process() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("echo");
        let (command, script_name, _, _, blocking_script, _, _) = command_and_scripts();
        let tool = write_test_plugin(&plugin, &command, script_name, blocking_script, "echo_text");
        let started = Instant::now();
        let error = plugin_error(
            tool.execute_with_limits(
                &json!({"text": "x".repeat(1_000_000)}),
                &call_context(&plugin),
                ProcessLimits::new(Duration::from_millis(100), 128, 128),
                &mut |_| Ok::<(), ()>(()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("timed out after 100 ms"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancellable_plugin_tool_stops_without_waiting_for_output() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("echo");
        let (command, script_name, _, _, _, _, _) = command_and_scripts();
        let tool = write_test_plugin(
            &plugin,
            &command,
            script_name,
            silent_cancellation_script(),
            "echo_text",
        );
        let marker = plugin.join("plugin-started");
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

        let outcome = tool.execute_with_limits_cancellable(
            &json!({}),
            &call_context(&plugin),
            ProcessLimits::new(Duration::from_secs(30), 128, 128),
            &cancellation,
            &mut |_| Ok::<(), ()>(()),
        );
        let (process_started, cancelled_at) = canceller.join().unwrap();
        let latency = cancelled_at.elapsed();
        let error = plugin_error(outcome.unwrap_err());

        assert!(process_started, "silent plugin process did not start");
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            latency < Duration::from_millis(250),
            "plugin cancellation took {latency:?}"
        );
    }

    #[test]
    fn cancellable_plugin_command_stops_a_silent_process_tree() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("command-tree");
        let (command, script_name, _, _, _, _, process_tree_script) = command_and_scripts();
        let plugin_command = write_test_command_plugin(
            &plugin,
            &command,
            script_name,
            process_tree_script,
            "tree_command",
        );
        let marker = plugin.join("child-started");
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

        let outcome = plugin_command.execute_cancellable(
            "",
            &call_context(&plugin),
            &cancellation,
            &mut |_| Ok::<(), ()>(()),
        );
        let (process_started, cancelled_at) = canceller.join().unwrap();
        let latency = cancelled_at.elapsed();
        let error = plugin_error(outcome.unwrap_err());

        assert!(
            process_started,
            "silent plugin command did not start its child"
        );
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            latency < Duration::from_millis(250),
            "plugin command cancellation took {latency:?}"
        );
        let child_pid = fs::read_to_string(marker).unwrap().parse::<u32>().unwrap();
        let fixture = FixtureProcess::new(child_pid);
        assert!(
            fixture.wait_for_exit(Duration::from_secs(4)),
            "fixture child {child_pid} survived command cancellation"
        );
    }

    #[test]
    fn stops_when_plugin_output_exceeds_the_limit() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("echo");
        let (command, script_name, _, _, _, overflow_script, _) = command_and_scripts();
        let tool = write_test_plugin(&plugin, &command, script_name, overflow_script, "echo_text");
        let error = plugin_error(
            tool.execute_with_limits(
                &json!({}),
                &call_context(&plugin),
                ProcessLimits::new(Duration::from_secs(2), 32, 32),
                &mut |_| Ok::<(), ()>(()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("stdout exceeded 32 bytes"), "{error}");
    }

    #[test]
    fn timeout_returns_promptly_after_the_parent_exits_with_inherited_pipes() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("process-tree");
        let (command, script_name, _, _, _, _, process_tree_script) = command_and_scripts();
        let tool = write_test_plugin(
            &plugin,
            &command,
            script_name,
            process_tree_script,
            "tree_tool",
        );
        let started = Instant::now();
        let outcome = tool.execute_with_limits(
            &json!({}),
            &call_context(&plugin),
            ProcessLimits::new(Duration::from_millis(500), 128, 128),
            &mut |_| Ok::<(), ()>(()),
        );
        let child_pid = fs::read_to_string(plugin.join("child-started"))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let fixture = FixtureProcess::new(child_pid);
        let error = plugin_error(outcome.unwrap_err());
        assert!(error.contains("timed out after 500 ms"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            fixture.wait_for_exit(Duration::from_secs(4)),
            "fixture child {child_pid} survived"
        );
    }

    #[test]
    fn resolves_a_path_bearing_program_against_the_plugin_root() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("relative-program");
        let bin = plugin.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("tool");
        fs::write(&executable, b"test executable").unwrap();
        fs::write(
            plugin.join("plugin.json"),
            r#"{
                "format":"agul/plugin/v2",
                "name":"relative",
                "version":"1.0.0",
                "command":["./bin/tool"],
                "tools":[{
                    "name":"relative_tool",
                    "description":"Test relative resolution",
                    "parameters":{"type":"object"}
                }]
            }"#,
        )
        .unwrap();

        let tool = discover(Some(&plugin)).unwrap().tools.remove(0);
        assert_eq!(
            PathBuf::from(tool.process.program),
            fs::canonicalize(executable).unwrap()
        );
    }

    #[test]
    fn rejects_tool_names_outside_the_wire_format() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("invalid-name");
        fs::create_dir_all(&plugin).unwrap();
        for name in ["has.dot".to_string(), "naïve".to_string(), "x".repeat(65)] {
            fs::write(
                plugin.join("plugin.json"),
                serde_json::to_vec(&json!({
                    "format": AGUL_PLUGIN_FORMAT,
                    "name": "invalid",
                    "version": "1.0.0",
                    "command": ["plugin-command"],
                    "tools": [{
                        "name": name,
                        "description": "Invalid name",
                        "parameters": {"type": "object"}
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
            let error = discover(Some(&plugin)).unwrap_err().to_string();
            assert!(error.contains("must match [A-Za-z0-9_-]{1,64}"), "{error}");
        }
    }

    #[test]
    fn loads_commands_and_capabilities_from_a_strict_v2_manifest() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("plugin.json"),
            serde_json::to_vec(&json!({
                "format": AGUL_PLUGIN_FORMAT,
                "name": "coordinator",
                "version": "2.0.0",
                "command": ["not-run"],
                "capabilities": ["agul/dependency-installer/v1"],
                "commands": [{
                    "name": "agent",
                    "description": "Delegate to a prepared specialist"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let plugins = discover(Some(root.path())).unwrap();
        assert!(plugins.tools.is_empty());
        assert_eq!(plugins.commands.len(), 1);
        assert_eq!(plugins.commands[0].name, "agent");
        assert_eq!(plugins.commands[0].detail(), "coordinator@2.0.0");
        assert_eq!(
            plugins.capabilities,
            vec![PluginCapability {
                plugin: "coordinator@2.0.0".to_string(),
                name: "agul/dependency-installer/v1".to_string(),
            }]
        );

        let mut invalid: Value =
            serde_json::from_slice(&fs::read(root.path().join("plugin.json")).unwrap()).unwrap();
        invalid["unexpected"] = json!(true);
        fs::write(
            root.path().join("plugin.json"),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();
        let error = discover(Some(root.path())).unwrap_err().to_string();
        assert!(error.contains("unknown field `unexpected`"), "{error}");
    }

    #[test]
    fn committed_manifest_fixtures_match_the_runtime_contract() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("plugin.json");
        fs::write(
            &path,
            include_str!("../../schemas/fixtures/plugin-v2-valid.json"),
        )
        .unwrap();
        let plugins = discover(Some(root.path())).unwrap();
        assert_eq!(plugins.tools.len(), 1);
        assert_eq!(plugins.commands.len(), 1);
        assert_eq!(plugins.capabilities.len(), 1);

        for fixture in [
            include_str!("../../schemas/fixtures/plugin-v2-invalid-v1.json"),
            include_str!("../../schemas/fixtures/plugin-v2-invalid-unknown-field.json"),
        ] {
            fs::write(&path, fixture).unwrap();
            assert!(discover(Some(root.path())).is_err());
        }
    }

    #[test]
    fn parser_requires_matching_call_ids_contiguous_sequences_and_one_last_result() {
        let mut parser = OutputParser::new("call-1");
        let preview = "界".repeat(200);
        let bytes = format!(
            "{{\"type\":\"progress\",\"call_id\":\"call-1\",\"seq\":1,\"stage\":\"thinking\",\"preview\":{}}}\n{{\"type\":\"result\",\"call_id\":\"call-1\",\"seq\":2,\"ok\":true,\"content\":{{\"done\":true}}}}\n",
            serde_json::to_string(&preview).unwrap()
        );
        let mut events = Vec::new();
        for chunk in bytes.as_bytes().chunks(7) {
            parser
                .push(chunk, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .unwrap();
        }
        parser.finish(&mut |_| Ok(())).unwrap();
        let PluginEvent::ToolProgress(progress) = &events[0] else {
            panic!("expected progress event");
        };
        assert_eq!(progress.preview.chars().count(), MAX_PROGRESS_PREVIEW_CHARS);
        assert_eq!(
            parser.terminal,
            Some(PluginTerminal::Success(json!({"done": true})))
        );

        let error = parser
            .push(
                b"{\"type\":\"progress\",\"call_id\":\"call-1\",\"seq\":3,\"stage\":\"done\",\"preview\":\"late\"}\n",
                &mut |_| Ok(()),
            )
            .unwrap_err();
        assert!(error.contains("after the result"), "{error}");

        let mut parser = OutputParser::new("call-1");
        let error = parser
            .push(
                b"{\"type\":\"progress\",\"call_id\":\"wrong\",\"seq\":1,\"stage\":\"x\",\"preview\":\"x\"}\n",
                &mut |_| Ok(()),
            )
            .unwrap_err();
        assert!(error.contains("expected \"call-1\""), "{error}");

        let mut parser = OutputParser::new("call-1");
        let error = parser
            .push(
                b"{\"type\":\"result\",\"call_id\":\"call-1\",\"seq\":2,\"ok\":true,\"content\":\"x\"}\n",
                &mut |_| Ok(()),
            )
            .unwrap_err();
        assert!(error.contains("expected 1"), "{error}");
    }
}
