# Changelog

## 0.6.0-rc.1 — release candidate

- Made `glm` the canonical GLM Coding Plan preset while retaining
  `glm-coding` as an input compatibility alias; the ordinary GLM API now
  requires an explicit endpoint instead of occupying the primary provider name.
- Added current-workspace `chat --continue` and a searchable `chat --resume`
  picker, while retaining exact `--session <id>` automation. Resume candidates
  skip empty chats, active roots, delegated sessions, and old schemas; restored
  chats show one folded history marker instead of replaying terminal output,
  and concurrent processes cannot overwrite the same resumed chat.
- Kept the floating composer interactive during streamed model and tool work,
  with steering input, immediate `/` and `@` menus, `/stop`, Ctrl+C, preserved
  drafts, stable scrollback, and a compact saved-session exit line.
- Promoted a native provider's reasoning-only completion to the final Assistant
  style in the full-screen workbench without replaying its streamed text.
- Made cancellation reach native HTTP/SSE waits, Codex `turn/interrupt`,
  compaction, Plugin commands, and shell or Plugin process trees. Usage already
  reported before cancellation remains in the per-response ledger.
- Changed the workbench `KV` field to the latest provider response while
  preserving request and session aggregates in the per-response Usage Ledger
  and `/usage`.
- Rejected explicitly configured Plugin directories that contain no immediate
  `plugin.json` manifests, and surfaced directory-entry inspection failures.
- Prepared four-platform archives with the project license, the locked Cargo
  dependency snapshot, and practical third-party notices.

## 0.6.0-alpha.4 - 2026-08-29

- Scoped every workbench metric to the latest completed user request while
  keeping `/usage`, `/cost`, and the per-response ledger cumulative. Stabilized
  the count labels as `turns <n>` and `tools <n>`.
- Made the deterministic terminal demo prove an exact warmed-request replay
  before reporting its declared KV fixture data.
- Defined `main` as the formal release line and `dev` as the development and
  internal-use integration line.

## 0.6.0-alpha.3 - 2026-08-29

- Made `agul` open the scrolling terminal workbench by default, with a floating
  multiline composer, streamed reasoning and tool activity, and one compact
  model/effort/turn/tool/token/KV/time/cost row.
- Reduced the native runtime to four general tools: `read`, `write`, `edit`, and
  `shell`. Added strict `agul/plugin/v2` external tools and slash commands while
  keeping launch-free sessions at exactly four built-ins.
- Added project, ancestor, prepared user, `.codex`, and `.claude` Skill
  discovery with `@skill:name` activation. Thin Agulater launches and
  conditional `system/agulater` and `system/agentkube` Skills connect the
  runtime to optional lifecycle management and AgentKube content without adding
  another core tool.
- Added durable `agul/chat-session/v5` sessions with exact native provider
  history, Codex upstream-thread resume, traces, interrupted-turn recovery,
  parent/child attribution, handoffs, aggregate usage, and manual transactional
  semantic compaction.
- Added a per-response Usage Ledger, provider-scoped dated price catalogs,
  periodic catalog synchronization, subscription-aware unpriced entries, and
  three-decimal terminal cost while retaining exact stored rates.
- Added first-class DeepSeek and GLM native connections, including the separate
  `glm-coding` subscription preset, plus explicit local OpenAI-compatible
  endpoints and stable prompt-prefix reuse for small-context workers.
- Added ChatGPT browser/device login through the supported Codex app-server,
  quota status, account-backed chat, live Web Search, upstream conversation
  resume, and per-response subscription usage.
- Defined ARI 0.2 as one stdin/stdout JSON-RPC service sharing Agul's project
  loader, model loops, Skills, Plugins, sessions, traces, handoffs, and billing.
  Its capability matrix distinguishes native Agul tools from Codex-owned tools.
- Added the repository's thin `.agents` self-maintenance entrypoint and CI
  regression gates for first-event latency, total response time, peak memory,
  and warm-turn prompt-prefix reuse.
