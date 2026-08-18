# @nubjs/build-jail-catalog

Measured capability grants for dependency lifecycle scripts, read by [nub](https://nubjs.com)'s build jail. This package holds data only — no code runs from it.

```sh
nub pm catalog fetch     # download and install the newest published catalog
nub pm catalog path      # where the installed catalog lives
```

Nub confines every dependency install script it runs, and each entry records what that package was measured to need:

| grant | what it covers |
| --- | --- |
| `network` | whether the script may reach the network at all |
| `write` | which scopes it may write — its own package, the dependency tree, the project |
| `writePaths` | caches nub relocates out of the throwaway home so they persist |

A package with no entry gets a conservative default profile instead, so an unknown package is never granted more than a measured one.

The catalog is also compiled into the nub binary. Fetching one matters because a grant correction otherwise waits for a nub release, and a wrong grant can stop a package installing.

## Why this ships separately

Grants are measurements, and measurements get corrected. Publishing them apart from the CLI means a fix reaches users on its own schedule.

The version tracks the date the catalog was generated rather than any nub version, so `2026.8.17` is the measurement taken on that date.

## What nub accepts

An installed catalog replaces the compiled one only when it is strictly newer, compared on the `provenance.generatedAt` stamp:

```json
{
  "provenance": { "generatedAt": "2026-08-17T00:00:00Z" },
  "packages": {
    "esbuild": { "versions": { "<0.28.2": { "network": true, "write": { "deps": true } } } }
  }
}
```

Nub refuses a catalog that is older than the one already in force, that carries no stamp, or that fails schema validation, and it leaves the previous catalog untouched when it does. An older catalog cannot roll grants back to a looser measurement.

## Licence

MIT
