# Chat and tools

Running `agul` without a subcommand opens chat in the current directory. The
equivalent explicit form is `agul chat`.

## Model connection

The defaults are DeepSeek's API, `deepseek-v4-flash`, and the key named by
`DEEPSEEK_API_KEY`.

```console
agul chat --model deepseek-v4-pro --reasoning-effort high
```

The `glm` preset is the GLM Coding Plan connection:

```console
export GLM_API_KEY='<key>'
agul chat --provider glm --reasoning-effort high
```

It selects `https://open.bigmodel.cn/api/coding/paas/v4`, `glm-4.7`, and
`GLM_API_KEY`, while reusing the GLM streaming/tool dialect. Its responses keep
token and cache telemetry but are treated as subscription quota rather than
general API billing. `AGUL_PROVIDER=glm` is the environment equivalent;
`glm-coding` remains accepted as a compatibility alias. `--model` and
`--api-key-env` remain available as explicit overrides. GLM accepts `low`,
`high`, and `max`; Agul maps its portable effort names to the nearest one and
shows the effective value in the status row. See
[Sessions and usage](sessions-and-usage.md) for the ledger fields and cost
behavior.

The ordinary pay-as-you-go GLM API is not a named Agul preset. Advanced users
who explicitly need it can supply its official URL and model; Agul then infers
the GLM wire protocol and price-card identity from that endpoint:

```console
agul chat --base-url https://open.bigmodel.cn/api/paas/v4 \
          --model glm-5.3 \
          --api-key-env GLM_API_KEY
```

For another OpenAI-compatible service, set the URL, model, and key variable:

```console
agul chat --base-url https://models.example.com/v1 \
          --model example-model \
          --api-key-env EXAMPLE_API_KEY
```

The URL may be either an API root or a full `/chat/completions` URL. Local HTTP
endpoints work directly and may omit the key. Both streamed SSE responses and
ordinary JSON responses are accepted.

Frequently used local models can be kept in the shell environment instead of
repeating connection flags. Command-line flags still override these values.

```text
AGUL_BASE_URL=http://127.0.0.1:51100/v1/chat/completions
AGUL_MODEL=qwen-local
AGUL_CONTEXT_WINDOW=32768
AGUL_MAX_TOKENS=16384
AGUL_TIMEOUT_SECONDS=600
```

`AGUL_MODEL` applies only to the native engine; command-line `--model` remains
an explicit override.

Native sessions retain their resolved connection and exact provider history.
See [Sessions and usage](sessions-and-usage.md) for resume behavior, provider
identity, and custom-proxy pricing.

A 32K local endpoint is best treated as a focused worker: documentation edits,
focused tests, repository search, and short patches with independently
checkable output. A primary model should decompose larger maintenance work and
verify or integrate the worker's result instead of asking one small-context
session to own the whole repository change.

`AGUL_CONTEXT_WINDOW` lets Agul conservatively fit the initial response budget
to a known model window. During a tool loop, oversized tool results are trimmed
deterministically with their head and tail retained, then kept as the canonical
history so later rounds reuse the same prefix. When an OpenAI-compatible service
instead reports its actual context window and input-token count in a rejection,
Agul lowers `max_tokens`, retries once, and reuses the learned limit for later
rounds in that chat; it never enters an implicit retry loop.

## ChatGPT account engine

Account-backed chat is a separate, explicitly selected engine. Native
DeepSeek, GLM, local-model, and four-tool behavior remains the default. See
[ChatGPT account mode](codex-account.md) for the authoritative login, model,
quota, Web Search, session, and billing behavior.

## Workbench

