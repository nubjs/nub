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

## `networkHosts` — the prefetcher's host allowlist

Exposed to policy authors as the `$downloads` token, and used as the allowlist for nub's own
out-of-jail prefetch. Adding a host widens THAT allowlist, which runs unconfined on
manifest-controlled URLs — so an entry here is an addition of trust, not the reduction a jail
grant is.

**The build jail no longer gates on this list.** Its egress is a per-package BOOLEAN, resolved
by `src/compiler/package_network.rs`: a package the catalog names may reach the network, a
package it does not name reaches nothing. Per-host was withdrawn because only macOS could
enforce it — Linux needs a network namespace it cannot require, and Windows' loopback exemption
is admin-only — so a list that gated one platform meant an incomplete list erroring for the
platform most developers use. The hosts remain as PROVENANCE: a package that used to fetch its
own CDN and now reaches somewhere else shows up as a reviewable diff on this file.

**So `fetchedBy` is where a package is admitted, and `host` is not.** Naming a package in
`fetchedBy` grants it egress; adding a host without naming a package grants no script anything,
and only widens the prefetcher.

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
| `fetchedBy` | The package(s) that fetch it. Load-bearing, not a note: this is one of the two sources of the per-package egress set, so naming a package here grants it the network. |
| `evidence` | How this was learned. One of `measured`, `vendor-documented`, `source-read`. |
| `observed` | What was actually seen. State the limits of the observation too. |
| `platform` | Where it was observed, or `any` for a platform-independent mechanism. |

**A host must be needed by an install-time lifecycle script.** A download the user triggers
by hand afterwards (`playwright install`) is not an install-time fetch and does not earn an
entry.

**The threat model is bytes LEAVING the machine, and the sender to picture is the
PREFETCHER.** Nub's prefetch GET is unconfined, and its URL is composed from a manifest an
attacker may have authored, so the request itself is a channel: a host that accepts a write —
a forge API, a registry publish route, a container blob push, a telemetry endpoint whose POST
body is the product — or a multi-tenant object store where an attacker can rent a namespace
under the same hostname and read back what was sent there, is a worse host than one that only
serves, and the set is kept as small as the evidence allows for that reason.

**A write-capable host is not automatically disqualified, but this list is not the place to
settle it, and `github.com` is the worked case.** It serves release assets and
`git-receive-pack` on the same hostname. For the JAIL that overlap stopped mattering once
egress became package-identity-gated: the only script that reaches any host is one a pull
request already ratified, so the hostname is not the boundary doing the work. The entry was
promoted on that reasoning on 2026-07-30 and reverted the same day, because this list is not
jail-local — it is also the allowlist for nub's own out-of-jail prefetch, which runs
unconfined on manifest-controlled URLs, and whose `github.com` widening is separately fenced
behind the off-by-default `prefetch-github-hosts` cargo feature pending ratification.
Promoting the host here resolved that contradiction toward widening, silently.

So `github.com` stays refused HERE, and a package that needs to fetch from it is admitted in
`packageNetwork` instead — which grants the package and touches no host list. Weigh a
write-capable host against the packages it breaks, but weigh it for the prefetcher, since that
is the only consumer this list still gates. A pure exfiltration sink with no measured
install-time demand behind it is still a refusal, because there the trade has nothing on the
other side.

Serving attacker-authored bytes *into* the jail is **not** disqualifying. Every host here
delivers third-party binaries by definition; that exposure is inherent in running the
postinstall at all, and the filesystem, environment and network confinement is what bounds
it. Do not cut an entry for being a supply-chain-integrity risk — that is a different
criterion, and conflating the two has removed correct entries before.

**Wildcards are rejected, and this is a security property rather than a style rule.** What
the rule buys differs by consumer, and only the first consumer is this list's own:

- **For the prefetcher it is SSRF containment.** Nub composes the URL from the package's
  manifest and performs the GET unconfined, so an exact hostname is what stops a manifest
  pointing `binary.host` at `169.254.169.254`, an intranet name, or an attacker's own
  subdomain under an admitted suffix.
- **For nub's agent sandbox it is resolver exfiltration.** That product routes egress through
  a proxy which resolves the upstream name and gates both the CONNECT authority and the TLS
  SNI, so an exact hostname pins every DNS label; a `*.example.com` entry would hand the
  confined process the label positions, and a lookup of `<secret>.cdn.example.com` leaks
  through the resolver with no payload sent. The build jail runs no proxy and gates no
  hostname, so this half does not describe it.

