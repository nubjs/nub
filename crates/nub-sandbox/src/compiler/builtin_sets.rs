//! The two built-in `$`-sets the compiler expands in place: `$trusted` (a curated
//! network host allowlist, net axis) and `$tooldirs` (the per-OS package-manager /
//! toolchain cache+store dirs, fs axis). Both are ORDINARY last-match-wins entries —
//! a set expands at its authored position and a later rule can override any member,
//! like a `...:#/pointer`-reused list's entries.
//!
//! Provenance / curation:
//! - `$trusted` derives from the Claude Code default-allowed-domains list, filtered to
//!   hosts that are READ-ONLY to the confined process. Two shapes are excluded, both
//!   because reaching them is indistinguishable from uploading to them: a write-back
//!   route (an authenticated API that can publish a package, create a repo, or post a
//!   paste — the Shai-Hulud propagation primitive) and rentable path/subdomain tenancy
//!   (multi-tenant object stores, forge user pages). Membership was settled by probing
//!   each ecosystem's *documented* publish route against a same-host bogus-path control,
//!   which is what distinguishes "route exists, auth-gated" from "host denies this
//!   method wholesale"; that is how `index.rubygems.org` was caught fronting the same
//!   Rails app as `rubygems.org`. Three caveats bound what this set can promise. An
//!   entry is NOT protocol-scoped — the egress proxy tunnels arbitrary TCP via
//!   CONNECT/SOCKS5, so allowing a host admits non-HTTP upload transports too (why the
//!   `dput` PPA target is absent). A CNAME onto a shared CDN is opaque to any host
//!   allowlist, since the tenant-selecting `Host` header travels inside TLS
//!   (`index.crates.io` / `static.crates.io` are kept as single-tenant buckets on that
//!   basis, not because the hostname proves it). And `registry.npmjs.org` is retained
//!   despite failing the rule: reading it is how anything installs, `npm publish` is a
//!   PUT to that same host, and the proxy gates only the CONNECT authority and TLS SNI
//!   before blind-forwarding — so it cannot tell the two apart. Metadata/link-local +
//!   RFC1918 are a SEPARATE always-on hard floor, never part of this set. Source data:
//!   `.fray/sandbox-builtin-sets.md`, `.fray/sandbox-shai-hulud-exfil.md`.
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
use crate::policy::{CanonGlob, Effect, FsAccess, FsRule, NetRule, NetTarget};

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
    // npm
    "registry.npmjs.org",
    // Node / JS
    "nodejs.org",
    "www.nodejs.org",
    "binaries.prisma.sh",
    "downloads.sentry-cdn.com",
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
    // Ruby / Perl / PHP / Swift / Haskell / CocoaPods
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
    // Containers / Kubernetes
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
            "github.com",
            "www.github.com",
            "api.github.com",
            "gist.github.com",
            "codeload.github.com",
            "raw.githubusercontent.com",
            "objects.githubusercontent.com",
            "*.github.io",
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
        ] {
            assert!(
                !TRUSTED_HOSTS.contains(&banned),
                "`{banned}` is an exfiltration sink and must not be in $trusted"
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