The blue, black, and gold workbench owns an alternate-screen terminal page.
Conversation output scrolls inside that page while the composer and status row
stay fixed at the bottom. PageUp and PageDown move through the transcript; new
output follows the tail until you scroll upward. Native terminal mouse
selection and paste remain available. Submitted user messages use a quiet
background block, provider reasoning uses dim italic text with `◌`, and the
final answer returns to high-contrast Markdown. Each tool appears as a short
`◆` line followed by `✓` or `!`. Pass `--hide-reasoning` when only answer text
is wanted. A new chat opens with a short Agul introduction and the `/` and `@`
entry points; resumed chats replay their visible conversation instead.

The inset rounded composer remains editable while the model reasons, streams
text, or runs a tool. It supports normal text movement and selection, history
search, Alt+Enter or Ctrl+J for a newline, and Tab or Shift+Tab completion.
Typing `/` or `@` opens commands or `@skill:name` references immediately.
Submit ordinary text during a turn to stop it and continue with that steering
message; `/stop` or Ctrl+C stops without queuing another message.

Its bottom status row uses compact symbols:

```text
turns 2 · tools 1 · ↑3.2k ↓412 · ctx 3.2k/1.00M 0.3% · KV 99.7% · $0.001 · 2.5s    ●  deepseek-v4-flash • high
```

Rounds, tools, tokens, direct provider cost, and elapsed time describe the
latest completed user request. `KV` is the latest provider response's reported
cache percentage, so a tool-heavy request does not hide whether its final
prompt prefix was reused. `ctx` is the latest response input divided by the
effective context window, not a sum of every response in the request. The
window comes from an explicit configuration, an exact official model binding,
a limit learned from the provider, or account-engine runtime metadata; when no
reliable total is available, Agul omits the field instead of inventing one. The
row shortens automatically with the terminal width. Subscription-backed
requests omit a currency cost; `/usage` and `/cost` retain cumulative
session-tree totals, including priced child sessions.

Commands inside chat:

- `/status` prints the current status row.
- `/skills` lists discovered Skills.
- `/usage` shows response and token totals.
- `/cost` shows the current total and price-card version.
- `/compact` summarizes older visible turns.
- `/sessions` lists resumable chats without exposing internal IDs; restart with
  `agul chat --resume` to choose one.
- `/clear` starts fresh inside the current session.
- `/stop` stops the active model/tool turn, Plugin command, or compaction.
- `/exit` leaves chat.

Submitted messages remain in the full-screen transcript. After `/exit`,
`/quit`, or interactive EOF, Agul restores the shell screen instead of dumping
that transcript into shell history. A non-empty saved chat prints
`closed · saved · ↩ agul chat --continue`; an empty saved chat prints `closed ·
saved`, and an ephemeral chat prints `closed · ephemeral`.

## Four tools

- `read` reads a UTF-8 file and can select a line offset and limit.
- `write` creates or overwrites a file and creates parent directories.
- `edit` replaces exact text once, or all matches when requested.
- `shell` runs PowerShell on Windows or `/bin/sh` on Unix from the workspace.

Tool errors are returned to the model, so it can inspect the result and try a
better action in the same turn.

`shell` retains at most 200,000 bytes from each output stream. Longer output is
drained while a useful head and tail are kept, with `truncated: true` in the
result. The model-facing context fitter may shorten that retained result further
when a configured context window requires it. Timeouts end the command tree,
return the output retained so far, and always return control to the model.

Available host commands and ecosystem helpers may appear as virtual
`system/*` Skills without increasing the built-in tool count. Their discovery,
precedence, and activation rules are documented in
[Skills and launches](skills.md).

## Non-interactive chat

Use `--prompt` when stdin is not a terminal. Add `--json` for one machine-readable
result containing the answer, rounds, tool count, session ID, cost, and usage.

```console
agul chat --workspace ./project --prompt "run the tests and fix the failure" --json
```

A failed model turn also returns completed rounds and tool calls, usage recorded
before the failure, and a resume hint. Any workspace changes already made remain
in place. With session storage enabled, the result also returns the stored
session ID so the same session can continue after the provider is available
again; `--no-session` instead returns a null session ID and starts fresh next
time.
