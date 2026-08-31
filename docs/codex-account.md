# ChatGPT account mode

Agul can use a ChatGPT plan's Codex allowance through the supported Codex
app-server protocol. On Windows, Agul prefers the current runtime bundled with
Codex Desktop so its account configuration and model catalog stay compatible,
then falls back to `codex.cmd` on `PATH`. Other platforms use `codex` on `PATH`.
Set `AGUL_CODEX_COMMAND` or pass `--codex-command` to select another executable
explicitly.

Connect and inspect the account:

```console
agul account login
agul account status
agul account logout
```

The default login opens the managed ChatGPT browser flow. On a remote or
headless machine, use `agul account login --device-code`. Codex owns the OAuth
session and refreshes it; Agul only uses the documented account RPC. The
credential store is shared with Codex CLI and Codex extensions, so
`agul account logout` signs those clients out too.

Run account-backed chat explicitly:

```console
agul chat --engine codex --reasoning-effort high
```

If `--model` is omitted, Agul selects the default model reported by
`model/list`; `AGUL_CODEX_MODEL` can set a separate account-engine default.
`AGUL_MODEL` remains native-only, so an existing local-model setup cannot leak
its model name into Codex. Native DeepSeek, GLM, local-model, and four-tool chat
remains the default when `--engine` is omitted.

## What Agul retains

Codex app-server owns the upstream model and tool loop. Agul retains its own
terminal presentation, visible session, operation trace, ARI-facing event
shapes, and Usage Ledger. A saved Codex session includes its upstream thread ID,
so `--continue`, the `--resume` picker, and exact `--session <id>` all resume the
real conversation rather than reconstructing it from a transcript. `/clear`
starts a new upstream thread.

Reasoning summaries stream with `◌`. Web Search emits compact `search`,
`open_page`, and `find_in_page` progress derived from Codex `webSearch` items.
Agul requests live search rather than the cached search mode, and the final
answer retains the citations produced by Codex.

Each upstream model response on the active thread writes its own input, cached
input, output, and reasoning tokens to the same response ledger used by native
providers. Tool loops therefore retain every parent-thread response rather
than only the turn total. Usage from Codex-spawned child threads is not yet
linked into the Agul parent ledger.

ChatGPT plan use is recorded as subscription quota rather than API cost.
[Sessions and usage](sessions-and-usage.md) defines the exact ledger fields and
mixed-billing display. `agul account status` shows both reported quota windows
and includes account token activity when the app-server reports it. An
API-key-only Codex login is rejected by this engine so it cannot be mistaken
for subscription use.

`--no-session` also requests an ephemeral upstream thread and returns a null
session ID. `AGUL_TIMEOUT_SECONDS` bounds startup calls and the whole Codex
turn. If an upstream turn fails or times out, interactive Agul exits that bridge
cleanly; start Agul again with `--continue` (or choose it with `--resume`)
instead of mixing a new turn with late events from the failed one.

Manual visible-turn `/compact` remains available in the native engine. Codex
has its own conversation compaction semantics, so Agul does not silently map
one operation to the other.

ARI can start the same account engine for an Agulater- or AgentKube-prepared
master. The app-server continues to own that session's tools, so use a native
ARI worker when an Agul Plugin is required. The request fields, streamed events,
and capability boundary are defined in the [ARI reference](ari/README.md).

The integration follows the official
[Codex authentication](https://developers.openai.com/codex/auth) and
[app-server](https://developers.openai.com/codex/app-server) interfaces.
