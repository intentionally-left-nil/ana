---
name: terminal-space-policy
description: Authors a terminal-space policy draft admitting packages a dry solve could only reach via widened channels, then walks the user through promoting the draft, binding it to a channel, and authorizing that channel in their ana config. Use when ana sync --dry exits with its widened-channels code (9), or when a package needs a policy change before ana can solve for it.
---

# Terminal Space policy

An exit code of `9` from `ana sync --dry` means the solve only succeeded
after widening to `dry_solve_channels`. The fix is a terminal-space
*policy* admitting those packages, bound to a channel the user's config
then authorizes -- not a config edit you perform. You may NEVER run
`ana config set` or edit `config.toml`; config belongs to the user.

## Workflow

Copy this checklist and check off each step:

```
- [ ] 1. Dry-solve and list the needed packages
- [ ] 2. Create the policy draft
- [ ] 3. Have the user promote it and bind it to a channel
- [ ] 4. Test-solve against that channel
- [ ] 5. If "channel not allowed", give the user the exact config line
- [ ] 6. Report the outcome and ask the user to exit (Ctrl-C)
```

**Step 1: Dry-solve.** Re-run `ana sync --dry` and read the
`+ name version` plan lines. NEVER redirect or suppress its output
(no `> /dev/null`, no `| tail`, no `-q`): the plan is how you learn
which packages to admit, and the user needs to see it too. The draft must admit EVERY package the
plan sources from the widened channel, not just the user's direct
requirements: the policy is deny-by-default, so a missing transitive
dependency fails the solve exactly as a missing direct requirement
would. The widened channel is a PyPI mirror, so that means every
pure-Python package in the plan (fastapi, starlette, pydantic, ...);
interpreter/system packages (python, openssl, bzip2, tk, ...) come
from the default channels and need no rule.

**Step 2: Create the draft.** Call `terminal-space:create_draft` (no
`draft_id`) with:

- `name`: a slug for a new policy (e.g. `allow-fastapi-0141`), OR
  `policy`: `{owner}/{name}` of an existing policy to revise (check
  `terminal-space:list_drafts` first -- one active draft per policy;
  pass its `id` as `draft_id` to keep editing it). Save the returned
  draft `id` for corrections.
- `default_effect`: `"deny"` (policies are allow-by-exception).
- `rules`: one allow rule per package from step 1, e.g.
  `[{"effect": "allow", "namespace": "pypi", "name_op": "eq",
  "name": "fastapi", "version_spec": ">=0.141.1"},
  {"effect": "allow", "namespace": "pypi", "name_op": "eq",
  "name": "starlette"}, ...]`.
  `namespace` is `"pypi"` for PyPI-sourced packages, `"conda"` for
  native conda packages. Put a `version_spec` (a conda version
  specifier) on the direct requirements the user asked for; transitive
  dependencies usually get none (any version the solver picks is
  fine).

**Step 3: Human promotion.** Use the `question` tool: a draft is ready;
a human must go to https://repo.terminal.space, **promote** it (not
exposed to MCP tools on purpose), and **bind** the promoted policy to
a channel (creating one if needed). Ask which channel they bound it to.
Accept a full channel URI or a bare `owner/name` (e.g.
`ash/fast_things_only`); a bare name expands to
`https://repo.terminal.space/api/channels/<owner>/<name>`. If their
answer is ambiguous, `terminal-space:list_channels` lists visible
channels by owner or search text. Then verify the bind yourself with
`terminal-space:get_channel_policies(channel)`: the promoted policy
must appear in the ordered bindings, `enabled`, at the revision just
promoted. If it doesn't, the user bound the wrong policy or channel --
go back and ask before solving.

**Step 4: Test-solve.** Binding triggers an asynchronous channel
rebuild, so first poll `terminal-space:get_channel_status(channel)`
until `rebuild_state` is `idle` and `stale` is false --
`last_rebuilt_at` advancing past the bind time is the signal the new
revision is live (solving against a mid-rebuild channel can read the
previous policy's packages). Then try the channel without writing
anything real, per the `ana-dependency-check` skill:
`# ana-channels: <channel-uri>` in a *temporary* manifest copy (never
the user's real manifest), then
`ana sync --dry --manifest <tmp> --manifest-type <kind>`.

- Solves -> step 6.
- "not in default_channels/allowed_channels" -> step 5.
- Anything else (still no candidates, an unadmitted version, a server
  error): diagnose from the output. The most common miss is a solve
  error like "X would require Y, for which no candidates were found"
  -- Y is a transitive dependency the draft's rules didn't admit;
  add an allow rule for it. Valid retries: fix the draft's
  rules/`version_spec` via `terminal-space:create_draft` with the
  saved `draft_id` (or open a new draft against the promoted policy
  if it was already promoted) and ask the user to re-promote, or
  confirm via `get_channel_policies` they bound the right
  policy/channel. After a re-promote, re-check
  `get_channel_policies` (revision advanced) and `get_channel_status`
  (`last_rebuilt_at` advanced) before re-solving. At most two
  alternations; if it still fails, stop and report the blocker
  plainly.

**Step 5: Channel not allowed.** Tell the user their config must
authorize the channel, and give the exact line for THEM to run:

```
ana config set allowed_channels <existing values...> <new channel uri>
```

(`ana config get allowed_channels` shows the current values; every
existing value must be repeated or it is lost.) Do NOT run this
yourself. Retry the solve once the user says it is done.

**Step 6: Wrap up.** Delete your temporary manifest. Summarize: the
policy admitted and the channel binding it, the config the user added,
and the command that now works. End by telling the user the task is
complete and they should press Ctrl-C to exit -- an interactive session
cannot end itself, and whatever launched it is waiting.

## Rules

- NEVER run `ana config set` or edit `config.toml`. Give the user the
  exact command; they run it.
- NEVER promote a draft or claim one was promoted -- promotion is a
  human-only web UI action. If unsure whether the user promoted,
  `terminal-space:list_drafts`: a promoted draft no longer exists.
- Binding a policy to a channel is also a human UI action, but the
  result is verifiable: `terminal-space:get_channel_policies` shows
  what is actually bound, and `terminal-space:get_channel_status`
  shows whether the rebuild it triggered has finished. Check both
  rather than taking the user's report on faith.
- Filesystem writes are temporary manifest copies only; remove them
  when done.
