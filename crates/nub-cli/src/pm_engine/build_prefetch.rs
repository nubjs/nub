//! Build-jail PREFETCH — land a dependency's prebuilt binary on the local path its own
//! installer checks BEFORE it opens a socket, so the confined script completes with the
//! net axis fully denied.
//!
//! WHY THIS EXISTS. `$downloads` (nub-sandbox's `DOWNLOAD_HOSTS`) is the surface a
//! dependency's ATTACKER-AUTHORED script gets to talk to, so it is kept as small as the
//! evidence allows. Prefetch is the lever that keeps it small: nub — not the script —
//! derives the artifact URL from the package's OWN manifest, fetches it out-of-jail, and
//! writes it where the installer already looks first. The script then finds a local file
//! and never reaches for the network, so the host serving that artifact never has to be
//! allowlisted at all. This is the same move `npm_config_nodedir` already makes for
//! node-gyp's headers, which is why `nodejs.org` is contacted by none of the corpus
//! despite being in the set.
//!
//! TWO ALLOWLISTS, DELIBERATELY DISTINCT — do not merge them. `$downloads` is what
//! CONFINED CODE may reach. [`PREFETCH_HOSTS`] is what NUB ITSELF will GET from on a
//! package's behalf, and it may be broader because the two grant categorically different
//! things: a `$downloads` entry hands a running attacker script a bidirectional socket,
//! whereas a prefetch entry only lets nub perform one anonymous GET whose body is written
//! to a file and never executed by nub. So `github.com` here covers the whole
//! prebuilt-binary population while `$downloads` gains nothing.
//!
//! The wildcard-free rule `$downloads` enforces does NOT carry over, and the reason is
//! worth recording: there, an exact host pins every DNS label so a confined script cannot
//! exfiltrate through the resolver. Here nub composes the URL from a manifest the attacker
//! already authored and already knows — there is no secret for a hostname to leak. What
//! the allowlist buys on THIS side is SSRF containment: without it a manifest could point
//! `binary.host` at `169.254.169.254` or an intranet name and have nub, unconfined, fetch
//! it. Entries are still added only on evidence.
//!
//! FAIL-SOFT, ALWAYS. Every failure path — unparseable manifest, unrecognized family, a
//! host off the allowlist, a 404, a dead network — returns having changed nothing, and
//! the script then runs exactly as it would have without prefetch (reaching for a socket
//! the jail denies, then falling back to a source build). Prefetch is an optimization and
//! a jail-compatibility lever; it must never become a new way for an install to fail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use super::build_jail::ProbeScope;

