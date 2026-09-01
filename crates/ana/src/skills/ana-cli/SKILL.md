---
name: ana-cli
description: Explains how to drive the ana CLI -- project-scoped conda environments for Python projects, solved and locked from pyproject.toml/requirements.txt/environment.yml/PEP 723 scripts. Use when running, syncing, inspecting, or troubleshooting a Python environment with ana, or when ana's exit codes (1 solve failure, 9 widened dry-solve, 10 script-assist declined) need interpreting.
---

# ana CLI

ana gives a project its own conda environment, solved and locked from its
manifest. NEVER use `pip`, `uv`, `conda`, or `pixi` to install into or
work around an ana environment -- they bypass the lockfile and channel
policy. If ana can't solve something, the fix goes through ana (see the
`ana-dependency-check` and `terminal-space-policy` skills), not another
package manager.

## Commands

- `ana run <primary> [program] [-- args...]` -- sync an environment,
  then run a command in it. What `<primary>` means depends on `-g`
  (below). Everything after a literal `--` is passed to the executed
  program untouched -- and in project mode ALL program arguments need
  it, even positional ones: `ana run python script.py` is an error
  (ana tells you so and shows the corrected command), because a second
  positional only has meaning under `-g`.

  **Project mode** (no `-g`): `<primary>` is the literal program to
  run inside the project's environment -- the one solved from the
  working directory's `pyproject.toml`, `requirements.txt`, or
  `environment.yml`:

  ```sh
  ana run pytest                        # run the project's test runner
  ana run python -- -m myapp            # flags and positionals alike...
  ana run python -- script.py --verbose # ...go after the literal --
  ana run --group test pytest           # include an extra dependency group
  ```

  If `<primary>` names a `.py` file with PEP 723 inline metadata, the
  script is its own project: ana solves from its `dependencies` block
  instead of any project manifest (`--manifest` overrides even that
  block). A `.py` file *without* metadata is routed to a Kilo
  script-assist session unless `--agent off|headless` says otherwise.

  **Global mode** (`-g`): there is no project at all -- `<primary>` is
  parsed as a *requirement* joining an ad hoc environment, and the
  program to run is derived from it (a `cowsay` requirement runs
  `cowsay`). The optional `[program]` slot -- the only place a second
  positional is legal -- overrides that derivation. `--group` and
  `--manifest` are illegal here because a CLI-declared environment has
  neither:

  ```sh
  ana run -g cowsay -- hello            # solve cowsay, run `cowsay hello`
  ana run -g "fastapi>=0.141" uvicorn   # requirement != program name
  ana run -g python -- -c "pass"        # derive `python`, pass it -c
  ```

  `-i <spec>` (repeatable, both modes) adds an extra requirement on
  top of whatever environment was targeted -- PEP 508, or a conda
  MatchSpec via `::`, e.g.
  `ana run -g fastapi -i "starlette>=0.46" uvicorn` or
  `ana run -i conda-forge::r-base Rscript`.
- `ana sync` -- bring the environment up to date without running
  anything. `--dry` prints the plan and writes nothing; `--frozen`
  fails instead of updating a stale `ana.lock`; `--clean` reinstalls
  from scratch.
- `ana info` -- preview what a sync would produce (manifest in effect,
  sync status, package set) without changing anything.
- `ana search <spec>` -- does a package exist on the configured
  channels? Accepts a bare name, `name>=version`, or
  `channel::name`. Exit 0 = matches, 1 = none matched, 2 = query
  couldn't run.
- `ana clean` -- delete materialized environments, keep lockfiles.
- `ana config get` / `ana config set <key> <values...>` -- inspect or
  edit config.toml. `set` overwrites the whole value: for list keys
  (`allowed_channels`, ...) repeat every existing value you want to
  keep. Never run `config set` on a user's behalf unless they asked
  for exactly that change -- config is theirs.

Manifests are auto-detected in the working directory
(`pyproject.toml`, `requirements.txt`, `environment.yml`, or a PEP
723 script for `run`). `--manifest <path> --manifest-type <kind>`
overrides both; `<kind>` is `pyproject`, `requirements-txt`, or
`environment-yml` (PEP 723 is only ever auto-detected from a `.py`
file, never `--manifest-type`).

## Exit codes worth knowing

- `1` -- ordinary failure, most often a solve failure: the requirement
  can't be met by the configured channels. Read the solver message;
  `ana search <spec>` confirms whether the package exists at all.
- `9` (`ana sync --dry` only) -- the printed plan solved *only* after
  widening to `dry_solve_channels`; a real sync would still fail. Load
  the `terminal-space-policy` skill.
- `10` (`ana run <script>.py` only) -- the script has no PEP 723
  metadata and the script-assist session ended without adding any.
  The `python-script-requirements` + `ana-dependency-check` skills
  cover that flow.
