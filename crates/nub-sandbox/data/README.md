# The build-jail catalog

`build-jail-catalog.json` is the curated list of carve-outs from nub's build jail — the
sandbox that confines a dependency's lifecycle scripts (`preinstall`, `install`,
`postinstall`) during `nub install`.

The jail is a **pure allowlist**. A lifecycle script gets its own package directory and
essentially nothing else: no network, no home directory, no access to the rest of your
project. That default breaks a small number of real packages that legitimately need to
reach further — a code generator writing next to itself, a native module fetching Node
headers. Each one that has earned an exception is recorded here, with the evidence that
earned it.

This file is data, not code. `build.rs` bakes it into the crate as `static` Rust at
compile time, so nothing is parsed at runtime and a malformed catalog fails the build.
Adding an entry is a one-line pull request; you do not need to read any Rust.

## Why the catalog is curated, and what that means for your PR

Every entry here is written by nub and reviewed like a security change, because an entry
**is** one. A package cannot put itself in this file, and the lookup key is the identity
nub's installer resolved for a package — not the `name` a package writes in its own
manifest — so a dependency cannot borrow another's exception by renaming itself.

The practical consequence: an entry is accepted on **measured evidence that the package is
broken without it**, and it is written as the **narrowest grant that fixes the measured
failure**. "This package would probably also like write access to X" is not evidence.
A PR that widens an existing entry needs its own measurement.

## `networkHosts` — the egress allowlist

The hosts a confined lifecycle script may reach. Also exposed to policy authors as the
`$downloads` token.

```json
{
  "host": "cdn.cypress.io",
  "artifact": "The Cypress binary (302 redirect target)",
  "fetchedBy": ["cypress"],
  "evidence": "measured",
  "observed": "Resolved by cypress's postinstall in the corpus harness-validation run...",
  "platform": "linux-arm64"
}
```

| field | meaning |
| --- | --- |
| `host` | An exact hostname. Wildcards are rejected — see below. |
| `artifact` | What is downloaded from it. |
| `fetchedBy` | The package(s) that fetch it. |
| `evidence` | How this was learned. One of `measured`, `vendor-documented`, `source-read`. |
| `observed` | What was actually seen. State the limits of the observation too. |
| `platform` | Where it was observed, or `any` for a platform-independent mechanism. |

**A host must be needed by an install-time lifecycle script.** A download the user triggers
by hand afterwards (`playwright install`) is not an install-time fetch and does not earn an
entry.

**The threat model is bytes LEAVING the machine.** A host is disqualified if it accepts a
write — a forge API, a registry publish route, a container blob push, a telemetry endpoint
whose POST body is the product — or if it is a multi-tenant object store where an attacker
can rent a namespace under the same hostname and read back what a confined script sent
there.

Serving attacker-authored bytes *into* the jail is **not** disqualifying. Every host here
delivers third-party binaries by definition; that exposure is inherent in running the
postinstall at all, and the filesystem, environment and network confinement is what bounds
it. Do not cut an entry for being a supply-chain-integrity risk — that is a different
criterion, and conflating the two has removed correct entries before.

**Wildcards are rejected, and this is a security property rather than a style rule.** The
egress proxy resolves the upstream name itself and gates both the CONNECT authority and the
TLS SNI, so an exact hostname pins every DNS label. A `*.example.com` entry would hand the
confined script the label positions, and a lookup of `<secret>.cdn.example.com` exfiltrates
through the resolver without a single byte of payload being sent.

This list is deliberately **not** merged with the broader `$trusted` set used by nub's agent
sandbox. That set is a read-only browsing surface and keeps some write-capable hosts on
credential-scoping grounds. This jail confines attacker-authored dependency code and
inherits none of those exceptions — `registry.npmjs.org` is absent precisely because
`npm publish` is a write to the host that serves the read.

## `packageGrants` — per-package filesystem carve-outs

```json
{
  "package": "@prisma/client",
  "versions": "6.x (7.0.0 dropped the postinstall entirely)",
  "siblingDirs": [".prisma"],
  "projectReads": ["prisma"],
  "projectCwd": true,
  "mechanism": "scripts/postinstall.js ... mkdirs path.join(__dirname, '../../../.prisma')",
  "evidence": "measured",
  "observed": "EPERM mkdir <own node_modules>/.prisma, then EPERM uv_cwd ...",
  "platform": "macos-arm64"
}
```

