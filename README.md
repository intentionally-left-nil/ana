# ana

An agentic package manager for Python. You ask for a package; `ana` figures
out whether it's a conda package or a wheel, solves for both at once, and
runs your command inside the result.

Ana manages your environments, channels, and uses AI to help manage policies so you don't have to

```sh
curl -fsSL https://raw.githubusercontent.com/intentionally-left-nil/ana/main/install.sh | bash
```

## `ana run`

```sh
ana run pytest
```

That's the whole model. `ana` finds your `pyproject.toml` (or
`requirements.txt`, or `environment.yml`), solves it, writes `ana.lock`,
materializes `.env/`, and execs `pytest` inside it. Every later `ana run` is
a no-op check against the lock.

Arguments go after `--`, so `ana`'s flags never collide with your program's:

```sh
ana run pytest -- -k test_solver --tb=short
```

Need something that isn't in the project? Add it for one command:

```sh
ana run -i ipdb pytest -- -x
```

## No project? `-g`

`-g` (--global) builds a throwaway environment from the command line itself. The spec
*is* the argument, and the program name is inferred from it:

```sh
ana run -g ruff -- check .
ana run -g 'fastapi[standard]' -- dev app.py
```

Name the program explicitly when it differs from the package:

```sh
ana run -g httpie http -- get https://example.com
```

Global environments are cached and keyed by their specs, so the second run
doesn't solve. `ana clean --global` throws them all away.

## Mixing PyPI and conda

One solve, one lock, one environment. `::` anywhere in a spec means "this is
a conda MatchSpec"; everything else is PEP 508.

```sh
ana run -g -i ::python==3.11 'polars>=1.0' python -- -c 'import polars'
```

`polars` is a pypi-style dependency, `python` is pinned by conda, and they both solve together.

```toml
[project]
requires-python = ">=3.11"
dependencies = ["polars>=1.0", "httpx"]   # PEP 508, resolved via PyPI

[tool.ana]
matchspec-dependencies = ["compilers", "conda-forge::pyarrow"]
```

`requires-python` becomes a `python >=3.11` matchspec automatically. Groups
work the same way — `[dependency-groups]` for PEP 508, `[tool.ana.matchspec-dependency-groups]`
for conda, both selected by `--group dev`.

In `requirements.txt`, comments carry the conda half:

```
httpx>=0.27
# ana-matchspec: numpy >=1.26
# ana-channels: conda-forge
```

And a PEP 723 script needs nothing but itself:

```sh
ana run analyze.py -- --input data.csv
```

## What you don't write

| | |
|---|---|
| `environment.yml` | `pyproject.toml` already says what you need |
| `conda activate` | `ana run` puts you inside the env for exactly one command |
| `pip install` after `conda install` | one solver sees both halves, so they can't disagree |
| a `.python-version` | `requires-python` is a real constraint in the solve |
| "which env was this again?" | the environment is a function of the manifest |

## Security

**Hard channel enforcement.** `allowed_channels` is a whitelist, not a
preference. A channel is authorized by exact match or by an explicit
`https://host/pkgs/main/*` prefix rule — `.../pkgs/mainline/` will not
match. `file://` channels, credentialed URLs, and `/t/<token>/` channels are
rejected outright. Authorization is decided from the artifact's real
download URL, never from the `channel` field a package claims for itself, so
a hand-edited `ana.lock` can't smuggle one in. The channel set is hashed
into the lock, so widening it registers as staleness.

**Sandboxing.** Packages from a `sandboxed_channels` channel force the whole
run under [nono](https://github.com/intentionally-left-nil/nono_packs). The
default profile makes the environment prefix read-write (pip needs it) and
your working directory read-only, redirects every cache and config location
into the prefix, and execs with a **cleared** environment — the sandboxed
process never sees the secrets sitting in your shell. `ana info` tells you
whether a sync would produce a sandboxed environment, before you run it.

**Enterprise binding.** The `ana-enterprise` build has its config compiled
into the binary at release time. `config.toml` is never read, `ana config
set` refuses to run, and no community defaults apply — if a channel isn't in
the baked-in `default_channels`, it does not exist. Policy travels with the
binary, not with the user's home directory.

```sh
curl -fsSL .../install.sh | bash -s -- enterprise
```

## Agents

Run `ana` with no arguments and you get an agent session that can drive all
of the above. It's provisioned with `uv`, `pip`, `conda`, and `pixi` denied
at the shell level — an agent cannot sidestep the lockfile — plus skills for
deriving dependencies from imports and for probing feasibility with
`ana search` / `ana sync --dry` before touching anything.

`ana run script.py` on a script with no metadata routes to that same agent
to write the `# /// script` block, asks you before installing anything
unfamiliar, and hands control back.

## Commands

| | |
|---|---|
| `ana run` | run a command in the environment |
| `ana sync` | update the environment without running anything (`--dry`, `--frozen`) |
| `ana info` | what the environment is, and what a sync would change |
| `ana search` | query the authorized channels |
| `ana clean` | drop materialized environments, keep the locks |
| `ana config` | inspect the effective config |
| `ana login` | log in to Anaconda.org |