This list is deliberately **not** merged with the broader `$trusted` set used by nub's agent
sandbox. That set is a read-only browsing surface for an agent the user is driving; this one
is the artifact surface for attacker-authored dependency code, and the two populations have no
reason to coincide. Membership here is earned by a measured install-time fetch **that the
prefetcher needs**, which is a narrower bar than "some script wanted it": `registry.npmjs.org`
is now measured — two lifecycle scripts shell `npm install` and die on it — and is still
absent, because those two packages are granted in `packageNetwork` and the host itself serves
an authenticated publish route on the same name that a project `.npmrc` token would reach.
That is the github.com shape, and it resolves the same way.

## `packageNetwork` — egress for a package whose hosts are not the point

The second of the two sources of the per-package egress set, and the one to reach for now that
the jail no longer gates on hostnames. `packageNetwork.full` names a package directly:

```json
"packageNetwork": {
  "full": [
    {
      "package": "@railway/cli",
      "evidence": "measured",
      "observed": "getaddrinfo ENOTFOUND github.com from its postinstall ...",
      "platform": "macos-arm64"
    },
    {
      "package": "esbuild",
      "versions": "<0.13.0",
      "evidence": "measured",
      "observed": "0.11.23 confined: npm error ... ENOTFOUND registry.npmjs.org ...",
      "platform": "macos-arm64"
    }
  ]
}
```

It carries the same `evidence` / `observed` / `platform` provenance as a host, and it resolves
to exactly the same grant as being named in a host's `fetchedBy` — the jail's egress is a
boolean, so there is no weaker or stronger spelling. The optional `versions` range is the one
thing `fetchedBy` cannot express, and it is why a version-scoped package must not be spelled
both ways: a `fetchedBy` observation hangs off a host and names no version, so the parser
rejects a package that is scoped here and unscoped there rather than letting one silently
outrank the other.

**Prefer it over `fetchedBy` whenever the package's demand is the reason for the entry.**
`fetchedBy` couples two decisions that are no longer related: it grants the package AND adds
the hostname to `$downloads`, which is the prefetcher's allowlist. `packageNetwork.full`
grants only the package. Use `fetchedBy` when the host itself has to be in `$downloads` for
the prefetcher's sake, and record the demand under `packageNetwork.full` otherwise.

`notGranted.packages` overrides both. A package refused on the merits gets nothing, and it may
still legitimately appear in a `fetchedBy` array as an observation of what it was seen
fetching — recording the observation must not become a grant.

## `packageGrants` — per-package filesystem carve-outs

