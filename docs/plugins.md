# Plugins

Agul starts with four built-in tools. A prepared launch can add external tools
and slash-command handlers without changing the model loop. Set `plugins` to a
plugin collection relative to the launch file:

```json
{
  "format": "agul/launch/v2",
  "instructions": "instructions.md",
  "plugins": "../plugins"
}
```

Each immediate child of that directory is a plugin root containing
`plugin.json`. A launch may also point directly at one plugin root. Agul accepts
only the current manifest format; `agul/plugin/v1` is not loaded.

```json
{
  "format": "agul/plugin/v2",
  "name": "example",
  "version": "1.0.0",
  "command": ["python", "plugin.py"],
  "timeout_seconds": 600,
  "capabilities": ["agul/dependency-installer/v1"],
  "commands": [
    {
      "name": "agent",
      "description": "Delegate to a prepared specialist"
    }
  ],
  "tools": [
    {
      "name": "example_lookup",
      "description": "Look up one value",
      "parameters": {
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "required": ["query"],
        "additionalProperties": false
      }
    }
  ]
}
```

`command` is the process argv and is unrelated to the optional `commands`
array. `commands` declares generic slash-command names. Agul registers the name,
description, and owning plugin; the workbench passes the unparsed text after
`/name` directly to that plugin. Duplicate command names are rejected rather
than given a silent priority.

`capabilities` contains versioned contracts implemented by the plugin. For
example, `agul/dependency-installer/v1` identifies interchangeable dependency
installers; it does not give one implementation priority over another.

## Invocation

Agul starts one process for each invocation and writes one JSON line to stdin.
A tool request is:

```json
{
  "tool": "example_lookup",
  "arguments": {"query": "agul"},
  "context": {
    "call_id": "call-7",
    "session_id": "019c...",
    "workspace": "/work/project",
    "launch_path": "/work/project/.agents/runtime/launch.json"
  }
}
```

A slash-command invocation uses `command` instead of `tool` and carries its raw
argument text as a string:

```json
{
  "command": "agent",
  "arguments": "repository-scout locate the package loader",
  "context": {
    "call_id": "call-8",
    "session_id": "019c...",
    "workspace": "/work/project",
    "launch_path": null
  }
}
```

`workspace` and a present `launch_path` are absolute. `launch_path` is `null`
when the session has no prepared launch.

## Output events

Stdout is newline-delimited JSON. Every non-empty line is one event with the
request's `call_id` and a contiguous `seq` beginning at 1. A plugin may emit
zero or more progress and related-session events, followed by exactly one final
result event:

```json
{"type":"progress","call_id":"call-7","seq":1,"task_id":"scan","stage":"thinking","preview":"Locating the package loader"}
{"type":"session","call_id":"call-7","seq":2,"relation":"delegated","session_id":"019d...","delegation_id":"delegation-1","task_id":"scan"}
{"type":"result","call_id":"call-7","seq":3,"ok":true,"content":{"status":"completed"}}
```

Progress is delivered while the process is still running. Agul retains at most
160 characters of `preview`. A `session` event is sent as soon as a delegated
child session exists; it is runtime metadata and is not inserted into the
model's tool result.

A plugin-level failure is also a final result:

```json
{"type":"result","call_id":"call-7","seq":1,"ok":false,"error":{"code":"provider_unavailable","stage":"send","message":"Local endpoint is offline","retryable":true}}
```

The process must then exit successfully. Missing, repeated, or non-final result
events; malformed JSON; a mismatched `call_id`; a sequence gap; or an unknown
field fail the invocation. Diagnostics belong on stderr.

Each invocation has 30 seconds to finish by default. A plugin can set the
optional positive integer `timeout_seconds`; it applies independently to every
tool or command call. Agul accepts at most 200,000 stdout bytes and 16,384
stderr bytes. A timeout, overflow, or protocol violation terminates the plugin
process tree and becomes an ordinary failed tool result.

A path-bearing program such as `./bin/tool` is resolved from the plugin root,
while a bare program name uses `PATH`. Tool and command names use 1–64 ASCII
letters, digits, `_`, or `-`. Tool names must also be unique across the four
built-ins and all loaded plugins.

An explicitly configured Plugin path must resolve either to one Plugin root or
to a collection containing at least one immediate child with `plugin.json`.
An empty collection, a wrong nesting level, or a directory-entry error fails at
startup instead of silently producing a launch with no Plugin tools.

The formal contracts are
[`plugin-v2.schema.json`](../schemas/plugin-v2.schema.json),
[`plugin-v2-invocation.schema.json`](../schemas/plugin-v2-invocation.schema.json),
and [`plugin-v2-event.schema.json`](../schemas/plugin-v2-event.schema.json).

ARI `ari.capabilities` advertises `agul/plugin/v2` plus the
`tool_progress` and `related_session` event kinds. `ari.start_session` returns
the actual tool, plugin-command, and plugin-capability registrations for a
native session. A Codex-account session returns empty Agul plugin registrations
because app-server owns that model's tool loop.
