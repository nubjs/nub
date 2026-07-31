//! The built-in `$`-sets the compiler expands in place: `$trusted` (a curated network
//! host allowlist) and `$downloads` (the install-time artifact hosts) on the net axis,
//! `$tooldirs` (the per-OS package-manager / toolchain cache+store dirs) on the fs axis.
//! All are ORDINARY last-match-wins entries — a set expands at its authored position and
//! a later rule can override any member, like a `...:#/pointer`-reused list's entries.
//!
//! Provenance / curation:
//! - `$trusted` derives from the Claude Code default-allowed-domains list, filtered by a
//!   SINGLE criterion: EXFILTRATION. A host is excluded only if the confined process can
//!   make bytes of its own choosing retrievable by someone outside the sandbox. Three
//!   mechanisms qualify: an authenticated write-back route (publish a package, create a
//!   repo, post a paste, push an image, POST a telemetry event — the Shai-Hulud
//!   propagation primitive), rentable path/subdomain tenancy the attacker reads back
//!   (multi-tenant object stores), and a host the attacker operates, where the request
//!   itself is the signal.
//!
//!   DELIVERY is deliberately NOT disqualifying, and the distinction is the whole point of
//!   the set. A host that only serves attacker-authored bytes INTO the sandbox is not an
//!   exfiltration channel: the malicious code is already executing here by construction, so
//!   denying it one CDN only moves it to the next one — and `registry.npmjs.org`, retained
//!   because nothing installs without it, is itself an arbitrary-payload delivery channel.
//!   That is why the read-only GitHub content hosts are IN the set while `api.github.com`
//!   is not. "A worm fetched its payload from this host" is an argument about delivery and
//!   carries no weight here; so does "a worm read data from here that it had exfiltrated
//!   somewhere else" — that names the OTHER host as the sink.
//!
//!   Membership was settled by probing each ecosystem's *documented* write route against a
//!   same-host bogus-path control. A live auth-gated route answers differently from the
//!   bogus path (`api.github.com` POST /user/repos -> 401 "Requires authentication", bogus
//!   -> 404); a host that denies the method wholesale answers identically (`codeload`
//!   POST -> 403 on both the real tarball path and a bogus one). That control is what
//!   caught `index.rubygems.org` fronting the same Rails app as `rubygems.org` — POST
//!   /api/v1/gems -> 401 and bogus -> 404 on both names, `server: RubyGems.org` throughout —
//!   so a stolen `RUBYGEMS_API_KEY` publishes through the name that looks like a read index.
//!
//!   Four caveats bound what this set can promise. An entry is NOT protocol-scoped — the
//!   egress proxy tunnels arbitrary TCP via CONNECT/SOCKS5, so allowing a host admits
//!   non-HTTP upload transports too (why the `dput` PPA target is absent). A CNAME onto a
//!   shared CDN is opaque to any host allowlist, since the tenant-selecting `Host` header
//!   travels inside TLS (`index.crates.io` / `static.crates.io` are kept as single-tenant
//!   buckets on that basis, not because the hostname proves it). A `*.suffix` wildcard
//!   cannot satisfy the criterion by inspection at all, because it admits whatever the
//!   operator ever hosts under that suffix: the retained `*.ubuntu.com` already covers
//!   `login.ubuntu.com`, whose POST /api/v2/tokens/oauth answers 400 against a 404 bogus
//!   control. The four retained wildcards are a standing exception pending a decision on
//!   the shape as a whole. And three entries are retained despite failing the rule outright
//!   — `registry.npmjs.org`, `api.anthropic.com`, `claude.ai` — because each is load-bearing
//!   and its write route answers on the same hostname the legitimate read uses (`npm
//!   publish` is a PUT to the registry that installs read; a model API call carries its
//!   payload in the request body). The proxy gates only the CONNECT authority and TLS SNI
//!   before blind-forwarding, so it cannot separate them; for these three, credential
//!   scoping is the control, not the host list. Metadata/link-local + RFC1918 are a SEPARATE
//!   always-on hard floor, never part of this set. This set is the AGENT-SANDBOX net axis
//!   and is unrelated to the build jail's download allowlist, which is a separate constant
//!   with its own, stricter membership. Any host added later must clear the same
//!   write-route probe; absent the probe, leave it out.
//! - `$downloads` is the narrower, install-scoped sibling of `$trusted`, and the two are
//!   kept strictly apart: `$trusted` serves an agent working with the user's own
//!   credentials, `$downloads` serves attacker-authored dependency code, so it inherits
//!   none of `$trusted`'s load-bearing write-capable retentions. It is also wildcard-free
//!   by construction (see [`DOWNLOAD_HOSTS`]).
//! - `$tooldirs` is per-OS because a tool's cache home differs across OSes (macOS
//!   `~/Library/Caches`, Linux `~/.cache`, Windows `%LOCALAPPDATA%`). Host OS ==
//!   target OS (the fold runs on the machine it enforces on), so the set is
//!   `#[cfg]`-selected — same precedent as `defaults::OS_ESSENTIAL_ENV`.
//!
//! Runtime-resolved dirs (`gem environment gemdir`, `pnpm store path`) ship STATIC
//! home-anchored defaults ONLY — the engine never shells out to build this set (that
//! would be a nub-authored command-exec surface, unconditional even in an untrusted
//! `dependenciesMeta` scope, and a missing tool would hard-fail the compile). True
//! runtime resolution is deferred to a host-provided `CompileCtx` field, fail-soft.