```json
{
  "package": "@prisma/client",
  "versionsObserved": "6.x (7.0.0 dropped the postinstall entirely)",
  "siblingDirs": [".prisma"],
  "dependencyDirs": [["prisma"], ["prisma", "@prisma/engines"]],
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
| `versions` | Optional semver range the grant is scoped to. Absent means every version. See below. |
| `versionsObserved` | Prose: which versions the measurement covers. Constrains nothing. |
| `siblingDirs` | Named entries of the package's own enclosing `node_modules` it may read and write. |
| `dependencyDirs` | Chains of package *names* whose resolved directories it may read and write. See below. |
| `homePaths` | Artifact caches under the real home it may read and write, each with the package's own variable that redirects it. See below. |
| `projectReads` | Project-relative subtrees it may read. |
| `projectWrites` | Where its project write targets come from — `manifestField` or `literal`. See below. |
| `projectCwd` | Grant read on the project root directory node alone. |
| `fullDisk` | The terminal tier: the whole filesystem, read and write. See below. |
| `mechanism` | What the package's own code does. This is what bounds the grant. |
| `evidence` / `observed` / `platform` | As for hosts. |

Omit any field that is not needed; the jail's baseline already covers it.

### `fullDisk` — the terminal tier

The last rung of the grant ladder, for a package that fails under every narrower grant. It is
not a rule: the filesystem axis stops confining that package's lifecycle scripts entirely.

```json
{ "package": "wordpos", "fullDisk": true, "evidence": "measured" }
```

It exists so the ladder terminates. Without it, a package no targeted grant fixes is an open
investigation, and a catalog that has to root-cause its own tail never reaches full
compatibility. With it, that package is one line, and narrowing the scope back down becomes a
later optimisation against a working baseline rather than a prerequisite.

**It is a reduction, not an escalation.** Outside nub a lifecycle script already runs with the
user's complete authority. This withholds two of the three axes: the environment is still
scrubbed of the credential family and `HOME` is still redirected, and egress is still decided
by `packageNetwork`, which this field does not touch. The gate is unchanged — the entry names
ONE package, matched on the installer-resolved name, so an uncatalogued package still gets
nothing.

`evidence` must be `measured`. Nothing else can establish that every narrower rung was tried
and failed: `policy` is a judgement, and `vendor-documented` / `source-read` say what a package
intends. Name the rungs you ran in `mechanism` — `grant-matrix.mjs` writes that sentence itself
for a `NEEDS-FULL-DISK` record. An explicit `"fullDisk": false` is rejected; omit the key, or
record the refusal under `notGranted`.

Other filesystem fields in the same entry are subsumed rather than contradicted, so they are
allowed but pointless. `homePaths` is the exception worth keeping: its environment half still
matters, because the environment axis keeps redirecting `HOME` and the tool still has to be
pointed at its real cache.

**Windows is broader than macOS and Linux here, and it is announced.** macOS renders the tier
as one `(allow file*)` line and Linux as one Landlock rule on `/`; both leave the network axis
untouched. Windows cannot render it inside the AppContainer at all — a LowBox token reaches an
object only where that object's own ACL names its container SID, so "everything" would mean an
inheritable modify ACE on each drive root, which is refused as a filesystem-wide write hole and
would in any case be ruinous to write on a launch that installs and revokes ACEs every time. So
a full-disk package launches without the token, and since egress is an AppContainer
*capability*, the network axis goes with it. That loss is reported through the degradation
channel and printed at the spawn. The environment axis is unaffected on every platform: it is
enforced by constructing the child's environment, which needs no token.

### `versions` — scoping an entry to the versions that need it

Optional on both `packageGrants` and `packageNetwork.full`, and honoured on both. Absent means
every version, which is what every entry written before this field existed still means — a
range is an act of narrowing, never a default.

```json
{ "package": "esbuild", "versions": "<0.13.0" }
```

Cargo range syntax (`<0.13.0`, `>=2, <3`, `1.4.2`), not npm's. These strings are ours, never a
package's own dependency spec, so the dialect only has to be one this file writes. A malformed
range fails the build.

**Write a range only where you measured the BOUNDARY, not the version you happened to test
on.** esbuild's is `0.13.0` because that is where `optionalDependencies` landed: from there up
its `install.js` resolves a prebuilt platform package and opens no socket, and below it the
`npm install` shell-out is the only path. That is a fact about the package's own code, on the
same footing as `mechanism`. "We ran the matrix against 1.2.1" is not — it goes in
`versionsObserved`, which is prose and gates nothing.

The direction of travel makes this worth doing: packages keep migrating from building at
install time to shipping prebuilt `optionalDependencies`, so the versions that need a
capability are the OLD ones and a cutoff gets more accurate over time. esbuild is the measured
case — by weekly downloads, 99.82% of its installs are above the boundary.

A scoped entry needs a version it can judge, so an unknown or non-semver one does not match.
That withholds rather than widens, which is the same reading the tables take for an absent
package name.

**A scoped entry does not reach prereleases, and a cutoff cannot be widened into one.** Cargo
semver admits a prerelease only when a comparator carries a prerelease at the same
major.minor.patch, so `<0.13.0`, `<0.13.0-0` and `>=0.0.0-0, <0.13.0` all refuse
`0.12.0-rc.1` — only `>=0.12.0-rc, <0.13.0` admits it. A catalogued package installed at a
prerelease therefore falls back to no grant. If that ever breaks a real install, drop the
entry's scope rather than trying to spell a wider range.

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

### `dependencyDirs`

For a package that writes into **another package's** directory. `@prisma/client`'s postinstall
re-execs the `prisma` CLI, and that CLI downloads the query engine into `@prisma/engines`'
package directory and copies it into its own — neither of which `package_dir` covers.

Each entry is a **chain of package names**, resolved by the ordinary `node_modules` ancestor
walk Node itself performs, starting at the granted package and continuing from each name it
resolves:

```json
"dependencyDirs": [["prisma"], ["prisma", "@prisma/engines"]]
```

reads *"the `prisma` this package resolves, and the `@prisma/engines` that one resolves."* A
chain rather than a flat list because resolution is relative: under the isolated layout
`@prisma/engines` is not reachable from `@prisma/client` at all, only from `prisma`. Writing it
as names is also what makes the field linker-agnostic — under a hoisted layout both hops land
on `<project>/node_modules/<name>` with no change here.

**A name, never a path.** A separator (beyond the single `@scope/` one), a traversal component,
a leading `.`, or the literal `node_modules` is rejected at build time. That is the security
property: nub authors the names, the installed tree decides what they resolve to, so a chain can
only reach a package the granted package could already `require`, and never the `node_modules`
container itself — which holds `.bin` (run **unconfined** by later tooling) and the virtual
store (every dependency's source before it executes).

**Symlinks are resolved, and the containment clamp runs on the resolved path.** Under the
isolated layout every dependency edge *is* a symlink into another store cell, and both enforcing
backends match on the canonicalized path — so a rule naming the link would compile to a grant no
access can hit. Because the emitted term is therefore the realpath, the clamp checks the realpath
too; clamping the link while granting its target would test a different path than it permits.

The consequence, stated rather than glossed: a chain that resolves **out of the project** — into
the machine-global virtual store, where a write would reach every project on the host — is
**dropped**, and the package then fails exactly as it would with no entry at all. aube
materializes a cell project-locally when its package carries a lifecycle script (it must, or an
in-place build would write through the shared store inode), so the cells a curated grant names
are the local ones; the peerless, script-free cells that stay as store symlinks are not.

### `homePaths`

For a package that downloads a large binary into a cache under your **home directory** and
reads it back later, when the app runs. Cypress and Puppeteer are the population.

```json
"homePaths": [
  {
    "env": "CYPRESS_CACHE_FOLDER",
    "macos": "~/Library/Caches/Cypress",
    "linux": "$cache/Cypress"
  }
]
```

nub sets `env` to the resolved path for that one lifecycle spawn and grants read-write on the
directory it names — nothing else under your home.

**The problem this solves is at run time, not install time.** A confined script already gets a
private, writable `HOME`, so a package that downloads into `$HOME/…` installs and exits 0
today. What breaks is the app afterwards: it runs with your real home, so it looks in
`~/Library/Caches/Cypress` and finds nothing (`No version of Cypress is installed in: …`).
Pointing the install at the same path the run-time lookup computes is what closes that — and it
is why the path has to be the tool's own documented default. A directory nub picked would be
just as unreachable, because nub is not in the loop when your app runs.

**Two anchors, and no others.** `~/…` is your home; `$cache/…` is the platform cache root
(`$XDG_CACHE_HOME` where set, `%LOCALAPPDATA%` on Windows, `~/.cache` otherwise). Between them
they reproduce how these packages compute their own defaults — Cypress consults
`XDG_CACHE_HOME` on Linux, so `$cache/Cypress` tracks it for free. An absolute path, `$tmp`, a
project-relative path, a `..`, or a glob is rejected at build time.

**The path is per-OS because the default is.** `cachedir('Cypress')` is
`~/Library/Caches/Cypress` on macOS and `$XDG_CACHE_HOME/Cypress` on Linux. Omit a platform and
the package gets nothing there — which is the right entry when nub has measured nothing there.

**`env` may not name a variable the jail itself decides.** `HOME` and `USERPROFILE` are what
point a confined script at its private home; `PATH`, `TMPDIR`, `LOCALAPPDATA`, `XDG_CACHE_HOME`
and the `NODE_*` resolution variables steer lookups a cache grant has no business touching. All
are refused at build time.

**It overwrites an ambient value.** If you have `CYPRESS_CACHE_FOLDER` set yourself, the
confined install still uses nub's path: the grant was compiled against that path, so honouring
yours would aim the download at a directory the sandbox denies. Set `install.buildJail` to `false`
in `nub.jsonc` if you need your own location — a GLOBAL switch, since the per-package opt-out this
used to name was removed in c5651408f4.

**A cache the package resolves from the temp directory does not qualify.** `geckodriver` and
`edgedriver` default their `*_CACHE_DIR` to `os.tmpdir()`, and they compute that same default
again when the driver is started — so redirecting the install into your home moves the artifact
somewhere the run-time lookup never reads. The entry would be a home grant that fixes nothing.

**Why this and not something broader.** Copying the private home out into your real one
afterwards would publish *dependency-chosen* paths as nub — `~/.zshrc`, a
`~/.config/git/config` carrying `core.hooksPath`, `~/Library/LaunchAgents/*`. Dropping the
private home entirely would break every package that uses it as free scratch and reopen the
`$HOME/.npmrc` channel between packages. This grants one named directory, per package, and
reads nothing else. Homebrew resolves the same tension the same way: the real home, home reads
denied, and a curated list of specific writable paths.

### `projectReads` vs `projectWrites`

Grant read where read is what the package needs — a code generator needs its schema
*readable*, not writable. Reach for the field that matches the mechanism, not the one that
sounds smaller: a read entry is not a way to *narrow* a write entry, and it cannot take
write away from a path another field granted. Nothing here subtracts. Every field in this
table adds, and a package that needs no grant gets no entry.

That last sentence is the backends' job as much as the catalog's, and Seatbelt lost it
once: `emit_fs` compiled a read grant into a write *deny* and emitted it last, so a
`projectReads` entry covering a `siblingDirs` target revoked it — 20 written entries went
to 0 with the read grant as the only variable. Fixed at the mechanism (an Allow now emits
no deny on any backend), which is why the guidance above can be stated as a flat rule.

`projectWrites` supports two shapes, and an entry carries exactly one of them. They are
split on **who authored the path**, which is the only distinction a reviewer has to check:

```json
"projectWrites": { "manifestField": ["msw", "workerDirectory"] }
"projectWrites": { "literal": [".git/hooks"] }
```

`manifestField` reads a dotted field path from the **consumer's** root `package.json` and
treats its value (a string, or an array of strings) as project-relative directories. nub
owns the field *name*; the consumer owns the *value*. This exists for a package that
imposes no directory convention of its own — the consumer already had to name the directory
for the package to work at all, so their manifest is the only place the answer exists.

`literal` names the path outright, for a package that writes where *it* decides. `.git/hooks`
is the worked case: git owns that path, the consumer configures nothing, and a hook
installer's entire function is to write there. Pair it with `projectReads` when the package
also has to *find* the repository — `shared-git-hooks` shells `git rev-parse`, and git's
detection reads `.git/HEAD` and `.git/config`. Name those two files; do **not** reach for the
`.git` subtree, which was measured to reach the same result while additionally granting
`objects/`, the consumer's entire source history.

Both are clamped back inside the project root and silently dropped if they escape, so the
shapes differ in provenance rather than in reach. A `literal` that traverses out is rejected
at build time rather than dropped, because a grant that appears present and does nothing is
worse than one that fails loudly.

**A `literal` grant is reviewed harder than its size suggests, and `.git/hooks` is why.** A
file written there runs **unconfined**, as the developer, on their next `git commit` — long
after the install that planted it. That makes it persistent code execution, not a
configuration write, so it is granted **per package** and never as a class rule: "looks like
a hook installer" is a shape any dependency can adopt, and the jail exists because a
lifecycle script is attacker-authored. The six hook installers in the table earned their
entries by being packages whose stated function is writing that file, which is the thing the
consumer installed them to do.

### `projectCwd`

Grants read on the project root **directory node** — the node alone, never its contents.

This exists for a package whose postinstall makes the consumer's project its working
directory: `@prisma/client` calls `process.chdir(INIT_CWD)`, msw spawns its CLI with `cwd`
set there. Node resolves a new working directory through `uv_cwd`, which needs the directory
itself readable, and the jail's baseline grants only `package.json` *inside* it. Without
this grant both die in `uv_cwd` before running a line of their own logic.

Node-only is what keeps this from being a project read: the project's *contents* stay
ungranted, and anything the package then reads there must still be named in `projectReads`.

It is not zero disclosure — stated rather than glossed. Both backends let a granted package
list the project root's top-level entry names: Landlock grants `READ_DIR` on the node,
Seatbelt a `(literal …)` read of it. Filenames, never contents, and only for the packages in
this table.

Holding that line is the backends' job, and both once lost it in the widening direction.
Seatbelt rendered the node as `(subpath …)`, which read the whole project *and* revoked every
write grant under it; the Linux mount plan collapsed it into a subtree grant whose rights
each file below inherits. Reach for the backend before assuming a bare path stays a node.

On Linux the grant is inert: `chdir` is not a Landlock-handled access, so the operation it
exists to permit was never denied there. It stays because Seatbelt does gate it.

## `notGranted`

Documentation only. Nothing in this object is compiled into any allowlist.

It records hosts that a real install was measured to need and that were **not** admitted, so
you can see the bar before opening a PR. A build-time check keeps it disjoint from
`networkHosts`, so an entry cannot be quietly promoted while its rejection rationale stays
behind. A promotion therefore *removes* the entry; where it went and why is recorded in the
object's `comment` and on the new `networkHosts` entry, so the decision stays legible.

Entries carry the same `evidence` / `observed` / `platform` provenance as `networkHosts`,
plus `requester` (the package that fetched it) and `observedUrl` (the URL actually seen).
They are held to that bar deliberately: a refusal is the *input to a later promotion
decision*, and an unevidenced one is worse than no entry at all, because it reads as a
settled verdict while carrying nothing a reviewer can re-check.

| host | reason | why |
| --- | --- | --- |
| `github.com` | `write-capable` | It serves `git-receive-pack` on the same hostname as its release assets, and this list is the unconfined prefetcher's allowlist as well as the jail's provenance record. The jail reaches it through a `packageNetwork` entry instead. |
| `storage.googleapis.com` | `multi-tenant` | An attacker can rent a bucket under the same hostname, so a manifest-composed URL the prefetcher fetches unconfined lands on storage the attacker reads back. The four packages that need it reach it through `packageNetwork` instead. |
| `package.cli.amplify.aws` | `not-blocking` | The fetch fails and the install still exits 0. A soft fetch that degrades does not earn an entry. |
| `workers.cloudflare.com` | `undecided` | No disqualification established — a single-tenant vendor binary path. Recorded as an evidenced candidate; admitting it is a maintainer call. |

There have been no promotions out of this table. `github.com` was promoted on 2026-07-30 and
reverted the same day; the reasoning on both sides is in the `networkHosts` section above, and
the short version is that the objection is now about the prefetcher rather than about the jail.

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

- ~~**A literal project write path.**~~ **Built 2026-07-31** as `projectWrites.literal`, for
  the six `.git/hooks` installers; see the `projectReads` vs `projectWrites` section.
- **Platform-conditional entries.** Every grant applies on every OS. A package that needs a
  carve-out only on Windows currently gets it everywhere, which is wider than necessary.
  `platform` is parsed and VALIDATED but its value is discarded — `PackageGrant` carries no
  such field, so nothing downstream can gate on it.

  ⛔ **THE REASON THIS WAS ACCEPTED HAS EXPIRED, and the cost is now countable (2026-08-06).**
  The original rationale was that *"the corpus that produced these entries ran on one platform
  per measurement, so a platform-scoped grant would today be asserting more than was measured."*
  That was true when written. The corpus now holds **6,648 records across all three platforms**,
  and **581 package/versions are measured on two or more**, of which **44 (7.6%) diverge in the
  expensive direction** — `write:"disk"` on one OS and something narrow on another. A
  platform-scoped grant would now assert **exactly** what was measured, which is the condition
  this bullet set for revisiting.

  **What it costs today, counted from this file:** of 34 `packageGrants`, **16 carry
  `fullDisk: true`** (the whole filesystem, read AND write). **14 are tagged `platform:
  win32-x64`**, 2 `linux-x64`, none macOS. So fourteen packages measured only on Windows hold
  whole-filesystem read+write on macOS and Linux — where the corpus records **zero** macOS
  packages needing whole-disk out of 1,672 measured, and 8 on Linux, none of them these.

  ⛔ **"Over-granting is safe" does not cover this rung.** That rule governs build capabilities;
  whole-filesystem read+write is credential exposure, and the `/proc` decision already
  established the asymmetry does not extend there.

  **Two ways to close it.** The v2 catalog already expresses per-OS overlays, so promoting it
  closes this as a side effect. The cheaper interim is to make this parser HONOUR `platform`
  instead of dropping it — the data is already in every entry. Either way it flips grants from
  applying-everywhere to applying-on-one-OS, so the cross-platform records had to be checked per
  package first: a package that genuinely needs disk on two platforms but carries one tag would
  break on the other.

  **✅ THAT CHECK IS DONE (2026-08-06).** Disk-records / MINIMUM-records per platform, read from
  the corpus for all 16:

  | | n | packages |
  | --- | --- | --- |
  | **scoping SUPPORTED** — zero disk records on any non-tagged platform | **12** | `bs-platform`, `dugite`, `electron-chromedriver`, `electron-prebuilt`, `fast-folder-size`, `gif2webp-bin`, `jpeg-recompress-bin`, `nodejieba`, `registry-js`, `unrs-resolver`, `wordpos`, `zopflipng-bin` |
  | ⛔ **must NOT scope** — disk on a second platform too | **2** | `dotnet-2.0.0` (win32 tag, also 1/1 on linux) · `opencode-ai` (linux tag, also 1/1 on win32) |
  | **no corpus data** — scoping would assert more than was measured | 2 | `git-win`, `playwright-firefox` |

  `electron-chromedriver` is the clearest case for scoping: **6 disk records of 44 on win32,
  0 of 41 on linux, 0 of 33 on darwin.**

  ★ The two that must not be scoped are exactly the pair
  [`../../../wiki/design/build-jail-architecture.md`](../../../wiki/design/build-jail-architecture.md)
  identifies from a separate 20-package cross-platform study — *"Only `@opencode-ai/cli` and
  `dotnet-2.0.0` genuinely need disk everywhere."* Two analyses sharing no machinery, same pair.

  ⇒ **Honouring `platform` would narrow 12 of 16 whole-disk grants from three platforms to one,
  with corpus evidence behind each**, leaving those four unscoped. That is a behavior change on
  the default-on path, so it is a maintainer decision — but it is no longer one with unknowns in it.
- ~~**Version-conditional entries.**~~ **Built 2026-07-31** as the optional `versions` semver
  range on `packageGrants` and `packageNetwork.full`; the prose that used to occupy the
  `versions` name is now `versionsObserved`. See the `versions` section above. Only `esbuild`
  is scoped so far, at the `<0.13.0` boundary where `optionalDependencies` landed; every other
  entry is deliberately unscoped, because we have measured no boundary for them and a cutoff
  invented from the version a matrix happened to run on is false precision.
- **Path-scoped hosts — a gap on purpose. Do not build it.** The schema records no URL
  prefix, and this entry used to argue it was the highest-value thing to add: `github.com`
  serves `git-receive-pack` on the same hostname as its release assets, a host grant cannot
  tell a fetch from a push, and a prefix limited to `/*/*/releases/download/` would.

  That argument is retired. The defense is **package identity, not host or path
  granularity**: no catalog entry means no network, and an entry means the network the review
  ratified. Path-scoping would refine what an already-vetted package may do, which is the
  cheaper half of the problem, and the effort it demands is real: the jail inspects no URL and
  runs no proxy on any platform, so scoping a path means building one — terminating TLS for
  those hosts and re-checking every redirect against the prefix, since a release download 302s
  to `release-assets.githubusercontent.com`, whose asset paths are opaque signed GUIDs. A
  package that needs `github.com` gets a `packageNetwork` entry rather than waiting for it.

  What follows for a reviewer: judge a host on whether an install-time fetch was measured and
  on what refusing it costs, not on whether some path under it accepts a write. A refusal
  still holds where nothing needs the host, and where it is an exfiltration sink with no
  measured demand behind it — but not on the theory that a finer grain is coming.

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
alarming about. Every grant is keyed on a package name, so a hostile catalog buys reach for
packages it names and nothing else: it could grant a named package egress (the largest risk —
that package's script then reaches any host it likes), widen a named package's filesystem
reach to another directory in the project, or add a hostname to the prefetcher's allowlist,
which points nub's own unconfined GET somewhere new. It could not turn the jail off, escape
the project root (paths are clamped at grant time), or grant anything to a package the
attacker does not already control code in. The realistic attack is therefore **a supply-chain
attacker who already owns a dependency**, using a catalog entry to open the egress or
filesystem path their payload needs.

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

Open question for whoever implements it: whether a fetched catalog should be allowed to touch
the **network** at all, or only per-package filesystem grants. Both network shapes are
higher-value targets than a filesystem grant. An egress grant hands one package the whole
network rather than one more directory, and a `networkHosts` entry additionally moves the
prefetcher, which runs unconfined as the user and is the one surface here that is not
sandboxed at all. So restricting remote updates to `packageGrants` and keeping `networkHosts`
and `packageNetwork` release-gated is worth considering as the conservative default.
