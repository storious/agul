# Contributing to Agul

Agul is built around one continuous product path: load a project, talk to a
model, let it use four general tools, stream progress in the terminal, and keep
the session useful across turns. Changes should make that path work better.

## Work on the checkout

1. Read `.agents/AGENTS.md`.
2. Reproduce the behavior from the CLI or its nearest test.
3. Change the smallest coherent area.
4. Run a focused test while iterating.
5. Run the full checks before handing off a substantial change.

```console
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Branch and release flow

- `main` is the formal, release-ready line. Tags and releases come from
  `main`.
- `dev` is the integration line for development, routine maintenance, and
  internal use.
- Start product features from the latest `main` as `feat/<short-slug>`.
- Start bug fixes from the latest `main` as `fix/<short-slug>`.
- Merge feature and fix branches into `dev` first. They must pass CI and the
  repository checks and prove useful in internal use before release.
- Promote `dev` to `main` when it contains a meaningful, complete release
  batch. This is a product milestone, not a requirement to accumulate one
  oversized diff.
- After a release, bring the resulting `main` state back into `dev` before the
  next release cycle diverges.

Routine documentation, dependency, and repository maintenance may land on
`dev` without a dedicated feature branch. Keep `dev` usable, and do not tag or
publish releases from `dev`, `feat/*`, or `fix/*`.

Use `fix/*` for defects reproducible from `main`. A defect found only in
unreleased feature work remains part of its `feat/*` branch during internal
use. Delete work branches after their changes reach `main`.

## Product shape

- Keep `agul` as the default interactive entrypoint.
- Prefer `read`, `write`, `edit`, and `shell` over adding narrow built-ins.
- Keep model reasoning, answer text, and tool activity visible while a turn is
  running.
- Keep package files readable and compatible with Agulater's thin launch.
- Keep ARI on the same chat and session paths instead of building a second
  runtime.
- Put optional integrations in Skills, Plugins, specialist packages, or
  AgentKube rather than growing the core CLI.

## Documentation

Document behavior that exists in the current source. Keep the README as the
short path to first use and place detail in the small set under `docs/`. Update
the example package when the thin launch format changes. When the workbench's
visible layout changes, refresh the terminal demo so it shows the released UI.

Batch related changes into a meaningful commit. Do not create release notes or
historical design documents for unfinished ideas.
