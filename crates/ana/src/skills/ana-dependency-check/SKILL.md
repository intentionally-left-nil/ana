---
name: ana-dependency-check
description: Checks whether a candidate set of Python/conda dependencies can actually be solved by ana, using ana info and ana sync --dry against a temporary manifest so nothing is written to the real project, plus ana search to confirm an individual package's availability. Use when asked whether a dependency set can be installed, or before any new dependency is committed to a project or script.
---

# ana dependency check

There is no `ana solve` command -- feasibility is checked with `ana info`, `ana sync --dry`, and `ana search`.

## Workflow

1. Write the candidate dependencies to a temporary `requirements.txt` (one PEP 508 requirement per line, e.g. `numpy>=1.26`). For a dependency that only exists as a conda package (no PyPI equivalent), use ana's own directive instead of a plain line:
   ```
   # ana-matchspec: <matchspec>
   ```
   To search extra channels for this check only, add (once, file-level):
   ```
   # ana-channels: <channel>, <channel>
   ```
2. Preview the plan without installing or writing anything real:
   ```
   ana info --manifest <tmp-file> --manifest-type requirements-txt
   ```
3. Confirm under the exact real-sync path:
   ```
   ana sync --dry --manifest <tmp-file> --manifest-type requirements-txt
   ```
   Exit codes:
   - `0` -- solves normally with the current channel configuration.
   - `9` -- solves *only* after widening to `config.toml`'s `dry_solve_channels`. Load the `terminal-space-policy` skill next; do not silently proceed as if this were a normal `0`.
   - anything else -- does not solve at all. Report the failure plainly; do not proceed to editing any file.
4. If a specific package's existence (rather than the whole set's solvability) is in question, check it directly instead of inferring from a full-solve failure:
   ```
   ana search <spec>
   ```
   `<spec>` is a bare name, a version constraint (`numpy>=2`), or a channel-qualified matchspec (`conda-forge::numpy`).
5. Report one of three outcomes to whoever asked for this check: solves cleanly, solves only via widened channels (handed to `terminal-space-policy`), or does not solve (with the reason).

Clean up the temporary manifest file once the check is done.
