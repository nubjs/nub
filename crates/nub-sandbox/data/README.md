# The build-jail catalog

`build-jail-catalog.json` is the curated list of carve-outs from nub's build jail — the
sandbox that confines a dependency's lifecycle scripts (`preinstall`, `install`,
`postinstall`) during `nub install`.

The jail is a **pure allowlist**. A lifecycle script gets its own package directory and
essentially nothing else: no network, no home directory, no access to the rest of your
project. That default breaks real packages that legitimately need to reach further — a code
generator writing next to itself, a native module fetching Node headers, a git-hook installer
writing the hooks you installed it for. Each one that has earned an exception is recorded
here, with the evidence that earned it.

**Not a small number, as it turns out.** v1 carried 4 hosts and 3 package grants from one
measured install pass. Reading the published source of 230 lifecycle-script packages found
that 146 of them contact a host, 67 touch the project filesystem, and 17 need system library
paths the schema could not even express. v2 is that correction: 31 hosts, 14 package grants,
a `systemPaths` axis, and a record of 9 packages whose access is refused on the merits.

This file is data, not code. `build.rs` bakes it into the crate as `static` Rust at
compile time, so nothing is parsed at runtime and a malformed catalog fails the build.
Adding an entry is a one-line pull request; you do not need to read any Rust.

## The jail is best effort, and that decides what belongs here

The build jail is **defense in depth, not a watertight boundary**. Its job is to stop the
bulk of ecosystem supply-chain attacks and to break the virality of a self-propagating worm
— not to withstand a determined attacker targeting you specifically. Two consequences that
run through every decision below:

- **A residual exposure is not a failure. Packages breaking is the failure.** A coarse grant
  that keeps the ecosystem working beats a precise one that breaks half of it, because a jail
  that breaks installs gets turned off and then protects nothing. "Just grant the whole
  directory read-only" is a legitimate answer and is often the right one.
- **Granting more never requires elevation.** Lifecycle scripts run with the user's full
  access by default, so *every* entry in this file is a **reduction** from the status quo, not
  a privilege being handed out. An entry cannot make things worse than not having the jail.

What this does **not** license is a denylist inside an allowlist, or a wildcard. Those are
structural properties the jail depends on and they are enforced at build time.

## Why the catalog is curated, and what that means for your PR

Every entry here is written by nub and reviewed like a security change, because an entry
**is** one. A package cannot put itself in this file, and the lookup key is the identity
nub's installer resolved for a package — not the `name` a package writes in its own
manifest — so a dependency cannot borrow another's exception by renaming itself.

An entry is accepted on evidence that the package is **broken without it**, written as the
**narrowest grant that fixes that failure**. "This package would probably also like write
access to X" is not evidence, and a PR that widens an existing entry needs its own evidence.

**`source-read` is a first-class evidence class, not a weaker substitute for `measured`.**
v1 accepted only measured denials. v2 changed that on a specific finding: **34 of 230
packages fail SILENTLY** when a grant is denied — a `try/catch` or a `|| exit 0` means the
script exits 0 having skipped its real work, so the install is green and the breakage
surfaces later at runtime. **No exit-code-based measurement can see that class at all**, so a
measurement-only bar structurally cannot cover the failures that hurt users most. Reading the
published source can. Both classes are honest; the `evidence` field says which one an entry
rests on, and `measured` remains the stronger claim.

## `catalogVersion` and the `corpus` block

`corpus` carries the provenance and the denominators for every count in this file — how the
230 packages were read, how many need network / project filesystem / system paths, and the
ranked `networkCoverage` table that shows how concentrated the network demand is. Read it
before quoting a percentage. `build.rs` pins each coverage row's package count to the
corresponding host entry's own `fetchedBy` length, so the table cannot drift from the data.

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
| `pathShape` | Optional. The URL path shape observed, e.g. `/<owner>/<repo>/releases/download/<tag>/<asset>`. |
| `fetchedBy` | The package(s) that fetch it. |
| `residual` | Required for a host admitted *despite* a write route or multi-tenant namespace. States what exposure remains and what bounds it. |
| `evidence` | How this was learned. One of `measured`, `vendor-documented`, `source-read`. |
| `observed` | What was actually seen. State the limits of the observation too. |
| `platform` | Where it was observed, or `any` for a platform-independent mechanism. |

**A host must be needed by an install-time lifecycle script.** A download the user triggers
by hand afterwards (`playwright install`) is not an install-time fetch and does not earn an
entry.

