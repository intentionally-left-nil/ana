---
name: python-script-requirements
description: Determines a Python script's runtime dependencies by reading its import statements, separating standard library modules from third-party packages, and mapping import names to PyPI distribution names. Use when a .py script has no PEP 723 inline metadata and its dependencies need to be figured out from its source rather than from a manifest.
---

# Python script requirements

## Workflow

1. Read the whole script.
2. Collect every top-level `import x` / `import x.y` / `from x import ...` statement. Ignore:
   - Relative imports (`from . import foo`, `from .utils import bar`) -- these are the script's own local modules, not dependencies.
   - Imports of a local file/package sitting next to the script (check the script's directory before assuming a name is a PyPI package).
   - Imports inside a `try`/`except ImportError` fallback where the `try` branch is a pure convenience (e.g. picking a faster implementation) -- note both branches, but only the one actually needed matters.
3. For each remaining top-level module name, decide:
   - **Standard library** (e.g. `os`, `json`, `pathlib`, `dataclasses`, `typing`): drop it, no dependency needed.
   - **Third-party**: keep it. Its PyPI distribution name is usually the import name itself, but some differ -- e.g. `cv2` → `opencv-python`, `PIL` → `pillow`, `yaml` → `pyyaml`, `sklearn` → `scikit-learn`, `bs4` → `beautifulsoup4`, `dotenv` → `python-dotenv`. Use your own knowledge of the ecosystem for others; don't guess a name you don't actually recognize.
4. For every third-party candidate, judge whether it's a real, well-known package from your own training knowledge. If it looks unfamiliar, obscure, unusually named, or could plausibly be the user's own private/local module rather than a published package, use the `question` tool to confirm it with the user by name *before* including it in the candidate list. Never assume a package is legitimate or popular just because the script imports it -- an unfamiliar import is exactly as likely to be a typo, a private package, or something malicious as it is to be real.
5. Hand the confirmed candidate list (PyPI distribution names, plus any version constraints visible in the code or worth asking about) to the `ana-dependency-check` skill to verify ana can actually solve them.

## Example

```python
import json          # stdlib -- drop
import numpy as np   # numpy -- keep
from sklearn import svm  # sklearn -- keep, but its distribution is scikit-learn
import obscurelib     # unfamiliar -- ask the user before including it
```

Candidate list to hand off: `numpy`, `scikit-learn`, plus a decision on `obscurelib` from the user.
