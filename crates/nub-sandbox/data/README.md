# The build-jail catalog

Nub confines every dependency lifecycle script (`preinstall`, `install`, `postinstall`) during
`nub install`. The jail is a pure allowlist: a script gets its own package directory and
essentially nothing else — no network, no home directory, no access to the rest of your
project. Some packages legitimately need to reach further, and `build-jail-catalog.json` is
the list of the ones that may.

```json
{
  "canvas": { "network": true, "hosts": ["github.com", "nodejs.org"] },
  "ghooks": { "project": "readwrite" },
  "node-libcurl": { "network": true, "hosts": ["github.com", "nodejs.org"] }
}
```

A flat map, package name to grant, three optional fields. This file is data, not code:
`build.rs` bakes it into the crate as `static` Rust at compile time, so nothing is parsed at
runtime and a malformed catalog fails the build. Adding an entry is a one-line pull request and
you do not need to read any Rust.

## The contract: a package that needs access files a PR against this file

This is the mechanism the whole catalog exists to provide, so it comes first.

**A dependency's lifecycle script gets network and out-of-package filesystem access only if it
has an entry here.** No entry, nothing. Needing access means opening a pull request against
`build-jail-catalog.json` and having it reviewed. That is what makes the surface **opt-in**
rather than ambient, and it is the sentence to remember if you remember one.

**Why that is the control, and not host granularity.** The attack this jail exists to blunt is
the one Shai-Hulud used: an attacker publishes a new `preinstall` or `postinstall` into a
package that *never had one*, and it runs with the user's complete access. The package is not
one anybody vetted — it is whatever the worm reached. When someone bolts a postinstall onto
`chalk`, `chalk` has no entry, so it cannot phone home *regardless of which host it wants*.

The corollary matters for reviewing: **narrowing an already-admitted package's reach buys very
little.** It constrains a package a human already looked at and trusted, and does nothing about
the unvetted one. So do not spend review effort shaving hosts or paths, and do not treat a
broad grant to a *listed* package as the problem. Granting a listed package the ability to
fetch arbitrary code is a **known, accepted fact** of this design.

## The jail is best effort, and that decides what belongs here

The build jail is **defense in depth, not a watertight boundary** — and not a vital security
boundary. Its job is to stop the bulk of ecosystem supply-chain attacks and to break the
virality of a self-propagating worm, not to withstand a determined attacker targeting you
specifically. Three consequences that run through every decision below:

- **A residual exposure is not a failure. Packages breaking is the failure.** A coarse grant
  that keeps the ecosystem working beats a precise one that breaks half of it, because a jail
  that breaks installs gets turned off and then protects nothing. "Just grant the whole
  directory read-only" is a legitimate answer and is often the right one — as is "grant this
  package the whole network".
- **Granting more never requires elevation.** Lifecycle scripts run with the user's full access
  by default, so *every* entry in this file is a **reduction** from the status quo, not a
  privilege being handed out. An entry cannot make things worse than not having the jail.
- **If a package needs a host to work, grant it.** "That host also serves something sensitive"
  is not sufficient to refuse. Two hosts were refused on exactly that reasoning and later
  admitted once it was re-examined.

What this does **not** license is a denylist inside an allowlist, or a wildcard. Those are
structural properties the jail depends on and they are enforced at build time.

## Why the catalog is curated, and what that means for your PR

Every entry here is written by nub and reviewed like a security change, because an entry **is**
one. A package cannot put itself in this file, and the lookup key is the identity nub's
installer resolved for a package — not the `name` a package writes in its own manifest — so a
dependency cannot borrow another's exception by renaming itself.

An entry is accepted on evidence that the package is **broken without it**. Establish that by
measurement or by reading the published source; both are legitimate, and source reading is the
only thing that works for one large class of failure.