use crate::matcher::path::{Homes, canonicalize_glob_prefix, expand_symbolic};
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, NetRule, NetTarget};

// ── $trusted (net host set) ────────────────────────────────────────────────────

/// The curated trusted-host allowlist. Each entry is a literal host or a leading
/// `*.suffix` subdomain wildcard — both accepted by [`crate::matcher::host::host_pattern_is_valid`]
/// and matched by [`crate::matcher::host::host_glob_matches`]. Write-capable and
/// multi-tenant hosts are deliberately absent (see the module doc for the rule and its
/// caveats); the `#[cfg(test)]` unit below guards both invariants so a future edit cannot
/// smuggle an invalid pattern or an exfil sink in.
pub const TRUSTED_HOSTS: &[&str] = &[
    // Anthropic / Claude
    "api.anthropic.com",
    "claude.ai",
    "code.claude.com",
    "docs.claude.com",
    "platform.claude.com",
    // GitHub content delivery. These SERVE bytes and cannot store them — every one denies
    // writes wholesale, unlike the github write surface (see the module doc's third bullet).
    "codeload.github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "avatars.githubusercontent.com",
    "camo.githubusercontent.com",
    // npm / yarn. `registry.yarnpkg.com` is an alias of the retained `registry.npmjs.org`,
    // so it grants no capability that host does not already grant.
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "npmjs.com",
    "www.npmjs.com",
    "npmjs.org",
    "www.npmjs.org",
    "yarnpkg.com",
    // Node / JS
    "nodejs.org",
    "www.nodejs.org",
    "binaries.prisma.sh",
    "downloads.sentry-cdn.com",
    "pkg.stainless.com",
    // Python
    "files.pythonhosted.org",
    "pypi.org",
    "www.pypi.org",
    "pypi.python.org",
    "pythonhosted.org",
    "pypa.io",
    "www.pypa.io",
    "conda.anaconda.org",
    "repo.anaconda.com",
    // Rust
    "index.crates.io",
    "static.crates.io",
    "static.rust-lang.org",
    "www.rust-lang.org",
    "rustup.rs",
    // Go
    "golang.org",
    "www.golang.org",
    "goproxy.io",
    "index.golang.org",
    "proxy.golang.org",
    "sum.golang.org",
    "pkg.go.dev",
    // Java / JVM
    "repo1.maven.org",
    "repo.maven.apache.org",
    "maven.org",
    "gradle.org",
    "www.gradle.org",
    "services.gradle.org",
    "spring.io",
    "kotlinlang.org",
    "www.kotlinlang.org",
    // .NET
    "api.nuget.org",
    "dot.net",
    "dotnet.microsoft.com",
    "packages.microsoft.com",
    // Ruby / Perl / PHP / Swift / Haskell / CocoaPods. The rubygems publish app is out
    // under all three of its names; these are its docs/marketing siblings.
    "ruby-lang.org",
    "www.ruby-lang.org",
    "rubyforge.org",
    "www.rubyforge.org",
    "rubyonrails.org",
    "www.rubyonrails.org",
    "rvm.io",
    "get.rvm.io",
    "cpan.org",
    "www.cpan.org",
    "metacpan.org",
    "www.metacpan.org",
    "api.metacpan.org",
    "repo.packagist.org",
    "swift.org",
    "www.swift.org",
    "haskell.org",
    "www.haskell.org",
    "cocoapods.org",
    "www.cocoapods.org",
    "cdn.cocoapods.org",
    // Containers / Kubernetes. Every registry that answers `docker push` is out;
    // `auth.docker.io` only issues tokens and stores nothing.
    "www.docker.com",
    "auth.docker.io",
    "download.docker.com",
    "production.cloudflare.docker.com",
    "mcr.microsoft.com",
    "*.data.mcr.microsoft.com",
    "dl.k8s.io",
    "pkgs.k8s.io",
    "k8s.io",
    "www.k8s.io",
    // OS distributions
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "ubuntu.com",
    "www.ubuntu.com",
    "*.ubuntu.com",
    "*.nixos.org",
    "yum.oracle.com",
    "download.oracle.com",
    // HashiCorp
    "hashicorp.com",
    "www.hashicorp.com",
    "releases.hashicorp.com",
    "apt.releases.hashicorp.com",
    "archive.releases.hashicorp.com",
    "rpm.releases.hashicorp.com",
    // Apache / Eclipse
    "apache.org",
    "www.apache.org",
    "archive.apache.org",
    "downloads.apache.org",
    "eclipse.org",
    "www.eclipse.org",
    "download.eclipse.org",
    // Vendor docs / marketing. These serve pages and store nothing; the write surfaces
    // that share their brand (dev.azure.com, portal.azure.com, anaconda.org,
    // api.statsig.com, the googleapis control planes) are each out on their own name.
    "oracle.com",
    "www.oracle.com",
    "java.com",
    "www.java.com",
    "java.net",
    "www.java.net",
    "microsoft.com",
    "www.microsoft.com",
    "azure.com",
    "visualstudio.com",
    "cloud.google.com",
    "gcloud.google.com",
    "anaconda.com",
    "www.anaconda.com",
    "continuum.io",
    "statsig.com",
    "www.statsig.com",
    // Identity endpoints. A token exchange returns a credential to the caller; it does
    // not retain caller-chosen bytes for a third party to read back.
    "accounts.google.com",
    "*.microsoftonline.com",
    // Schemas / fonts / vendor docs
    "json-schema.org",
    "www.json-schema.org",
    "json.schemastore.org",
    "www.schemastore.org",
    "fonts.googleapis.com",
    "fonts.gstatic.com",
    "developer.android.com",
    "developer.apple.com",
];

