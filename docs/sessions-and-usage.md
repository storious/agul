# Sessions and usage

Agul stores visible chat turns and one Usage Ledger entry per provider response
by default. Start an ephemeral chat with `--no-session`.

## Inspect and resume

```console
agul chat --continue
agul chat --resume
agul sessions list
agul sessions show <id>
```

`--continue` reopens the most recent non-empty top-level chat in the selected
workspace. `--resume` opens a searchable picker for that workspace, so normal
interactive use never requires copying a session ID. `--session <id>` remains
available for scripts and exact recovery.

`list` prints recent sessions with source, status, child count, and cost. `show`
prints the `agul/chat-session/v5` record, including visible turns, semantic
summary, attribution, related sessions, captured handoff, and every usage entry.
Add `--trace` to include the append-only `agul/trace-event/v1` operation log.
Inside the workbench, `/sessions` shows a compact ID-free list of resumable
chats and the picker command. Leaving a non-empty saved chat restores the shell
screen and prints the `--continue` shortcut. Reopening it restores recent
retained user/final-answer turns inside the alternate-screen transcript using
the same Markdown presentation. Very old UI history and compacted history use
folded markers; the complete saved session is unchanged.

A resumed session reopens its recorded workspace, engine, requested model, and
native provider route. The new process may select a different API-key variable,
reasoning effort, or custom proxy, but Agul rejects a conflicting built-in
provider before sending a request. Native sessions retain the exact provider
message history; Codex sessions resume their recorded upstream thread instead.
Only v5 session records can be resumed; older schemas are ignored rather than
migrated. A damaged v5 record that can be attributed to the selected workspace
is reported instead of silently selecting an older chat.

The default session directory is `%LOCALAPPDATA%\Agul\sessions` on Windows,
`$XDG_STATE_HOME/agul/sessions` when set on Unix, or
`~/.local/state/agul/sessions`. Traces live beside it under `traces`.
`--state-dir` selects another root.

Related child sessions own their own ledgers and attribution. Parent views
aggregate each reachable session once and list every price-card version used by
the resulting tree.

## Manual semantic compaction

`/compact` sends only older visible turns to the model and supplies no tools.
The four most recent turns remain verbatim. Agul replaces the older turns only
after a summary succeeds, so an interrupted or failed compaction leaves them in
place. Compaction is never automatic, and its provider response receives a
separate Usage Ledger entry.

## Per-response Usage Ledger

Each response records its purpose, provider and origin, response ID,
observation time, reported model, token fields, and quoted cost when an exact
price-card match is available. Missing usage or a card mismatch leaves the
entry unpriced without interrupting chat.

The Usage Ledger's `KV` percentage uses only responses for which the provider
reports a cache hit/miss split. Responses without cache telemetry are omitted
from that aggregate ratio; their input tokens remain in the ledger. The
workbench status row shows the latest response's reported KV percentage so it
answers whether the current prompt prefix was reused, while its token, cost,
and time fields cover the latest completed user request. Its `ctx` ratio pairs
that latest response's input with the effective model window; it is not a
session-ledger total. `/usage` and `/cost` remain cumulative across the session
tree.

If a request is stopped after the provider has already reported usage, that
observation is still written once to the ledger; Agul never invents usage for
a stopped request that reported none.

| Connection | Session billing and ledger behavior |
| --- | --- |
| Official DeepSeek | Uses its embedded or selected provider price card. |
| GLM Coding Plan | The session reports `billing: "subscription_quota"`; its ledger records provider `glm`, Coding Plan origin, tokens, cache telemetry, and `unpriced_reason: "subscription_quota"`. |
| Explicit ordinary GLM API URL | Uses the matching embedded or selected provider price card; it is not the `glm` preset. |
| Custom proxy or local endpoint | Retains its real origin and remains unpriced unless `--price-card` supplies its rates. |
| ChatGPT account through Codex | The session reports `billing: "chatgpt_quota"` and `cost: null`; its provider `codex` ledger uses `unpriced_reason: "subscription_quota"`. |

`agul account status` reports the ChatGPT plan and current Codex quota windows.
An account-backed parent can still display the real price-card cost of linked
native child sessions as mixed billing.

## Price catalogs

Agul includes separate dated catalogs for the official DeepSeek and GLM API
origins. Rates remain in versioned JSON rather than being copied into prose or
terminal strings. Inspect the active selection or synchronize an explicitly
configured machine-readable source with:

```console
agul price status
agul price status --provider glm
agul price sync --url https://prices.example/agul.json
agul price sync --provider glm --url https://prices.example/glm.json
```

`AGUL_PRICE_CATALOG_URL` may supply the sync URL. A successful sync stores an
immutable provider-scoped catalog and remembers the source for later checks.
The downloaded catalog must match the selected provider, origin, and model
before it can replace the last known good card.

The official pricing pages are HTML rather than documented catalog APIs, so
Agul does not scrape them. Sync accepts only an explicitly configured
`agul/price-catalog/v0.3` JSON source. Without one, chat makes no pricing network
request. With one, it checks at most once every 24 hours with a two-second total
timeout; failure does not block chat or replace the cached card. The embedded
catalog sources are the
[official DeepSeek pricing page](https://api-docs.deepseek.com/quick_start/pricing/)
and [official GLM pricing page](https://bigmodel.cn/pricing).

Provider dialect and price origin are separate. A custom endpoint remains
unpriced unless its actual rates are supplied explicitly:

```console
agul chat --price-card ./prices.json
```

The workbench displays cost to three decimal places. `/cost` also prints the
price-card ID and version; `≈` means at least one response could not be quoted
exactly. Saved priced entries retain their chosen band and exact rates, so a
newer catalog never recalculates old usage.

The persistence contracts are
[`chat-session-v5.schema.json`](../schemas/chat-session-v5.schema.json) and
[`trace-event-v1.schema.json`](../schemas/trace-event-v1.schema.json).