/// Hosts nub will GET a prebuilt artifact from on a package's behalf. See the module doc
/// for why this is separate from — and broader than — `$downloads`, and why it is not
/// held to that set's wildcard-free rule.
///
/// THE REDIRECT TARGETS ARE LOAD-BEARING ENTRIES, not conveniences. [`fetch`] re-applies
/// this list to every hop, so a `github.com` release-asset URL only resolves if the host
/// it 302s to is here too. Measured 2026-07-28 against a real asset:
/// `github.com/<o>/<r>/releases/download/…` → **`release-assets.githubusercontent.com`**
/// (a signed, expiring URL). `objects.githubusercontent.com` is the older spelling of the
/// same asset CDN, retained because directly-published asset URLs still use it.
///
/// Deliberately ABSENT: `raw.githubusercontent.com`. `github.com/<o>/<r>/raw/…` redirects
/// there (verified), which is a fine way to serve arbitrary repo content but is not how a
/// release artifact is published — admitting it would widen the fetchable surface from
/// "release assets" to "any file in any repo" for no covered package.
const PREFETCH_HOSTS: &[&str] = &[
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

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
    let Some(family) = detect_family(&spawn.args) else {
        return Vec::new();
    };
    let Some(manifest) = read_manifest(&spawn.package_dir) else {
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
fn detect_family(args: &[std::ffi::OsString]) -> Option<Family> {
    let script = args
        .iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
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

fn read_manifest(package_dir: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(package_dir.join("package.json")).ok()?;
    serde_json::from_str(&raw).ok()
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

/// Split the probe's single line. Separated from the spawn so the parse is unit-testable
/// without a Node on disk.
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
    let url = prebuild_install_url(manifest, ambient, node)?;
    // Anchored on the script's CWD, not `package_dir`: `localPrebuild` is cwd-relative
    // (`util.js` `localPrebuild` joins onto `rc.path`, which `bin.js` chdir's to). The two
    // coincide for an ordinary dependency hook but diverge for a fetched git dependency's
    // root script, where writing into the packed checkout would make its fingerprint
    // host-dependent.
    let root = spawn.cwd.clone();
    let dest = contained_dest(
        &root,
        [
            local_prebuilds_prefix(manifest, ambient).as_str(),
            url_basename(&url),
        ],
    )?;
    place(&url, &root, &dest)
}

/// The `prebuilds/` directory name, overridable per package via
/// `npm_config_<sanitized-name>_local_prebuilds` (`util.js` `localPrebuild`).
fn local_prebuilds_prefix(manifest: &Value, ambient: &BTreeMap<String, String>) -> String {
    let name = manifest["name"].as_str().unwrap_or_default();
    ambient
        .get(&format!("{}_local_prebuilds", prebuild_env_prefix(name)))
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
) -> Option<String> {
    let name = manifest["name"].as_str()?;
    let version = manifest["version"].as_str()?;
    let config = &manifest["config"];

    let runtime = config["runtime"]
        .as_str()
        .or_else(|| ambient.get("npm_config_runtime").map(String::as_str))
        .unwrap_or("node");
    // `config.target` is read with `json_scalar`, not `as_str`, because it is routinely a
    // JSON NUMBER — `keytar` ships `"config": { "target": 3, "runtime": "napi" }`. Reading
    // only the string spelling silently falls through to the running Node's version and
    // derives the wrong ABI slot for every package that pins one this way.
    let target = json_scalar(&config["target"])
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

    let platform = ambient
        .get("npm_config_platform")
        .cloned()
        .unwrap_or_else(|| node.platform.clone());
    let arch = ambient
        .get("npm_config_arch")
        .cloned()
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
        ("libc", prebuild_install_libc(&node.platform, ambient)),
        ("configuration", "Release".to_string()),
        (
            "module_name",
            manifest["binary"]["module_name"]
                .as_str()
                .unwrap_or("undefined")
                .to_string(),
        ),
        ("tag_prefix", "v".to_string()),
    ]);

    Some(expand(
        &prebuild_url_template(manifest, ambient, name)?,
        &vars,
    ))
}

/// `util.js` `urlTemplate`, in its own precedence order.
fn prebuild_url_template(
    manifest: &Value,
    ambient: &BTreeMap<String, String>,
    name: &str,
) -> Option<String> {
    const DEFAULT_ASSET: &str = "{name}-v{version}-{runtime}-v{abi}-{platform}{libc}-{arch}.tar.gz";

    if let Some(explicit) = ambient.get("npm_config_download") {
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
    let dest = contained_dest(&root, [remote_path.as_str(), package_name.as_str()])?;
    place(url.as_str(), &root, &dest)?;

    let var = format!(
        "npm_config_{}_binary_host_mirror",
        module_name.replacen('-', "_", 1)
    );
    // `versioning.js:317` gives this env var precedence OVER `binary.host`, so a mirror the
    // user configured deliberately (a corporate artifact host in `.npmrc`) outranks the
    // manifest. Set-if-absent keeps that true — the same `.entry().or_insert()` discipline
    // build_jail uses for `npm_config_nodedir`.
    let mirror = url::Url::from_directory_path(&root).ok()?;
    if ambient.contains_key(&var) {
        return None;
    }
    ambient.insert(var, mirror.to_string());
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
        ("version", version.split(['-', '+']).next()?.to_string()),
        ("prerelease", after_or_empty(version, '-')),
        ("build", after_or_empty(version, '+')),
        ("major", version.split('.').next().unwrap_or("").to_string()),
        ("minor", nth_dot(version, 1)),
        ("patch", nth_dot(version, 2)),
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
    if !cached.exists() {
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
        let fetched = fetch(url, &tmp);
        if fetched.is_none() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        std::fs::rename(&tmp, &cached).ok()?;
    }
    // Copy, never hardlink: prebuild-install probes the pickup path for W_OK and the
    // installer owns the file afterwards, so it must not share an inode with the cache.
    // Via a temp + rename so an interrupted copy cannot leave a truncated file that the
    // `dest.exists()` check above would then honour as a user override forever.
    let staged = dest.with_extension(format!("{}.nub-part", std::process::id()));
    std::fs::copy(&cached, &staged).ok()?;
    if std::fs::rename(&staged, dest).is_err() {
        let _ = std::fs::remove_file(&staged);
        return None;
    }
    tracing::debug!(url, dest = %dest.display(), "prefetch: placed");
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
fn fetch(url: &str, dest: &Path) -> Option<()> {
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
    let declared = resp.content_length();
    let mut file = std::fs::File::create(dest).ok()?;
    let written = std::io::copy(&mut resp, &mut file).ok()?;
    // A truncated body must never be cached: the hit test downstream is existence only,
    // so a short read would poison the blob for every later install.
    if declared.is_some_and(|n| n != written) {
        return None;
    }
    Some(())
}

fn host_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https" && u.host_str().is_some_and(|h| PREFETCH_HOSTS.contains(&h))
    })
}

