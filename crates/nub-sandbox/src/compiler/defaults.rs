//! Shared compiler defaults: curated environment handling, filesystem subtree
//! expansion, and compatibility classifiers used by backend controls.

/// Case-insensitive substring test for a secret name-word anywhere in a key. Used by
/// [`is_npm_config_credential`] for the registry-credential family.
pub fn word_in_substr(word: &str, key: &str) -> bool {
    key.to_ascii_uppercase()
        .contains(&word.to_ascii_uppercase())
}

/// Whole-segment (case-insensitive) test. The key is split on `_`/`-`/`.` and the word
/// must EQUAL one segment, so `key` hits `NPM_CONFIG_KEY` but misses `KEYTAR`. Used by
/// [`is_npm_config_credential`] for the registry-credential family.
pub fn word_is_segment(word: &str, key: &str) -> bool {
    let w = word.to_ascii_uppercase();
    segments(key).contains(&w)
}

/// Split a name into non-empty, upper-cased segments on `_`/`-`/`.` boundaries.
fn segments(s: &str) -> Vec<String> {
    s.split(['_', '-', '.'])
        .filter(|seg| !seg.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

/// A subtree grant expands to two globs — the node itself and everything under
/// it — so a bare path grants both the named node and its descendants.
/// A pattern already carrying a glob metachar is emitted as-is (no `/**` suffix).
pub fn subtree_globs(expanded: &str) -> Vec<String> {
    if expanded.contains(['*', '?', '[', '{']) {
        return vec![expanded.to_string()];
    }
    // `/` is the whole-filesystem spelling every backend recognizes (`is_whole_fs` /
    // `is_whole_root` accept it), but the globs it expands to — `""` and `/**` — are
    // drive-less, and a drive-less path matches nothing on Windows. So `fs: { "/": "r" }`
    // compiled to rules no candidate could hit and the grant silently evaporated, while
    // the `""` half reached `literal_subtree` as a Some(empty path) the Windows ACL
    // planner would try to grant. `**` is the drive-agnostic spelling of the same rule and
    // is already classified as whole-fs everywhere. Matched exactly, not via the trimmed
    // form: `//` and a slash-normalized bare `\\` are not root spellings on Windows.
    #[cfg(windows)]
    if expanded == "/" {
        return vec!["**".to_string()];
    }
    let trimmed = expanded.trim_end_matches('/');
    vec![trimmed.to_string(), format!("{trimmed}/**")]
}

/// Kernel trees the Linux backend handles without granting a literal subtree.
#[cfg(any(target_os = "linux", test))]
pub(crate) const RESERVED_KERNEL_TREES: &[&str] = &["/proc", "/sys", "/dev"];

/// Non-secret operational env keys that pass through in the `sandbox: true`
/// curated baseline: PATH + system/locale/toolchain-discovery vars + the
/// build-hint `npm_config_*` subset. Ambient secrets never ride this list. The
/// exact baseline is the deferred build-jail thread's product surface; this is a
/// usable, safe default for the frontend-less engine.
///
/// The Windows container-essential block (`SystemRoot` … `PROCESSOR_ARCHITECTURE`)
/// is load-bearing: `CreateProcessW` with a constructed environment block that
/// omits `SystemRoot` fails `ERROR_ENVVAR_NOT_FOUND` (the loader resolves system
/// DLLs relative to it), and a normal Windows exe (node.exe) needs the
/// `USERPROFILE`/`APPDATA`/`LOCALAPPDATA` family to resolve its home/temp/config.
/// The `ProgramFiles`/`ProgramFiles(x86)`/`ProgramW6432`/`ProgramData` OS-location
/// vars are non-secret and load-bearing for native builds: node-gyp's Python and
/// toolchain discovery reads them (`find-python.js`), falling back to a wrong path on
/// a non-C: / relocated install when they are absent. These names never appear on unix
/// (the filter is over the ambient env, so the baseline stays OS-appropriate without a
/// `cfg`).
const BASELINE_ENV_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "TERM",
    "TZ",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    // Windows container-essential (see the doc note above).
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "LOCALAPPDATA",
    "APPDATA",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    // Windows OS-location vars for native-build toolchain discovery (see doc note).
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "ProgramData",
];
const BASELINE_ENV_PREFIXES: &[&str] = &["LC_", "npm_config_"];

/// The `npm_config_` prefix carries BOTH build hints (kept) and registry
/// CREDENTIALS (must never reach sandboxed code). See [`is_npm_config_credential`].
const NPM_CONFIG_PREFIX: &str = "npm_config_";

/// Unambiguous credential words in an `npm_config_*` key — long/specific enough that
/// a case-insensitive SUBSTRING hit has no realistic collision with a node-gyp /
/// node-pre-gyp build hint (none of which embed these). Case-insensitive substring
/// discipline (via [`word_in_substr`]), scoped to the registry-credential family.
const NPM_CRED_SUBSTR_TOKENS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "apikey",
];

/// Short/ambiguous credential words matched ONLY as a whole `_`/`-`/`.` segment, so
/// `npm_config_key` (npm's inline registry client SSL key) and `npm_config_api_key`
/// scrub while a package binary-host hint whose name merely contains the letters
/// (`npm_config_keytar_binary_host_mirror`, `..._monkey_...`) is spared. `auth` is
/// deliberately NOT here — it stays the anchored `_auth` test below so `always-auth` /
/// `_author` are not swept.
const NPM_CRED_SEGMENT_TOKENS: &[&str] = &["key"];

/// Whether an `npm_config_*` key (given the part AFTER the `npm_config_` prefix) is a
/// registry CREDENTIAL rather than a build hint — such keys must never reach sandboxed
/// lifecycle code (.fray/sandbox.md thread #6: registry auth never rides the lifecycle
/// env). Three tiers, all case-insensitive + delimiter-aware. First, the anchored legacy
/// markers: `_auth*` (the leading `_` spares `always-auth` / `_author`) and `email` as the
/// whole key or a registry-scoped `:email` suffix (an unanchored `email` would wrongly
/// scrub `npm_config_nodemailer_binary_host_mirror`). Then the unambiguous credential
/// words ([`NPM_CRED_SUBSTR_TOKENS`]) anywhere in the key — catching `password` /
/// `_authToken` / scoped `//host/:_password` and the undelimited `foo_token` / `my_secret`
/// forms an exact-segment rule would miss. Finally the short `key` family as a whole
/// segment ([`NPM_CRED_SEGMENT_TOKENS`]). Kept build hints
/// (`target`/`arch`/`runtime`/`nodedir`/`python`/`*_binary_host_mirror`/…) match none.
/// Best-effort per §8: the rare native package literally named after a credential word
/// loses its binary-host MIRROR hint (falling back to the default host), acceptable next
/// to leaking a token.
fn is_npm_config_credential(remainder: &str) -> bool {
    let r = remainder.to_ascii_lowercase();
    if r.contains("_auth") || r == "email" || r.ends_with(":email") {
        return true;
    }
    if NPM_CRED_SUBSTR_TOKENS
        .iter()
        .any(|w| word_in_substr(w, remainder))
    {
        return true;
    }
    NPM_CRED_SEGMENT_TOKENS
        .iter()
        .any(|w| word_is_segment(w, remainder))
}