**The threat model is bytes LEAVING the machine.** The disqualifying shapes are a host that
accepts a write — a registry publish route, a container blob push, a telemetry endpoint whose
POST body is the product — and a multi-tenant object store where an attacker can rent a
namespace under the same hostname and read back what a confined script sent there.

Serving attacker-authored bytes *into* the jail is **not** disqualifying. Every host here
delivers third-party binaries by definition; that exposure is inherent in running the
postinstall at all, and the filesystem, environment and network confinement is what bounds
it. Do not cut an entry for being a supply-chain-integrity risk — that is a different
criterion, and conflating the two has removed correct entries before.

### `residual` — the v2 reversal, and the rule that replaced it

v1 refused every write-capable and multi-tenant host outright. **v2 admits four of them**, and
the reason is a count: `github.com` alone is the artifact host for **40 of the 230**
source-read packages, and adding it moves network coverage from 55% to 79%. A jail that breaks
a quarter of the native ecosystem is a jail someone turns off.

What makes the reversal defensible is a property of the jail rather than of the hosts. The
lifecycle env is **scrubbed of the whole credential family** (`defaults::lifecycle_scrubbed_env`
withholds registry auth and every key containing `TOKEN`, `SECRET` or `AUTH`), and `~/.ssh`,
`~/.git-credentials`, `.npmrc` and the project's `.git` are all ungranted. A confined script
therefore has **no credential** with which to use a write route: what is left is an
unauthenticated push or publish, which the host rejects.

So the invariant moved from *never admitted* to **never admitted silently**:

| host | residual |
| --- | --- |
| `github.com` | `git-receive-pack` on the same hostname. Unauthenticated only, per the env scrub. |
| `api.github.com` | The whole REST API, write included — and equally credential-gated. |
| `registry.npmjs.org` | `npm publish` is a PUT to the host that serves the read. |
| `storage.googleapis.com` | Genuinely multi-tenant: the bucket is in the *path*. An attacker can rent one. **This is a real accepted residual**, taken because refusing it breaks puppeteer. |

`build.rs` generates `DOWNLOAD_HOSTS_WITH_RESIDUAL` from these declarations, and a unit test
holds the two in agreement — so a future PR cannot admit a host of this class without saying
what it costs.

A **bucket-scoped hostname is not multi-tenant** and needs no residual, which is the
distinction to get right when reviewing: `hummus.s3-us-west-2.amazonaws.com` and a CloudFront
distribution name identify *one* tenant, so an exact-host rule already pins it.
`storage.googleapis.com` does not, because the tenant is a path segment.

**Wildcards are still rejected, and no residual declaration admits one.** The egress proxy
resolves the upstream name itself and gates both the CONNECT authority and the TLS SNI, so an
exact hostname pins every DNS label. A `*.example.com` entry would hand the confined script
the label positions, and a lookup of `<secret>.cdn.example.com` exfiltrates through the
resolver without a single byte of payload being sent. Container registries and telemetry
endpoints are likewise refused outright.

This list is deliberately **not** merged with the broader `$trusted` set used by nub's agent
sandbox: that set is an agent *browsing* surface spanning every language ecosystem's package
index, and this one is the install-time artifact hosts a lifecycle script actually fetches.
The two overlap; neither is a subset of the other by construction.

### Windows

Seven entries carry `"platform": "win32"` and **none of them is reachable today**:
`preset::build_jail_net` compiles to deny-all on Windows, because the backend refuses a
per-host policy outright (the available AppContainer exemption exposes every loopback
listener, so a local forwarder would bypass the hostname gate). They are recorded as the
evidence a future Windows per-host gate would be written against — an allowlist that exists
before the capability is what stops the capability shipping with a guessed one.

## `systemPaths` — system libraries and headers for a native build

**17 of the 230 packages cannot build without reading a system prefix, 12 of them on their
default build path.** The v1 schema had no field for this, so the jail was not deliberately
withholding those paths — it had never represented them.

**The recommendation is a coarse, read-only grant, on by default.** That is the correct shape
rather than a lazy one, and the argument is mechanical: the paths are **discovered at build
time** by executing `pkg-config`, `pg_config`, `krb5-config` or `curl-config` during
`binding.gyp` variable evaluation, and their stdout supplies the include and link flags.
Nothing in a manifest names them, so a per-package enumeration has nothing to enumerate. The
17 packages also do not cluster on one mechanism — they are the ordinary native long tail
(OpenSSL, ODBC, X11, Postgres, Kerberos, libsecret, JDK, Xcode frameworks) — so any list is
already wrong for the eighteenth package. And the directories are world-readable and hold no
secrets, so read access confers nothing a dependency could not get by vendoring a header.