/// Expand `$trusted` into one [`NetRule`] per host with the given effect (Allow for a
/// bare `$trusted`, Deny for `!$trusted`). The trailing-dot normalization matches
/// [`super::fold::push_net_rule`]'s D12 handling so a `$trusted` rule and a hand-written
/// host rule for the same name produce byte-identical IR.
pub fn trusted_net_rules(effect: Effect) -> Vec<NetRule> {
    TRUSTED_HOSTS
        .iter()
        .map(|h| NetRule {
            target: NetTarget::Host(crate::matcher::host::strip_trailing_dot(h).to_string()),
            effect,
        })
        .collect()
}

// ── $downloads (net host set) ──────────────────────────────────────────────────

// THE HOST LIST IS DATA, NOT CODE. `DOWNLOAD_HOSTS` is generated by `build.rs` from
// `data/build-jail-catalog.json`, which is the single home for the membership criteria
// (the exfiltration rule that disqualifies a host), each entry's provenance, and the
// record of measured-but-refused hosts. That file is what a contributor edits and what
// nub may later publish; keeping the rationale beside the data rather than here is what
// stops the two from drifting. `data/README.md` is the contributor-facing schema doc.
//
// Generated as a `static` rather than parsed at startup: the catalog is fixed at compile
// time, so a malformed one is a build failure, and there is no runtime parse to fail.
//
// WHO READS THIS SET — and the BUILD JAIL IS NOT AMONG THEM. Two real consumers: the
// `$downloads` token in the `nub sandbox` policy language, where a loopback proxy does
// enforce per-host; and nub's own prefetcher, which GETs an artifact OUTSIDE the jail
// before a script runs, making this an SSRF bound on nub's fetches. The build jail gates
// egress per PACKAGE as a boolean and starts no proxy — an admitted package reaches every
// host, a refused one reaches none — so nothing here narrows or widens a confined script.
// Per-host in the jail was withdrawn because only macOS could enforce it: Linux needs a
// netns it cannot require, and Windows' loopback exemption is admin-only, so gating on it
// would have failed exactly the platform most developers use. Stated at this length
// because the shorter version kept being read as "the jail's allowlist" — a four-entry
// list sitting beside a package grant set in the hundreds invites the inference, and
// `tests/compiler.rs` pins the real behaviour in both directions so it cannot drift back.
//
// Deliberately NOT `$trusted`, and never merged with it: that set is the AGENT sandbox's
// far broader read-only surface, and it retains three write-capable hosts on
// credential-scoping grounds. The confinement of attacker-authored dependency code
// inherits none of those exceptions — `registry.npmjs.org` is absent precisely because
// `npm publish` is a PUT to the host that serves the read.
include!(concat!(env!("OUT_DIR"), "/download_hosts.rs"));

