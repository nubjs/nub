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
/// `objects.githubusercontent.com` is where `github.com/<o>/<r>/releases/download/…`
/// 302s; the client follows redirects internally, so only the INITIAL host is matched
/// here — the entry is what makes a directly-spelled asset URL work.
const PREFETCH_HOSTS: &[&str] = &["github.com", "objects.githubusercontent.com"];

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
fn node_facts(ambient: &BTreeMap<String, String>, probe: &ProbeScope) -> Option<&'static NodeFacts> {
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

/// The `libc` slot, which is EMPTY except on musl Linux.
///
/// PROVENANCE (prebuild-install `rc.js:56`): `rc.libc = rc.platform !== 'linux' || rc.libc
/// === detectLibc.GLIBC ? '' : rc.libc`. Off Linux, and on glibc Linux, the slot is
/// force-blanked — so the `{platform}{libc}` pair is a bare `darwin` / `linux`, and only a
/// musl host produces the concatenated `linuxmusl` (no separator: the template is
/// `{platform}{libc}`, not `{platform}-{libc}`). Getting this wrong misses every path.
fn detect_libc(platform: &str, ambient: &BTreeMap<String, String>) -> String {
    if platform != "linux" {
        return String::new();
    }
    if let Some(v) = ambient.get("LIBC").or_else(|| ambient.get("npm_config_libc")) {
        return if v == "glibc" { String::new() } else { v.clone() };
    }
    // detect-libc's non-glibc test, reduced to its observable: a musl system ships its
    // loader as `/lib/ld-musl-<arch>.so.1` and has no glibc `ldd` report to parse.
    let musl = std::fs::read_dir("/lib").is_ok_and(|d| {
        d.filter_map(Result::ok).any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("ld-musl-")
        })
    });
    if musl { "musl".to_string() } else { String::new() }
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
    let dest = spawn
        .package_dir
        .join(local_prebuilds_prefix(manifest, ambient))
        .join(url_basename(&url));
    place(&url, &dest)
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
    let target = config["target"]
        .as_str()
        .or_else(|| ambient.get("npm_config_target").map(String::as_str))
        .unwrap_or(&node.version);

    // The ABI slot. `node` resolves through node-abi's crosswalk and `electron` /
    // `node-webkit` through their own tables — none of which nub carries (see
    // `node_facts`). So the only cases derivable here are the two that need no table: the
    // default runtime AT the running Node's version (ABI = its own `modules`), and a napi
    // runtime (ABI = the negotiated Node-API level). Anything else declines, and the
    // script falls back to what it would have done unprefetched.
    let abi = match runtime {
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
        ("libc", detect_libc(&node.platform, ambient)),
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

    Some(expand(&prebuild_url_template(manifest, ambient, name)?, &vars))
}