```json
"platforms": { "linux": [ { "path": "/usr/lib/pkgconfig", "why": "the .pc files pkg-config reads" } ] }
```

Every entry carries a `why`; a bare path list would be indistinguishable from a guess a year
from now. A path must be absolute or anchored on `$VAR` / `%VAR%` (the **embedder** resolves
those — `$SDKROOT` and `$JAVA_HOME` mean nothing to a path matcher), and globs are rejected
because `subtree_globs` supplies the recursion, so a `*` here could only widen the rule.
`requiredBy` lists all 17 packages with the file:line evidence and whether the need is on the
default build path.

**Recorded, not yet enforced.** `build.rs` generates `SYSTEM_READ_PATHS_{LINUX,MACOS,WINDOWS}`
and `compiler::system_read_paths()` returns the host platform's, but nothing folds them into
the jail surface: that changes the granted read set on every platform and interacts with each
backend's minimal-root closure, which is a measured change rather than a data change. Tracked
in `knownDefects`.

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
| `projectNodes` | Project-relative directory **nodes** it may read — the node alone, never the contents. |
| `projectWrites` | Where its project write targets come from. See below. |
| `projectCwd` | Grant read on the project root directory node alone. |
| `failureMode` | What a denial does to the package: `LOUD`, `SILENT`, `GRACEFUL` or `UNDETERMINED`. Required. |
| `mechanism` | What the package's own code does. This is what bounds the grant. |
| `evidence` / `observed` / `platform` | As for hosts. |

Omit any field that is not needed; the jail's baseline already covers it.

### What the baseline already grants — read this before adding an entry

67 of the 230 packages recorded `read` or `readwrite` on the project axis, which looks like 67
carve-out candidates and is not. **Three quarters of them need nothing added**, and knowing why
is what keeps this table small:

- **31 touch only their own package directory** (`./build/Release`, `./prebuilds/`, `./vendor/`).
  The jail's baseline grants `package_dir` read-write, so these are already covered. A
  `./`-relative path in a source-read record is *usually* this case, not a project access.
- **`<project>/package.json` is granted, as one file.** Every package that reads the consumer's
  manifest through `INIT_CWD` (react-particles, tsparticles-engine, cldr-data, …) needs no
  entry.
- **`<project>/node_modules` is granted read.** The upward-`require` walks that resolve a peer's
  version (`@intlify/vue-i18n-bridge`, `vue-inbrowser-compiler-demi`) need no entry.

So the residue that genuinely needs a grant is small and specific: the git-hook installers, and
two packages that write one project file each.

### `failureMode`

Required on every grant and every refusal, because it changes what a measurement can prove.
`SILENT` is why the field exists: **34 of 230 packages** swallow a denial and exit 0 with their
real work skipped, so the install is green and the user hits the breakage at runtime. Record it
honestly — a `SILENT` grant is one whose absence no CI signal will ever catch.

The catalog's `failureModes.silent` array lists all of them with the swallow mechanism, and
`failureModes.normalization` documents how the source-read lanes' free-text values were mapped
onto the enum (composite cases keep both modes rather than being flattened).

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

`projectWrites` supports two shapes, and which one applies is a fact about the package rather
than a choice. Exactly one per entry; build.rs rejects both together.

```json
"projectWrites": { "literal": [".git/hooks"] }
"projectWrites": { "manifestField": ["msw", "workerDirectory"] }
```

**`literal`** — the package defines the directory itself, so nub can name it: `.git/hooks` for
the git-hook installers, `snapshots.js` for `@cypress/snapshot`, `hooks` for
`@nativescript/core`. This is the common case and the narrower one, because the target is fixed
at review time: widening it needs a new measurement rather than a different value in somebody's
manifest.

**`manifestField`** — the package imposes *no* convention, so the only place the answer exists
is the consumer's own root `package.json`. This reads a dotted field path and treats its value
(a string, or an array of strings) as project-relative directories. nub owns the field *name*;
the consumer owns the *value*. It is the narrow alternative to granting the whole project tree
for a package like msw, where the consumer already had to name the directory for the package to
work at all.

Either way every resolved path is **clamped back inside the project root** and silently dropped
if it escapes — which is what makes a package's unbounded upward directory walk (`pre-push`,
`@nativescript/core`) safe to grant: the out-of-project case simply fails as it would with no
exception, the conservative direction.

### `projectNodes`

Grants read on a named project-relative **directory node** — the node alone, never `/**`.
`projectCwd` generalized to a named path, for a package that *probes* for a directory rather
than making it its cwd.

