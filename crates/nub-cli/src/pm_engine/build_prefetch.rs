//! Prefetch recognized dependency binaries before lifecycle confinement.
//!
//! Artifact URLs come from package manifests. Fetches and redirects are limited to
//! `nub_sandbox::DOWNLOAD_HOSTS`; the optional `prefetch-github-hosts` Cargo feature
//! adds the explicit GitHub release hosts below. This bounds the installer's network
//! access independently of the lifecycle script's catalog policy.
//!
//! Existing artifacts remain usable without a fetch. Prefetch failures are nonfatal:
//! the package's own installer decides whether to download or build from source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use super::build_jail::ProbeScope;

/// Additional release hosts admitted by the optional prefetch feature.
///
/// THE REDIRECT TARGETS ARE LOAD-BEARING ENTRIES, not conveniences. [`fetch`] re-applies
/// the allowlist to every hop, so a `github.com` release-asset URL only resolves if the
/// host it 302s to is here too. Measured 2026-07-28 against a real asset:
/// `github.com/<o>/<r>/releases/download/…` → **`release-assets.githubusercontent.com`**
/// (a signed, expiring URL). `objects.githubusercontent.com` is the older spelling of the
/// same asset CDN, retained because directly-published asset URLs still use it.
///
/// Deliberately ABSENT: `raw.githubusercontent.com`. `github.com/<o>/<r>/raw/…` redirects
/// there (verified), which is a fine way to serve arbitrary repo content but is not how a
/// release artifact is published — admitting it would widen the fetchable surface from
/// "release assets" to "any file in any repo" for no covered package.
#[cfg(feature = "prefetch-github-hosts")]
const GITHUB_PREFETCH_HOSTS: &[&str] = &[
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

fn extra_prefetch_hosts() -> &'static [&'static str] {
    #[cfg(feature = "prefetch-github-hosts")]
    {
        GITHUB_PREFETCH_HOSTS
    }
    #[cfg(not(feature = "prefetch-github-hosts"))]
    {
        &[]
    }
}

/// What a lifecycle script's install command will look for locally before it opens a
/// socket. Selected by which family token appears FIRST in the script line, because a
/// `A || B` chain runs A first and B only if A fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `prebuild-install` — checks `prebuilds/<basename(url)>` in the package dir before
    /// the npm cache and before any request (`download.js`, the `opts.nolocal` branch).
    PrebuildInstall,
    /// `@mapbox/node-pre-gyp install` — a `file://` `binary.host` takes
    /// `extract_from_local` and never constructs a request (`lib/install.js`).
    NodePreGyp,
}

/// Prefetch the artifact for `spawn`'s package if its install command belongs to a family
/// with a local-pickup contract, mutating `ambient` where the family needs an env var to
/// find it. Returns read subtrees the placed artifact needs granted (empty unless the
/// pickup path lies outside the package dir).
///
/// Infallible by construction — see the module doc's fail-soft rule.
pub(super) fn prefetch(
    spawn: &aube_util::LifecycleSandboxSpawn,
    ambient: &mut BTreeMap<String, String>,
    probe: &ProbeScope,
) -> Vec<PathBuf> {
    let Some(family) = detect_family(&spawn.args.command_line()) else {
        return Vec::new();
    };
    // ANCHOR: `bin.js:7` does `require(path.resolve('package.json'))` BEFORE the
    // `chdir(rc.path)` on line 20, so prebuild-install reads its manifest from the script's
    // original cwd, not from the package dir. The two coincide for an ordinary dependency
    // hook and diverge for a git-dependency's root script; reading the wrong one derives a
    // URL for the wrong package, so the pickup silently misses. node-pre-gyp resolves its
    // own module root instead, and is left on `package_dir`.
    let manifest_dir = match family {
        Family::PrebuildInstall => &spawn.cwd,
        Family::NodePreGyp => &spawn.package_dir,
    };
    let Some(manifest) = read_manifest(manifest_dir).or_else(|| read_manifest(&spawn.package_dir))
    else {
        return Vec::new();
    };
    let Some(node) = node_facts(ambient, probe) else {
        return Vec::new();
    };
    match family {
        Family::PrebuildInstall => {
            prebuild_install(spawn, ambient, &manifest, node);
            Vec::new()
        }
        Family::NodePreGyp => node_pre_gyp(ambient, &manifest, node).unwrap_or_default(),
    }
}

/// Which install family the script line belongs to, by FIRST occurrence. `prebuild-install
/// || node-gyp rebuild` is the canonical shape: the fallback never runs when the prefetched
/// artifact satisfies the first command, so keying on position (not on a fixed precedence)
/// is what makes a chain resolve to the command that actually decides the outcome.
fn detect_family(script: &str) -> Option<Family> {
    let prebuild = script.find("prebuild-install");
    // `node-pre-gyp` also appears in `… node-pre-gyp rebuild`, which has no download step
    // to short-circuit; require the install verb before claiming the family.
    let pre_gyp = script
        .contains("install")
        .then(|| script.find("node-pre-gyp"))
        .flatten();
    match (prebuild, pre_gyp) {
        (Some(p), Some(g)) => Some(if p <= g {
            Family::PrebuildInstall
        } else {
            Family::NodePreGyp
        }),
        (Some(_), None) => Some(Family::PrebuildInstall),
        (None, Some(_)) => Some(Family::NodePreGyp),
        (None, None) => None,
    }
}

/// Parse a `package.json`, refusing an implausibly large one.
///
/// The cap bounds WORK, not trust. Every template string downstream is manifest-derived,
/// and `expand` makes ~25 sequential `String::replace` passes while `github_from_package`
/// serialises the whole document — all of it unconfined, on the install's critical path,
/// before any network call. A real prebuilt-addon manifest is a few KB, so a dependency
/// shipping megabytes of JSON is buying compute, not describing a package.
fn read_manifest(package_dir: &Path) -> Option<Value> {
    const MAX_MANIFEST: u64 = 1024 * 1024;
    let path = package_dir.join("package.json");
    if std::fs::metadata(&path).ok()?.len() > MAX_MANIFEST {
        tracing::debug!(path = %path.display(), "prefetch: manifest over the size cap");
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
}

// ── the running interpreter's build identity ───────────────────────────────────

/// The values every artifact filename is keyed on, read from the interpreter that will
/// actually load the addon.
#[derive(Debug, Clone)]
pub(super) struct NodeFacts {
    /// `process.versions.node`.
    version: String,
    /// `process.versions.modules` — the ABI tag (`node-v137`, `-v127-`).
    modules: String,
    /// `process.versions.napi`, 0 when unsupported.
    napi: u32,
    /// `process.platform` (`darwin` / `linux` / `win32`).
    platform: String,
    /// `process.arch` (`arm64` / `x64` / …).
    arch: String,
}

/// ASK THE INTERPRETER, do not tabulate. Every other input here is derivable in Rust, but
/// `process.versions.modules` and `.napi` are not: mapping a Node version to its ABI needs
/// node-abi's crosswalk table, and a table copied into this repo is stale the day a Node
/// major ships — silently producing a URL that 404s. One `-p` probe is exact and
/// self-maintaining, and it is amortized: memoized for the process, so an install of N
/// native packages pays it once.
///
/// Probes `npm_node_execpath` (aube's spelling of the provisioned Node), NOT `NODE` — the
/// latter is the PATH shim, and the shim's own re-exec makes it the wrong thing to
/// interrogate. The candidate is filtered through [`ProbeScope`] for the same reason the
/// Python probe is: this runs UNCONFINED in nub's own process before any policy exists, so
/// anything a dependency can author into the path must not be executed. A refusal is a
/// skip — prefetch simply does not happen.
fn node_facts(
    ambient: &BTreeMap<String, String>,
    probe: &ProbeScope,
) -> Option<&'static NodeFacts> {
    // PROCESS-WIDE memo against a PER-SPAWN `probe`: the first package to reach here
    // decides for the whole install. Deliberate — one install provisions one Node, so the
    // answer is invariant, and the alternative (a keyed map) buys nothing. What it is NOT
    // is a capability leak: the memo holds parsed version strings, and the execution that
    // produced them already passed the scope that authorized it. A later spawn whose scope
    // would have refused simply inherits inert data. If the execpath ever did vary
    // mid-install, the stale facts would derive a wrong ABI and the prefetch would miss —
    // fail-soft, like every other decline here.
    static FACTS: OnceLock<Option<NodeFacts>> = OnceLock::new();
    FACTS
        .get_or_init(|| {
            let exec = Path::new(ambient.get("npm_node_execpath")?);
            if !probe.allows(exec) {
                return None;
            }
            let out = std::process::Command::new(exec)
                .arg("-p")
                .arg(
                    "[process.versions.node,process.versions.modules,\
                     process.versions.napi||0,process.platform,process.arch].join(' ')",
                )
                .output()
                .ok()?;
            parse_node_facts(&String::from_utf8_lossy(&out.stdout))
        })
        .as_ref()
}

/// The interpreter's `process.versions.node`, when it could be asked. Shares the memo above,
/// so a caller that only wants the version pays nothing beyond what the header prefetch has
/// already spent.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn node_version(
    ambient: &BTreeMap<String, String>,
    probe: &ProbeScope,
) -> Option<&'static str> {
    node_facts(ambient, probe).map(|facts| facts.version.as_str())
}

/// The cache key for a copy of this interpreter's distribution: the version it reports plus its
/// architecture. Shares the memo above, so it costs nothing beyond what the header prefetch has
/// already spent.
///
/// The VERSION is what makes a staged copy ABI-correct by construction rather than by assertion
/// (see `jail_bin`), and the ARCH is what keeps a same-version x64 and arm64 install from
/// colliding on one directory.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn node_dist_key(
    ambient: &BTreeMap<String, String>,
    probe: &ProbeScope,
) -> Option<String> {
    let facts = node_facts(ambient, probe)?;
    Some(format!("{}-{}", facts.version, facts.arch))
}

/// Separated from the spawn so the parse is unit-testable without a Node on disk.
fn parse_node_facts(stdout: &str) -> Option<NodeFacts> {
    let line = stdout.lines().next()?;
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() != 5 {
        return None;
    }
    Some(NodeFacts {
        version: f[0].to_string(),
        modules: f[1].to_string(),
        napi: f[2].parse().unwrap_or(0),
        platform: f[3].to_string(),
        arch: f[4].to_string(),
    })
}

