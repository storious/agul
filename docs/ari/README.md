# ARI

ARI lets another program drive Agul's project loader, Skills, streamed events,
durable sessions, and per-response Usage Ledger. A session selects either the
native Agul loop or the ChatGPT-account Codex loop.

Start the newline-delimited JSON-RPC 2.0 service:

```console
agul ari serve
```

It reads one JSON request per stdin line and writes one response or event per
stdout line. Call `ari.initialize` before other methods; the current handshake
returns `"ari": "0.2"`.

```json
{"jsonrpc":"2.0","id":1,"method":"ari.initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"ari.start_session","params":{"workspace":"."}}
{"jsonrpc":"2.0","id":3,"method":"ari.send","params":{"session_id":"session-1","input":"read README.md"}}
```

Current methods:

- `ari.initialize`
- `ari.capabilities`
- `ari.start_session`
- `ari.send`
- `ari.compact`
- `ari.close_session`

`ari.start_session` accepts `engine: "native" | "codex"`; native is the default.
Both engines accept optional `workspace`, `launch_path`, attribution fields,
`model`, `reasoning_effort`, and `timeout_seconds`.

The native engine also accepts `provider: "deepseek" | "glm"`, `base_url`,
`api_key_env`, `price_card`, `context_window`, `max_tokens`, `max_rounds`, and
`max_tool_calls`. `provider: "glm"` selects the GLM Coding Plan endpoint,
`glm-4.7`, `GLM_API_KEY`, and GLM's wire dialect. The result and Usage Ledger
identify the wire provider as `glm` and report
`billing: "subscription_quota"`; token and cache telemetry is retained without
applying the ordinary API price card. `glm-coding` remains accepted as an input
compatibility alias but is not advertised as a separate provider. With
no override, every native provider uses the shared loop defaults: no declared
context window, a 300-second provider-request timeout, 16,384 output tokens, 32
model rounds, and 128 tool calls. A relative price-card path is resolved from
the session workspace for price-card-backed connections.

The Codex engine accepts `codex_command`; otherwise it uses
`AGUL_CODEX_COMMAND` and then the platform discovery described in
[ChatGPT account mode](../codex-account.md). It requires a managed ChatGPT
login, uses the plan's Codex quota, and requests live Web Search. Native-only
fields are rejected instead of being silently ignored, and native sessions
likewise reject `codex_command`.

The start result reports the effective engine, provider, model, reasoning
effort, endpoint, billing mode, upstream thread ID, capability set, and actual
tools.
Codex returns no Agul-side tools or plugin registrations because its tool loop
belongs to app-server; `tool_owner: "codex_app_server"` makes that boundary
explicit. `ari.send`, `ari.compact`, and `ari.close_session` take a `session_id`;
`ari.send` also takes `input`.

`ari.capabilities` contains an engine matrix rather than one ambiguous global
tool or billing claim. Native advertises the four core tools, optional
Plugin-provided Web Search, price-card billing, plugin tools, and manual
compaction. Codex advertises ChatGPT quota, live Web Search, app-server-owned
tools, and no manual visible-turn compaction. The `agul/plugin/v2` format
remains advertised for native prepared sessions. The separate
`usage.ledger: "per_response"` field
states the shared ledger granularity without confusing it with either billing
source.

During `ari.send` and `ari.compact`, `ari.event` notifications stream
`reasoning`, `text`, `tool`, `tool_progress`, `related_session`, and `usage`
events before the final response. A usage event includes the response
observation and its ledger entry. The final `ari.send` result includes
`handoff`: the canonical parsed `agul/handoff/v1` object, or `null` when the
assistant's final text does not contain a schema-valid terminal handoff. ARI
clients must use this field instead of reparsing the visible text.

ARI and terminal chat use the same `agul/chat-session/v5` store. A session has
one durable ID, source and status, optional parent/delegation/task/Specialist/
Pool attribution, related child sessions, an optional
[`agul/handoff/v1`](../../schemas/handoff-v1.schema.json) value, and its own
response ledger. Native `ari.compact` summarizes visible turns
without tools and replaces them only after the summary succeeds. Codex manual
compaction is rejected without mutating the session. A local input error remains
correctable; an upstream or persistence failure terminates a Codex bridge so a
later turn cannot mix with late app-server events. `ari.close_session` marks the
durable record completed instead of deleting it.

Every model operation also appends `agul/trace-event/v1` records under the
state directory. The event stream and saved trace carry the same operation ID;
`agul sessions show <id> --trace` reads the trace without reopening a model
session. Agul intentionally stops here: stdin/stdout transport, the shared
model loop, durable runtime facts, and no separate coordination framework.
