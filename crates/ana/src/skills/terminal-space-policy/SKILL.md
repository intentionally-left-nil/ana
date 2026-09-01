---
name: terminal-space-policy
description: Proposes a policy change through the terminal-space MCP tool to allow a package or conda channel that ana's own configuration does not currently authorize, after a dry-solve reports it would only succeed with a widened channel search. Use when ana sync --dry exits with its widened-channels code (9), or when a package needs to be added to an allowlist before ana can solve for it normally.
---

# Terminal Space policy

TODO: not yet implemented. Describe how to use the `terminal-space` MCP
tool (the `terminal-space` server registered in this Kilo session) to
propose an allowlist/policy change for the package(s) or channel(s) a
dry-solve reported needing widening for.

Until this is filled in: stop here and report the widened-channels
finding to the user instead of proceeding as if a policy change had
been made.