/// The `$downloads` hosts in force: [`DOWNLOAD_HOSTS`], unless the dev-only catalog override
/// replaced it. Every consumer must read the set through here rather than the `const`, or a
/// dev override would apply to some call sites and not others.
pub fn download_hosts() -> &'static [&'static str] {
    crate::catalog_override::download_hosts().unwrap_or(DOWNLOAD_HOSTS)
}

/// Expand `$downloads` into one [`NetRule`] per host with the given effect (Allow for a
/// bare `$downloads`, Deny for `!$downloads`). Mirrors [`trusted_net_rules`], including
/// its trailing-dot normalization, so the two sets produce byte-comparable IR.
pub fn download_net_rules(effect: Effect) -> Vec<NetRule> {
    download_hosts()
        .iter()
        .map(|h| NetRule {
            target: NetTarget::Host(crate::matcher::host::strip_trailing_dot(h).to_string()),
            effect,
        })
        .collect()
}

// ── $tooldirs (fs cache/store set) ─────────────────────────────────────────────

// Per-OS surface patterns (`~`/`$cache`-anchored) that `expand_symbolic` +
// `subtree_globs` turn into fs rules. nub's OWN dirs are code-grounded against the
// `NUB` embedder profile (crates/nub-cli/src/pm_engine/identity.rs) and
// vendor/aube/crates/aube-store/src/dirs.rs: `data_namespace = "nub"` →
// `~/.local/share/nub/store/…`; `cache_namespace = "nub/pm"` → `<cache>/nub/pm`.
// `$cache` is the platform cache home (XDG_CACHE_HOME else `~/.cache`), which matches
// aube's own `cache_dir()` base on POSIX; the nub store is anchored at the literal
// `~/.local/share` per the deferred-runtime-resolution decision (no XDG_DATA_HOME
// capture in `Homes`). Third-party paths are documented defaults; the override env is
// intentionally NOT read here (static defaults only — runtime resolution is deferred).