| field | meaning |
| --- | --- |
| `package` | The installer-resolved package name. Matched exactly: no prefix, suffix or case folding. |
| `versions` | Which versions the measurement covers. |
| `siblingDirs` | Named entries of the package's own enclosing `node_modules` it may read and write. |
| `projectReads` | Project-relative subtrees it may read. |
| `projectWrites` | Where its project write targets come from. See below. |
| `projectCwd` | Grant read on the project root directory node alone. |
| `mechanism` | What the package's own code does. This is what bounds the grant. |
| `evidence` / `observed` / `platform` | As for hosts. |

Omit any field that is not needed; the jail's baseline already covers it.

### `siblingDirs`

A package's own enclosing `node_modules` — the directory its internal `../..` arithmetic
reaches, under any linker layout. Each entry is **one directory name**. A name containing a
path separator or `..` is rejected at build time, because it would leave the subtree the
grant is bounded by.

Enumerating names is load-bearing, and a pattern over dot-entries would not be equivalent.
The dot-entries at a `node_modules` root are not scratch space — they are the install
itself. `.store`/`.pnpm` hold every materialized dependency's source *before it is
executed*, and `.bin` is the shim directory that later tooling runs **unconfined**. Naming
`.prisma` grants one directory; a dot-entry pattern would grant those.

### `projectReads` vs `projectWrites`

Read is the smaller grant — prefer it. A code generator needs its schema *readable*, not
writable.

`projectWrites` currently supports one shape:

```json
"projectWrites": { "manifestField": ["msw", "workerDirectory"] }
```

This reads a dotted field path from the **consumer's** root `package.json` and treats its
value (a string, or an array of strings) as project-relative directories. nub owns the
field *name*; the consumer owns the *value*; every resolved path is clamped back inside the
project root and silently dropped if it escapes.

This exists for a package that imposes no directory convention of its own — the consumer
already had to name the directory for the package to work at all, so their manifest is the
only place the answer exists. It is the narrow alternative to granting the whole project
tree. If a package writes to a directory *it* defines, that is a literal, and this catalog
does not yet have a field for it (see "Known gaps").

### `projectCwd`

Grants read on the project root **directory node** — the node alone, never its contents.

This exists for a package whose postinstall makes the consumer's project its working
directory: `@prisma/client` calls `process.chdir(INIT_CWD)`, msw spawns its CLI with `cwd`
set there. Node resolves a new working directory through `uv_cwd`, which needs the directory
itself readable, and the jail's baseline grants only `package.json` *inside* it. Without
this grant both die in `uv_cwd` before running a line of their own logic.

Node-only is what keeps this from being a project read: the project's *contents* stay
ungranted, and anything the package then reads there must still be named in `projectReads`.

It is not zero disclosure, and the platforms differ — stated rather than glossed. A Landlock
read rule on a directory carries `READ_DIR`, so on Linux a granted package can list the
project root's top-level entry names. macOS grants metadata only. Filenames, never contents,
and only for the packages in this table.

## `notGranted`

Documentation only. Nothing in this object is compiled into any allowlist.

It records hosts that a real install was measured to need and that were **not** admitted, so
you can see the bar before opening a PR. A build-time check keeps it disjoint from
`networkHosts`, so an entry cannot be quietly promoted while its rejection rationale stays
behind.

Entries carry the same `evidence` / `observed` / `platform` provenance as `networkHosts`,
plus `requester` (the package that fetched it) and `observedUrl` (the URL actually seen).
They are held to that bar deliberately: a refusal is the *input to a later promotion
decision*, and an unevidenced one is worse than no entry at all, because it reads as a
settled verdict while carrying nothing a reviewer can re-check. `observedUrl` is also the
field a path-scoped grant would have to be written against.

The recorded entries cover four shapes, and only three are disqualifications:

| host | reason | why |
| --- | --- | --- |
| `github.com` | `write-capable` | Carries an authenticated write on the same hostname. The one a path-scoped grant would fix — see below. |
| `storage.googleapis.com` | `multi-tenant` | An attacker can rent a bucket under the same hostname and read back what a confined script sends there. |
| `package.cli.amplify.aws` | `not-blocking` | The fetch fails and the install still exits 0. A soft fetch that degrades does not earn an entry. |
| `workers.cloudflare.com` | `undecided` | No disqualification established — a single-tenant vendor binary path. Recorded as an evidenced candidate; admitting it is a maintainer call. |