/// `detect-libc`'s `familySync()`: the C library family on Linux, `None` elsewhere.
///
/// The two families consume this through DIFFERENT rules, so it deliberately returns the
/// raw family rather than a formatted slot — collapsing them into one helper is what
/// produced a node-pre-gyp URL of `…-linux-unknown-…` on every glibc machine.
fn libc_family(platform: &str) -> Option<&'static str> {
    if platform != "linux" {
        return None;
    }
    // The observable half of detect-libc's interpreter/filesystem probes: a musl system
    // ships its loader as `/lib/ld-musl-<arch>.so.1`.
    let musl = std::fs::read_dir("/lib").is_ok_and(|d| {
        d.filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
    });
    Some(if musl { "musl" } else { "glibc" })
}

/// prebuild-install's `libc` slot, which is EMPTY except on musl Linux.
///
/// PROVENANCE (`rc.js:56`): `rc.libc = rc.platform !== 'linux' || rc.libc ===
/// detectLibc.GLIBC ? '' : rc.libc`. Off Linux, and on glibc Linux, the slot is
/// force-blanked — so the `{platform}{libc}` pair is a bare `darwin` / `linux`, and only a
/// musl host produces the CONCATENATED `linuxmusl` (no separator: the template is
/// `{platform}{libc}`, not `{platform}-{libc}`). Getting this wrong misses every path.
fn prebuild_install_libc(platform: &str, ambient: &BTreeMap<String, String>) -> String {
    if platform != "linux" {
        return String::new();
    }
    let family = ambient
        .get("LIBC")
        .or_else(|| ambient.get("npm_config_libc"))
        .map(String::as_str)
        .or_else(|| libc_family(platform))
        .unwrap_or_default();
    if family == "glibc" {
        String::new()
    } else {
        family.to_string()
    }
}

/// node-pre-gyp's `{libc}` slot, which is a very different rule from the above.
///
/// PROVENANCE (`versioning.js:305`): `libc: options.target_libc || detect_libc.familySync()
/// || 'unknown'`. There is NO glibc-blanking — `familySync()` returns the literal `glibc`
/// on a glibc host, so packages templating `{libc}` publish `…-linux-glibc-x64.tar.gz`.
/// Only off Linux, where `familySync()` is null, does the slot become `unknown`.
fn node_pre_gyp_libc(platform: &str, ambient: &BTreeMap<String, String>) -> String {
    ambient
        .get("npm_config_target_libc")
        .map(String::as_str)
        .or_else(|| libc_family(platform))
        .unwrap_or("unknown")
        .to_string()
}

// ── the Node headers node-gyp compiles against ─────────────────────────────────

/// Land the Node headers (and, for a Windows target, the `node.lib` import library) on
/// local disk out of jail, and return the directory to name in `npm_config_nodedir`.
/// `None` on any refusal — the caller then leaves `nodedir` alone, exactly as before.
///
/// WHY A FETCH IS NEEDED AT ALL. Every POSIX distribution ships `include/node` inside the
/// Node root, so `nodedir` names that root, node-gyp reads the headers off the filesystem
/// and never opens a socket. The WINDOWS distribution ships neither: `node-v22.20.0-win
/// -x64.zip` contains zero `include/`, `.h` or `.lib` entries — it is `node.exe`,
/// `node_modules/` and a handful of shims. And node-gyp does not degrade gracefully from
/// that. With `nodedir` set it SKIPS its header download outright (`configure.js`,
/// `getNodeDir`), so the compile fails on a missing `node.h`; with `nodedir` unset it
/// downloads into its devdir — which the Windows jail's net deny-all refuses. Either way
/// no native addon could build on Windows under the jail.
///
/// So this is the [module doc's](self) prefetch move applied to the toolchain itself: nub
/// performs the GET, unconfined, against a host `$downloads` already carries, and the
/// confined build finds a local tree. The Windows jail STAYS deny-all — nothing here
/// widens a net axis.
///
/// The result is cached under nub's cache dir keyed on the Node version, so an install of
/// N native packages fetches once and later installs fetch not at all.
pub(super) fn node_headers(
    ambient: &BTreeMap<String, String>,
    probe: &ProbeScope,
) -> Option<PathBuf> {
    // A user who named their own dist URL is building against a Node that is not
    // nodejs.org's, so upstream headers would answer a question they did not ask. Decline
    // and leave them the behavior they had.
    if ["npm_config_disturl", "npm_config_dist_url"]
        .iter()
        .any(|key| ambient.get(*key).is_some_and(|v| !v.is_empty()))
    {
        return None;
    }
    let facts = node_facts(ambient, probe)?;
    // The TARGET's platform, read off the interpreter that will load the addon — not
    // `cfg!(windows)`. The import library is a property of the Node being compiled
    // against, and that is what `node_facts` reports.
    let want_lib = facts.platform == "win32";
    let dir = cache_root()?.join("node-headers").join(&facts.version);
    if headers_ready(&dir, want_lib) {
        return Some(dir);
    }
    materialize_headers(&dir, facts, want_lib)
}

/// The two files whose presence means a later install can skip the fetch. Existence-only,
/// like every other hit test here — which is why [`materialize_headers`] publishes by
/// renaming a fully-populated staging dir rather than filling `dir` in place.
fn headers_ready(dir: &Path, want_lib: bool) -> bool {
    dir.join("include").join("node").join("node.h").is_file()
        && (!want_lib || dir.join("Release").join("node.lib").is_file())
}

fn materialize_headers(dir: &Path, facts: &NodeFacts, want_lib: bool) -> Option<PathBuf> {
    let version = &facts.version;
    let parent = dir.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let staging = tempfile::TempDir::new_in(parent).ok()?;

    let tarball = staging.path().join("headers.tar.gz");
    let url = format!("https://nodejs.org/dist/v{version}/node-v{version}-headers.tar.gz");
    if !host_allowed(&url) {
        return None;
    }
    fetch(&url, &tarball)?;
    extract_headers(&tarball, staging.path(), version)?;
    std::fs::remove_file(&tarball).ok();

    if want_lib {
        let url = format!(
            "https://nodejs.org/dist/v{version}/win-{}/node.lib",
            win_lib_arch(&facts.arch)?
        );
        if !host_allowed(&url) {
            return None;
        }
        // node-gyp resolves the import library as `<nodedir>/$(Configuration)/node.lib`
        // whenever nodedir is set (`configure.js`: the `!gyp.opts.nodedir` ternary picks
        // `<(target_arch)` only when it is NOT). `$(Configuration)` is an MSBuild macro
        // resolved long after nub is gone, and `create-config-gypi.js` can emit exactly
        // `Release` or `Debug`, so both names must hold the file.
        let release = staging.path().join("Release").join("node.lib");
        std::fs::create_dir_all(release.parent()?).ok()?;
        fetch(&url, &release)?;
        let debug = staging.path().join("Debug").join("node.lib");
        std::fs::create_dir_all(debug.parent()?).ok()?;
        std::fs::copy(&release, &debug).ok()?;
    }

    let staged = staging.keep();
    match std::fs::rename(&staged, dir) {
        Ok(()) => Some(dir.to_path_buf()),
        // A concurrent install published first — its tree is equivalent, so adopt it
        // rather than failing. Anything else (a real error) leaves nodedir unset.
        Err(_) => {
            std::fs::remove_dir_all(&staged).ok();
            headers_ready(dir, want_lib).then(|| dir.to_path_buf())
        }
    }
}

/// `process.arch` in the spelling nodejs.org publishes the import library under.
fn win_lib_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x64" => Some("x64"),
        "arm64" => Some("arm64"),
        "ia32" => Some("x86"),
        _ => None,
    }
}

/// Unpack the tarball's `node-v<version>/include/` subtree into `<dest>/include/`.
///
/// PATH SAFETY IS THE POINT, not a precaution. This runs UNCONFINED, as the user, over a
/// body that arrived from the network, so an entry naming `../` or an absolute path would
/// be an arbitrary file write — the same defect class this module's own review already
/// found twice. Every entry must sit under the expected prefix AND consist only of
/// ordinary components, checked on the DECODED path rather than the raw string.
///
/// The byte and entry budgets bound the other half: [`fetch`]'s cap bounds the compressed
/// body, which says nothing about what it expands to.
fn extract_headers(tarball: &Path, dest: &Path, version: &str) -> Option<()> {
    const MAX_UNPACKED: u64 = 256 * 1024 * 1024;
    const MAX_ENTRIES: usize = 50_000;
    let file = std::fs::File::open(tarball).ok()?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let want = PathBuf::from(format!("node-v{version}")).join("include");
    let mut budget = MAX_UNPACKED;
    let mut entries = 0usize;
    let mut wrote_a_header = false;
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return None;
        }
        let path = entry.path().ok()?.into_owned();
        let Ok(relative) = path.strip_prefix(&want) else {
            continue;
        };
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        let out = dest.join("include").join(relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&out).ok()?;
            continue;
        }
        // Regular files only. A symlink or hardlink entry is the other way a tar reaches
        // outside its root, and the headers tarball contains none.
        if !kind.is_file() {
            continue;
        }
        budget = budget.checked_sub(entry.size())?;
        std::fs::create_dir_all(out.parent()?).ok()?;
        std::io::copy(&mut entry, &mut std::fs::File::create(&out).ok()?).ok()?;
        wrote_a_header = true;
    }
    wrote_a_header.then_some(())
}

// ── prebuild-install ───────────────────────────────────────────────────────────

/// Derive the artifact URL, fetch it, and drop it at `<pkgdir>/prebuilds/<basename(url)>`.
///
/// That path is checked FIRST — before the npm cache, before any request — and the check
/// is `fs.access(R_OK | W_OK)`, so the placed file must be writable, not a read-only copy.
/// The bytes are placed VERBATIM: `download.js` gunzips and untars them itself, so nub
/// never needs to understand the archive.
fn prebuild_install(
    spawn: &aube_util::LifecycleSandboxSpawn,
    ambient: &BTreeMap<String, String>,
    manifest: &Value,
    node: &NodeFacts,
) -> Option<()> {
    let flags = prebuild_flags(&spawn.args.command_line());
    // Three flags make a placed file unreadable by construction, so writing one would be
    // pure waste: `--nolocal` returns straight to `download()` ahead of the local probe
    // (`download.js:18`), `--build-from-source` skips the download path outright, and
    // `--token` diverts to the asset-API URL (`bin.js:67`), whose basename is a numeric
    // asset id the local probe would look for instead. Tested by VALUE, not presence —
    // `--no-build-from-source` sets the key to `false` and must not decline.
    if ["nolocal", "buildFromSource", "token"]
        .iter()
        .any(|k| flags.get(*k).is_some_and(|v| v != "false"))
    {
        return None;
    }
    let url = prebuild_install_url(manifest, ambient, node, &flags)?;
    // Anchored on the script's CWD, not `package_dir`: `localPrebuild` is cwd-relative
    // (`util.js` `localPrebuild` joins onto `rc.path`, which `bin.js` chdir's to). The two
    // coincide for an ordinary dependency hook but diverge for a fetched git dependency's
    // root script, where writing into the packed checkout would make its fingerprint
    // host-dependent.
    let root = spawn.cwd.clone();
    // `-p/--path` moves that anchor (`bin.js:20` chdir's to the resolved `rc.path`). It
    // stays a SEGMENT rather than a new root so `contained_dest` still adjudicates it —
    // an absolute or `..`-bearing value is then a decline, not a write outside the cwd.
    let rc_path = flags
        .get("path")
        .map(|p| p.trim_start_matches("./"))
        .filter(|p| !p.is_empty() && *p != ".");
    let prefix = local_prebuilds_prefix(manifest, ambient, &flags);
    let basename = Some(url_basename(&url)).filter(|b| !b.is_empty())?;
    let dest = contained_dest(
        &root,
        rc_path.into_iter().chain([prefix.as_str(), basename]),
    )?;
    place(&url, &root, &dest)
}

