# Fixtures

`packages/empty-0.1.0-h4616a5c_0.conda` is copied verbatim from
`intentionally-left-nil/rattler`'s own test data
(`test-data/packages/empty-0.1.0-h4616a5c_0.conda` at the rev this
workspace pins), used there for the fork's own `PackageCache`/`Installer`
unit tests. It's a genuinely empty, no-payload `noarch: generic` conda
package -- ideal for exercising the install pipeline (download, hash
verify, link) without needing a hand-built archive. The upstream repo is
BSD-3-Clause licensed (see its `LICENSE`); this copy carries the same
license.

Metadata (`info/index.json`), verified directly:

```json
{
  "build": "h4616a5c_0",
  "build_number": 0,
  "name": "empty",
  "noarch": "generic",
  "subdir": "noarch",
  "version": "0.1.0"
}
```

`sha256`: `af8000ad3ad6af83b294b0e700f7c6f17fa85c6b9db08207813f47af8a94d52c`,
`size`: 1538 bytes.