`undecided` is a real and useful value, not a placeholder. A measured host with no
established disqualification should be recorded as a candidate rather than silently dropped
or quietly admitted, and the difference between "we refused this" and "nobody has ruled yet"
is exactly what the next reviewer needs.

## Opening a PR

1. **Measure the failure.** Run the install with the jail on and with it off. A grant is
   justified by a denial you observed, not by reading the package's source and predicting
   one.
2. **Check the artifact, not the exit code.** Several of these packages write fallback stubs
   and exit 0 having generated nothing. `@prisma/client` does exactly this, and an earlier
   pass "passed" on precisely those stubs. Assert on real output — for a generator, content
   that could only exist if it actually ran.
3. **Write the narrowest grant that fixes it**, and record the mechanism that bounds it.
4. **Fill in `evidence`, `observed` and `platform` honestly.** `vendor-documented` and
   `source-read` are legitimate values; a documented host reported as `measured` is worse
   than one reported accurately, because the next reader cannot re-check what you did not do.
5. Run `cargo test -p nub-sandbox`. The build fails on a malformed or escaping entry.

Entries are ordered by when they were added, not alphabetically. Order is meaningful for
`networkHosts` — rule expansion follows list order — so append rather than insert.

## Iterating without a rebuild (development only)

This file is codegen'd into the binary, so every edit normally costs a full Rust rebuild —
which dominates a corpus loop that installs hundreds of packages, watches one fail, adds its
grant, and re-runs. A development-only seam removes the rebuild from that loop:

```sh
cargo build -p nub-cli --profile fast --features build-jail-catalog-override
NUB_BUILD_JAIL_CATALOG=/path/to/build-jail-catalog.json nub install
```

The loaded catalog **replaces** the compiled one outright — all three derived tables
(`$downloads` hosts, `packageGrants`, per-package egress), never a merge, so the file on disk
means exactly what it says.

Four properties are worth knowing before you rely on it:

- **It exists in no shipped build.** Without the `build-jail-catalog-override` cargo feature
  the catalog parser is not compiled into the crate at all, and setting the variable is a hard
  startup **refusal** rather than a silent no-op — so a run can never iterate against a catalog
  the binary is quietly ignoring.
- **It runs this file's validations.** The override and `build.rs` share one parser, so an
  override cannot introduce a shape the build would have rejected.
- **Any failure falls back to the compiled catalog, and says so on stderr.** A missing file,
  bad JSON, or a validation rejection costs one banner, not the run.
- **An active override announces itself** (`build-jail catalog OVERRIDDEN from …`), which is
  the line a harness greps to prove which catalog it actually measured.

## Known gaps

Things the current schema cannot express. Each is unbuilt because no shipped entry has
needed it yet; adding a field ahead of a real case would be guessing at its shape.

- **A literal project write path.** `projectWrites` only supports `manifestField`. A package
  that writes to a directory it defines itself would need a `literal` variant.
- **Platform-conditional entries.** Every grant applies on every OS. A package that needs a
  carve-out only on Windows currently gets it everywhere, which is wider than necessary.
  The corpus that produced these entries ran on one platform per measurement, so a
  platform-scoped grant would today be asserting more than was measured.
- **Version-conditional entries.** `versions` is prose, not a constraint that is enforced.
  `@prisma/client` 7.0.0 dropped its postinstall entirely, so its grant is dead weight on 7
  — harmless, since an unused grant confers nothing on a script that never runs, but not
  expressible.