**Some packages fail SILENTLY when a grant is denied.** A `try/catch` or a `|| exit 0` means
the script exits 0 having skipped its real work, so the install is green and the breakage
surfaces later at runtime — worse for a user than an install error. 34 of the 230 packages read
during the catalog's construction behave this way, and **no exit-code-based measurement can see
that class at all.** So do not treat a green install as evidence that a denial was harmless:
check the artifact. Several of these packages write fallback stubs and exit 0 having generated
nothing.

## `network`

Egress, as an OS-enforced boolean.

```json
{ "hasura-cli": { "network": true } }
```

Only `true` is accepted; omit the field to grant no egress. There is no per-host enforcement and
there is no path scoping — a package either reaches the network or it does not.

That is a deliberate reduction from a per-host model, on two grounds. The threat model says
package identity is the control, so narrowing an admitted package's host list constrains
somebody already reviewed. And a per-host list could not be made correct anyway: the measured
redirect chains break it, because a `github.com` release asset is a 302 to
`release-assets.githubusercontent.com` and an `/archive/` URL a 302 to `codeload.github.com`. A
per-package host list is the set of hosts a package was seen to *ask for*, never the set it must
reach.

## `hosts`

Bare hostnames the package was observed to fetch. **Evidence, not enforcement.**

```json
{ "duckdb": { "network": true, "hosts": ["npm.duckdb.org"] } }
```

Nothing gates a request against this list, and nothing should be built to. It is kept
structured per package for one reason: **a changing host list is a detection signal.** A package
that used to fetch from its own CDN and now reaches somewhere else shows up as a diff in a pull
request, which is the Shai-Hulud tell, and prose does not diff usefully. It is also the only
answer to the question `network: true` raises — to where? — which cannot be read off the code.

Rules: bare hostnames only, no scheme, port, path or annotation. Wildcards are rejected at build
time, because the egress proxy resolves the upstream name itself and gates both the CONNECT
authority and the TLS SNI, so an exact hostname pins every DNS label — a `*.example.com` entry
would hand a confined script the label positions, and a lookup of `<secret>.cdn.example.com`
exfiltrates through the resolver without a single byte of payload being sent. A host list
requires `network: true`; one without it is a half-written entry and fails the build.

Omit the field when no literal host could be resolved. Three shapes produce that, all
legitimate: an unenumerable transitive set (`node-libcurl` clones vcpkg, then hits vcpkg's own
per-port hosts), a host chosen at run time (`windows-build-tools` picks its Python mirror from a
network probe), and code that is not published at all (`hasura-cli`'s `dist/` is absent from its
own tarball).

The union of every entry's `hosts` is what a policy author's `$downloads` token expands to.

## `project`

Access to the consumer's project tree, at one of two levels.

```json
{ "@cypress/snapshot": { "project": "readwrite" } }
```

- **`read`** — the project tree, readable. The smaller grant; prefer it. A code generator needs
  its schema readable, not writable.
- **`readwrite`** — the project tree, readable and writable, **`node_modules` included**.

Both are the whole tree. That replaced six separate path fields, each with its own resolution
rule, clamp and platform caveat, and the precision they bought is not precision the threat model
spends: every package in this table was reviewed in a pull request, so narrowing a reviewed
package's reach to one directory constrains somebody already trusted.

The precision also did not survive contact with the backends. A grant naming
`node_modules/.prisma` could not be attached on Linux at all when the directory did not yet
exist, because `landlock_add_rule` takes an `O_PATH` descriptor — so it needed a `create_dir_all`
side effect during policy compilation, and the same defect reappeared for `.git/hooks`. A
separate field granting read on the project root *node* was load-bearing on macOS Seatbelt and a
measured no-op on Landlock, which is how one entry shipped broken after being measured only on
macOS. Both problems are properties of granting a path that may not exist yet; the project root
always exists.