/// The `prebuild-install` invocation's OWN flags, parsed from its segment of the script
/// line.
///
/// PROVENANCE (`rc.js:35`, `rc/index.js`): these are handed to `rc` as its ARGV layer, and
/// `rc` returns `deepExtend(defaults, …files, env, argv)` — argv LAST. So a command-line
/// flag outranks `package.json#config`, every `npm_config_*`, and the built-in defaults.
/// `"install": "prebuild-install -r napi"` is the shape this exists for: honouring only
/// `config.runtime` derives the wrong ABI slot, the placed filename never matches the one
/// the installer probes for, and the prefetch silently misses.
///
/// Scoped to the ONE command rather than the whole line: `prebuild-install || node-gyp
/// rebuild` is the canonical shape, and letting the fallback's flags leak in would
/// attribute node-gyp's `--arch` to prebuild-install. Cutting at the first shell operator
/// errs toward attributing too FEW flags, which is the fail-soft direction.
fn prebuild_flags(line: &str) -> BTreeMap<String, String> {
    let Some(idx) = line.find("prebuild-install") else {
        return BTreeMap::new();
    };
    let tail = &line[idx + "prebuild-install".len()..];
    let end = tail.find([';', '\n', '&', '|', ')']).unwrap_or(tail.len());
    parse_flag_tokens(&shell_tokens(&tail[..end]))
}

/// Whitespace-split honouring single/double quotes — enough for the flag values a
/// lifecycle line carries (`--download 'https://…'`), not a general shell parser.
fn shell_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// minimist's shapes, restricted to the ones a lifecycle script actually writes:
/// `--key=v`, `--key v`, `-k v`, `-k=v`, bare `--flag`, and `--no-key`. A value-taking
/// flag consumes the next token only when it does not itself look like a flag.
fn parse_flag_tokens(tokens: &[String]) -> BTreeMap<String, String> {
    // `rc.js`'s own alias table, plus its `buildFromSource`/`build-from-source` pair.
    fn canonical(k: &str) -> &str {
        match k {
            "t" => "target",
            "r" => "runtime",
            "a" => "arch",
            "p" => "path",
            "d" => "download",
            "T" => "token",
            "build-from-source" => "buildFromSource",
            other => other,
        }
    }
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        let body = match token.strip_prefix("--") {
            Some(b) if !b.is_empty() => b,
            _ => match token.strip_prefix('-') {
                Some(b) if !b.is_empty() && !b.starts_with('-') => b,
                _ => {
                    i += 1;
                    continue;
                }
            },
        };
        if let Some((k, v)) = body.split_once('=') {
            out.insert(canonical(k).to_string(), v.to_string());
        } else if let Some(k) = token
            .starts_with("--")
            .then(|| body.strip_prefix("no-"))
            .flatten()
        {
            out.insert(canonical(k).to_string(), "false".to_string());
        } else if tokens.get(i + 1).is_some_and(|n| !n.starts_with('-')) {
            // An EMPTY next token is still a value: minimist gates only on `/^-/`, and
            // `--tag-prefix ''` is the real spelling for a package whose assets carry no
            // `v` prefix. Only an explicit `''`/`""` reaches here — unquoted whitespace
            // never produces an empty token.
            out.insert(canonical(body).to_string(), tokens[i + 1].clone());
            i += 1;
        } else {
            out.insert(canonical(body).to_string(), "true".to_string());
        }
        i += 1;
    }
    out
}

/// The `prebuilds/` directory name. `util.js` `localPrebuild` reads
/// `process.env[<prefix>_local_prebuilds] || opts['local-prebuilds'] || 'prebuilds'` — the
/// env var outranks the flag here, the one place in this family where it does.
fn local_prebuilds_prefix(
    manifest: &Value,
    ambient: &BTreeMap<String, String>,
    flags: &BTreeMap<String, String>,
) -> String {
    let name = manifest["name"].as_str().unwrap_or_default();
    ambient
        .get(&format!("{}_local_prebuilds", prebuild_env_prefix(name)))
        .or_else(|| flags.get("local-prebuilds"))
        .cloned()
        .unwrap_or_else(|| "prebuilds".to_string())
}

/// prebuild-install's env-var prefix for a package (`util.js` `getEnvPrefix`):
/// `npm_config_` + the FULL name with every non-alphanumeric collapsed to `_`, then a
/// leading `_` stripped. Note the contrast with node-pre-gyp's spelling, which replaces
/// only the first dash — the two families are NOT interchangeable here.
fn prebuild_env_prefix(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("npm_config_{}", sanitized.trim_start_matches('_'))
}

/// Build the download URL exactly as `util.js` `getDownloadUrl` would.
fn prebuild_install_url(
    manifest: &Value,
    ambient: &BTreeMap<String, String>,
    node: &NodeFacts,
    flags: &BTreeMap<String, String>,
) -> Option<String> {
    let name = manifest["name"].as_str()?;
    let version = manifest["version"].as_str()?;
    let config = &manifest["config"];

    let runtime = flags
        .get("runtime")
        .map(String::as_str)
        .or_else(|| config["runtime"].as_str())
        .or_else(|| ambient.get("npm_config_runtime").map(String::as_str))
        .unwrap_or("node");
    // `config.target` is read with `json_scalar`, not `as_str`, because it is routinely a
    // JSON NUMBER — `keytar` ships `"config": { "target": 3, "runtime": "napi" }`. Reading
    // only the string spelling silently falls through to the running Node's version and
    // derives the wrong ABI slot for every package that pins one this way.
    let target = flags
        .get("target")
        .cloned()
        .or_else(|| json_scalar(&config["target"]))
        .or_else(|| ambient.get("npm_config_target").cloned())
        .unwrap_or_else(|| node.version.clone());

    // The ABI slot. `node` resolves through node-abi's crosswalk and `electron` /
    // `node-webkit` through their own tables — none of which nub carries (see
    // `node_facts`), so those decline and the script falls back to what it would have
    // done unprefetched.
    //
    // PROVENANCE (`rc.js:53-55`): for a napi runtime the ABI slot IS the target, and
    // `getBestNapiBuildVersion()` is consulted ONLY when the package pinned no target of
    // its own — the replacement is gated on `rc.target === process.versions.node`, i.e. on
    // the default still being in place. A package that pins `target: 3` gets ABI 3
    // regardless of what `binary.napi_versions` says.
    let abi = match runtime {
        "napi" if target != node.version => target.clone(),
        "napi" => best_napi_version(manifest, node.napi)?.to_string(),
        "node" if target == node.version => node.modules.clone(),
        _ => return None,
    };

    // `rc.js:22-24`: platform has NO `pkgConf` layer while arch does — reproduced rather
    // than unified, since a package pinning `config.arch` really does move only the one.
    let platform = flags
        .get("platform")
        .or_else(|| ambient.get("npm_config_platform"))
        .cloned()
        .unwrap_or_else(|| node.platform.clone());
    let arch = flags
        .get("arch")
        .cloned()
        .or_else(|| config["arch"].as_str().map(str::to_string))
        .or_else(|| ambient.get("npm_config_arch").cloned())
        .unwrap_or_else(|| node.arch.clone());

    // PROVENANCE (`util.js:8`): the `{name}` slot is the package name with its `@scope/`
    // STRIPPED — `@serialport/bindings-cpp` publishes `bindings-cpp-v…`. The env-var
    // spelling above uses the full name; only this one is unscoped.
    let unscoped = strip_scope(name);
    let vars: BTreeMap<&str, String> = BTreeMap::from([
        ("name", unscoped.to_string()),
        ("package_name", unscoped.to_string()),
        ("version", version.to_string()),
        ("major", version.split('.').next().unwrap_or("").to_string()),
        ("minor", nth_dot(version, 1)),
        ("patch", nth_dot(version, 2)),
        // `String(undefined)` is the literal `"undefined"` in expand-template, and
        // `version.split('-')[1]` IS undefined for a release version — so a template
        // naming `{prerelease}` on `1.2.3` really does resolve to `…undefined…`. Faithful
        // reproduction is the point: a "tidier" empty string would build a URL that
        // differs from the one prebuild-install asks for, and the prefetch would miss.
        ("prerelease", after_or_undefined(version, '-')),
        ("build", after_or_undefined(version, '+')),
        ("abi", abi.clone()),
        ("node_abi", node.modules.clone()),
        ("runtime", runtime.to_string()),
        ("platform", platform),
        ("arch", arch),
        (
            "libc",
            flags
                .get("libc")
                .cloned()
                .unwrap_or_else(|| prebuild_install_libc(&node.platform, ambient)),
        ),
        // `util.js:24` keys this off `opts.debug`, whose rc default is the STRING test
        // `env.npm_config_debug === 'true'` (`rc.js:26`) — so any other env spelling is
        // false, while a bare `--debug` on the line is true.
        (
            "configuration",
            if flags.get("debug").is_some_and(|v| v != "false")
                || ambient.get("npm_config_debug").is_some_and(|v| v == "true")
            {
                "Debug".to_string()
            } else {
                "Release".to_string()
            },
        ),
        (
            "module_name",
            manifest["binary"]["module_name"]
                .as_str()
                .unwrap_or("undefined")
                .to_string(),
        ),
        (
            "tag_prefix",
            flags
                .get("tag-prefix")
                .cloned()
                .unwrap_or_else(|| "v".to_string()),
        ),
    ]);

    Some(expand(
        &prebuild_url_template(manifest, ambient, name, flags)?,
        &vars,
    ))
}

