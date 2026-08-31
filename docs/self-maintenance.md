# Self-maintenance

Agul works on its own checkout through the same path it uses for any project.
The repository contains a thin `.agents` package, so its instructions and
launch are discovered automatically.

```console
cd agul
agul chat --reasoning-effort high
```

A useful request names one concrete problem and ends with observable checks:

```text
Find the cause of the failing test, fix it, run the focused test, then run the
relevant full Cargo checks. Summarize the changed files and anything unfinished.
```

The model uses Agul's ordinary four tools: `read` and `edit` for focused source
work, `write` for new files or complete rewrites, and `shell` for search, Cargo,
Git, formatting, and other project commands. Tool failures return to the same
model loop so it can inspect the result and try again.

There is no separate self-maintenance mode or special Git tool. Ask for a
commit or push when it belongs to the task, and the model uses `shell` just as
it uses Cargo. Saved sessions make longer repairs resumable; manual `/compact`
can reduce older visible turns without changing the four-tool runtime.

## Prepared help and delegation

Agulater can prepare project context, Skills, a harness, and Plugins before Agul
starts. AgentKube supplies optional specialist packages and a coordinator
Plugin. Together they let one capable Agul instance remain the master while
delegating narrow work to independent Agul sessions through ARI.

A small 32K local model is usually most useful for one checkable task, such as
a documentation edit, repository search, focused test, or short patch. The
master should retain cross-file decisions, final verification, and integration.
This keeps the local worker's prompt focused while its session, handoff, trace,
and usage remain visible to the caller.

Use [Skills and launches](skills.md) for prepared inputs,
[Plugins](plugins.md) for external tools, and the [ARI reference](ari/README.md)
for delegated sessions. The optional [ChatGPT account mode](codex-account.md)
can provide a Codex-backed master with live Web Search; native four-tool
execution remains the default when prepared Agul Plugins are required.