#[cfg(target_os = "macos")]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine)
    "~/.local/share/nub/store",
    "$cache/nub/pm",
    // JS package managers
    "~/.npm/_cacache",
    "~/Library/pnpm/store",
    "~/Library/Caches/pnpm",
    "~/Library/Caches/Yarn",
    "~/.yarn/berry/cache",
    "~/.bun/install/cache",
    // Python
    "~/Library/Caches/pip",
    "~/Library/Caches/uv",
    // Other toolchains
    "~/.cargo/registry",
    "~/go/pkg/mod",
    "~/.gradle/caches",
    "~/.m2/repository",
    "~/.nuget/packages",
    "~/.composer/cache",
];

#[cfg(target_os = "windows")]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine) — %LOCALAPPDATA% is `~/AppData/Local` by default
    "~/AppData/Local/nub/store",
    "~/AppData/Local/nub/pm",
    // JS package managers
    "~/AppData/Local/npm-cache",
    "~/AppData/Local/pnpm/store",
    "~/AppData/Local/pnpm-cache",
    "~/AppData/Local/Yarn/Cache",
    "~/AppData/Local/Yarn/Berry/cache",
    "~/.bun/install/cache",
    // Python
    "~/AppData/Local/pip/Cache",
    "~/AppData/Local/uv/cache",
    // Other toolchains
    "~/.cargo/registry",
    "~/go/pkg/mod",
    "~/.gradle/caches",
    "~/.m2/repository",
    "~/.nuget/packages",
    "~/AppData/Local/Composer",
];

// Linux + any other unix (freebsd, …): the XDG layout.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine)
    "~/.local/share/nub/store",
    "$cache/nub/pm",
    // JS package managers
    "~/.npm/_cacache",
    "~/.local/share/pnpm/store",
    "~/.cache/pnpm",
    "~/.cache/yarn",
    "~/.yarn/berry/cache",
    "~/.bun/install/cache",
    // Python
    "~/.cache/pip",
    "~/.cache/uv",
    // Other toolchains
    "~/.cargo/registry",
    "~/go/pkg/mod",
    "~/.gradle/caches",
    "~/.m2/repository",
    "~/.nuget/packages",
    "~/.cache/composer",
];

/// The per-OS `$tooldirs` surface patterns (host OS == target OS).
pub fn tooldir_patterns() -> &'static [&'static str] {
    TOOLDIR_PATTERNS
}