/// `util.js` `urlTemplate`, in its own precedence order.
fn prebuild_url_template(
    manifest: &Value,
    ambient: &BTreeMap<String, String>,
    name: &str,
    flags: &BTreeMap<String, String>,
) -> Option<String> {
    const DEFAULT_ASSET: &str = "{name}-v{version}-{runtime}-v{abi}-{platform}{libc}-{arch}.tar.gz";

    // `urlTemplate` takes `opts.download` as the template only when it is a STRING
    // (`util.js:38`); a bare `-d` is boolean true and falls through to the host rules.
    if let Some(explicit) = flags
        .get("download")
        .filter(|v| *v != "true" && *v != "false")
        .or_else(|| ambient.get("npm_config_download"))
    {
        return Some(explicit.clone());
    }
    let prefix = prebuild_env_prefix(name);
    let mirror = ambient
        .get(&format!("{prefix}_binary_host"))
        .or_else(|| ambient.get(&format!("{prefix}_binary_host_mirror")));
    if let Some(mirror) = mirror {
        return Some(format!(
            "{mirror}/{{tag_prefix}}{{version}}/{DEFAULT_ASSET}"
        ));
    }
    let binary = &manifest["binary"];
    if let Some(host) = binary["host"].as_str() {
        let asset = binary["package_name"].as_str().unwrap_or(DEFAULT_ASSET);
        let parts = [Some(host), binary["remote_path"].as_str(), Some(asset)];
        return Some(
            parts
                .into_iter()
                .flatten()
                .map(trim_slashes)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
    Some(format!(
        "{}/releases/download/{{tag_prefix}}{{version}}/{DEFAULT_ASSET}",
        github_from_package(manifest)?
    ))
}

// ── @mapbox/node-pre-gyp ───────────────────────────────────────────────────────

/// Fetch the artifact and hand node-pre-gyp a `file://` mirror pointing at it.
///
/// Of the family's two zero-network levers this takes the MIRROR, not the pre-placed
/// `.node`. Both work, but the mirror leaves node-pre-gyp doing its own extraction and its
/// own `module_path` resolution — so nub never has to reproduce `eval_template` over the
/// binding variables correctly, only the `remote_path`/`package_name` layout it already
/// has to compute to build the URL at all. The narrower dependency is the whole reason:
/// a wrong `module_path` would silently place the binary where nothing loads it.
///
/// `lib/install.js` takes the local branch on `from.startsWith('file://')`, which reaches
/// `extract_from_local` and constructs no request. The mirror is re-joined with
/// `remote_path` and `package_name` downstream, so the scratch tree must mirror that
/// layout, not just hold the file.
fn node_pre_gyp(
    ambient: &mut BTreeMap<String, String>,
    manifest: &Value,
    node: &NodeFacts,
) -> Option<Vec<PathBuf>> {
    let binary = &manifest["binary"];
    let module_name = binary["module_name"].as_str()?;
    let vars = node_pre_gyp_vars(manifest, node, ambient)?;

    let remote_path = binary["remote_path"]
        .as_str()
        .map(|t| drop_double_slashes(&fix_slashes(&expand(t, &vars))))
        .unwrap_or_default();
    let package_name = expand(
        binary["package_name"]
            .as_str()
            .unwrap_or("{module_name}-v{version}-{node_abi}-{platform}-{arch}.tar.gz"),
        &vars,
    );
    let host = fix_slashes(&expand(binary["host"].as_str()?, &vars));

    let url = url::Url::parse(&host)
        .ok()?
        .join(&remote_path)
        .ok()?
        .join(&package_name)
        .ok()?;

    // PROVENANCE (`versioning.js:316`): `opts.module_name.replace('-', '_')` — a STRING
    // pattern, so JS replaces only the FIRST dash. A module named `a-b-c` yields
    // `a_b-c`, and a spelling that "helpfully" replaced all of them would set a variable
    // node-pre-gyp never reads. Reproduce the bug or the mirror is ignored.
    //
    // The key is built from a manifest field, and nub-sandbox hard-REJECTS a constructed
    // env key containing `=` or NUL — which the build jail turns into a fail-closed error,
    // i.e. a hostile `module_name` could break a spawn that would otherwise have worked.
    // Refusing anything but a plain identifier keeps this module's infallibility contract.
    if module_name.is_empty()
        || !module_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return None;
    }

    // The mirror tree lives under nub's PM cache, which the jail already read-grants
    // (`NUB_PM_CACHE_PATTERN`) — so the placed artifact needs no new fs rule, and it is
    // shared across packages and installs instead of being re-fetched per package dir.
    // `extra_reads` is still returned rather than relied on implicitly: build_jail resolves
    // the cache home with a bare `var_os`, so an empty `XDG_CACHE_HOME` (which aube treats
    // as unset) would leave that pattern anchored somewhere this path is not under.
    let root = cache_root()?.join("prefetch").join(digest(url.as_str()));
    // Segments come from RAW manifest strings, while the URL above went through
    // `Url::join`, which normalizes `..` and drops a `#fragment`. The two therefore
    // DISAGREE by construction: `package_name = "a#/../../../../.zshenv"` keeps the URL on
    // an allowlisted host and pointed at a real asset while the naive `Path::join` walks
    // out of the cache into $HOME. `contained_dest` is what re-couples them.
    // A LEADING `./` is stripped for the PATH side only. `remote_path: "./v{version}/"` is a
    // common node-pre-gyp spelling and the `.` is meaningful to `Url::join` above, but
    // `contained_dest` reads it as a traversal primitive and declines — silently, so the one
    // package in the corpus carrying a full structured `binary.*` declaration never got a
    // prefetch attempt. Only the leading `./` goes: an INTERIOR `.` or any `..` still refuses,
    // because those are the shapes that can walk out of the cache root.
    let path_remote = remote_path.strip_prefix("./").unwrap_or(&remote_path);
    let dest = contained_dest(&root, [path_remote, package_name.as_str()])?;

    let var = format!(
        "npm_config_{}_binary_host_mirror",
        module_name.replacen('-', "_", 1)
    );
    // `versioning.js:317` gives this env var precedence OVER `binary.host`, so a mirror the
    // user configured deliberately (a corporate artifact host in `.npmrc`) outranks the
    // manifest. Set-if-absent keeps that true — the same `.entry().or_insert()` discipline
    // build_jail uses for `npm_config_nodedir`.
    if ambient.contains_key(&var) {
        return None;
    }
    let mirror = url::Url::from_directory_path(&root).ok()?.to_string();
    // `install.js:283` does `from.slice('file://'.length)` — a RAW slice, no percent-decode
    // — and then `fs.existsSync`. So any escape `Url` introduces (a space in the cache
    // path, routine under `C:\Users\First Last\`) becomes a literal `%20` on the filesystem
    // and the file is never found. Setting the mirror FORCES the local branch, so that
    // would convert a working download into a hard failure — declining first is what keeps
    // this fail-soft.
    if mirror.contains('%') {
        tracing::debug!(mirror, "prefetch: mirror path needs escaping; declining");
        return None;
    }
    // Everything that can decline is settled — only now spend the network.
    place(url.as_str(), &root, &dest)?;
    ambient.insert(var, mirror);
    Some(vec![root])
}

/// The `eval_template` variable set from `versioning.js` `evaluate`, restricted to the
/// default `node` runtime for the same table-free reason as [`prebuild_install_url`].
fn node_pre_gyp_vars(
    manifest: &Value,
    node: &NodeFacts,
    ambient: &BTreeMap<String, String>,
) -> Option<BTreeMap<&'static str, String>> {
    let binary = &manifest["binary"];
    let version = manifest["version"].as_str()?;
    // `get_runtime_abi` throws for an unknown runtime and needs a crosswalk for an
    // explicit target; both are out of scope, so decline rather than guess.
    if ambient.contains_key("npm_config_target") || ambient.contains_key("npm_config_runtime") {
        return None;
    }
    // Build metadata starts at the FIRST `+`; the prerelease is the first `-` in what
    // remains. Splitting on `['-','+']` at once mis-assigns `1.2.3-rc.1+b`.
    let (core_with_prerelease, build) = version.split_once('+').unwrap_or((version, ""));
    let (core, prerelease) = core_with_prerelease
        .split_once('-')
        .unwrap_or((core_with_prerelease, ""));
    let node_abi = format!("node-v{}", node.modules);
    let napi_build_version = best_napi_version(manifest, node.napi)
        .map(|v| v.to_string())
        .unwrap_or_default();
    let node_napi_label = if napi_build_version.is_empty() {
        node_abi.clone()
    } else {
        format!("napi-v{napi_build_version}")
    };
    Some(BTreeMap::from([
        (
            "name",
            manifest["name"].as_str().unwrap_or_default().to_string(),
        ),
        ("configuration", "Release".to_string()),
        (
            "module_name",
            binary["module_name"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
        // `versioning.js:283-289` runs these through `semver.parse`, NOT string splits —
        // which is why this family diverges from prebuild-install's naive `split` above.
        // `SemVer#version` is `major.minor.patch` plus the prerelease and WITHOUT build
        // metadata, so `1.2.3-rc.1` keeps its `-rc.1` and `1.2.3+b` sheds `+b`; and
        // major/minor/patch come off the parsed core, so `patch` is `3`, never `3-rc`.
        ("version", core_with_prerelease.to_string()),
        ("prerelease", prerelease.to_string()),
        ("build", build.to_string()),
        ("major", core.split('.').next().unwrap_or("").to_string()),
        ("minor", nth_dot(core, 1)),
        ("patch", nth_dot(core, 2)),
        ("runtime", "node".to_string()),
        ("node_abi", node_abi.clone()),
        (
            "node_abi_napi",
            if node.napi > 0 {
                "napi".to_string()
            } else {
                node_abi
            },
        ),
        ("napi_version", node.napi.to_string()),
        ("napi_build_version", napi_build_version),
        ("node_napi_label", node_napi_label),
        ("target", String::new()),
        ("platform", node.platform.clone()),
        ("target_platform", node.platform.clone()),
        ("arch", node.arch.clone()),
        ("target_arch", node.arch.clone()),
        ("libc", node_pre_gyp_libc(&node.platform, ambient)),
        (
            "module_main",
            manifest["main"].as_str().unwrap_or_default().to_string(),
        ),
        ("toolset", String::new()),
        (
            "bucket",
            binary["bucket"].as_str().unwrap_or_default().to_string(),
        ),
        (
            "region",
            binary["region"].as_str().unwrap_or_default().to_string(),
        ),
    ]))
}

// ── fetch + place ──────────────────────────────────────────────────────────────

/// Resolve `segments` under `root`, admitting ONLY plain path components.
///
/// THE WHOLE SECURITY BOUNDARY FOR PATH CONSTRUCTION. Every segment here originates in a
/// dependency-authored manifest, and the URL those same strings build goes through
/// `Url::join`, which normalizes `..` and discards a `#fragment` — so the URL and the
/// naive `Path::join` of the same strings do NOT describe the same location. A
/// `package_name` of `a#/../../../../../../.zshenv` yields a perfectly ordinary asset URL
/// on an allowlisted host while `Path::join` walks out of the cache and into `$HOME`; nub
/// runs this UNCONFINED, before the jail exists, so that is an arbitrary file create with
/// the user's full authority. Splitting on BOTH separators is deliberate: `\` is not a
/// separator in a URL but is one on Windows, so a segment must not be able to smuggle a
/// component past a POSIX-only split.
fn contained_dest<'a>(root: &Path, segments: impl IntoIterator<Item = &'a str>) -> Option<PathBuf> {
    let mut dest = root.to_path_buf();
    let mut any = false;
    for segment in segments {
        // An ABSOLUTE segment is a refusal rather than a strip. Silently dropping the
        // leading separator would keep the write contained but make the mirror layout
        // disagree with what `url.resolve` computes downstream (an absolute path resets to
        // the URL root), so the artifact would never be found — a confusing miss where a
        // clean decline is honest. Trailing separators are still fine: `fix_slashes` adds
        // one to every `remote_path` by design.
        if segment.starts_with('/') || segment.starts_with('\\') {
            return None;
        }
        for part in segment.split(['/', '\\']).filter(|p| !p.is_empty()) {
            // `.` and `..` are the traversal primitives; a part that `Path` reads as
            // anything but a single normal component (an absolute root, a Windows drive
            // prefix) is equally disqualifying.
            if part == "." || part == ".." {
                return None;
            }
            let mut components = Path::new(part).components();
            match (components.next(), components.next()) {
                (Some(std::path::Component::Normal(c)), None) => dest.push(c),
                _ => return None,
            }
            any = true;
        }
    }
    any.then_some(dest)
}

/// Create every directory from `root` down to `dir`, refusing to follow a SYMLINK.
///
/// `create_dir_all` follows symlinks, and the script's own package dir is WRITABLE by the
/// confined script — while a package's hooks run sequentially, so its `preinstall` can
/// replace `prebuilds/` with a link to anywhere the user can write and have the
/// unconfined prefetch for `install` write through it. `HOME` is on the jail's env
/// allowlist, so the target is freely computable. Checking each existing level with
/// `symlink_metadata` (which does NOT follow) closes that, and is a distinct root cause
/// from the lexical traversal `contained_dest` handles — neither check subsumes the other.
fn create_dir_all_nofollow(root: &Path, dir: &Path) -> Option<()> {
    let rest = dir.strip_prefix(root).ok()?;
    let mut current = root.to_path_buf();
    std::fs::create_dir_all(&current).ok()?;
    for component in rest.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(md) if md.is_dir() => {}
            // A symlink (even one resolving inside the root) or a plain file where a
            // directory must go: refuse rather than write through it.
            Ok(_) => return None,
            Err(_) => std::fs::create_dir(&current).ok()?,
        }
    }
    Some(())
}