fn cache_root() -> Option<PathBuf> {
    aube_store::dirs::cache_dir()
}

fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(s.as_bytes()))
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

/// `version.split(sep)[1]` with JS's `String(undefined)` stringification (see the
/// `prerelease` note in [`prebuild_install_url`]).
fn after_or_undefined(version: &str, sep: char) -> String {
    version
        .split_once(sep)
        .map_or_else(|| "undefined".to_string(), |(_, tail)| tail.to_string())
}

fn after_or_empty(version: &str, sep: char) -> String {
    version
        .split_once(sep)
        .map(|(_, t)| t.to_string())
        .unwrap_or_default()
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

fn github_match(text: &str) -> Option<String> {
    let idx = text.find("github.com")?;
    let rest = &text[idx + "github.com".len()..];
    // The upstream regex is `github.com[:/]([^/"]+)/([^/"]+)`, so both the `git@host:owner`
    // and `https://host/owner` spellings land on the same two segments.
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
    use std::ffi::OsString;

    fn args(script: &str) -> Vec<OsString> {
        vec![OsString::from("-c"), OsString::from(script)]
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
        let url = prebuild_install_url(&manifest, &BTreeMap::new(), &node26()).unwrap();
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
        let url = prebuild_install_url(&manifest, &musl, &facts).unwrap();
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
        let url = prebuild_install_url(&manifest, &BTreeMap::new(), &node26()).unwrap();
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
        assert!(prebuild_install_url(&manifest, &BTreeMap::new(), &node26()).is_none());
    }

    #[test]
    fn binary_host_wins_over_the_repository_url() {
        let manifest = serde_json::json!({
            "name": "leveldown", "version": "6.1.1",
            "binary": { "host": "https://github.com/Level/leveldown/releases/download/",
                        "remote_path": "v{version}" },
            "repository": "https://github.com/Level/leveldown",
        });
        let url = prebuild_install_url(&manifest, &BTreeMap::new(), &node26()).unwrap();
        assert_eq!(
            url,
            "https://github.com/Level/leveldown/releases/download/v6.1.1/\
             leveldown-v6.1.1-node-v140-darwin-arm64.tar.gz"
        );
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
        assert!(host_allowed(
            "https://github.com/o/r/releases/download/v1/a.tar.gz"
        ));
        assert!(host_allowed("https://objects.githubusercontent.com/x"));
        // The SSRF cases the allowlist exists to refuse.
        assert!(!host_allowed("http://169.254.169.254/latest/meta-data/"));
        assert!(!host_allowed("https://internal.corp/x.tar.gz"));
        assert!(!host_allowed("file:///etc/passwd"));
        // A lookalike must not match by suffix.
        assert!(!host_allowed("https://evil-github.com/x"));
        assert!(!host_allowed("https://github.com.evil.net/x"));
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
            prebuild_install_url(&bs, &BTreeMap::new(), &host).unwrap(),
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
            prebuild_install_url(&kt, &BTreeMap::new(), &host).unwrap(),
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
}