- **Path-scoped hosts — the highest-value gap, and the one with a concrete argument behind
  it.** The schema records no URL prefix, because DNS-level gating cannot enforce a path.
  Adding one depends on proxy work rather than on a catalog change, and it is worth doing
  rather than a nice-to-have. The reason is `github.com`.

  `github.com` is not refused because downloading a release tarball from it is dangerous.
  It is refused because the **same hostname** serves `git-receive-pack` — an authenticated
  **write** — and CI environments routinely hand lifecycle scripts a token with push
  rights. A host-level grant cannot tell the two apart, so admitting `github.com` today
  would open a push path, not just a fetch path. That is a categorically different exposure
  from serving bytes into the jail, and it is why the host-level answer has to be "no".

  A path-scoped grant limited to `/*/*/releases/download/` **separates them**: the release
  asset path is a plain GET surface, and `git-receive-pack` lives at a different path
  entirely. The same mechanism applies to `storage.googleapis.com`, whose observed prefixes
  (`/tensorflow/`, `/chrome-for-testing-public/`) are stable and single-tenant even though
  the hostname is not.

  So path scoping is not a refinement of the existing allowlist — it converts a
  permanently-refused host into an admissible one, and by count `github.com` and the object
  stores are the largest group of currently-refused hosts. Both requirements it implies are
  real: the proxy must gate on the request path, not only the CONNECT authority and SNI,
  which for HTTPS means terminating TLS for those hosts; and redirects must be re-checked
  against the prefix, since a release download 302s to a different host
  (`release-assets.githubusercontent.com`) whose asset paths are opaque signed GUIDs. That
  second half is unsolved and is the reason this is proxy work rather than a schema edit.

## Remote updates: designed, not built

The catalog is baked in at compile time today. It is shaped so it could later be fetched and
cached at runtime, letting nub ship a carve-out without a release. **That path is not
implemented**, and the security design below is the reason it needs to be settled before it
is.

**The trust position changes materially.** A compile-time constant is authored by nub,
reviewed in a pull request, and shipped inside a signed release artifact. A fetched document
is a **remote authority over the sandbox**: whoever controls it can grant any listed package
any carve-out, on every machine that fetches it. Compromising the endpoint that serves this
file would be equivalent to shipping a malicious nub release, without needing the release
signing key.

What that compromise could actually do is bounded, and worth stating precisely rather than
alarming about. The grants are narrow and per-package: a hostile catalog could add an egress
host (the largest risk — an exfiltration sink reachable by any lifecycle script), or widen
one named package's filesystem reach to another directory in the project. It could not turn
the jail off, escape the project root (paths are clamped at grant time), or grant anything
to a package the attacker does not already control code in. The realistic attack is
therefore **a supply-chain attacker who already owns a dependency**, using a catalog entry to
open the egress or filesystem path their payload needs.

The design the implementation must satisfy:

- **Integrity: signature, not just a hash.** The document is fetched over TLS and verified
  against a public key shipped in the nub binary. A pinned hash alone cannot work — the
  point of the mechanism is that the document changes between releases, so the binary cannot
  know the hash of a catalog published after it. TLS alone is not enough either: it
  authenticates the server, not the document, so it fails open against a compromised or
  substituted endpoint. Signing keeps the trust root in the binary, where it already is.
- **Freshness is the hazard, not staleness.** A stale catalog is safe — it grants strictly
  what an older nub granted, and the failure mode is an install that breaks, which is
  visible and recoverable. A hostile *fresh* one is the whole risk. So the design must never
  trade integrity for freshness: no "accept unsigned if newer", no shortened verification on
  a cache miss. Signed documents carry a monotonic version so a rollback to a
  correctly-signed older catalog is detectable.
- **Failure falls back to the baked-in copy, never to "grant everything".** Fetch failure,
  signature failure, parse failure and version-rollback all resolve to the catalog compiled
  into the binary. The compiled copy is never deleted or superseded on disk — it is the
  floor. A fetched catalog may only be consulted after it verifies, and a verification
  failure is logged rather than silent, because it is indistinguishable from an attack.
- **Users can opt out, and the opt-out is a real one.** A single setting disables remote
  fetching entirely and pins the binary's own catalog. Environments that need
  reproducibility (CI, air-gapped builds, anyone who does not want install behavior changing
  without a version bump) should be able to take it, and it must not degrade to "fetch but
  ignore" — no request is made.
- **A fetched catalog may only narrow the trust decision, never widen the mechanism.** The
  document contains data for the existing compiled-in shapes. It must never be able to
  introduce a new *kind* of grant, name a path outside the clamps, or alter the authorship
  key. The parser for a fetched catalog is the same one as for the baked-in file, with the
  same build-time validations re-run at load time — which is the one place a runtime parse
  is unavoidable, and where a rejection must fail closed to the compiled copy.

Open question for whoever implements it: whether a fetched catalog should be allowed to add
**network hosts** at all, or only per-package filesystem grants. Hosts are the higher-value
target for an attacker — an egress sink benefits any compromised package, while a filesystem
grant only helps one named package — so restricting remote updates to `packageGrants` and
keeping `networkHosts` release-gated is worth considering as the conservative default.