/// Build the curated-baseline child env from the ambient env (the `sandbox: true`
/// / build-jail env posture). Only the non-secret operational allowlist passes.
pub fn curated_baseline_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    ambient
        .iter()
        .filter(|(k, _)| baseline_allows(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Whether an env key is credential-shaped. Under the build-jail's default-deny
/// allowlist ([`build_jail_env_allowed`]) this is a BELT-AND-SUSPENDERS reject, not
/// the primary control: the allowlist already admits only known-safe namespaces, and
/// this rejects any credential-shaped name that somehow rides an allowed prefix in. It
/// also backs [`baseline_allows`]'s `npm_config_*` carve-out. Two tiers, both
/// delimiter/case-aware: an `npm_config_*` key routes to the registry-credential
/// predicate ([`is_npm_config_credential`], which also scrubs `_auth*`/`email`/`key`),
/// and any other key is scrubbed when it CONTAINS an unambiguous credential word
/// (`token`/`secret`/`password`/`passwd`/`credential`/`apikey`) or carries `auth` as a
/// delimited segment (`AUTH_TOKEN`/`GITHUB_AUTH`, sparing `AUTHOR`). Best-effort by
/// NAME — what actually keeps the general credential family (`*_API_KEY`,
/// `AWS_ACCESS_KEY_ID`, `PRIVATE_KEY`, `DATABASE_URL`, …) out is the allowlist NOT
/// admitting them, since a denylist structurally misses names with no credential word.
pub fn is_credential_env_key(key: &str) -> bool {
    if let Some(rest) = strip_prefix_ci(key, NPM_CONFIG_PREFIX) {
        return is_npm_config_credential(rest);
    }
    if NPM_CRED_SUBSTR_TOKENS
        .iter()
        .any(|w| word_in_substr(w, key))
    {
        return true;
    }
    word_is_segment("auth", key)
}

/// Env-name PREFIXES admitted into the build-jail lifecycle env on top of
/// [`baseline_allows`]: the package-metadata and lifecycle namespaces npm/aube set for
/// a dependency's build script. `npm_config_*` (with its credential carve-out) and
/// `LC_*` already come through [`baseline_allows`]; these are the build-jail additions.
const BUILD_JAIL_EXTRA_PREFIXES: &[&str] = &["npm_package_", "npm_lifecycle_"];

/// Exact env names admitted into the build-jail lifecycle env on top of
/// [`baseline_allows`]. Three groups:
///  - Lifecycle re-invocation / install-root discovery: `NODE` + `npm_node_execpath`
///    (a build script re-runs the SAME node), `INIT_CWD` (npm's invocation dir).
///  - Proxy config for prebuilt-binary fetchers (node-pre-gyp / prebuild-install),
///    both cases — POSIX tools split by convention (curl reads lowercase, others upper).
///  - Non-secret native-toolchain config a real addon linking a system library needs
///    (node-gyp/gyp/make/cc/ld honor these). The runtime-linker-injection family
///    (`LD_LIBRARY_PATH`/`DYLD_*`) is DELIBERATELY EXCLUDED: link-time lib dirs ride
///    `LDFLAGS`, and `*_LIBRARY_PATH` is a runtime-injection surface, not a build need.
///
/// PATH/HOME/TMPDIR/TEMP/TMP + the OS-essential + locale/operational set come through
/// [`baseline_allows`]; of those, only PATH/HOME/TMPDIR were EMPIRICALLY load-bearing
/// for a from-source node-gyp compile (HOME anchors the header cache on a clean host).
///
/// `__NUB_NODE_GYP_EXE` is DELIBERATELY absent. The engine stamps it so its lazy
/// `npm_config_node_gyp` shim can trampoline back through the PM binary
/// (`<exe> __node-gyp-bootstrap <dir>`), but that trampoline is neither needed nor safe
/// here: the engine bootstraps the real node-gyp OUTSIDE the jail and prepends its
/// `$tooldirs`-readable bin dir to the lifecycle PATH, and the shim resolves that
/// bootstrapped copy DIRECTLY from its own location, never by a PATH lookup — measured
/// end-to-end, a from-source addon links under the jail through both the bare-`node-gyp`
/// and the `npm_config_node_gyp` route.
///
/// A PATH lookup is the LAST-RESORT branch, not the mechanism, and `8f7ac1033f` is why:
/// PATH is exactly the namespace npm's run-script rewrites, so resolving `node-gyp` by
/// name walked into npm's own stub, which re-exports the inherited `npm_config_node_gyp`
/// — i.e. back into this shim. Withholding the var is what made that cycle the ONLY
/// reachable branch here, so the jail was causal for BUG-C via this very scrub.
/// Admitting the var would REPLACE that working fallback with an exec of the PM binary,
/// which no read grant covers, and the shim does not guard that call. Whether that is
/// survivable is BACKEND-DEPENDENT, so it must not be assumed: under macOS Seatbelt the
/// binary is unreadable yet still executable (the base profile allows `process-exec`
/// outright), and the trampoline was measured to succeed; under a Linux bwrap namespace
/// an ungranted path is simply not mounted, so the same exec is ENOENT and the compile
/// hard-fails with no fallback left. Admitting the var therefore requires read-granting
/// the binary in the same change — see `build_jail_withholds_the_node_gyp_trampoline_exe`.
const BUILD_JAIL_EXTRA_EXACT: &[&str] = &[
    // ⛔⛔ THE JAIL STAMPS ALL THREE, and without these entries the stamps were INERT — this
    // scrub dropped them before the child ever saw them. Same class as `NODE_OPTIONS` below:
    // safe ONLY because `build_jail.rs` writes nub's own value into `ambient` (and purges every
    // case-variant) before the scrub runs, so an ambient user value is already overwritten and
    // cannot ride the entry in. The entry and the stamp move together, in both directions.
    //
    // MEASURED, which is how the inertness surfaced: electron-chromedriver@43.2.0 re-measured on
    // 0d9c2c575b — a binary that CARRIES `redirect_electron_cache` — stayed at 55 cells
    // write:"disk", and its restored-over-runner-up paths were still the DEFAULT location:
    //     home/AppData/Local/electron/Cache/<sha>/chromedriver-v43.2.0-win32-x64.zip
    //     home/AppData/Local/electron/Cache/<sha>/SHASUMS256.txt
    // The consumer does forward the knob (`download-chromedriver.js:12` passes
    // `cacheRoot: process.env.electron_config_cache`) and the redirect's target sits under
    // `$cache/nub/pm/tools`, which is granted at EVERY rung — so had the variable arrived, the
    // download would have landed somewhere already granted. It did not arrive.
    //
    // ⛔ `npm_config_prefix` needs NO entry: it rides `baseline_allows`' `npm_config_*` prefix
    // carve-out. That asymmetry is exactly why one of the three redirects appeared to work while
    // the other two did not — it is not evidence that their premises were wrong.
    "electron_config_cache",
    "ELECTRON_CACHE",
    "PLAYWRIGHT_BROWSERS_PATH",
    "NODE",
    // The jail STAMPS this (`build_jail.rs`), so it must survive the scrub. A dependency's
    // lifecycle script runs on vanilla Node: nub's preload is a developer-facing
    // augmentation a published postinstall never asked for, and loading nub's runtime —
    // including the dlopen'd native addon — into untrusted code is surface the jail exists
    // to remove. Bubblewrap already behaved this way by accident (its mount view hides
    // nub's runtime dir, so preload discovery found nothing and degraded); Landlock leaves
    // the dir VISIBLE-but-unreadable, so discovery succeeded and the read then hard-failed.
    "NODE_COMPAT",
    // Also STAMPED by the jail, and in the same silent-if-dropped class as the MSVC trio
    // below: it names the Node the package's own pin chain asked for, resolved OUT of the
    // jail because the confined shim can reach none of discovery's answers — not `~/.nvm`,
    // not nub's store, not nodejs.org. Withheld, the child re-runs that walk and fails
    // closed on a version the host may already have unpacked. The value is one nub itself
    // resolved, never an ambient a dependency could aim elsewhere: `build_jail.rs`
    // overwrites the key unconditionally whenever the pin chain yields a pin.
    "NODE_EXECUTABLE",
    "npm_node_execpath",
    "INIT_CWD",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "CC",
    "CXX",
    "CPP",
    "LD",
    "AR",
    "AS",
    "NM",
    "RANLIB",
    "STRIP",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "LIBRARY_PATH",
    "PKG_CONFIG",
    "PKG_CONFIG_PATH",
    "MAKE",
    "MAKEFLAGS",
    "PYTHON",
    "GYP_DEFINES",
    // macOS SDK / deployment-target overrides a real native build sets (non-secret;
    // absent → the toolchain's default SDK/min-OS, so a package pinning either needs them).
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    // The Windows MSVC trio `node-gyp`'s `findVisualStudio` short-circuits on. WITHOUT THESE
    // THE JAIL CANNOT BUILD FROM SOURCE AT ALL: its only other discovery routes are a COM
    // server an AppContainer may not activate and a PowerShell module it cannot see, so the
    // pre-resolved answer `pm_engine::jail_msvc` stamps is the whole mechanism. Non-secret
    // toolchain path/version pointers, the same class as `SDKROOT` and `PYTHON` above, so an
    // ambient value (a developer command prompt's) rides in on the same terms — which is also
    // the behaviour node-gyp is built around. Inert on POSIX, where nothing sets them, exactly
    // as the two macOS names above are.
    "VCINSTALLDIR",
    "VSCMD_VER",
    "WindowsSDKVersion",
];

/// Whether an env key is admitted into the build-jail's lifecycle env — the
/// DEFAULT-DENY allowlist that replaced the old credential denylist. A denylist
/// structurally missed the general credential family (`OPENAI_API_KEY`,
/// `AWS_ACCESS_KEY_ID`, `PRIVATE_KEY`, `SSH_KEY`, `GH_PAT`, `DATABASE_URL`, …) — none
/// carry a credential WORD, so all passed straight through. This admits the curated
/// baseline ([`baseline_allows`] — OS/startup essentials, locale/operational vars,
/// `npm_config_*` minus registry credentials) plus the build-jail additions
/// ([`BUILD_JAIL_EXTRA_PREFIXES`]/[`BUILD_JAIL_EXTRA_EXACT`]); everything else is
/// DENIED. [`is_credential_env_key`] is applied first as a belt-and-suspenders reject
/// so a credential-shaped name can never ride an allowed prefix in. Matching is
/// case-sensitive, as Linux env names are.
pub fn build_jail_env_allowed(key: &str) -> bool {
    if is_credential_env_key(key) {
        return false;
    }
    if baseline_allows(key) {
        return true;
    }
    BUILD_JAIL_EXTRA_EXACT.contains(&key)
        || BUILD_JAIL_EXTRA_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Drop whole-line comments and indentation before a payload is encoded. Applied to the stamped
/// net-gate shim, so the source stays densely commented (which is where its provenance lives)
/// while the delivered payload does not carry the prose. The stdio and realpath shims it also
/// served were Windows-only and went with the AppContainer backend.
///
/// The composed stamp must fit downstream tools' environment APIs as well as process launch.
/// `stamped_node_options_fits_the_env_block` checks the compressed preload against that budget.
///
/// WHOLE LINES ONLY, and NOT a step on the way to a character-level stripper. Deciding what a
/// mid-line `/` means is context-sensitive grammar — regex-literal versus division is settled by
/// the PARSER's expectation state, not lexically, which is why doing it properly costs a full
/// lexer coupled to a parser (oxc's `lexer/regex.rs` is entered from the parser, not from a
/// standalone scan). Reaching for one here would trade a dead-simple transform for a parser whose
/// failure mode is a silently half-valid module evaluated inside a confined child, and the payoff
/// is nil: line-leading comments are already over half of every shim (55-62%).
///
/// ITS PRECONDITION, machine-checked below rather than promised in prose: no string or template
/// literal may span a newline. Trimming and dropping whole lines is otherwise exactly
/// semantics-preserving — comments and blank lines are already whitespace to the parser, and one
/// newline per surviving line is kept, so ASI is untouched.
fn strip_js_comments(src: &str) -> String {
    let out = src
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // Checked on the OUTPUT, where the comment lines that legitimately carry lone backticks
    // (prose quoting an identifier) are already gone, so only code is measured.
    debug_assert!(
        out.lines().all(|l| l.matches('`').count() % 2 == 0) && !out.contains("\\\n"),
        "strip_js_comments is unsound where a string or template literal spans a newline"
    );
    out
}

/// The JS enforcing per-package egress inside the confined Node. Kept as a FILE rather than a
/// Rust string so it stays readable, greppable and lintable.
const NET_GATE_SHIM: &str = include_str!("../backend/net_gate_shim.js");

/// The token the shim reserves for its compiled policy. Substituted, never appended: the shim
/// reads it as a bare initializer, so a MISSING placeholder yields a module that throws on an
/// undefined identifier — which aborts the confined Node at startup rather than degrading to
/// an ungated one. [`net_gate_node_options`] therefore asserts the substitution happened.
const NET_GATE_POLICY_PLACEHOLDER: &str = "__NUB_NET_POLICY_JSON__";

/// The `--import` term delivering the per-package network gate for `package_name`.
///
/// WHAT THIS IS, PLAINLY: userland, and NOT a security boundary. It patches
/// `net.Socket.prototype.connect`, `dns`, `dgram` and the `child_process` seams inside the
/// confined Node, so a native addon opening a raw socket bypasses it entirely, and so does any
/// client that ignores proxy env (`curl --noproxy '*'`, a static binary, Windows PowerShell
/// 5.1, which reads HKCU proxy settings rather than the environment — a named, accepted
/// residual). Do not describe it as an OS-enforced guarantee.
///
/// ⛔ THE OLD JUSTIFICATION FOR ACCEPTING THE POWERSHELL RESIDUAL — "no corpus package uses
/// PowerShell as a lifecycle entry" — IS FALSE, measured 2026-08-04. `dprint@0.19.2`'s postinstall
/// runs `install.ps1`, whose body is `Invoke-WebRequest $DprintUri -OutFile $DprintZip`: a
/// PowerShell download as the lifecycle entry point, exactly the shape the claim ruled out.
///
/// What actually contains it on Windows is a DIFFERENT mechanism, and it is worth stating so the
/// residual is not re-justified on the wrong grounds: inside the AppContainer that PowerShell
/// cannot resolve its own cmdlets at all — the corpus logs show
/// `Invoke-WebRequest … CommandNotFoundException` in 100% of dprint's non-control cells, i.e. at
/// EVERY grant level — which is the same "a PowerShell module it cannot see" limitation noted on
/// `OS_ESSENTIAL_ENV` above. So the bypass is unreachable in practice HERE, by accident of the
/// LowBox rather than by design, and nothing about the gate itself prevents it. On a platform
/// whose backend does not decline PowerShell this residual would be live.
///
/// What it DOES buy is the shape the threat actually has. Shai-Hulud grew by publishing a new
/// lifecycle hook into packages that never had one, phoning home with plain
/// `https.get`/`fetch`/`axios`. All of that is denied for any package the catalog does not
/// name, on all three platforms and at both Node tiers. To spread, a worm must now ship and
/// load a per-platform native socket addon — a far smaller and far more conspicuous blast
/// radius than one line of JS. Against nub's 344-package corpus the preload reaches 178 of the
/// 179 packages that contact any host, the exception being a POSIX `.sh` that does not run on
/// Windows at all.
///
/// THE POLICY IS A PER-PACKAGE BOOLEAN, sourced from package identity
/// ([`super::build_jail_net_allowed`]). There is deliberately no host list: see that module for
/// why per-host permissioning was dropped rather than deferred.
///
/// PLATFORM-INDEPENDENT on purpose, though only the Windows jail stamps it today (the one
/// platform with no unprivileged OS egress lever; Linux has a seccomp `AF_INET` ceiling and
/// macOS denies outright in Seatbelt — NO proxy on any platform, because the jail's egress
/// decision is a per-package boolean and there is nothing to route). Nothing branches on OS, so serving as a
/// defence-in-depth layer elsewhere needs no porting — and keeping it un-gated is what makes
/// the behaviour testable off Windows.
pub fn net_gate_node_options(package_name: Option<&str>, package_version: Option<&str>) -> String {
    let policy = serde_json::json!({
        "package": package_name,
        // ⛔ THE SHARED DECISION, NOT THE v1 TABLE. This called
        // `package_network::build_jail_net_allowed` directly, which knows nothing of the v2 catalog or
        // the baseline — so on Windows, where this shim IS the egress enforcement, an uncatalogued
        // package was denied the network although `baseline_caps().network` grants it. Measured: 17 of
        // the 86 win32 jail-blaming records die on
        // `blocked network access to node-precompiled-binaries.grpc.io by grpc`, and `grpc` has no
        // catalog entry at all.
        "allow": super::preset::build_jail_net_allowed_for(package_name, package_version),
    });

    // Stripped before substitution, for the reason given on `strip_js_comments`.
    let js = strip_js_comments(NET_GATE_SHIM).replace(
        NET_GATE_POLICY_PLACEHOLDER,
        &serde_json::to_string(&policy).expect("a policy of strings and bools always serializes"),
    );
    debug_assert!(
        !js.contains(NET_GATE_POLICY_PLACEHOLDER),
        "net_gate_shim.js must contain exactly one {NET_GATE_POLICY_PLACEHOLDER}"
    );
    data_url_import(&js)
}

/// A compressed inline module keeps `NODE_OPTIONS` below Windows environment-value limits.
/// `CreateProcessW` accepts larger blocks, but MSBuild copies values through
/// `SetEnvironmentVariable` and rejects values at 32,767 characters. The outer module uses
/// only Node builtins; the actual preload still resolves from `data:` without filesystem access.
fn data_url_import(js: &str) -> String {
    use base64::Engine as _;
    use std::io::Write as _;
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(js.as_bytes())
        .expect("writing to a Vec cannot fail");
    let compressed = encoder.finish().expect("finishing in-memory deflate");
    let payload = base64::engine::general_purpose::STANDARD.encode(compressed);
    let loader = format!(
        "import{{inflateRawSync}}from'node:zlib';\
         await import('data:text/javascript;base64,'+\
         inflateRawSync(Buffer.from('{payload}','base64')).toString('base64')+\
         '#loader='+encodeURIComponent(import.meta.url));"
    );
    format!(
        "--import data:text/javascript;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(loader)
    )
}

/// The build-jail ENV posture (D1): a DEFAULT-DENY allowlist over the effective child
/// env the unconfined lifecycle spawn would have had. Only the known build namespace
/// ([`build_jail_env_allowed`]) is admitted; every other key — including the whole
/// general credential family a denylist structurally missed — is WITHHELD. The
/// returned policy enforces, carries the admitted keys as `constructed`, and records
/// the withheld ones (sorted — `BTreeMap` iteration order — for a stable failure hint).
pub fn lifecycle_scrubbed_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> crate::policy::EnvPolicy {
    let mut constructed = std::collections::BTreeMap::new();
    let mut withheld = Vec::new();
    for (key, value) in ambient {
        if build_jail_env_allowed(key) {
            constructed.insert(key.clone(), value.clone());
        } else {
            withheld.push(key.clone());
        }
    }
    crate::policy::EnvPolicy {
        resolved: true,
        enforce: true,
        constructed,
        schema: Vec::new(),
        withheld,
        // The allowlist admits only known-safe namespaces (a credential-shaped name is
        // rejected by is_credential_env_key), so nothing kept in `constructed` is a
        // secret — the output redactor has nothing to scrub.
        sensitive_keys: Vec::new(),
    }
}

/// OS-STARTUP-mechanism env names a sandboxed child needs merely to EXIST — the
/// spawning OS's own bootstrap essentials, per OS. Distinct from (and far narrower
/// than) [`BASELINE_ENV_EXACT`]: the baseline is "what a build child needs to
/// operate usefully" (PATH/HOME/USERPROFILE/npm hints); THIS is "what any child
/// needs before it can run at all." All names here are non-secret path/topology
/// pointers, so injecting their real ambient values does NOT breach the deny-all
/// floor (which denies USER/ambient env + secrets, not OS mechanism). Grounded in
/// `wiki/research/sandbox-os-essentials-env.md` (libuv `required_vars[]` + prior-art
/// survey) alongside nub's Windows-VM subset pin.
///
/// POSIX (macOS + Linux): EMPTY — an absolute-path `execve` starts with an empty
/// environ (`node`/`sh`/`true` all rc=0; `os.tmpdir()` falls back to `/tmp`), so the
/// floor injects nothing regardless of ambient contents.
///
/// Windows: `{SystemRoot, SystemDrive, TEMP, TMP, LOCALAPPDATA}`. Provenance:
///  - `SystemRoot`, `SystemDrive`, `TEMP` — libuv's own Windows `required_vars[]`
///    strict-essentials (`deps/uv/src/win/process.c`): winsock's `WSAStartup` fails
///    without `SystemRoot`; some APIs reference `TEMP`; `SystemDrive` is the
///    loader/CLR essential a managed child (`powershell.exe`) resolves System32 from.
///  - `TMP` — the other half of Node's `os.tmpdir()` fallback pair (`lib/os.js`:
///    `TEMP || TMP || <SystemRoot>\temp`); without the pair, AppContainer temp work
///    lands in a non-writable dir.
///  - `LOCALAPPDATA` — the AppContainer essential. The ENFORCING path (fs/net
///    confined → a LowBox AppContainer) resolves the per-container profile dir
///    (`%LOCALAPPDATA%\Packages\…`) from the env, so a block missing it fails
///    `CreateProcessW` with `ERROR_ENVVAR_NOT_FOUND` (203). The VM subset sweep
///    pinned it: `{SystemRoot}` and `{SystemRoot,USERPROFILE}` both fail 203,
///    `{SystemRoot,LOCALAPPDATA}` is the smallest that starts. It embeds the OS
///    username, but that disclosure is REDUNDANT (the child runs AS that user and
///    can read its own username) and empirically REQUIRED — the real value is the
///    minimal correct choice (a synthetic non-disclosing value needs a writable
///    scratch dir + fs grant for zero privacy gain).
///
/// The `SystemDrive`/`TEMP`/`TMP` widen over the earlier VM-pinned `{SystemRoot,
/// LOCALAPPDATA}` minimum is libuv-+-`os.tmpdir()`-grounded, not re-pinned on the VM;
/// a `windows-latest` conformance run at main-merge validates it end-to-end. libuv's
/// Cygwin-subprocess-compat vars (`USERNAME`/`USERDOMAIN`/`LOGONSERVER`/…) are NOT on
/// the floor — subprocess-compat, not start-essential, and identity-bearing.
///
/// `#[cfg]`-gated on the SPAWNING OS (= the child's OS) so a POSIX floor provably
/// injects nothing regardless of ambient contents, while the selection logic stays
/// host-independently testable via [`os_essential_env_from`].
#[cfg(windows)]
const OS_ESSENTIAL_ENV: &[&str] = &["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"];
#[cfg(not(windows))]
const OS_ESSENTIAL_ENV: &[&str] = &[];

/// Select the OS-essential names present in `ambient`, matched case-insensitively
/// (Windows env names are case-insensitive by OS contract — `SYSTEMROOT` and
/// `SystemRoot` are the same var — and the child keeps the ambient's actual cased
/// key + real value). Split from [`os_essential_env`] so the selection is unit-
/// testable on any host by passing an explicit name list.
fn os_essential_env_from(
    ambient: &std::collections::BTreeMap<String, String>,
    names: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let mut selected = std::collections::BTreeMap::new();
    for (key, value) in ambient {
        if names.iter().any(|name| name.eq_ignore_ascii_case(key)) {
            // This helper models Windows selection even on a POSIX test host.
            // A malformed synthetic map can contain aliases that a real Windows
            // environment cannot; keep one deterministic logical entry anyway.
            insert_env_with_case(&mut selected, key.clone(), value.clone(), true);
        }
    }
    selected
}

/// The OS-essential env for the spawning OS, read from the host ambient env at
/// compile time. Only the whitelisted NAMES are admitted; their VALUES come from
/// the real ambient env, and an essential absent from the host is skipped (never
/// fabricated).
pub fn os_essential_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    os_essential_env_from(ambient, OS_ESSENTIAL_ENV)
}

/// Environment variable names follow the spawning OS's name contract: Windows
/// folds ASCII case, while POSIX keeps it significant.
pub(crate) fn env_key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Whether `env` already contains `key`, honoring the spawning OS's env-name
/// contract. This is deliberately not a plain `BTreeMap::contains_key`: Windows
/// treats `PATH` and `Path` as one variable even though Rust's map does not.
pub(crate) fn env_contains_key(
    env: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> bool {
    env.keys().any(|existing| env_key_eq(existing, key))
}

/// Insert one environment value while preserving one logical Windows key. A
/// literal entry is folded before ambient source values, so its spelling and value
/// replace any same-folded ambient entry rather than serializing both aliases.
pub(crate) fn insert_env(
    env: &mut std::collections::BTreeMap<String, String>,
    key: String,
    value: String,
) {
    insert_env_with_case(env, key, value, cfg!(windows));
}

fn insert_env_with_case(
    env: &mut std::collections::BTreeMap<String, String>,
    key: String,
    value: String,
    case_insensitive: bool,
) {
    if case_insensitive {
        env.retain(|existing, _| !existing.eq_ignore_ascii_case(&key));
    }
    env.insert(key, value);
}

/// Add the OS bootstrap variables after every constraining env fold. These are
/// mechanism values, not user-provided capabilities: Windows must retain them for
/// `CreateProcessW`/AppContainer startup even when an array or object allowlist
/// otherwise excludes them. Existing policy entries win, including an explicit
/// literal override.
pub(crate) fn add_os_essential_env(
    policy: &mut crate::policy::EnvPolicy,
    ambient: &std::collections::BTreeMap<String, String>,
) {
    // `EnvPolicy` is public IR, so normalize even a direct caller's synthetic
    // map. Normal compiler folds have already made literal-over-ambient choice
    // explicit; this only gives an ambiguous externally-built alias pair one
    // deterministic Windows representation.
    let constructed = std::mem::take(&mut policy.constructed);
    for (key, value) in constructed {
        insert_env(&mut policy.constructed, key, value);
    }
    for (key, value) in os_essential_env(ambient) {
        if !env_contains_key(&policy.constructed, &key) {
            insert_env(&mut policy.constructed, key, value);
        }
    }
    policy.withheld = ambient
        .keys()
        .filter(|key| !env_contains_key(&policy.constructed, key))
        .cloned()
        .collect();
}

/// The strip-all env FLOOR: an enforcing env that WITHHOLDS all user/ambient env
/// but injects the minimal OS-startup essentials so the child spawns reliably
/// instead of only where the OS tolerates an empty block. Injecting these does NOT
/// breach the deny-all floor — the floor denies USER/ambient env and secrets; the
/// essentials are OS MECHANISM (where Windows is installed / how its loader finds
/// System32), never user config or a credential. Single source of truth for both
/// strip-all constructors: the complete-statement floor (`floor_env`) and the
/// explicit `env: false`.
pub fn strip_all_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> crate::policy::EnvPolicy {
    let constructed = os_essential_env(ambient);
    let withheld = ambient
        .keys()
        .filter(|k| !env_contains_key(&constructed, k))
        .cloned()
        .collect();
    crate::policy::EnvPolicy {
        resolved: true,
        enforce: true,
        constructed,
        schema: Vec::new(),
        withheld,
        // OS-essential baseline carries no user secrets.
        sensitive_keys: Vec::new(),
    }
}

/// Case-insensitive prefix strip: returns the remainder after `prefix` if `key`
/// starts with it (ignoring ASCII case), else `None`. Used to gate the credential
/// carve-out uniformly across platforms.
fn strip_prefix_ci<'a>(key: &'a str, prefix: &str) -> Option<&'a str> {
    key.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &key[prefix.len()..])
}

