# Agul {{TAG}}

Agul is a small terminal agent runtime that can inspect, change, run, and verify
real projects. It keeps a four-tool native core while Agulater prepares focused
launches and AgentKube supplies optional extensions.

![Agul terminal demo](https://github.com/storious/agul/releases/download/{{TAG}}/agul-demo.gif)

## Current capabilities

- A blue, black, and gold scrolling workbench with a floating multiline input,
  `/` and `@skill:name` completion, visible reasoning, appended tool activity,
  and a single compact latest-request status line.
- A launch-free native loop with exactly `read`, `write`, `edit`, and `shell`;
  prepared launches can add external tools through `agul/plugin/v2`.
- DeepSeek, GLM Coding Plan, and local OpenAI-compatible connections, plus
  a separate ChatGPT account engine backed by the official Codex app-server.
- Codex allowance, upstream conversation resume, and Codex-owned live Web
  Search without pretending subscription quota is an API charge.
- Durable visible sessions, manual semantic `/compact`, per-response Usage
  Ledger entries, provider cache telemetry, and versioned price references.
- Project and user Skills, virtual System Skills for available host commands,
  and Agulater/AgentKube discovery without increasing the built-in tool count.
- ARI 0.2 over stdin/stdout for native and account-backed sessions, including
  streamed reasoning, tool progress, durable traces, relations, and usage.

## Install

Agul does not require Bun or Node.js. The installers attached to this release
are pinned to this tag.

Linux or macOS:

```console
curl -fsSL https://github.com/storious/agul/releases/download/{{TAG}}/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://github.com/storious/agul/releases/download/{{TAG}}/install.ps1 | iex
```

The release also contains standalone platform archives. Agulater is optional;
install it separately only when managed updates or AgentKube packages are
needed. Agulater's published installer is also standalone; ordinary users do
not need Bun, Node.js, or npm.

## Try it

Minimal native mode:

```console
export DEEPSEEK_API_KEY='<key>'
cd my-project
agul
```

GLM Coding Plan:

```console
export GLM_API_KEY='<key>'
agul chat --provider glm
```

ChatGPT allowance and real Web Search:

```console
agul account login
agul account status
agul chat --engine codex --reasoning-effort high
```

One non-interactive maintenance turn:

```console
agul chat --prompt "find the failing test, fix it, and rerun it" --json
```

Small local models remain useful as short-task workers for documentation,
focused tests, repository search, and contained patches. A capable primary
model can verify and integrate their results through an optional coordinator.

## The three-part boundary

- **Agul** runs models, tools, sessions, usage, and ARI.
- **Agulater** installs or updates Agul and extensions, resolves Catalogs, and
  prepares `.agents` launches; it does not run the model loop.
- **AgentKube** publishes optional Skills, Plugins, Agents, coordinators, and
  starters; it is not another wrapper CLI.

The release archive contains the binary, README, Apache-2.0 license, locked
Cargo dependency snapshot, third-party notices, contribution guide, demo,
and documentation. Start with its local `docs/README.md` or the tagged
[documentation index](https://github.com/storious/agul/blob/{{TAG}}/docs/README.md).