Every git-hook installer `existsSync`es the project's `.git` (several then walk upward from it)
before writing into `.git/hooks`, and a node-only read is what makes that probe succeed.

**It is deliberately not a `projectReads` entry, and that distinction is the point.** `.git` as
a *subtree* would expose `.git/config`, whose remote URL can carry an HTTPS credential — the
same leak that was reproduced under the real jail through nub's git-dependency cache and closed
by narrowing that grant. Naming the node grants the probe and nothing else.

### A note on `.git/hooks`, the one grant class with real weight

Nine packages get `.git/hooks` write. Say plainly what that is: a git hook is **code that runs
on the developer's next commit**, which makes it the most attack-relevant write in this whole
file and exactly the persistence vector a worm wants.

It is granted anyway, and **per package rather than jail-wide** — that is the load-bearing part.
Installing git hooks is the entire declared purpose of every one of the nine; the user chose to
install them; and eight of the nine fail **SILENTLY** when denied, so refusing produces a green
install with no hooks and a user who finds out at their next commit. Package identity is the
gate that makes this sound. **Do not generalize it into a baseline grant.** A jail-wide
`.git/hooks` write would hand the same persistence vector to every dependency in the tree,
which is a categorically different thing from handing it to nine named hook installers.

`simple-git-hooks` is a tenth hook installer that is **refused**, and its entry in
`notGranted.packages` explains why (it deletes hooks it does not own) and states the cost.

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

### `notGranted.hosts`

| host | reason | why |
| --- | --- | --- |
| `www.google-analytics.com` | `telemetry-sink` | A POST whose **body is the product**. The package's own `PACT_DO_NOT_TRACK` means denial costs nothing. |
| `www.googleapis.com` | `write-capable` | Fronts nearly every Google API on one hostname, upload routes included — materially broader than the `storage.googleapis.com` entry that *is* admitted. |
| `npm.taobao.org` | `retired-host` | Sunset in 2021. An entry would grant nothing, since nothing serves the path. |
| `saucelabs.com` | `not-blocking` | The bare apex also serves an authenticated web app, and the fetch is opt-in and GRACEFUL by default. |
| `workers.cloudflare.com` | `undecided` | No disqualification established — a single-tenant vendor binary path. An evidenced candidate; admitting it is a maintainer call. |

`undecided` is a real and useful value, not a placeholder. A measured host with no
established disqualification should be recorded as a candidate rather than silently dropped
or quietly admitted, and the difference between "we refused this" and "nobody has ruled yet"
is exactly what the next reviewer needs.

`not-blocking` needs care after v2, because it was used wrongly once: `package.cli.amplify.aws`
was refused on that ground and is now **admitted**. The install did exit 0 — but with the
binary missing, which is the SILENT class, and the CLI is broken at first use. "The install
still passes" is not a test for whether a fetch matters.

### `notGranted.packages` — the third verdict, and why it had to exist

Nine packages are refused **on the merits**: the access they want is something a dependency
should not have, whatever the compatibility cost. `ngx-popperjs` runs
`exec("rm -rf ../")` when a competitor package resolves up-tree. `egg-ci` writes
`.github/workflows/nodejs.yml` into your repo. `esoftplay` patches source inside *other
installed packages*.

**A `packages` entry means the denial is the jail working correctly.** Without this verdict every
measured break reads as a carve-out candidate — which is exactly how a break list got
mischaracterized once. If you find one of these failing, that is the feature; a PR to "fix" it
should be closed with a link here.

Each entry carries `reason`, `wants` (the access), `failureMode`, `detail`, and the same
`evidence`/`observed`/`platform` provenance as a grant. `build.rs` keeps the list disjoint from
`packageGrants`.

Two neighbouring lists exist so a reader is not forced to guess:

- **`notGranted.deferred`** — a demonstrated need whose grant *shape* cannot yet be written
  (`egg-bin`'s write target lives in an untraced helper package; `iobroker.js-controller`'s
  footprint was not traced). Not a refusal. Also where `node-libcurl`'s Windows `git clone` of
  vcpkg is recorded — the one observed `github.com` use that does not fit the admitted path
  shape, and therefore the case a future path-scoped grant would break.
- **`notGranted.notNeededRatherThanRefused`** — packages whose source-read record says "no grant"
  for the opposite reason: nothing is denied because nothing is attempted (`@percy/core` is a
  no-op on the default install path). Also where `cordova.plugins.diagnostic` is corrected: an
  earlier summary listed it as a refusal, and the source read had actually *refuted* that
  framing.