/// Whether a key is in the curated baseline. Case-SENSITIVE on unix (POSIX env
/// keys are); case-INSENSITIVE on Windows, where env names are case-insensitive by
/// OS contract and a process may report `SYSTEMROOT` or `SystemRoot` — an
/// exact-case miss would drop a container-essential var and re-open the
/// `ERROR_ENVVAR_NOT_FOUND` spawn failure the baseline exists to prevent.
///
/// Public because [`curated_baseline_env`] uses it as the match predicate for
/// `sandbox: true`'s env — the single source of truth for the curated allowlist,
/// never a drifting reimplementation.
pub fn baseline_allows(key: &str) -> bool {
    // Registry credential keys ride the build-hint `npm_config_*` prefix; scrub them
    // before the prefix pass would admit them. Case-insensitive prefix match so a
    // Windows-cased `NPM_CONFIG_//…:_authToken` is caught too (env names are
    // case-insensitive there); on unix npm always emits the lowercase prefix, so a CI
    // match only ever affects `npm_config_`-shaped keys and never widens the allow.
    if let Some(rest) = strip_prefix_ci(key, NPM_CONFIG_PREFIX)
        && is_npm_config_credential(rest)
    {
        return false;
    }
    #[cfg(windows)]
    {
        BASELINE_ENV_EXACT
            .iter()
            .any(|e| e.eq_ignore_ascii_case(key))
            || BASELINE_ENV_PREFIXES.iter().any(|p| {
                key.get(..p.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(p))
            })
    }
    #[cfg(not(windows))]
    {
        BASELINE_ENV_EXACT.contains(&key)
            || BASELINE_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
    }
}