/// `util.js` `urlTemplate`, in its own precedence order.
fn prebuild_url_template(
    manifest: &Value,
    ambient: &BTreeMap<String, String>,
    name: &str,
) -> Option<String> {
    const DEFAULT_ASSET: &str =
        "{name}-v{version}-{runtime}-v{abi}-{platform}{libc}-{arch}.tar.gz";

    if let Some(explicit) = ambient.get("npm_config_download") {
        return Some(explicit.clone());
    }
    let prefix = prebuild_env_prefix(name);
    let mirror = ambient
        .get(&format!("{prefix}_binary_host"))
        .or_else(|| ambient.get(&format!("{prefix}_binary_host_mirror")));
    if let Some(mirror) = mirror {
        return Some(format!("{mirror}/{{tag_prefix}}{{version}}/{DEFAULT_ASSET}"));
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

    // The mirror tree lives under nub's PM cache, which `$tooldirs` already read-grants to
    // the jail — so the placed artifact needs no new fs rule, and it is shared across
    // packages and across installs instead of being re-fetched per package dir.
    let root = cache_root()?.join("prefetch").join(digest(url.as_str()));
    let dest = root.join(remote_path.trim_start_matches('/')).join(&package_name);
    place(url.as_str(), &dest)?;

    // PROVENANCE (`versioning.js:316`): `opts.module_name.replace('-', '_')` — a STRING
    // pattern, so JS replaces only the FIRST dash. A module named `a-b-c` yields
    // `a_b-c`, and a spelling that "helpfully" replaced all of them would set a variable
    // node-pre-gyp never reads. Reproduce the bug or the mirror is ignored.
    let var = format!(
        "npm_config_{}_binary_host_mirror",
        module_name.replacen('-', "_", 1)
    );
    ambient.insert(var, format!("file://{}/", root.to_str()?));
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
        ("name", manifest["name"].as_str().unwrap_or_default().to_string()),
        ("configuration", "Release".to_string()),
        (
            "module_name",
            binary["module_name"].as_str().unwrap_or_default().to_string(),
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
            if node.napi > 0 { "napi".to_string() } else { node_abi },
        ),
        ("napi_version", node.napi.to_string()),
        ("napi_build_version", napi_build_version),
        ("node_napi_label", node_napi_label),
        ("target", String::new()),
        ("platform", node.platform.clone()),
        ("target_platform", node.platform.clone()),
        ("arch", node.arch.clone()),
        ("target_arch", node.arch.clone()),
        (
            "libc",
            match detect_libc(&node.platform, ambient) {
                s if s.is_empty() => "unknown".to_string(),
                s => s,
            },
        ),
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

/// Fetch `url` into nub's prefetch cache (once per URL, machine-wide) and copy it to
/// `dest`. `None` on any refusal or failure — the caller then changes nothing.
///
/// An existing `dest` is left ALONE. That is not just idempotence: `prebuilds/` is a
/// documented user-facing drop point ("build it yourself and put it here"), so a file
/// already present is a deliberate local override and nub must not clobber it.
fn place(url: &str, dest: &Path) -> Option<()> {
    if dest.exists() {
        return Some(());
    }
    if !host_allowed(url) {
        tracing::debug!(url, "prefetch: host not on the prefetch allowlist");
        return None;
    }
    let cached = cache_root()?.join("prefetch-blobs").join(digest(url));
    if !cached.exists() {
        std::fs::create_dir_all(cached.parent()?).ok()?;
        // Download to a sibling temp first: a concurrent install must never observe a
        // half-written blob at the cache path and treat it as a complete artifact.
        let tmp = cached.with_extension("part");
        nub_core::version_management::download::download_to_file_auth(url, &tmp, None, |_, _| {})
            .map_err(|e| tracing::debug!(url, error = %e, "prefetch: download failed"))
            .ok()?;
        std::fs::rename(&tmp, &cached).ok()?;
    }
    std::fs::create_dir_all(dest.parent()?).ok()?;
    // Copy, never hardlink: prebuild-install probes the pickup path for W_OK and the
    // installer owns the file afterwards, so it must not share an inode with the cache.
    std::fs::copy(&cached, dest).ok()?;
    tracing::debug!(url, dest = %dest.display(), "prefetch: placed");
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
    version.split_once(sep).map(|(_, t)| t.to_string()).unwrap_or_default()
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
    if s.ends_with('/') { s.to_string() } else { format!("{s}/") }
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

    #[test]
    fn libc_slot_is_blank_off_linux_and_concatenated_on_musl() {
        assert_eq!(detect_libc("darwin", &BTreeMap::new()), "");
        let glibc = BTreeMap::from([("LIBC".to_string(), "glibc".to_string())]);
        assert_eq!(detect_libc("linux", &glibc), "");
        let musl = BTreeMap::from([("LIBC".to_string(), "musl".to_string())]);
        assert_eq!(detect_libc("linux", &musl), "musl");

        // The template concatenates with no separator: `linux` + `musl`.
        let manifest = serde_json::json!({
            "name": "sharp", "version": "0.33.0",
            "repository": "https://github.com/lovell/sharp",
        });
        let mut facts = node26();
        facts.platform = "linux".into();
        facts.arch = "x64".into();
        let url = prebuild_install_url(&manifest, &musl, &facts).unwrap();
        assert!(url.ends_with("sharp-v0.33.0-node-v140-linuxmusl-x64.tar.gz"), "{url}");
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
        assert!(url.ends_with("sodium-native-v4.0.0-napi-v8-darwin-arm64.tar.gz"), "{url}");
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
        assert_eq!(asset, "better_sqlite3-v11.5.0-node-v140-darwin-arm64.tar.gz");
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
        assert!(host_allowed("https://github.com/o/r/releases/download/v1/a.tar.gz"));
        assert!(host_allowed("https://objects.githubusercontent.com/x"));
        // The SSRF cases the allowlist exists to refuse.
        assert!(!host_allowed("http://169.254.169.254/latest/meta-data/"));
        assert!(!host_allowed("https://internal.corp/x.tar.gz"));
        assert!(!host_allowed("file:///etc/passwd"));
        // A lookalike must not match by suffix.
        assert!(!host_allowed("https://evil-github.com/x"));
        assert!(!host_allowed("https://github.com.evil.net/x"));
    }

    #[test]
    fn node_facts_parse_is_strict_about_shape() {
        let f = parse_node_facts("26.0.0 140 10 darwin arm64\n").unwrap();
        assert_eq!((f.modules.as_str(), f.napi, f.arch.as_str()), ("140", 10, "arm64"));
        assert!(parse_node_facts("").is_none());
        assert!(parse_node_facts("26.0.0 140\n").is_none());
    }
}
