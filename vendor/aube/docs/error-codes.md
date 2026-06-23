# Error and warning codes

<script setup>
import { data } from './error-codes.data.ts'
</script>

aube emits a stable string identifier with every error and most
warnings. Codes look like `ERR_AUBE_NO_LOCKFILE` or
`WARN_AUBE_IGNORED_BUILD_SCRIPTS`, and they're attached as a
structured field — no regex on stderr required.

Use the search box and category chips below to filter the list
directly from the registry.

## How to read codes

**Default text output**: errors include the code in their
miette-rendered output, e.g. `× foo (ERR_AUBE_NO_LOCKFILE)`. Warnings
include the code as a structured field after the message.

**ndjson output** (`aube --reporter ndjson <cmd>`): every record carries
a `code` field. Branch on `code == "ERR_AUBE_..."` instead of
substring-matching the human message.

```jsonc
{
  "level": "WARN",
  "code": "WARN_AUBE_IGNORED_BUILD_SCRIPTS",
  "count": 2,
  "packages": ["esbuild", "opencode-ai"],
  "message": "ignored build scripts for 2 package(s): esbuild, opencode-ai. ..."
}
```

**Exit codes**: errors exit with `1` (generic) by default. The errors
called out as having a bespoke exit in the table below get a stable
numeric exit code instead, so shell scripts can branch without parsing
stderr.

## Naming

- `ERR_AUBE_*` — fatal errors. Process exits non-zero.
- `WARN_AUBE_*` — non-fatal warnings. Install continues; the warning
  is informational.

aube does not emit `ERR_PNPM_*` codes. Where a code maps onto a pnpm
concept (lockfile, peer-deps, tarball, etc.) the suffix matches pnpm's
naming so the meaning is obvious to anyone familiar with pnpm's
[error page](https://pnpm.io/errors), but aube reserves its own
prefix so the codes can evolve independently.

## Stability

Once published, a code's identifier and meaning don't change. New
codes can be added at any time. Removing or repurposing a code is a
breaking change.

Bespoke exit codes follow the same contract. The exit-code allocation
is grouped by category — see [`crates/aube-codes/src/exit.rs`][exit-src]
for the full layout — but consumers should branch on the exit *value*,
not the category, since categories are documentation, not API.

[exit-src]: https://github.com/jdx/aube/blob/main/crates/aube-codes/src/exit.rs

## Errors

<ErrorCodesTable
  :codes="data.errors"
  :categories="data.categories.errors"
  :show-exit="true"
/>

## Warnings

Codes prefixed `WARN_AUBE_*` indicate non-fatal conditions. The
install proceeds; the warning surfaces something the user may want to
act on. Bespoke exit codes do not apply to warnings.

<ErrorCodesTable
  :codes="data.warnings"
  :categories="data.categories.warnings"
  :show-exit="false"
/>

## Adding a code

1. Add a `pub const` to `crates/aube-codes/src/errors.rs` or
   `warnings.rs`.
2. Add a `CodeMeta` entry to the local `ALL` slice carrying the
   category, one-line description, and (for errors) optional bespoke
   exit code. The self-tests in `crates/aube-codes/src/lib.rs` and
   `exit.rs` reject typos, missing descriptions, duplicate exit
   codes, and out-of-range exits.
3. Reference the constant from the call site:
   - For warnings: `tracing::warn!(code = aube_codes::warnings::WARN_AUBE_X, ...)`.
   - For thiserror enums: `#[diagnostic(code(ERR_AUBE_X))]`.
   - For ad-hoc miette: `miette::miette!(code = aube_codes::errors::ERR_AUBE_X, ...)`.
4. Run `mise run render` to regenerate `docs/error-codes.data.json`.