#[cfg(test)]
mod tests {
    /// ⛔ EVERY KEY THE JAIL STAMPS MUST SURVIVE THIS SCRUB — the entry and the stamp move
    /// together, in BOTH directions, and this is the guard for the reverse direction.
    ///
    /// Without their `BUILD_JAIL_EXTRA_EXACT` entries the tool-cache redirects in
    /// `build_jail.rs` were INERT: the scrub dropped them before the child saw them, so
    /// `@electron/get` and playwright kept their DEFAULT cache roots — outside every rung on
    /// Windows, where those roots hang off `LOCALAPPDATA` rather than the redirected `HOME` —
    /// and the packages walked the ladder to `write:"disk"`. The failure is silent from both
    /// ends: the stamp looks landed and the redirect looks correct.
    ///
    /// The list below is every literal `ambient.insert` in `build_jail.rs`. Two more stamps
    /// are deliberately absent because they need no entry — `npm_config_prefix` and
    /// `npm_config_python` ride `baseline_allows`' `npm_config_*` PREFIX carve-out. That
    /// asymmetry is exactly why one redirect appeared to work while two did not.
    #[test]
    fn the_build_jail_admits_every_key_it_stamps() {
        for key in [
            "electron_config_cache",
            "ELECTRON_CACHE",
            "PLAYWRIGHT_BROWSERS_PATH",
            "NODE_COMPAT",
            "NODE_EXECUTABLE",
            "npm_node_execpath",
        ] {
            assert!(
                build_jail_env_allowed(key),
                "{key} is stamped by build_jail.rs but scrubbed before the child sees it, \
                 which makes the redirect inert"
            );
        }
        // CONTROL — the gate must still REJECT, or the assertions above prove nothing.
        assert!(
            !build_jail_env_allowed("ELECTRON_MIRROR_SECRET_TOKEN"),
            "the allowlist must not admit arbitrary keys"
        );
        assert!(
            !build_jail_env_allowed("NODE_GYP_FORCE_PYTHON"),
            "a key documented as withheld must stay withheld"
        );
    }
    use super::*;
    use crate::matcher::path::Homes;
    use crate::policy::Effect;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn homes() -> Homes {
        Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        }
    }

    /// `NODE_OPTIONS` is whitespace-separated, so an over-long or under-encoded payload does
    /// not produce a malformed URL that Node rejects — it produces a TRUNCATED preload that
    /// silently loads half a shim, or an option fragment Node aborts on. Both are quiet, so
    /// the encoding is asserted rather than eyeballed.
    #[test]
    fn the_stamped_shim_is_one_whole_import_term() {
        let stamped = net_gate_node_options(Some("chalk"), Some("5.6.2"));
        let terms: Vec<&str> = stamped.split(' ').collect();
        assert_eq!(
            terms.len(),
            2,
            "expected one `--import <url>` pair and nothing else: {stamped}"
        );
        assert_eq!(terms[0], "--import");
        assert!(terms[1].starts_with("data:text/javascript;base64,"));
        // The whole payload must arrive, not merely its opening — an identifier from the
        // shim's tail is what makes a truncation visible. Decoded rather than matched against
        // a re-encoding, because base64 is offset-sensitive: the encoding of a substring is
        // not generally a substring of the encoding.
        let source = decode_import(terms[1]);
        assert!(source.contains("ERR_NUB_JAIL_NET_DENIED"));
        assert!(source.contains("allowScripts"));
        assert!(!source.contains("allowBuilds"));
    }

    /// Round-trips an `--import data:…;base64,…` term back to its JS, so an assertion can be
    /// written against what the confined Node will actually EVALUATE rather than against the
    /// opaque encoding of it.
    fn decode_import(url: &str) -> String {
        use base64::Engine as _;
        let b64 = url
            .strip_prefix("data:text/javascript;base64,")
            .expect("a base64 data: URL");
        let loader = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("the generator emitted valid base64"),
        )
        .expect("the loader is UTF-8");
        let payload = loader
            .split("Buffer.from('")
            .nth(1)
            .unwrap()
            .split('\'')
            .next()
            .unwrap();
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        let mut decoder = flate2::read::DeflateDecoder::new(compressed.as_slice());
        let mut js = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut js).unwrap();
        js
    }

    /// The placeholder must be SUBSTITUTED, and the delivered module must carry a real policy
    /// literal. A missed substitution is the quiet catastrophe: the shim reads
    /// `__NUB_NET_POLICY_JSON__` as a bare initializer, so an unsubstituted payload throws on
    /// an undefined identifier and aborts the confined Node at startup.
    #[test]
    fn the_compiled_policy_is_substituted_into_the_delivered_module() {
        let js = decode_import(
            net_gate_node_options(Some("chalk"), Some("5.6.2"))
                .split_once(' ')
                .unwrap()
                .1,
        );
        assert!(
            !js.contains(NET_GATE_POLICY_PLACEHOLDER),
            "the policy placeholder survived into the delivered module"
        );
        assert!(
            // `chalk` carries no catalog entry, so its `allow` is the BASELINE's network value rather
            // than a hardcoded false — see `an_unlisted_package_takes_the_baseline_and_a_listed_one_is_
            // admitted_wholesale` for why that changed and what it was breaking on Windows.
            js.contains(&format!(
                r#"const POLICY = {{"package":"chalk","allow":{}}}"#,
                crate::catalog_v2::baseline_caps().network
            )),
            "{js}"
        );
    }

    /// The compiled policy line, as the confined Node will evaluate it.
    fn compiled_policy(package_name: Option<&str>, package_version: Option<&str>) -> String {
        decode_import(
            net_gate_node_options(package_name, package_version)
                .split_once(' ')
                .unwrap()
                .1,
        )
        .lines()
        .find(|l| l.starts_with("const POLICY = "))
        .expect("the policy line")
        .to_string()
    }

    /// THE GATE IS A BOOLEAN, AND IT NOW RESOLVES THROUGH THE SAME DECISION AS THE COMPILED NET AXIS.
    ///
    /// ⛔⛔ THIS ASSERTED "UNLISTED ⇒ DENIED" AND THAT WAS A SHIPPING BUG, not merely a stale test. The
    /// gate called `package_network::build_jail_net_allowed` — the v1 table — so it knew nothing of the v2
    /// catalog or of `baseline_caps().network`. On Windows this shim IS the egress enforcement, so an
    /// uncatalogued package was denied the network there although the baseline grants it. Measured: 17 of
    /// the 86 win32 records whose verdict blames the jail die on
    /// `blocked network access to node-precompiled-binaries.grpc.io by grpc`, and `grpc` has no entry.
    ///
    /// An unlisted package therefore takes the BASELINE now. What the test still pins is that the answer
    /// is a boolean with no host list in either direction — per-host permissioning is out of scope, and a
    /// stray `hosts` key is the tell that it came back.
    #[test]
    fn an_unlisted_package_takes_the_baseline_and_a_listed_one_is_admitted_wholesale() {
        let baseline_allows = crate::catalog_v2::baseline_caps().network;
        let want = if baseline_allows {
            r#""allow":true"#
        } else {
            r#""allow":false"#
        };
        for unlisted in [None, Some("chalk"), Some("definitely-not-in-the-catalog")] {
            let line = compiled_policy(unlisted, Some("1.0.0"));
            assert!(
                line.contains(want),
                "{unlisted:?} has no catalog entry, so the gate must match the baseline \
                 (network={baseline_allows}): {line}"
            );
        }
        // ⛔⛔ THE GATE MUST AGREE WITH THE COMPILED NET AXIS, PACKAGE FOR PACKAGE — that agreement IS
        // the invariant, and asserting `allow:true` for every v1-listed name was asserting the opposite.
        // While a v2 catalog is in force it is authoritative, and a v2 entry may DELIBERATELY grant less
        // than v1's table did: `@apollo/protobufjs` is listed in v1 and denied by its v2 entry, which is
        // the catalog tightening a high-value target exactly as intended. A test that demands v1's answer
        // would force the gate back onto the v1 table, which is the bug this whole change removes.
        //
        // Covering three classes on purpose: a v1-listed name, an uncatalogued one, and `None`. Each must
        // match `build_jail_net_allowed_for`, whatever that answers.
        let mut agree = vec![
            (None, "1.0.0".to_string()),
            (Some("definitely-not-catalogued"), "1.0.0".to_string()),
        ];
        agree.extend(
            super::super::PACKAGE_NETWORK_ALLOWED
                .iter()
                .map(|(name, range)| (Some(*name), admitted_version(range).to_string())),
        );
        for (pkg, version) in agree {
            let want = crate::compiler::preset::build_jail_net_allowed_for(pkg, Some(&version));
            let line = compiled_policy(pkg, Some(&version));
            assert!(
                line.contains(&format!(r#""allow":{want}"#)),
                "{pkg:?}@{version}: the node-level gate must match the compiled net axis \
                 (build_jail_net_allowed_for said {want}): {line}"
            );
        }

        // Neither direction may compile a host list; per-host permissioning is out of scope,
        // and a stray `hosts` key would be the tell that it came back.
        let mut probes = vec![(None, "1.0.0"), (Some("chalk"), "5.6.2")];
        probes.extend(
            super::super::PACKAGE_NETWORK_ALLOWED
                .iter()
                .map(|(name, range)| (Some(*name), admitted_version(range))),
        );
        for (pkg, version) in probes {
            assert!(
                !compiled_policy(pkg, Some(version)).contains("hosts"),
                "egress is a per-package boolean; a host list must not be compiled: {pkg:?}"
            );
        }
    }

    /// A version the given catalog scope admits — any version for an unscoped entry.
    fn admitted_version(range: &Option<&str>) -> &'static str {
        let Some(range) = range else {
            return "1.2.3";
        };
        let req = semver::VersionReq::parse(range).expect("build.rs validated it");
        ["0.0.1", "0.1.0", "1.0.0", "9999.0.0"]
            .into_iter()
            .find(|v| req.matches(&semver::Version::parse(v).expect("literal")))
            .unwrap_or_else(|| panic!("`{range}` matches none of the probe versions"))
    }

    /// Base64's alphabet contains no whitespace, which is the structural reason the payload
    /// survives `NODE_OPTIONS`' whitespace split. Asserted on the generator rather than left
    /// to the encoder's reputation, because a switch back to percent-encoding (or to any
    /// encoding with a space in its alphabet) reintroduces silent truncation.
    #[test]
    fn the_delivered_term_is_whitespace_free_after_the_flag() {
        let term = net_gate_node_options(Some("chalk"), Some("5.6.2"));
        let (flag, url) = term.split_once(' ').expect("flag then payload");
        assert_eq!(flag, "--import");
        assert!(
            !url.chars().any(char::is_whitespace),
            "the data: URL must survive NODE_OPTIONS' whitespace split"
        );
    }

    /// The stamp must fit a downstream tool's environment APIs as well as process launch, so the
    /// budget is asserted on the composed value rather than left to the launcher to discover.
    #[test]
    fn stamped_node_options_fits_the_env_block() {
        const BUDGET: usize = 26_000;
        let stamped = net_gate_node_options(Some("esbuild"), Some("0.21.5"));
        assert!(
            stamped.len() <= BUDGET,
            "the stamped NODE_OPTIONS is {} chars, over the {BUDGET} budget",
            stamped.len()
        );

        // Asserted on what the child will EVALUATE, not on the constants, so a call site that
        // forgets the stripper is caught alongside a stripper that stops working. Stripping must
        // remove PROSE and nothing else: a tail identifier proves the code survived past the point
        // a truncation would bite, and the absence of a line-leading `//` proves the prose did not.
        let payload = decode_import(
            stamped
                .split(' ')
                .find(|t| t.starts_with("data:"))
                .expect("the stamp carries one data: payload"),
        );
        assert!(payload.contains("origCpSpawnSync"));
        assert!(
            !payload.lines().any(|line| line.starts_with("//")),
            "whole-line comments must be gone from the delivered payload"
        );
    }

    #[test]
    fn baseline_keeps_windows_essentials_drops_secrets() {
        let ambient: BTreeMap<String, String> = [
            ("PATH", "/bin"),
            ("USERPROFILE", "C:/Users/me"),
            ("LOCALAPPDATA", "C:/Users/me/AppData/Local"),
            ("APPDATA", "C:/Users/me/AppData/Roaming"),
            ("NUMBER_OF_PROCESSORS", "8"),
            ("PROCESSOR_ARCHITECTURE", "AMD64"),
            ("SystemRoot", "C:/Windows"),
            ("ProgramFiles", "C:/Program Files"),
            ("ProgramFiles(x86)", "C:/Program Files (x86)"),
            ("ProgramW6432", "C:/Program Files"),
            ("ProgramData", "C:/ProgramData"),
            ("MY_SECRET_TOKEN", "leak"),
            ("AWS_SECRET_ACCESS_KEY", "leak"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let out = curated_baseline_env(&ambient);
        for k in [
            "PATH",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "SystemRoot",
            // OS-location vars native-build toolchain discovery reads (F1 regression fix).
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
        ] {
            assert!(out.contains_key(k), "baseline must keep {k}");
        }
        assert!(
            !out.contains_key("MY_SECRET_TOKEN"),
            "secret not in baseline"
        );
        assert!(
            !out.contains_key("AWS_SECRET_ACCESS_KEY"),
            "aws secret not in baseline"
        );
    }

    fn ambient(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn os_essential_selection_is_case_insensitive_and_value_preserving() {
        // Host-independent: exercise the selection with an explicit name list so the
        // Windows contract (case-insensitive names, real ambient value kept) is proven
        // on any dev host. A non-essential name is never admitted.
        let env = ambient(&[
            ("SYSTEMROOT", "C:/Windows"), // upper-cased ambient key still matches
            ("windir", "C:/Windows"),
            ("SECRET_TOKEN", "leak"),
        ]);
        let out = os_essential_env_from(&env, &["SystemRoot", "windir"]);
        assert_eq!(
            out.get("SYSTEMROOT").map(String::as_str),
            Some("C:/Windows"),
            "case-insensitive name match keeps the ambient key + value"
        );
        assert!(out.contains_key("windir"));
        assert!(
            !out.contains_key("SECRET_TOKEN"),
            "a non-essential (secret) name is never injected"
        );
    }

    #[test]
    fn windows_essential_selection_deduplicates_case_aliases() {
        // This is intentionally host-runnable: the selector models Windows
        // case-folding even when the test process itself is POSIX.
        let env = ambient(&[
            ("SYSTEMROOT", "C:/old"),
            ("SystemRoot", "C:/Windows"),
            ("TEMP", "C:/Temp"),
        ]);
        let out = os_essential_env_from(&env, &["SystemRoot", "TEMP"]);
        assert_eq!(out.len(), 2, "Windows aliases are one logical env key");
        assert_eq!(
            out.get("SystemRoot").map(String::as_str),
            Some("C:/Windows")
        );
        assert_eq!(out.get("TEMP").map(String::as_str), Some("C:/Temp"));
    }

    #[test]
    fn windows_env_insert_replaces_same_folded_ambient_key() {
        // The literal insertion seam is host-runnable with the Windows case rule
        // passed explicitly. It protects both the compiler's constructed map and
        // the Windows launch block from `Path`/`PATH` duplication.
        let mut env = ambient(&[("Path", "ambient")]);
        insert_env_with_case(&mut env, "PATH".to_string(), "literal".to_string(), true);
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("PATH").map(String::as_str), Some("literal"));
    }

    #[test]
    fn strip_all_injects_only_essentials_and_withholds_the_rest() {
        // The security property that matters: an ambient secret must NEVER ride the
        // strip-all floor's constructed env; only whitelisted OS essentials do, and
        // everything else is recorded withheld. On POSIX the essential set is empty,
        // so `constructed` is empty and even a `SystemRoot`-named ambient var is
        // withheld — the floor injects nothing where the OS needs nothing.
        let env = ambient(&[
            ("SystemRoot", "C:/Windows"),
            ("SystemDrive", "C:"),
            ("TEMP", "C:/Users/me/AppData/Local/Temp"),
            ("TMP", "C:/Users/me/AppData/Local/Temp"),
            ("LOCALAPPDATA", "C:/Users/me/AppData/Local"),
            ("AWS_SECRET_ACCESS_KEY", "leak"),
            ("GITHUB_TOKEN", "leak"),
            ("PATH", "/bin"),
        ]);
        let p = strip_all_env(&env);
        assert!(p.enforce, "strip-all always enforces");
        // No secret or user-config var ever appears in the constructed child env.
        for secret in ["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN", "PATH"] {
            assert!(
                !p.constructed.contains_key(secret),
                "{secret} must never ride the strip-all floor"
            );
            assert!(
                p.withheld.contains(&secret.to_string()),
                "{secret} must be recorded withheld"
            );
        }
        #[cfg(windows)]
        {
            for essential in ["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"] {
                assert!(
                    p.constructed.contains_key(essential),
                    "Windows floor injects the OS-startup essential {essential}"
                );
                assert!(
                    !p.withheld.contains(&essential.to_string()),
                    "an injected essential is provided, not withheld"
                );
            }
        }
        #[cfg(not(windows))]
        {
            assert!(
                p.constructed.is_empty(),
                "POSIX floor injects no essentials (empty-env exec starts fine)"
            );
            assert!(
                p.withheld.contains(&"SystemRoot".to_string()),
                "on POSIX a SystemRoot-named ambient var is withheld, not injected"
            );
        }
    }

    #[test]
    fn npm_config_build_hints_pass_but_credentials_scrubbed() {
        // The `npm_config_*` family passes build hints through, but registry auth
        // rides the same prefix and must be scrubbed — thread #6. Both the bare
        // legacy keys and the registry-scoped `//host/:_auth…` forms are excluded.
        // Build hints (kept) — incl. two regression guards for false positives: a
        // package whose name embeds "email" (`nodemailer`) must survive the anchored
        // `email` marker, and a package whose name embeds "key" (`keytar`) must survive
        // the whole-SEGMENT `key` rule. `always-auth` is `-auth`, not `_auth` — kept.
        let hints = [
            "npm_config_target",
            "npm_config_arch",
            "npm_config_target_arch",
            "npm_config_runtime",
            "npm_config_nodedir",
            "npm_config_python",
            "npm_config_build_from_source",
            "npm_config_registry",
            "npm_config_sharp_binary_host",
            "npm_config_nodemailer_binary_host_mirror",
            "npm_config_keytar_binary_host_mirror",
            "npm_config_always-auth",
        ];
        // Credentials (scrubbed) — the anchored legacy markers, the broadened
        // credential-word set (token/secret/password/passwd/credential/apikey), and
        // the short `key` family as a delimited segment. Covers undelimited, hyphen,
        // and dot forms an exact-segment rule would miss.
        let creds = [
            "npm_config__auth",
            "npm_config__authToken",
            "npm_config__password",
            "npm_config_email",
            "npm_config_//registry.npmjs.org/:_authToken",
            "npm_config_//registry.npmjs.org/:_password",
            "npm_config_//registry.npmjs.org/:_auth",
            "npm_config_password",
            "npm_config_passwd",
            "npm_config_foo_token",
            "npm_config_authtoken",
            "npm_config_my_secret",
            "npm_config_credential",
            "npm_config_apikey",
            "npm_config_api_key",
            "npm_config_signing_key",
            "npm_config_key",
            "npm_config_my-token",
            "npm_config_x.secret.y",
        ];
        let ambient: BTreeMap<String, String> = hints
            .iter()
            .chain(creds.iter())
            .map(|k| (k.to_string(), "v".to_string()))
            .collect();
        let out = curated_baseline_env(&ambient);
        for k in hints {
            assert!(out.contains_key(k), "build hint {k} must pass");
        }
        for k in creds {
            assert!(!out.contains_key(k), "credential {k} must be scrubbed");
        }
    }

    /// The MSVC trio is the one allowlist entry whose absence is SILENT: nub would resolve
    /// Visual Studio, stamp the answer, and the scrub would drop it on the way into the child,
    /// leaving node-gyp back on the COM server an AppContainer cannot activate — with no error
    /// anywhere to say why. `WindowsSDKVersion` additionally has to survive
    /// [`is_credential_env_key`], which rejects a `key`-shaped segment.
    #[test]
    fn the_msvc_trio_reaches_the_jailed_child() {
        for key in ["VCINSTALLDIR", "VSCMD_VER", "WindowsSDKVersion"] {
            assert!(
                build_jail_env_allowed(key),
                "{key} must reach node-gyp or the Visual Studio pre-resolution is inert"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_python_adapter_reaches_the_jailed_child() {
        assert!(
            build_jail_env_allowed("PYTHONPATH"),
            "the Nub-owned Python startup directory must reach confined node-gyp"
        );
    }

    /// `NODE_EXECUTABLE` fails the same silent way, and its symptom is worse than inertness:
    /// the child re-runs nub's version discovery inside the jail, where the only granted
    /// interpreter is the one that already failed to satisfy the pin and every other answer
    /// — `~/.nvm`, nub's store, nodejs.org — is out of reach, so it hard-errors
    /// `ERR_NUB_NODE_PROVISION_FAILED` on a version the host may already have. Withholding it
    /// turns the out-of-jail resolution into a download nothing consumes.
    #[test]
    fn the_pre_resolved_node_reaches_the_jailed_child() {
        assert!(
            build_jail_env_allowed("NODE_EXECUTABLE"),
            "NODE_EXECUTABLE must reach the child or the confined shim re-resolves and fails closed"
        );
    }

    #[test]
    fn os_essential_floor_is_the_libuv_grounded_set() {
        // The exact per-OS floor NAME set — the contract from
        // wiki/research/sandbox-os-essentials-env.md. Windows keeps libuv's three
        // strict-essentials + os.tmpdir()'s TMP + the AppContainer LOCALAPPDATA;
        // POSIX keeps nothing (empty-environ exec starts fine).
        #[cfg(windows)]
        {
            let expected: &[&str] = &["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"];
            assert_eq!(OS_ESSENTIAL_ENV, expected);
        }
        #[cfg(not(windows))]
        assert!(
            OS_ESSENTIAL_ENV.is_empty(),
            "POSIX floor is empty (macOS + Linux start from an empty environ)"
        );
    }

    #[test]
    fn lifecycle_scrubbed_env_is_a_default_deny_allowlist() {
        // The build-jail env posture (D1) is a DEFAULT-DENY allowlist: admit the known
        // build namespace (PATH/HOME/TMPDIR floor, NODE re-invocation, npm_package_*/
        // npm_lifecycle_*/npm_config_* build env, proxy, non-secret toolchain config),
        // WITHHOLD everything else — crucially the general credential family a denylist
        // structurally missed (no credential WORD → passed straight through before).
        let ambient = ambient(&[
            // Lifecycle essentials — must be ADMITTED.
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("TMPDIR", "/tmp"),
            ("NODE", "/n/node"),
            // The jail STAMPS this to keep a dependency's script on vanilla Node; the
            // scrubber dropping it would silently restore augmentation, and with it the
            // unreadable-preload abort under Landlock.
            ("NODE_COMPAT", "1"),
            ("npm_node_execpath", "/n/node"),
            ("INIT_CWD", "/proj"),
            ("npm_package_name", "left-pad"),
            ("npm_lifecycle_event", "install"),
            ("npm_config_registry", "https://r/"),
            ("npm_config_target_arch", "arm64"),
            ("https_proxy", "http://proxy:8080"),
            ("CFLAGS", "-O2"),
            ("PKG_CONFIG_PATH", "/opt/lib/pkgconfig"),
            ("SDKROOT", "/sdk"),
            ("MACOSX_DEPLOYMENT_TARGET", "11.0"),
            ("ProgramFiles(x86)", "C:/Program Files (x86)"),
            // The *_KEY / *_API_KEY family the OLD denylist let through — must be WITHHELD.
            ("OPENAI_API_KEY", "sk-x"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("PRIVATE_KEY", "-----BEGIN"),
            ("SSH_KEY", "ssh-rsa"),
            ("GH_PAT", "ghp_x"),
            ("DATABASE_URL", "postgres://u:p@h/db"),
            // Credential-word / registry-auth family — must be WITHHELD.
            ("NPM_TOKEN", "t"),
            ("GITHUB_TOKEN", "t"),
            ("AWS_SECRET_ACCESS_KEY", "t"),
            ("MY_PASSWORD", "t"),
            ("AUTH_HEADER", "t"),
            ("npm_config_//registry.npmjs.org/:_authToken", "t"),
            // An unrelated ambient var with no build role — DENIED by default-deny.
            ("EDITOR", "vim"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        assert!(p.enforce && p.resolved);
        for kept in [
            "PATH",
            "HOME",
            "TMPDIR",
            "NODE",
            "NODE_COMPAT",
            "npm_node_execpath",
            "INIT_CWD",
            "npm_package_name",
            "npm_lifecycle_event",
            "npm_config_registry",
            "npm_config_target_arch",
            "https_proxy",
            "CFLAGS",
            "PKG_CONFIG_PATH",
            "SDKROOT",
            "MACOSX_DEPLOYMENT_TARGET",
            "ProgramFiles(x86)",
        ] {
            assert!(p.constructed.contains_key(kept), "must admit {kept}");
        }
        for denied in [
            "OPENAI_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "PRIVATE_KEY",
            "SSH_KEY",
            "GH_PAT",
            "DATABASE_URL",
            "NPM_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "MY_PASSWORD",
            "AUTH_HEADER",
            "npm_config_//registry.npmjs.org/:_authToken",
            "EDITOR",
        ] {
            assert!(
                !p.constructed.contains_key(denied),
                "must withhold {denied}"
            );
            assert!(p.withheld.contains(&denied.to_string()));
        }
    }

    /// Nub runs npm lifecycle scripts through cmd.exe on Windows, so the build jail's
    /// ambient snapshot carries cmd's hidden `=C:`/`=ExitCode` per-drive entries. A
    /// `=`-named key cannot round-trip a `KEY=VALUE` block and the backend rejects one
    /// outright, so this pins that every posture builder DROPS them by allowlist —
    /// i.e. the build jail was never able to reach that reject.
    #[test]
    fn env_postures_drop_cmd_exe_shell_positional_keys() {
        let ambient = ambient(&[
            ("=C:", "C:\\Users\\me"),
            ("=D:", "D:\\work"),
            ("=ExitCode", "00000000"),
            ("SystemRoot", "C:/Windows"),
            ("PATH", "C:/Windows/System32"),
        ]);
        for key in ["=C:", "=D:", "=ExitCode"] {
            assert!(!build_jail_env_allowed(key), "build jail must deny {key}");
        }
        let jail = lifecycle_scrubbed_env(&ambient);
        for posture in [
            &jail.constructed,
            &curated_baseline_env(&ambient),
            &strip_all_env(&ambient).constructed,
        ] {
            assert!(
                !posture.keys().any(|k| k.starts_with('=')),
                "no posture may construct a `=`-named key: {posture:?}"
            );
        }
        assert!(
            jail.constructed.contains_key("PATH"),
            "the drop is name-scoped, not a blanket scrub"
        );
    }

    /// The env channel a from-source native compile rides, pinned by name: losing any one
    /// of these breaks an offline in-jail node-gyp build, which no other test would catch.
    #[test]
    fn build_jail_admits_the_node_gyp_compile_channel() {
        let ambient = ambient(&[
            ("PATH", "/tools/node-gyp/.bin:/usr/bin"),
            ("npm_config_node_gyp", "/cache/tools/node-gyp/node-gyp.js"),
            ("npm_config_nodedir", "/nodes/v26.5.0"),
            ("npm_node_execpath", "/nodes/v26.5.0/bin/node"),
            ("NODE", "/shim/node"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        for kept in [
            "PATH",
            "npm_config_node_gyp",
            "npm_config_nodedir",
            "npm_node_execpath",
            "NODE",
        ] {
            assert!(
                p.constructed.contains_key(kept),
                "{kept} is load-bearing for an offline node-gyp compile and must be admitted"
            );
        }
    }

    /// `__NUB_NODE_GYP_EXE` stays WITHHELD, and that is what keeps offline compiles working
    /// — see [`BUILD_JAIL_EXTRA_EXACT`]. The engine's `npm_config_node_gyp` shim only
    /// trampolines back through the PM binary when this var is present; withheld, it falls
    /// back to `node-gyp` on the lifecycle PATH, which the jail does grant. Admitting it
    /// without a matching read grant for the PM binary would turn a working fallback into
    /// an exec of an ungranted path, so this assertion is a tripwire on that pairing, not
    /// incidental coverage of the default-deny shape.
    ///
    /// The names are nub's own (`__NUB_*`, from the embedder profile's
    /// `internal_env_prefix`), NOT the engine's `AUBE_*`: the jail reasons about them by
    /// name, and the embedded engine's brand is never nub's env surface. This crate has
    /// no aube dependency by design, so the spelling is duplicated here rather than
    /// imported — the profile's compile-time assertion in `pm_engine::identity` pins the
    /// other side.
    #[test]
    fn build_jail_withholds_the_node_gyp_trampoline_exe() {
        let ambient = ambient(&[
            ("__NUB_NODE_GYP_EXE", "/usr/local/bin/nub"),
            ("__NUB_NODE_GYP_PROJECT_DIR", "/proj"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        for withheld in ["__NUB_NODE_GYP_EXE", "__NUB_NODE_GYP_PROJECT_DIR"] {
            assert!(
                !p.constructed.contains_key(withheld),
                "{withheld} must stay withheld unless the PM binary is read-granted too"
            );
        }
    }

    #[test]
    fn is_credential_env_key_spares_legit_build_vars() {
        // False-positive guards: an npm build hint whose name embeds a credential word
        // survives (npm_config_* routes to the registry-credential predicate), and a
        // bare `AUTHOR`/`AUTHORS` var is not swept by the `auth`-segment rule.
        for legit in [
            "PATH",
            "NODE_OPTIONS",
            "npm_config_target",
            "npm_config_keytar_binary_host_mirror",
            "npm_package_author",
            "AUTHOR",
            "AUTHORS",
        ] {
            assert!(!is_credential_env_key(legit), "{legit} is not a credential");
        }
        for cred in [
            "NPM_TOKEN",
            "SECRET_VALUE",
            "MY_PASSWORD",
            "AUTH_TOKEN",
            "GITHUB_AUTH",
            "npm_config__authToken",
        ] {
            assert!(is_credential_env_key(cred), "{cred} is a credential");
        }
    }

    #[test]
    fn secure_default_fs_is_positive_project_read_with_private_tmp() {
        let mut homes = homes();
        homes.project = crate::matcher::path::canonicalize_including_nonexistent(&homes.project);
        let ctx = crate::compiler::CompileCtx::new(
            homes.clone(),
            homes.project.clone(),
            crate::compiler::ScopeCapabilities::approved(),
            BTreeMap::new(),
        );
        let policy = crate::compiler::compile(&serde_json::Value::Bool(true), &ctx)
            .expect("secure defaults compile");
        let matcher = crate::matcher::path::PathMatcher::new(&policy.fs.rules);

        assert!(
            policy
                .fs
                .rules
                .entries
                .iter()
                .all(|rule| rule.effect != Effect::Deny),
            "the secure default must emit positive grants only"
        );
        assert_eq!(policy.fs.tmp, crate::policy::TmpMode::Private);
        let project_input = matcher.decide(&homes.project.join("src/input.js"));
        assert_eq!(
            project_input,
            crate::matcher::path::FsDecision {
                effect: Effect::Allow,
                access: crate::policy::FsAccess::Read,
            }
        );
        // Read-only access is the IR's structural no-write representation.
        let project_write_attempt = matcher.decide(&homes.project.join("out/result.js"));
        assert_eq!(
            project_write_attempt,
            crate::matcher::path::FsDecision {
                effect: Effect::Allow,
                access: crate::policy::FsAccess::Read,
            }
        );
        assert_eq!(
            matcher.decide(&homes.home.join(".npmrc")).effect,
            Effect::Deny,
            "a path outside the project remains denied"
        );
    }
}