/// Fetch `url` into nub's prefetch cache (once per URL, machine-wide) and copy it to
/// `dest`, which must lie under `root`. `None` on any refusal or failure — the caller then
/// changes nothing.
///
/// An existing `dest` is left ALONE. That is not just idempotence: `prebuilds/` is a
/// documented user-facing drop point ("build it yourself and put it here"), so a file
/// already present is a deliberate local override and nub must not clobber it.
fn place(url: &str, root: &Path, dest: &Path) -> Option<()> {
    if dest.exists() {
        return Some(());
    }
    if !host_allowed(url) {
        tracing::debug!(url, "prefetch: host not on the prefetch allowlist");
        return None;
    }
    let parent = dest.parent()?;
    create_dir_all_nofollow(root, parent)?;
    // Belt-and-braces after the directories exist: resolve both ends and re-assert
    // containment, so a link swapped in concurrently with the walk above still cannot
    // redirect the write.
    if !parent
        .canonicalize()
        .ok()?
        .starts_with(root.canonicalize().ok()?)
    {
        tracing::debug!(dest = %dest.display(), "prefetch: destination escaped its root");
        return None;
    }

    let cached = cache_root()?.join("prefetch-blobs").join(digest(url));
    // Source the bytes: the permanent blob when it exists, else a fresh transfer. A
    // transfer whose completeness could NOT be established is used for this run and never
    // promoted — the cache hit test is existence-only, so a short body admitted here would
    // be served to every later install until someone wiped the cache by hand. Sticky
    // failure is exactly what the module's fail-soft contract forbids.
    let source = if cached.exists() {
        cached.clone()
    } else {
        std::fs::create_dir_all(cached.parent()?).ok()?;
        // A UNIQUE temp, not a fixed `.part`: lifecycle jobs run `child_concurrency`-wide
        // in parallel, so a shared name lets two racers truncate the same inode and both
        // rename — caching a torn blob permanently, since the hit test is existence only.
        let tmp = cached.with_extension(format!(
            "{}.{}.part",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        match fetch(url, &tmp) {
            None => {
                let _ = std::fs::remove_file(&tmp);
                return None;
            }
            Some(BodyLength::Unverifiable) => tmp,
            Some(BodyLength::Verified) => {
                std::fs::rename(&tmp, &cached).ok()?;
                cached.clone()
            }
        }
    };
    let placed = stage_and_rename(&source, dest);
    if source != cached {
        let _ = std::fs::remove_file(&source);
    }
    placed?;
    tracing::debug!(url, dest = %dest.display(), "prefetch: placed");
    Some(())
}

/// Copy the cached blob to `dest` through a staged temp.
///
/// Copy, never hardlink: prebuild-install probes the pickup path for W_OK and the installer
/// owns the file afterwards, so it must not share an inode with the cache. Via a temp +
/// rename so an interrupted copy cannot leave a truncated file that `place`'s `dest.exists()`
/// check would then honour as a user override forever.
///
/// The staged file is opened with `create_new` (O_EXCL), NOT `fs::copy` — copy opens its
/// destination O_CREAT|O_TRUNC, which FOLLOWS a symlink. The staged name is derivable by the
/// package itself (its own artifact name plus nub's pid, readable as `$PPID`) and lands in a
/// directory the package's confined `preinstall` may write, so a link planted there would
/// redirect this UNCONFINED write anywhere the user can reach. Neither neighbouring guard
/// covers it: `create_dir_all_nofollow` checks the directory chain, and `dest.exists()` is a
/// different path. O_EXCL refuses a live OR dangling link, and incidentally makes the
/// pid-only name safe against a sibling lifecycle thread racing the same artifact.
fn stage_and_rename(cached: &Path, dest: &Path) -> Option<()> {
    let staged = dest.with_extension(format!("{}.nub-part", std::process::id()));
    let copied = std::fs::File::create_new(&staged).and_then(|mut out| {
        std::io::copy(&mut std::fs::File::open(cached)?, &mut out)?;
        Ok(())
    });
    if copied.is_err() || std::fs::rename(&staged, dest).is_err() {
        // Only unlinks a path `create_new` proved was not a pre-existing link.
        if copied.is_ok() {
            let _ = std::fs::remove_file(&staged);
        }
        return None;
    }
    Some(())
}

/// Stream `url` to `dest` under prefetch's OWN client — deliberately not nub-core's
/// provisioning downloader.
///
/// Two properties that downloader cannot give us. (1) The redirect policy re-applies the
/// host allowlist to EVERY hop: nub-core's follows up to ten and re-checks nothing, and
/// `github.com/<o>/<r>/raw/…` really does 302 to `raw.githubusercontent.com`, so without
/// this the allowlist bounds only the first request and the SSRF containment claim is
/// hollow. (2) The budget is short and single-attempt: nub-core's 600s timeout × 3
/// attempts is a 30-minute worst case, and this call sits on the install's critical path
/// holding a concurrency permit for a purely SPECULATIVE optimization. A prefetch that is
/// slow is worse than no prefetch — failing fast just falls back to the script's own path.
///
/// Returns HOW the body's completeness was established, which decides whether the caller
/// may keep the blob (see [`BodyLength`]).
fn fetch(url: &str, dest: &Path) -> Option<BodyLength> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("nub/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            if host_allowed(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .ok()?;
    let mut resp = client
        .get(url)
        // The body must BE the published artifact: a transparently re-encoded response
        // would be a different byte stream than the installer expects to untar.
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(
            url,
            status = resp.status().as_u16(),
            "prefetch: fetch failed"
        );
        return None;
    }
    // A redirect the policy STOPPED surfaces as a 3xx response here rather than an error,
    // so the success check above is what refuses an off-allowlist hop's body.
    //
    // The write is CAPPED, and the cap is what keeps the fail-soft contract honest: the
    // body's length is chosen by whoever the manifest points at, and an uncapped
    // `io::copy` bounded only by the 120s timeout will fill the user's disk — turning a
    // speculative optimization into a new way for an install to fail. The largest real
    // prebuilt addons are tens of MB.
    const MAX_ARTIFACT: u64 = 512 * 1024 * 1024;
    let declared = resp.content_length();
    if declared.is_some_and(|n| n > MAX_ARTIFACT) {
        tracing::debug!(url, declared, "prefetch: artifact over the size cap");
        return None;
    }
    // O_EXCL for the same reason as the staged copy in `place`.
    use std::io::Read;
    let mut file = std::fs::File::create_new(dest).ok()?;
    let written = std::io::copy(&mut (&mut resp).take(MAX_ARTIFACT + 1), &mut file).ok()?;
    // A body that must never reach the cache: the hit test downstream is existence only,
    // so either a short read or an over-cap stream would poison the blob for every later
    // install. An absent `Content-Length` (a chunked response) gets no length check, so
    // the cap is the only bound there — hence the `+1` probe rather than `== MAX`.
    if written > MAX_ARTIFACT {
        tracing::debug!(url, written, "prefetch: body exceeded the size cap");
        return None;
    }
    // TRUNCATION, and what can actually be proven about it. A declared length is checked
    // here. Chunked framing needs no check: it is self-terminating, and a drop before the
    // terminating chunk surfaces as a READ ERROR the `io::copy` above already refused —
    // verified by `a_body_truncated_under_chunked_framing_is_refused`. What remains is a
    // close-delimited body (neither header, the body ends at EOF), where a truncated
    // transfer is byte-indistinguishable from a complete one. That case is reported
    // UNVERIFIABLE so the caller declines to cache it, rather than asserting an integrity
    // guarantee this function cannot make.
    match declared {
        Some(n) if n != written => None,
        Some(_) => Some(BodyLength::Verified),
        None => Some(BodyLength::Unverifiable),
    }
}

/// Whether a transfer's completeness could be ESTABLISHED, which is what decides if the
/// blob may be kept. Not a quality signal about the bytes — only about the framing.
enum BodyLength {
    /// A declared `Content-Length` matched what was written.
    Verified,
    /// No length to check. Safe to use now, never safe to cache — see [`place`].
    Unverifiable,
}

fn host_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some_and(|h| {
                // Through the accessor, not the `const`: prefetch must admit exactly what the
                // jail's egress allowlist admits, so a dev catalog override has to move both
                // together or a promoted host would be fetchable by one and not the other.
                nub_sandbox::download_hosts().contains(&h) || extra_prefetch_hosts().contains(&h)
            })
    })
}

