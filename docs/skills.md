# Skills and launches

Agul consumes the small `agul/launch/v2` runtime file normally prepared by
Agulater. Agulater owns package authoring, Catalog resolution, installation,
and preparation; its
[documentation](https://github.com/storious/agulater) is the authority
for `agulater/package/v2` and its commands. AgentKube supplies optional
extension content rather than another runtime.

This page describes only what Agul loads. A minimal launch uses paths relative
to the launch file:

```json
{
  "format": "agul/launch/v2",
  "instructions": "../AGENTS.md",
  "skills": "../skills"
}
```

## Discovery

Without `--launch`, Agul looks for `.agents/runtime/launch.json` from the
workspace upward through its parent directories. If none exists, it tries the
user package at `~/.agents/runtime/launch.json`. `--launch <path>` selects one
file directly.

Project launches use their declared instructions. A user-level launch is
combined with the nearest project `.agents/AGENTS.md` or root `AGENTS.md`.
Without a launch, Agul uses that nearest project instruction directly.

Skill names use the first matching directory in this order:

1. The project launch directory.
2. `.agents/skills` from the workspace and its project ancestors.
3. The prepared user launch directory, even when a project launch is active.
4. User `~/.codex/skills`, then `~/.claude/skills`, then raw
   `~/.agents/skills`.

A Skill is a directory containing `SKILL.md`. Its frontmatter supplies `name`
and `description`. Duplicate names use the first discovered Skill.

Type `@skill:name` to include that Skill in the current request. Type
`@@skill:name` when the text should remain literal.

## System Skills

System Skills are small runtime-generated hints about capabilities already
present on the host. They do not add tools or become project package content.
Agul exposes one virtual Skill per detected command: `system/rg`, `system/fzf`,
and `system/git`. When Agulater is on `PATH`, `system/agulater` explains how to
use it through the existing shell tool. If its local
`agulater catalog list --json` report includes the registered `agentkube`
catalog, `system/agentkube` also appears. The probe is local and bounded; it
does not refresh or read the catalog itself. AgentKube remains extension
content, not another CLI.

On Windows, command discovery follows `PATHEXT` and includes `.cmd` and `.bat`
shims. `AGUL_HOST_TOOLS=tool-a,tool-b` can declare other commands the user
knows are available, producing `system/tool-a` and `system/tool-b`. A user can
also tell the model about a command directly in the request.

These commands run through the existing `shell` tool. Because its stdin is
non-interactive, the `fzf` hint supplies explicit input, for example
`rg --files | fzf --filter QUERY`. Agul checks only the named executables and
does not scan for or install host software. System Skills appear in `/skills`
and activate with the same `@skill:system/rg` syntax. Their base prompt entries
remain one-line summaries; full usage instructions enter the request only
after activation. They cannot be replaced by a package Skill of the same name.

The optional `plugins` launch entry loads external tools described by
[`agul/plugin/v2`](plugins.md). Specialist registries, pools, harnesses, and
snapshots remain Agulater output consumed by plugins; they are deliberately
absent from the small `agul/launch/v2` runtime contract.