**Say plainly what a project grant costs.** It covers `node_modules/.bin` (shims later tooling
runs unconfined), the virtual store (every materialized dependency's source before it executes),
and `.git/hooks` (code that runs on the developer's next commit) — the persistence vectors the
jail exists to close. It also reaches the project's **secret files**: `.env*`, `.npmrc`, and
`.git/config`'s credential-bearing remote URL. Those are protected by not being *granted* rather
than by a deny rule, because the jail emits no denies at all, so a grant over the tree removes
that protection — and the exception cannot be expressed, since withholding one file from inside
a granted subtree is precisely what an allowlist-only backend cannot represent.

Fourteen named, reviewed packages hold this. **Do not generalize it into a baseline grant.** A
jail-wide `.git/hooks` write would hand the same persistence vector to every dependency in the
tree, which is a categorically different thing.

Nine of the fourteen are git-hook installers, and installing hooks is the entire declared
purpose of every one of them — the user chose to install them, and eight of the nine fail
*silently* when denied, so refusing produces a green install with no hooks and a user who finds
out at their next commit. Package identity is the gate that makes that sound.

## What the jail already grants — read this before adding an entry

Most packages that look like they need an entry do not. Knowing why is what keeps this file
small.

- **Its own package directory, read-write.** A path under `./build/Release`, `./prebuilds/` or
  `./vendor/` is already covered. A `./`-relative path in a bug report is *usually* this case,
  not a project access.
- **The consumer's `package.json`, as one file.** Every package that reads the manifest through
  `INIT_CWD` needs no entry.
- **The consumer's `node_modules`, readable.** The upward-`require` walks that resolve a peer's
  version need no entry.
- **System libraries and headers, readable, on every platform.** `/usr/lib`, `/usr/include`,
  the Homebrew library subpaths, the Xcode SDK. A native build compiles against these, and a
  system library path holds no victim-specific information — a hijacked package reading
  `/usr/lib/libssl.so` learns nothing it could not have got by vendoring its own copy of the
  header. Library subpaths only: a package-manager prefix root would drag in `etc/` service
  config and `var/` live databases, so those stay out.
- **Nub's own bootstrapped node-gyp.** A confined script skips the ambient-PATH probe entirely,
  so this is the only node-gyp a native build can reach, and it is already granted.

## Opening a PR

1. **Establish the failure**, by measurement or by reading the published source. Measurement is
   the stronger claim; source reading is the only thing that works for the silent class.
2. **Check the artifact, not the exit code.** Assert on real output — for a generator, content
   that could only exist if it actually ran.
3. **Pick the smaller field where it works.** Prefer `read` over `readwrite`. Omit `hosts` if
   you could not resolve a literal host rather than guessing one.
4. **Run `cargo test -p nub-sandbox`.** The build fails on a malformed entry, an unknown field,
   a wildcard host, or an entry that grants nothing.

An unknown field is a hard error rather than ignored, which matters if you are working from an
older example: the fields this schema dropped — `siblingDirs`, `projectCwd`, `projectReads`,
`projectNodes`, `projectWrites`, `mechanism`, `observed`, `evidence`, `platform`, `versions`,
`failureMode`, `refused` — are exactly what you would reach for, and silently accepting one
would leave you believing a grant is narrower than it is. The provenance those carried is prose
now, in `wiki/research/build-jail-provenance.md`, one section per package.

## What the schema cannot express

Each of these is unbuilt because no shipped entry has needed it. Adding a field ahead of a real
case would be guessing at its shape.

- **Platform-conditional entries.** Every grant applies on every OS. One package's project
  grant is load-bearing on macOS and provably inert on Linux; another's network grant is a
  Windows-only need granted everywhere. Each is wider than what was measured.
- **Version-conditional entries.** A grant covers every version of its package.
  `@prisma/client` 7.0.0 dropped its postinstall entirely, so its grant is dead weight on 7 —
  harmless, since an unused grant confers nothing on a script that never runs.
- **A build-time refusal guard.** Nine packages were measured to want access that a dependency
  should not have, and the catalog used to record them alongside a build-time check that a
  refused package could not also be granted. Absence is now the only way to express a refusal,
  which is enforcement-identical but carries no verdict: a reviewer cannot tell "we refused
  this" from "nobody has looked". The refusals and their reasoning are in the provenance
  write-up.

## Windows

Egress is deny-all on Windows regardless of what this file says. The backend refuses a per-host
policy outright, because the available AppContainer exemption exposes every loopback listener,
so a local forwarder would bypass the hostname gate — and an unappliable jail fails the install
rather than degrading. Deny-all is the stricter posture, so the divergence loses a capability,
never enforcement.

## Remote updates: designed, not built

The catalog is baked in at compile time today. It is shaped so it could later be fetched and
cached at runtime, letting nub ship a carve-out without a release. **That path is not
implemented**, and the security design below is the reason it needs to be settled before it is.

**The trust position changes materially.** A compile-time constant is authored by nub, reviewed
in a pull request, and shipped inside a signed release artifact. A fetched document is a
**remote authority over the sandbox**: whoever controls it can grant any listed package any
carve-out, on every machine that fetches it. Compromising the endpoint that serves this file
would be equivalent to shipping a malicious nub release, without needing the release signing
key.

What that compromise could actually do is bounded, and worth stating precisely rather than
alarming about. The grants are per-package: a hostile catalog could give a package egress, or
give it the project tree. It could not turn the jail off, escape the project root, or grant
anything to a package the attacker does not already control code in. The realistic attack is
therefore **a supply-chain attacker who already owns a dependency**, using a catalog entry to
open the path their payload needs.

The design the implementation must satisfy:

- **Integrity: signature, not just a hash.** The document is fetched over TLS and verified
  against a public key shipped in the nub binary. A pinned hash alone cannot work — the point of
  the mechanism is that the document changes between releases, so the binary cannot know the
  hash of a catalog published after it. TLS alone is not enough either: it authenticates the
  server, not the document, so it fails open against a compromised or substituted endpoint.
  Signing keeps the trust root in the binary, where it already is.
- **Freshness is the hazard, not staleness.** A stale catalog is safe — it grants strictly what
  an older nub granted, and the failure mode is an install that breaks, which is visible and
  recoverable. A hostile *fresh* one is the whole risk. So the design must never trade integrity
  for freshness: no "accept unsigned if newer", no shortened verification on a cache miss.
  Signed documents carry a monotonic version so a rollback to a correctly-signed older catalog
  is detectable.
- **Failure falls back to the baked-in copy, never to "grant everything".** Fetch failure,
  signature failure, parse failure and version-rollback all resolve to the catalog compiled into
  the binary. The compiled copy is never deleted or superseded on disk — it is the floor. A
  fetched catalog may only be consulted after it verifies, and a verification failure is logged
  rather than silent, because it is indistinguishable from an attack.
- **Users can opt out, and the opt-out is a real one.** A single setting disables remote fetching
  entirely and pins the binary's own catalog. Environments that need reproducibility (CI,
  air-gapped builds, anyone who does not want install behavior changing without a version bump)
  should be able to take it, and it must not degrade to "fetch but ignore" — no request is made.
- **A fetched catalog may only narrow the trust decision, never widen the mechanism.** The
  document contains data for the existing compiled-in shapes. It must never be able to introduce
  a new *kind* of grant or alter the authorship key. The parser for a fetched catalog is the same
  one as for the baked-in file, with the same build-time validations re-run at load time — which
  is the one place a runtime parse is unavoidable, and where a rejection must fail closed to the
  compiled copy.

Open question for whoever implements it: whether a fetched catalog should be allowed to grant
`network` at all, or only `project`. Egress is the higher-value target for an attacker — a
reachable sink benefits any compromised package, while a project grant only helps one named
package — so restricting remote updates to `project` and keeping egress release-gated is worth
considering as the conservative default.