pub(super) fn cache_root() -> Option<PathBuf> {
    aube_store::dirs::cache_dir()
}

fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

// ── small template/string helpers, each mirroring a named upstream function ─────

/// `{key}` substitution, matching both families' expanders (`expand-template` for
/// prebuild-install, `eval_template` for node-pre-gyp) on the only form either produces.
fn expand(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// A manifest field JS would interpolate directly, whether it was authored as a string or
/// a number. `JSON.stringify`-free by design: an object or array has no sensible template
/// spelling, so those decline rather than emitting `[object Object]`.
fn json_scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn strip_scope(name: &str) -> &str {
    name.strip_prefix('@')
        .and_then(|rest| rest.split_once('/'))
        .map_or(name, |(_, tail)| tail)
}

fn nth_dot(version: &str, n: usize) -> String {
    version.split('.').nth(n).unwrap_or("").to_string()
}

/// `version.split(sep)[1]` — JS splits on EVERY separator and takes the second field, so
/// `1.0.0-alpha-1` yields `alpha`, not `alpha-1`. `String(undefined)` is the literal
/// `"undefined"`, which is what a template naming `{prerelease}` on a release version
/// really resolves to.
fn after_or_undefined(version: &str, sep: char) -> String {
    version
        .split(sep)
        .nth(1)
        .map_or_else(|| "undefined".to_string(), str::to_string)
}

/// `util.js` `trimSlashes`: strip a leading `./` or `/`, and one trailing `/`.
fn trim_slashes(s: &str) -> &str {
    s.strip_prefix("./")
        .unwrap_or_else(|| s.strip_prefix('/').unwrap_or(s))
        .strip_suffix('/')
        .unwrap_or_else(|| {
            s.strip_prefix("./")
                .unwrap_or_else(|| s.strip_prefix('/').unwrap_or(s))
        })
}

fn fix_slashes(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}

fn drop_double_slashes(s: &str) -> String {
    s.replace("//", "/")
}

/// `path.basename(url)` — Node splits on `/` only, so a query string rides along, exactly
/// as it does in the pickup path prebuild-install computes.
/// `path.basename(url)`. Empty for a URL ending in `/` — the caller must decline rather
/// than let `contained_dest` fall back to the directory itself and create a FILE named
/// `prebuilds` where the pickup directory belongs.
fn url_basename(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// `github-from-package`: the first `github.com[:/]<owner>/<repo>` in the JSON text of
/// `repository`, else in the JSON text of the whole manifest, with a trailing `.git`
/// dropped. Reproduced rather than approximated because the fallback to the WHOLE manifest
/// is load-bearing — many packages carry the URL only in `homepage` or `bugs`.
fn github_from_package(manifest: &Value) -> Option<String> {
    github_match(&manifest["repository"].to_string())
        .or_else(|| github_match(&manifest.to_string()))
}

/// The upstream regex is `/\bgithub.com[:\/]([^\/"]+)\/([^\/"]+)/`, and `RegExp#exec`
/// scans for the first substring that matches the WHOLE pattern — not the first occurrence
/// of the literal. Anchoring on `find("github.com")` and giving up when that one occurrence
/// fails to be followed by `<owner>/<repo>` therefore declines on manifests upstream
/// resolves: a bare `"homepage": "https://github.com"` earlier in the JSON, or the
/// `"url": "https://github.com/"` shape, shadows the real `repository` match that follows.
fn github_match(text: &str) -> Option<String> {
    text.match_indices("github.com")
        .find_map(|(idx, _)| github_owner_repo(&text[idx + "github.com".len()..]))
}

/// The capture half: `[:/]` then two `[^/"]+` segments. Both the `git@host:owner` and
/// `https://host/owner` spellings land here.
fn github_owner_repo(rest: &str) -> Option<String> {
    let tail = rest.strip_prefix(':').or_else(|| rest.strip_prefix('/'))?;
    let mut segments = tail.split(['/', '"']);
    let owner = segments.next().filter(|s| !s.is_empty())?;
    let repo = segments.next().filter(|s| !s.is_empty())?;
    Some(format!(
        "https://github.com/{owner}/{}",
        repo.strip_suffix(".git").unwrap_or(repo)
    ))
}

/// `getBestNapiBuildVersion` (prebuild-install `util.js`) and
/// `get_best_napi_build_version` (node-pre-gyp `util/napi.js`) are the same rule: the
/// HIGHEST level in `binary.napi_versions` that the running interpreter can actually load.
/// `None` when the package declares none, or declares only levels above this Node — which
/// is the case where prefetching a guessed level would fetch an artifact that cannot load.
fn best_napi_version(manifest: &Value, interpreter_napi: u32) -> Option<u32> {
    manifest["binary"]["napi_versions"]
        .as_array()?
        .iter()
        .filter_map(|v| u32::try_from(v.as_u64()?).ok())
        .filter(|v| *v <= interpreter_napi)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The joined script line for a `sh -c <script>` spawn — what the seam's
    /// `command_line()` hands the family/flag heuristics.
    fn args(script: &str) -> String {
        format!("-c {script}")
    }

    fn node26() -> NodeFacts {
        NodeFacts {
            version: "26.0.0".into(),
            modules: "140".into(),
            napi: 10,
            platform: "darwin".into(),
            arch: "arm64".into(),
        }
    }

    #[test]
    fn family_follows_the_first_command_in_an_or_chain() {
        assert_eq!(
            detect_family(&args("prebuild-install || node-gyp rebuild")),
            Some(Family::PrebuildInstall)
        );
        assert_eq!(
            detect_family(&args("node-pre-gyp install --fallback-to-build")),
            Some(Family::NodePreGyp)
        );
        // `rebuild` has no download step to short-circuit.
        assert_eq!(detect_family(&args("node-pre-gyp rebuild")), None);
        assert_eq!(detect_family(&args("node-gyp rebuild")), None);
    }

    #[test]
    fn prebuild_url_strips_the_scope_from_the_asset_name_only() {
        let manifest = serde_json::json!({
            "name": "@serialport/bindings-cpp",
            "version": "12.0.1",
            "repository": "https://github.com/serialport/bindings-cpp.git",
        });
        let url =
            prebuild_install_url(&manifest, &BTreeMap::new(), &node26(), &BTreeMap::new()).unwrap();
        assert_eq!(
            url,
            "https://github.com/serialport/bindings-cpp/releases/download/v12.0.1/\
             bindings-cpp-v12.0.1-node-v140-darwin-arm64.tar.gz"
        );
        // The env-var spelling keeps the scope, collapsed and de-underscored.
        assert_eq!(
            prebuild_env_prefix("@serialport/bindings-cpp"),
            "npm_config_serialport_bindings_cpp"
        );
    }

    /// The two families read `libc` by DIFFERENT rules, and collapsing them produced a
    /// node-pre-gyp URL of `…-linux-unknown-…` on every glibc machine — i.e. the family
    /// silently never prefetched on its primary platform.
    #[test]
    fn the_two_families_disagree_about_the_libc_slot_on_glibc() {
        let glibc = BTreeMap::from([
            ("LIBC".to_string(), "glibc".to_string()),
            ("npm_config_target_libc".to_string(), "glibc".to_string()),
        ]);
        // prebuild-install force-blanks glibc (rc.js:56) …
        assert_eq!(prebuild_install_libc("linux", &glibc), "");
        // … while node-pre-gyp emits it verbatim (versioning.js:305).
        assert_eq!(node_pre_gyp_libc("linux", &glibc), "glibc");
        // Off Linux the slot is blank for one and the literal `unknown` for the other.
        assert_eq!(prebuild_install_libc("darwin", &BTreeMap::new()), "");
        assert_eq!(node_pre_gyp_libc("darwin", &BTreeMap::new()), "unknown");
    }

    #[test]
    fn libc_slot_is_blank_off_linux_and_concatenated_on_musl() {
        assert_eq!(prebuild_install_libc("darwin", &BTreeMap::new()), "");
        let glibc = BTreeMap::from([("LIBC".to_string(), "glibc".to_string())]);
        assert_eq!(prebuild_install_libc("linux", &glibc), "");
        let musl = BTreeMap::from([("LIBC".to_string(), "musl".to_string())]);
        assert_eq!(prebuild_install_libc("linux", &musl), "musl");

        // The template concatenates with no separator: `linux` + `musl`.
        let manifest = serde_json::json!({
            "name": "sharp", "version": "0.33.0",
            "repository": "https://github.com/lovell/sharp",
        });
        let mut facts = node26();
        facts.platform = "linux".into();
        facts.arch = "x64".into();
        let url = prebuild_install_url(&manifest, &musl, &facts, &BTreeMap::new()).unwrap();
        assert!(
            url.ends_with("sharp-v0.33.0-node-v140-linuxmusl-x64.tar.gz"),
            "{url}"
        );
    }

    #[test]
    fn napi_runtime_negotiates_the_abi_against_the_interpreter() {
        let manifest = serde_json::json!({
            "name": "sodium-native", "version": "4.0.0",
            "config": { "runtime": "napi" },
            "binary": { "napi_versions": [6, 8, 30] },
            "repository": "https://github.com/holepunchto/sodium-native",
        });
        // 30 is above the interpreter's level, so 8 wins.
        let url =
            prebuild_install_url(&manifest, &BTreeMap::new(), &node26(), &BTreeMap::new()).unwrap();
        assert!(
            url.ends_with("sodium-native-v4.0.0-napi-v8-darwin-arm64.tar.gz"),
            "{url}"
        );
    }

    #[test]
    fn a_non_node_runtime_declines_rather_than_guessing_an_abi() {
        let manifest = serde_json::json!({
            "name": "x", "version": "1.0.0",
            "config": { "runtime": "electron" },
            "repository": "https://github.com/o/x",
        });
        assert!(
            prebuild_install_url(&manifest, &BTreeMap::new(), &node26(), &BTreeMap::new())
                .is_none()
        );
    }

    #[test]
    fn binary_host_wins_over_the_repository_url() {
        let manifest = serde_json::json!({
            "name": "leveldown", "version": "6.1.1",
            "binary": { "host": "https://github.com/Level/leveldown/releases/download/",
                        "remote_path": "v{version}" },
            "repository": "https://github.com/Level/leveldown",
        });
        let url =
            prebuild_install_url(&manifest, &BTreeMap::new(), &node26(), &BTreeMap::new()).unwrap();
        assert_eq!(
            url,
            "https://github.com/Level/leveldown/releases/download/v6.1.1/\
             leveldown-v6.1.1-node-v140-darwin-arm64.tar.gz"
        );
    }

    /// A leading `./` is the common node-pre-gyp `remote_path` spelling and must reach a
    /// dest; every other `.`/`..` shape must still refuse. The strip lives at the
    /// `contained_dest` CALL SITE (the URL side keeps the `.`, which `Url::join` needs), so
    /// this pins the property rather than the wiring — end-to-end proof is a `node-libcurl`
    /// re-measure, which is why that package is the one to check after any change here.
    #[test]
    fn a_leading_dot_slash_reaches_a_dest_while_real_traversal_still_refuses() {
        let root = Path::new("/cache/prefetch/abc");
        let strip = |s: &'static str| s.strip_prefix("./").unwrap_or(s);
        assert_eq!(
            contained_dest(root, [strip("./v1.0.0/"), "pkg.tar.gz"]),
            Some(PathBuf::from("/cache/prefetch/abc/v1.0.0/pkg.tar.gz")),
            "the one structured-declaration form in the corpus must not decline"
        );
        // The strip is deliberately ONLY the leading pair: these are what walk out.
        for still_hostile in ["./../v1.0.0", "v1.0.0/./..", "./.."] {
            assert_eq!(
                contained_dest(root, [strip(still_hostile), "pkg.tar.gz"]),
                None,
                "stripping the leading `./` must not admit {still_hostile:?}"
            );
        }
    }

    #[test]
    fn node_pre_gyp_mirror_var_replaces_only_the_first_dash() {
        // versioning.js:316 uses a string pattern, so `a-b-c` → `a_b-c`.
        assert_eq!("sqlite3".replacen('-', "_", 1), "sqlite3");
        assert_eq!("node-expat".replacen('-', "_", 1), "node_expat");
        assert_eq!("a-b-c".replacen('-', "_", 1), "a_b-c");
    }

    #[test]
    fn node_pre_gyp_vars_build_the_default_asset_name() {
        let manifest = serde_json::json!({
            "name": "better-sqlite3", "version": "11.5.0",
            "binary": { "module_name": "better_sqlite3",
                        "host": "https://github.com/WiseLibs/better-sqlite3/releases/download/",
                        "remote_path": "v{version}" },
        });
        let vars = node_pre_gyp_vars(&manifest, &node26(), &BTreeMap::new()).unwrap();
        let asset = expand(
            "{module_name}-v{version}-{node_abi}-{platform}-{arch}.tar.gz",
            &vars,
        );
        assert_eq!(
            asset,
            "better_sqlite3-v11.5.0-node-v140-darwin-arm64.tar.gz"
        );
        assert_eq!(expand("v{version}", &vars), "v11.5.0");
    }

    #[test]
    fn github_url_falls_back_from_repository_to_the_whole_manifest() {
        let via_repo = serde_json::json!({
            "repository": { "type": "git", "url": "git+ssh://git@github.com:owner/repo.git" }
        });
        assert_eq!(
            github_from_package(&via_repo).unwrap(),
            "https://github.com/owner/repo"
        );
        let via_homepage = serde_json::json!({ "homepage": "https://github.com/o/r#readme" });
        assert_eq!(
            github_from_package(&via_homepage).unwrap(),
            "https://github.com/o/r#readme"
        );
        assert!(github_from_package(&serde_json::json!({ "name": "x" })).is_none());
    }

    #[test]
    fn only_https_hosts_on_the_allowlist_are_fetched() {
        // The default allowlist IS `$downloads` — nothing confined code cannot already reach.
        assert!(host_allowed("https://nodejs.org/download/release/x.tar.gz"));
        // The SSRF cases the allowlist exists to refuse.
        assert!(!host_allowed("http://169.254.169.254/latest/meta-data/"));
        assert!(!host_allowed("https://internal.corp/x.tar.gz"));
        assert!(!host_allowed("file:///etc/passwd"));
        // A lookalike must not match by suffix.
        assert!(!host_allowed("https://evil-nodejs.org/x"));
        assert!(!host_allowed("https://nodejs.org.evil.net/x"));
        // Plaintext is refused even for an allowlisted host.
        assert!(!host_allowed("http://nodejs.org/x.tar.gz"));
    }

    /// The unratified widening is INERT unless compiled in — the property that keeps this
    /// branch from quietly adding a network-trust surface nobody approved.
    #[test]
    fn github_is_fetchable_only_under_the_opt_in_feature() {
        let asset = "https://github.com/o/r/releases/download/v1/a.tar.gz";
        if cfg!(feature = "prefetch-github-hosts") {
            assert!(host_allowed(asset));
            assert!(host_allowed("https://objects.githubusercontent.com/x"));
        } else {
            assert!(!host_allowed(asset));
            assert!(!host_allowed("https://objects.githubusercontent.com/x"));
        }
    }

    /// `"install": "prebuild-install -r napi"` is common, and reading only `config.runtime`
    /// derived the wrong ABI slot — so the placed filename never matched the probed one.
    #[test]
    fn a_command_line_flag_outranks_config_and_env() {
        let manifest = serde_json::json!({
            "name": "pkg",
            "version": "1.0.0",
            "config": { "runtime": "node" },
            "binary": { "napi_versions": [3] },
            "repository": "https://github.com/o/r.git",
        });
        let env = BTreeMap::from([("npm_config_runtime".to_string(), "node".to_string())]);
        let flags = prebuild_flags(&args("prebuild-install -r napi"));
        assert_eq!(flags.get("runtime").map(String::as_str), Some("napi"));

        // argv wins over BOTH `config.runtime` and `npm_config_runtime` (rc deepExtends
        // argv last), so the asset is the napi-v3 spelling, not node-v140.
        let url = prebuild_install_url(&manifest, &env, &node26(), &flags).unwrap();
        assert!(
            url.ends_with("pkg-v1.0.0-napi-v3-darwin-arm64.tar.gz"),
            "{url}"
        );
        // Without the flag the same manifest resolves to the node-ABI asset.
        let unflagged = prebuild_install_url(&manifest, &env, &node26(), &BTreeMap::new()).unwrap();
        assert!(
            unflagged.ends_with("pkg-v1.0.0-node-v140-darwin-arm64.tar.gz"),
            "{unflagged}"
        );
    }

    #[test]
    fn flags_are_scoped_to_the_prebuild_install_command_alone() {
        // The fallback's flags must not be attributed to prebuild-install.
        let chained = prebuild_flags(&args(
            "prebuild-install -r napi || node-gyp rebuild --arch=ia32",
        ));
        assert_eq!(chained.get("runtime").map(String::as_str), Some("napi"));
        assert_eq!(chained.get("arch"), None);
        // Long, `=`, and quoted-value spellings all parse.
        let long = prebuild_flags(&args(
            "prebuild-install --runtime=electron --tag-prefix '' --download https://m/a.tar.gz",
        ));
        assert_eq!(long.get("runtime").map(String::as_str), Some("electron"));
        assert_eq!(long.get("tag-prefix").map(String::as_str), Some(""));
        assert_eq!(
            long.get("download").map(String::as_str),
            Some("https://m/a.tar.gz")
        );
        // A bare boolean flag does not swallow the following token.
        let bare = prebuild_flags(&args("prebuild-install --debug --verbose"));
        assert_eq!(bare.get("debug").map(String::as_str), Some("true"));
    }

    /// `--nolocal` and `--build-from-source` both bypass the local-pickup probe, so a
    /// placed file could never be read — prefetch must decline rather than write one.
    #[test]
    fn flags_that_defeat_local_pickup_decline_the_prefetch() {
        for script in [
            "prebuild-install --nolocal",
            "prebuild-install --build-from-source",
        ] {
            let flags = prebuild_flags(&args(script));
            assert!(
                flags.contains_key("nolocal") || flags.contains_key("buildFromSource"),
                "{script} -> {flags:?}"
            );
        }
    }

    /// node-pre-gyp runs the version through `semver.parse`, so `{version}` keeps the
    /// prerelease and sheds build metadata, and `{patch}` is the parsed core — not `3-rc`.
    #[test]
    fn node_pre_gyp_version_slots_follow_semver_not_string_splits() {
        let manifest = serde_json::json!({
            "name": "m",
            "version": "1.2.3-rc.1+build5",
            "binary": { "module_name": "m" },
        });
        let vars = node_pre_gyp_vars(&manifest, &node26(), &BTreeMap::new()).unwrap();
        assert_eq!(vars["version"], "1.2.3-rc.1");
        assert_eq!(vars["prerelease"], "rc.1");
        assert_eq!(vars["build"], "build5");
        assert_eq!(vars["major"], "1");
        assert_eq!(vars["minor"], "2");
        assert_eq!(vars["patch"], "3");
    }

    /// `RegExp#exec` scans for the first full MATCH, so an earlier bare `github.com` must
    /// not shadow the real `<owner>/<repo>` that follows.
    #[test]
    fn github_match_scans_past_a_non_matching_occurrence() {
        let manifest = serde_json::json!({
            "homepage": "https://github.com",
            "repository": { "url": "git+https://github.com/owner/repo.git" },
        });
        assert_eq!(
            github_from_package(&manifest).unwrap(),
            "https://github.com/owner/repo"
        );
    }

    /// The manifest-to-disk path is the one place a dependency gets to steer an
    /// UNCONFINED write, so the traversal forms are pinned individually. The `#fragment`
    /// case is the one that makes this a real primitive rather than a theoretical one: the
    /// fragment never goes on the wire, so the URL stays a genuine asset on an allowlisted
    /// host while the naive `Path::join` of the same string walks out to `$HOME`.
    #[test]
    fn manifest_supplied_segments_cannot_escape_their_root() {
        let root = Path::new("/cache/prefetch/abc");
        assert_eq!(
            contained_dest(root, ["v1.0.0", "pkg.tar.gz"]),
            Some(PathBuf::from("/cache/prefetch/abc/v1.0.0/pkg.tar.gz"))
        );
        for hostile in [
            "payload#/../../../../../../../.zshenv",
            "../../../../.zshenv",
            "..",
            "a/../../b",
            "/etc/cron.d/x",
            // `\` is not a URL separator but IS one on Windows, so a POSIX-only split
            // would let this smuggle a component past the check.
            "..\\..\\..\\evil",
        ] {
            assert_eq!(
                contained_dest(root, ["v1.0.0", hostile]),
                None,
                "escaped with {hostile:?}"
            );
        }
        // An empty segment set is a refusal, not a silent write to the root itself.
        assert_eq!(contained_dest(root, ["", ""]), None);
    }

    /// A package's hooks run sequentially, its own dir is jail-WRITABLE, and prefetch runs
    /// unconfined — so a confined `preinstall` that turns `prebuilds/` into a symlink
    /// would otherwise redirect the `install` prefetch's write anywhere. Lexical
    /// containment does not catch this; only refusing to follow the link does.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_pickup_directory_is_refused_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("package");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("prebuilds")).unwrap();

        let dest = contained_dest(&root, ["prebuilds", "a.tar.gz"]).unwrap();
        assert!(create_dir_all_nofollow(&root, dest.parent().unwrap()).is_none());
        // Nothing was written through the link.
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

        // The same layout without the symlink is created normally.
        std::fs::remove_file(root.join("prebuilds")).unwrap();
        assert!(create_dir_all_nofollow(&root, dest.parent().unwrap()).is_some());
        assert!(root.join("prebuilds").is_dir());
    }

    /// The directory guard above does NOT cover the staged temp file: its name is derivable
    /// by the package (its own artifact name plus nub's pid, readable as `$PPID`) and it
    /// lands in a directory the confined `preinstall` may write. `fs::copy` would have
    /// opened it O_CREAT|O_TRUNC and followed the link, writing through it as the USER.
    // Gated like the directory-guard test above: the fixture plants a symlink through
    // `std::os::unix`, which does not exist on Windows — without this the whole `nub-cli`
    // test binary fails to compile there.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_staged_temp_is_refused_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("package");
        let prebuilds = root.join("prebuilds");
        std::fs::create_dir_all(&prebuilds).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"original").unwrap();

        let cached = tmp.path().join("blob");
        std::fs::write(&cached, b"artifact").unwrap();
        let dest = prebuilds.join("a.tar.gz");
        let staged = dest.with_extension(format!("{}.nub-part", std::process::id()));
        std::os::unix::fs::symlink(&victim, &staged).unwrap();

        assert!(stage_and_rename(&cached, &dest).is_none());
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
        assert!(!dest.exists());
        // The refusal must not have unlinked the planted link either — only a staged file
        // this call actually created is ours to remove.
        assert!(std::fs::symlink_metadata(&staged).unwrap().is_symlink());

        // Without the link the same call places the bytes.
        std::fs::remove_file(&staged).unwrap();
        assert!(stage_and_rename(&cached, &dest).is_some());
        assert_eq!(std::fs::read(&dest).unwrap(), b"artifact");
    }

    /// ORACLE TEST. Every URL below was captured from the REAL installer's own request
    /// line on Node 26.5.0 (`prebuild-install --verbose` → `http request GET …`,
    /// `node-pre-gyp install` → `http GET …`), so this pins nub's derivation against what
    /// the tool actually asks for rather than against our reading of its source. Facts are
    /// that host's: `modules` 147, napi 10, darwin/arm64.
    #[test]
    fn derivation_matches_the_real_installers_requested_url() {
        let host = NodeFacts {
            version: "26.5.0".into(),
            modules: "147".into(),
            napi: 10,
            platform: "darwin".into(),
            arch: "arm64".into(),
        };

        // better-sqlite3@11.5.0 — prebuild-install, no `binary`, URL from `repository`.
        let bs = serde_json::json!({
            "name": "better-sqlite3", "version": "11.5.0",
            "repository": { "type": "git", "url": "git://github.com/WiseLibs/better-sqlite3.git" },
        });
        assert_eq!(
            prebuild_install_url(&bs, &BTreeMap::new(), &host, &BTreeMap::new()).unwrap(),
            "https://github.com/WiseLibs/better-sqlite3/releases/download/v11.5.0/\
             better-sqlite3-v11.5.0-node-v147-darwin-arm64.tar.gz"
        );

        // keytar@7.9.0 — the numeric `config.target` case: the ABI slot is the pinned
        // target (3), NOT a level negotiated from `binary.napi_versions` (which keytar
        // does not even declare). Reading `target` as a string only would silently derive
        // `napi-v10` here and never hit.
        let kt = serde_json::json!({
            "name": "keytar", "version": "7.9.0",
            "config": { "target": 3, "runtime": "napi" },
            "repository": { "type": "git", "url": "https://github.com/atom/node-keytar.git" },
        });
        assert_eq!(
            prebuild_install_url(&kt, &BTreeMap::new(), &host, &BTreeMap::new()).unwrap(),
            "https://github.com/atom/node-keytar/releases/download/v7.9.0/\
             keytar-v7.9.0-napi-v3-darwin-arm64.tar.gz"
        );

        // bcrypt@5.1.1 — node-pre-gyp: templated host/remote_path/package_name, the
        // `{libc}` slot resolving to `unknown` off Linux, and `napi_build_version` 3.
        let bc = serde_json::json!({
            "name": "bcrypt", "version": "5.1.1", "main": "./bcrypt",
            "binary": {
                "module_name": "bcrypt_lib",
                "module_path": "./lib/binding/napi-v{napi_build_version}",
                "package_name": "{module_name}-v{version}-napi-v{napi_build_version}-{platform}-{arch}-{libc}.tar.gz",
                "host": "https://github.com",
                "remote_path": "kelektiv/node.bcrypt.js/releases/download/v{version}",
                "napi_versions": [3]
            },
        });
        let vars = node_pre_gyp_vars(&bc, &host, &BTreeMap::new()).unwrap();
        let remote = drop_double_slashes(&fix_slashes(&expand(
            bc["binary"]["remote_path"].as_str().unwrap(),
            &vars,
        )));
        let asset = expand(bc["binary"]["package_name"].as_str().unwrap(), &vars);
        let url = url::Url::parse(&fix_slashes(bc["binary"]["host"].as_str().unwrap()))
            .unwrap()
            .join(&remote)
            .unwrap()
            .join(&asset)
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://github.com/kelektiv/node.bcrypt.js/releases/download/v5.1.1/\
             bcrypt_lib-v5.1.1-napi-v3-darwin-arm64-unknown.tar.gz"
        );
        // And the mirror layout must reproduce what node-pre-gyp re-joins onto `file://`.
        let root = Path::new("/cache/prefetch/abc");
        assert_eq!(
            contained_dest(root, [remote.as_str(), asset.as_str()]).unwrap(),
            root.join("kelektiv/node.bcrypt.js/releases/download/v5.1.1")
                .join(&asset)
        );
    }

    #[test]
    fn node_facts_parse_is_strict_about_shape() {
        let f = parse_node_facts("26.0.0 140 10 darwin arm64\n").unwrap();
        assert_eq!(
            (f.modules.as_str(), f.napi, f.arch.as_str()),
            ("140", 10, "arm64")
        );
        assert!(parse_node_facts("").is_none());
        assert!(parse_node_facts("26.0.0 140\n").is_none());
    }

    /// A manifest big enough to be buying compute rather than describing a package is
    /// refused before `expand`'s ~25 replace passes ever run on its strings.
    #[test]
    fn an_oversized_manifest_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");

        std::fs::write(&path, br#"{"name":"ok","version":"1.0.0"}"#).unwrap();
        assert!(read_manifest(tmp.path()).is_some());

        let bloat = format!(
            r#"{{"name":"big","version":"1.0.0","x":"{}"}}"#,
            "a".repeat(2 * 1024 * 1024)
        );
        std::fs::write(&path, bloat).unwrap();
        assert!(read_manifest(tmp.path()).is_none());
    }

    /// Serve ONE verbatim HTTP/1.1 response on an ephemeral port, then close. Raw bytes so
    /// a test can express framing a real server would not produce.
    fn serve_raw(raw: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                use std::io::{Read, Write};
                let _ = sock.read(&mut [0u8; 4096]);
                let _ = sock.write_all(raw);
            }
        });
        format!("http://127.0.0.1:{port}/a.tar.gz")
    }

    /// Truncation across all three transfer framings, each with its passing control. The
    /// property under test is what `place` keys caching on: a body whose completeness
    /// cannot be PROVEN must never be promoted to the existence-only blob cache, or one
    /// short read is served to every later install until the cache is wiped by hand.
    #[test]
    fn only_a_provably_complete_body_is_cacheable() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |n: &str| tmp.path().join(n);

        // Declared length, honoured — the one shape that can be verified.
        let ok = fetch(
            &serve_raw(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"),
            &at("len"),
        );
        assert!(matches!(ok, Some(BodyLength::Verified)));
        assert_eq!(std::fs::read(at("len")).unwrap(), b"hello");

        // Declared length, body cut short: refused outright.
        assert!(
            fetch(
                &serve_raw(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nhello"),
                &at("len-short"),
            )
            .is_none()
        );

        // Chunked, terminated: self-framing, so complete but with no length to check.
        let chunked = fetch(
            &serve_raw(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            ),
            &at("chunk"),
        );
        assert!(matches!(chunked, Some(BodyLength::Unverifiable)));
        assert_eq!(std::fs::read(at("chunk")).unwrap(), b"hello");

        // Chunked, dropped before the terminator: hyper surfaces a read error, so this is
        // refused rather than merely unverifiable.
        assert!(
            fetch(
                &serve_raw(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n"),
                &at("chunk-short"),
            )
            .is_none()
        );

        // Close-delimited: a truncated transfer is byte-identical to a complete one, which
        // is precisely why it is reported unverifiable and never cached.
        let eof = fetch(&serve_raw(b"HTTP/1.0 200 OK\r\n\r\nhello"), &at("close"));
        assert!(matches!(eof, Some(BodyLength::Unverifiable)));
    }
}