/// Expand `$tooldirs` into fs rules under the resolved home anchors — one rule per
/// subtree glob per pattern, mirroring [`super::defaults::secret_read_denies`] and the
/// [`super::fold::push_fs_rules`] funnel (a Deny normalizes to the inert `FsAccess::DENY`,
/// so two denies differing only in access don't yield divergent IR — D20).
pub fn tooldirs_fs_rules(homes: &Homes, effect: Effect, access: FsAccess) -> Vec<FsRule> {
    let access = if effect == Effect::Deny {
        FsAccess::DENY
    } else {
        access
    };
    let mut out = Vec::new();
    for pattern in tooldir_patterns() {
        let expanded = expand_symbolic(pattern, homes);
        for g in super::defaults::subtree_globs(&expanded) {
            out.push(FsRule {
                matcher: CanonGlob(canonicalize_glob_prefix(&g)),
                effect,
                access,
                origin: FsOrigin::Speculative,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::host::host_pattern_is_valid;
    use crate::matcher::path::compile_glob;
    use std::path::PathBuf;

    fn homes() -> Homes {
        Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        }
    }

    #[test]
    fn every_trusted_host_is_a_valid_pattern() {
        for h in TRUSTED_HOSTS {
            assert!(
                host_pattern_is_valid(h),
                "`{h}` is not a valid host pattern for $trusted"
            );
        }
    }

    #[test]
    fn no_exfiltration_sink_leaked_into_trusted() {
        // Re-syncing the Claude Code base list must not reintroduce a host the confined
        // process can upload to. Two shapes, both disqualifying: an authenticated
        // write-back route, and rentable path/subdomain tenancy.
        for banned in [
            // Multi-tenant object stores.
            "*.amazonaws.com",
            "storage.googleapis.com",
            "*.googleapis.com",
            "*.blob.core.windows.net",
            // Write-back APIs and per-account tenancy (the Shai-Hulud exfil channel).
            // Only the WRITE surface belongs here: the read-only content CDNs on the same
            // brand are trusted, and `delivery_hosts_are_trusted` below pins that split.
            "github.com",
            "www.github.com",
            "api.github.com",
            "gist.github.com",
            "*.github.io",
            // Serves package tarballs, but is a bare Azure Blob account: PUT of a blob
            // path answers 409 PublicAccessNotPermitted where PUT / answers 400, so the
            // storage write API — not a CDN — is what terminates this hostname.
            "pkg-npm.githubusercontent.com",
            // Package registries whose publish route shares the listed hostname.
            // `index.rubygems.org` is the trap: it looks like a read-only index and
            // fronts the same Rails app that serves `gem push`.
            "crates.io",
            "rubygems.org",
            "api.rubygems.org",
            "index.rubygems.org",
            "nuget.org",
            "hex.pm",
            "pub.dev",
            "packagist.org",
            "plugins.gradle.org",
            "upload.pypi.org",
            "anaconda.org",
            "repo.spring.io",
            // A wildcard cannot be shown read-only by inspection: the docs site at the
            // apex is inert, but `registry.` under the same suffix answers /v0/publish.
            "*.modelcontextprotocol.io",
            // Container registries — every one of these is a `docker push` target. The
            // retained Docker entries are artifact CDNs (`download.docker.com`), not
            // registries; that is the whole distinction, and it is easy to lose.
            "ghcr.io",
            "registry-1.docker.io",
            "public.ecr.aws",
            // Not a push target, but its API creates repositories carrying an
            // attacker-authored description.
            "hub.docker.com",
            // Telemetry ingest — an attacker-shaped event payload is exfiltration with a
            // vendor SDK in front of it. `downloads.sentry-cdn.com` is retained because it
            // serves artifacts; the ingest hostnames are a different surface.
            "sentry.io",
            "*.datadoghq.com",
            "api.statsig.com",
            // Non-GitHub forges and cloud control planes — the same per-account write
            // surface that disqualifies `github.com`, minus the name that makes it obvious.
            // A control plane exfiltrates by storing the secret in a resource field the
            // attacker reads back, so it needs no object store of its own.
            "gitlab.com",
            "bitbucket.org",
            "dev.azure.com",
            "sourceforge.net",
            "compute.googleapis.com",
            // Siblings and `www.` twins of RETAINED hosts. The likeliest re-sync mistake:
            // each of these reads as a host already on the list, but fronts its own upload
            // route (`pypi.org` is trusted, `test.pypi.org` is not).
            "test.pypi.org",
            "hackage.haskell.org",
            "npm.pkg.github.com",
            "www.crates.io",
        ] {
            assert!(
                !TRUSTED_HOSTS.contains(&banned),
                "`{banned}` is an exfiltration sink and must not be in $trusted"
            );
        }
    }

    #[test]
    fn delivery_only_hosts_stay_trusted() {
        // The counterweight to the banned list: each of these serves attacker-authorable
        // bytes INTO the sandbox and accepts none back, so an audit that cuts them has
        // silently swapped the exfiltration criterion for an integrity one. They were cut
        // on exactly that mistake once. Every entry answers a write with the same status
        // on a real path as on a bogus one, which is a host refusing the method rather
        // than an auth-gated route declining a caller.
        for delivery in [
            "codeload.github.com",
            "raw.githubusercontent.com",
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
            "avatars.githubusercontent.com",
            "camo.githubusercontent.com",
        ] {
            assert!(
                TRUSTED_HOSTS.contains(&delivery),
                "`{delivery}` only DELIVERS bytes and cannot store them — cutting it \
                 confuses supply-chain integrity with exfiltration (see the module doc)"
            );
        }
    }

    /// The catalog changed WHERE the host list is written, and must not have changed WHAT
    /// it admits. Frozen as a literal rather than re-read from the JSON, so both sides
    /// cannot agree on the same bad parse — and ORDER-SENSITIVE, because the expansion in
    /// `download_net_rules` emits one rule per host in list order and the IR is compared
    /// byte-wise elsewhere.
    #[test]
    fn the_catalog_reproduces_the_pre_catalog_host_list() {
        assert_eq!(
            DOWNLOAD_HOSTS,
            [
                "nodejs.org",
                "binaries.prisma.sh",
                "download.cypress.io",
                "cdn.cypress.io",
            ],
            "the generated $downloads set diverged from the hand-written one it replaced"
        );
    }

    #[test]
    fn every_download_host_is_a_valid_wildcard_free_pattern() {
        // Wildcard-freedom is the set's structural anti-exfiltration property, not a
        // formatting preference: the proxy resolves the name, so a `*.suffix` member would
        // let a confined script put chosen bytes in a DNS label and leak them to an
        // attacker-run nameserver without sending a payload at all.
        for h in DOWNLOAD_HOSTS {
            assert!(
                host_pattern_is_valid(h),
                "`{h}` is not a valid host pattern for $downloads"
            );
            assert!(
                !h.contains('*'),
                "`{h}` — $downloads must stay wildcard-free: a subdomain wildcard admits \
                 DNS-label exfiltration under the same hostname"
            );
        }
    }

    #[test]
    fn no_write_capable_or_multi_tenant_host_leaked_into_downloads() {
        // The set is meant to grow by PR as more install-time downloaders are covered.
        // These are the shapes such a PR must never add: a host the confined script can
        // upload to, and a namespace an attacker can rent under the same hostname and read
        // back. The GitHub-release and object-store families are UNSOLVED here on purpose —
        // they need pre-download brokering, not an allowlist entry.
        for banned in [
            "storage.googleapis.com",
            "*.googleapis.com",
            "*.amazonaws.com",
            "*.blob.core.windows.net",
            "github.com",
            "api.github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
            "raw.githubusercontent.com",
            "*.github.io",
            "registry.npmjs.org",
            "ghcr.io",
            "sentry.io",
        ] {
            assert!(
                !DOWNLOAD_HOSTS.contains(&banned),
                "`{banned}` accepts a write or is multi-tenant — it must not be in $downloads"
            );
        }
    }

    #[test]
    fn every_tooldir_glob_compiles() {
        let rules = tooldirs_fs_rules(&homes(), Effect::Allow, FsAccess::Read);
        assert!(!rules.is_empty());
        for r in &rules {
            compile_glob(r.matcher.as_str()).unwrap_or_else(|e| {
                panic!(
                    "tooldir glob `{}` failed to compile: {e}",
                    r.matcher.as_str()
                )
            });
        }
    }

    #[test]
    fn nub_store_and_cache_paths_are_present() {
        // Guards the coupling to the NUB embedder profile (identity.rs
        // `data_namespace = "nub"` / `cache_namespace = "nub/pm"`): if those move, this
        // set must move with them.
        let rules = tooldirs_fs_rules(&homes(), Effect::Allow, FsAccess::Read);
        let matchers: Vec<&str> = rules.iter().map(|r| r.matcher.as_str()).collect();
        assert!(
            matchers.iter().any(|m| m.contains("nub/store")),
            "nub CAS store path missing from $tooldirs: {matchers:?}"
        );
        assert!(
            matchers.iter().any(|m| m.contains("nub/pm")),
            "nub PM cache path missing from $tooldirs: {matchers:?}"
        );
    }

    #[test]
    fn deny_effect_normalizes_access_to_the_inert_value() {
        // A `!$tooldirs` deny carries the canonical inert access (D20), same as the fs funnel.
        let rules = tooldirs_fs_rules(&homes(), Effect::Deny, FsAccess::ReadWrite);
        assert!(
            rules
                .iter()
                .all(|r| r.effect == Effect::Deny && r.access == FsAccess::DENY)
        );
    }
}