## Opening a PR

1. **Establish the failure, by measurement or by source.** A measured denial is the stronger
   claim and is preferred. Reading the published source is *also* accepted — see the
   `source-read` note above — and is the only thing that works for the SILENT class.
2. **Check the artifact, not the exit code.** Several of these packages write fallback stubs
   and exit 0 having generated nothing. `@prisma/client` does exactly this, and an earlier
   pass "passed" on precisely those stubs. Assert on real output — for a generator, content
   that could only exist if it actually ran. This is the same trap as `not-blocking` above.
3. **Write the narrowest grant that fixes it**, and record the mechanism that bounds it — with
   the caveat that "narrowest" is bounded by "does not break the ecosystem". For a read-only
   grant over world-readable system directories, coarse *is* narrowest-that-works.
4. **Fill in `evidence`, `observed`, `platform` and `failureMode` honestly.** `vendor-documented`
   and `source-read` are legitimate values; a documented host reported as `measured` is worse
   than one reported accurately, because the next reader cannot re-check what you did not do.
5. **If the host has a write route or a multi-tenant namespace, write a `residual`.** A test
   fails otherwise.
6. Run `cargo test -p nub-sandbox`. The build fails on a malformed or escaping entry.

Entries are ordered by when they were added, not alphabetically. Order is meaningful for
`networkHosts` — rule expansion follows list order — so append rather than insert. A unit test
pins the four pre-catalog hosts as the leading entries, so an insert ahead of them fails.

## `knownDefects`

The catalog records its own open defects rather than leaving them in a thread, because a reader
acting on an entry needs to know which ones are not yet trustworthy. Four today, and the one
that matters most: **`@prisma/client`'s grant is measured on macOS and is not known to work on
Linux** — a differential there returned `DIFFERS` identically with and without it, so at least
one more denial is in the way. Also recorded: two v1 grants whose later no-op measurement was
never reconciled, `systemPaths` being generated but not enforced, and the source-read-not-
measured status of most v2 entries.

## Known gaps

Things the current schema cannot express. Each is unbuilt because no shipped entry has
needed it yet; adding a field ahead of a real case would be guessing at its shape.

- **Platform-conditional entries.** Every grant applies on every OS. A package that needs a
  carve-out only on Windows currently gets it everywhere, which is wider than necessary. This
  gap grew in v2: `systemPaths` *is* per-platform, so the two axes now disagree about whether
  platform is expressible, and `@prisma/client`'s Linux defect is exactly a case where scoping
  a grant to the platform it was measured on would state the truth more precisely than the
  prose `platform` field does.
- **Version-conditional entries.** `versions` is prose, not a constraint that is enforced.
  `@prisma/client` 7.0.0 dropped its postinstall entirely, so its grant is dead weight on 7
  — harmless, since an unused grant confers nothing on a script that never runs, but not
  expressible.
- **Path-scoped ENFORCEMENT — still the highest-value gap, though its argument changed shape in
  v2.** The catalog now *records* a `pathShape` per host, but nothing enforces it: DNS-level
  gating cannot see a path, so this depends on proxy work rather than a catalog change.

  In v1 the argument was that path scoping would convert a permanently-refused `github.com`
  into an admissible one. v2 admitted the host at the host level instead, on best-effort
  grounds and with the residual named — so path scoping is no longer what unblocks the
  ecosystem. What it now buys is **retiring the residuals**: `github.com` would narrow to
  `/<owner>/<repo>/releases/download/` (plus `/archive/` for cldr-data), which is a plain GET
  surface with `git-receive-pack` at a different path entirely; and `storage.googleapis.com`
  would narrow to `/chrome-for-testing-public/` and `/chromium-browser-snapshots/`, which are
  single-tenant prefixes even though the hostname is not. That turns two accepted exposures
  into no exposure, which is worth doing — but the ecosystem is no longer waiting on it.

  Both requirements it implies are real. The proxy must gate on the request path, not only the
  CONNECT authority and SNI, which for HTTPS means terminating TLS for those hosts. And
  redirects must be re-checked against the prefix, since a release download 302s to a
  different host (`release-assets.githubusercontent.com`) whose asset paths are opaque signed
  GUIDs — that second half is unsolved.

  **One package would break** and it is recorded in `notGranted.deferred`: `node-libcurl`'s
  Windows fallback `git clone`s `github.com/microsoft/vcpkg.git`, which is whole-repo access
  under no admitted path shape. Whoever builds path gating has to decide about it explicitly
  rather than discover it.

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
