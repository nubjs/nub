//! Pre-resolve Visual Studio outside the jail and hand node-gyp the answer.
//!
//! THE DEFECT. node-gyp finds MSVC by activating the Visual Studio SetupConfiguration COM
//! server (`lib/find-visualstudio.js` → `Find-VisualStudio.cs` →
//! `CLSID {177F0C4A-1CD3-4DE7-A32C-71DBBB9FA36D}`), and an AppContainer cannot activate it:
//! `Retrieving the COM class factory ... 80070005 Access is denied` (run 30522944978, both
//! Windows images). Its second path is dead too — `Get-Module -ListAvailable -Name VSSetup`
//! returns null confined and 2.2.16 unconfined. No filesystem grant repairs either: this is a
//! DCOM activation denial, not a DACL, and launch permission for an AppContainer SID is
//! machine-wide and privileged — disqualified by the jail's zero-privilege contract.
//!
//! WORSE THAN A FAILURE: it is SILENT. PowerShell exits 0 with empty stdout, so node-gyp's
//! `execFile` reports success and `parseData` fails only in `JSON.parse`. On Node >= 22 every
//! remaining check is version-gated off and `findVisualStudio` throws
//! `Could not find any Visual Studio installation to use` — clean. On Node 18 it is not: the
//! VS2015 registry probe (`findVisualStudio2015`, gated `nodeSemver.major >= 19`) still runs
//! and would select a VS2015 toolset if one is installed.
//!
//! THE FIX, and it is the shape this codebase already uses twice — `npm_config_nodedir`
//! prefetches headers the confined node-gyp cannot fetch, `npm_config_python` names an
//! interpreter it cannot search for. Here nub resolves VS in the UNCONFINED parent, where the
//! COM server works, and stamps the three variables `findVisualStudio` short-circuits on
//! before it probes anything:
//!
//! ```text
//! VCINSTALLDIR      => envVcInstallDir = path.resolve(VCINSTALLDIR, '..')  -- the VS ROOT
//! VSCMD_VER         => info.version, whose MAJOR maps to the version year (17 => 2022)
//! WindowsSDKVersion => the sole source of the Windows10SDK package, hence of info.sdk
//! ```
//!
//! ALL THREE OR NOTHING, because a partial stamp is the silent-wrong-answer shape again.
//! `VCINSTALLDIR` alone sets `envVcInstallDir`, which makes `checkConfigVersion` reject every
//! installation whose path differs while supplying no version or SDK of its own — node-gyp
//! then proceeds on an assumption nub half-made. So [`accept`] re-runs node-gyp's OWN
//! acceptance predicates in Rust (the version parse, the supported-year map, the SDK regex,
//! the `existsSync` on MSBuild) and stamps only a trio that all of them accept.
//!
//! WHERE THE VALUES COME FROM: `vswhere.exe` for the installation, then Microsoft's own
//! `vcvarsall.bat` for the variables. Deriving them by hand is the tempting alternative and
//! the wrong one — every value is a spelling node-gyp parses (`VCINSTALLDIR` names the `VC`
//! SUBDIRECTORY, because node-gyp takes its parent), and the SDK version has no honest source
//! but the SDK layout. Letting the toolchain's own script compute them removes that whole
//! class of bug, and it is the same block a developer command prompt would have exported.
//!
//! WHAT IT DOES NOT DO. It does not stamp `INCLUDE`/`LIB`/`LIBPATH`, which `vcvarsall` also
//! sets: MSBuild derives those from the toolset props, an unconfined `npm install` on a plain
//! machine has none either, and injecting them would make the jail's builds diverge from the
//! builds it is meant to reproduce. It does not touch gyp, which needs nothing — node-gyp
//! exports `GYP_MSVS_OVERRIDE_PATH`/`GYP_MSVS_VERSION` from this same result and
//! `MSVSVersion.py` short-circuits on them rather than re-detecting.

// Windows-only in production, but COMPILED EVERYWHERE on purpose, for the same reason
// `jail_bin` is: the derivation is where the bugs live, and it is ordinary string logic whose
// tests should run on the dev host rather than only on a Windows runner.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The env names node-gyp's short-circuit reads. Stamped as a unit; see the module doc.
pub(super) const ENV_KEYS: [&str; 3] = ["VCINSTALLDIR", "VSCMD_VER", "WindowsSDKVersion"];

/// A Visual Studio installation node-gyp will accept, and the read subtrees that make it
/// usable inside the jail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MsvcToolchain {
    /// `VCINSTALLDIR` — the `VC` SUBDIRECTORY, not the root. node-gyp takes its parent.
    vc_install_dir: String,
    /// `VSCMD_VER` — the installation version; its major is what maps to the version year.
    vscmd_ver: String,
    /// `WindowsSDKVersion` — the only thing `getSDK` can derive an SDK from.
    sdk_version: String,
    /// Read grants: the VS root and the Windows SDK root. Both are commonly REDUNDANT — the
    /// backend skips a grant a path already publishes to `ALL APPLICATION PACKAGES`
    /// inheritably, which every default `%ProgramFiles%` install does — so the cost is paid
    /// only by an off-default layout such as `C:\BuildTools`.
    pub(super) reads: Vec<PathBuf>,
}

impl MsvcToolchain {
    /// Write the trio into the child env, overwriting whatever was there. Overwrite rather
    /// than set-if-absent is deliberate and only reachable when the ambient trio was
    /// INCOMPLETE (see [`resolve`]): a partial trio cannot succeed, so replacing it with a
    /// consistent one is the outcome that lets the package build.
    pub(super) fn stamp(&self, ambient: &mut BTreeMap<String, String>) {
        for (key, value) in
            ENV_KEYS
                .iter()
                .zip([&self.vc_install_dir, &self.vscmd_ver, &self.sdk_version])
        {
            ambient.insert((*key).to_string(), value.clone());
        }
    }
}

/// Resolve the toolchain to stamp, or `None` to leave the spawn byte-identical to what it
/// would have been.
///
/// Declines when the ambient env ALREADY carries a complete trio: that is a developer command
/// prompt, whose choice of installation is deliberate and is exactly what node-gyp is designed
/// to honour. An INCOMPLETE ambient trio is not honoured — it cannot work — and is replaced.
pub(super) fn resolve(
    ambient: &BTreeMap<String, String>,
    spawn: &aube_util::LifecycleSandboxSpawn,
    probe: &super::build_jail::ProbeScope,
) -> Option<MsvcToolchain> {
    // THE SEAM, and it exists for the same reason as the backend's two: a green repaired arm
    // measures nothing without an arm where the defect still reproduces, and rebuilding the
    // binary twice does not fit a bounded Windows job. It only ever WITHHOLDS a stamp and a
    // grant, so it cannot widen anything.
    if std::env::var_os("NUB_SANDBOX_WIN_NO_MSVC_INJECT").is_some() {
        return None;
    }
    // Same gate, and the same reason, as the Python pre-resolution: resolving spends two
    // process spawns, and only a package node-gyp will configure can spend them usefully.
    // SEARCHED, not root-only, for the reason `has_gyp_manifest` documents: `ssh2` keeps its
    // manifest at `lib/protocol/crypto/binding.gyp`, and a root-only gate reads that as "no
    // native build here" — which is exactly the miss that cost the Python grant a lane.
    if !super::build_jail::has_gyp_manifest(
        &spawn.package_dir,
        super::build_jail::GYP_MANIFEST_SEARCH_DEPTH,
    ) {
        return None;
    }
    if ENV_KEYS.iter().all(|key| lookup(ambient, key).is_some()) {
        return None;
    }
    // `None` means the machine genuinely has no Visual Studio, so node-gyp's own report of
    // that is true and there is nothing to translate.
    let toolchain = cached()?;
    // Re-gated per call rather than inside the cache, because the scope is per-package while
    // the machine's Visual Studio is not. A path under the project or any `node_modules` is
    // not a toolchain nub will name or grant, however it was arrived at.
    if !toolchain.reads.iter().all(|read| probe.allows(read)) {
        warn_toolchain_will_look_absent();
        return None;
    }
    Some(toolchain)
}

/// Say out loud that the "no Visual Studio" node-gyp is about to print is nub's doing.
///
/// THE ONLY STATE THIS FIRES IN is: the machine HAS an installation nub resolved out of jail,
/// and nub withheld the stamp anyway. node-gyp then falls back to discovery routes an
/// AppContainer cannot complete — the COM server is denied outright, and the PowerShell route
/// compiles a C# assembly into `%TEMP%`, which is measurably where a jailed node-gyp reports
/// `find VS could not use PowerShell to find Visual Studio 2017 or newer` and
/// `Could not find any Visual Studio installation to use`. That message names a broken
/// toolchain and the toolchain is fine; without this line the reader spends the afternoon on
/// their VS install. Once per process, because the resolution is a property of the machine.
fn warn_toolchain_will_look_absent() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        super::present::warn(
            "warning: a Visual Studio installation is present but outside what the build \
             sandbox may name, so native builds will report that no Visual Studio was found. \
             The toolchain is not the problem.",
        );
    });
}

/// The resolution is a property of the MACHINE, so it is computed once per process and reused
/// by every package in the install — including the negative answer, which costs the same two
/// spawns to re-derive and cannot change underneath a running install.
fn cached() -> Option<MsvcToolchain> {
    static RESOLVED: std::sync::OnceLock<Option<MsvcToolchain>> = std::sync::OnceLock::new();
    RESOLVED.get_or_init(harvest).clone()
}

#[cfg(not(windows))]
fn harvest() -> Option<MsvcToolchain> {
    None
}

/// Ask `vswhere` for the installation, then ask that installation's own `vcvarsall.bat` for
/// the variables, and keep the answer only if [`accept`] agrees node-gyp would.
#[cfg(windows)]
fn harvest() -> Option<MsvcToolchain> {
    use std::os::windows::process::CommandExt;

    let vswhere = vswhere_path()?;
    // `-requiresAny` with both toolsets, rather than the x64 component alone: an ARM64-only
    // installation is a real configuration and rejecting it here would send node-gyp back to
    // the COM path that cannot answer.
    let mut found = std::process::Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "Microsoft.VisualStudio.Component.VC.Tools.ARM64",
            "-requiresAny",
            "-property",
            "installationPath",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let install = first_line(&super::build_jail::read_bounded(
        &mut found,
        RESOLVE_TIMEOUT,
    )?)?;

    let vcvarsall = Path::new(&install).join(r"VC\Auxiliary\Build\vcvarsall.bat");
    if !vcvarsall.is_file() {
        return None;
    }
    // `raw_arg`, not `arg`: Rust quotes arguments by the `CommandLineToArgvW` rules, which
    // `cmd.exe` does not implement — the same mismatch the lifecycle spawn's
    // `WindowsVerbatim` path exists for. The line is built by [`vcvars_command_line`], which
    // is unit-tested against its exact rendering.
    let mut vars = std::process::Command::new("cmd.exe")
        .raw_arg(vcvars_command_line(
            &vcvarsall.to_string_lossy(),
            VCVARS_ARCH,
        ))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let block = super::build_jail::read_bounded(&mut vars, RESOLVE_TIMEOUT)?;
    accept(
        &parse_set_block(&String::from_utf8_lossy(&block)),
        &|p: &Path| p.is_file(),
    )
}

/// `vswhere` ships with the Visual Studio INSTALLER, at a fixed location independent of where
/// Visual Studio itself was placed — which is what makes it reachable for an installation at
/// `C:\BuildTools` that no `%ProgramFiles%` search would find. The 32-bit Program Files is the
/// documented home; the 64-bit one is checked only so a future relocation does not silently
/// disable this.
#[cfg(windows)]
fn vswhere_path() -> Option<PathBuf> {
    ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|dir| PathBuf::from(dir).join(r"Microsoft Visual Studio\Installer\vswhere.exe"))
        .find(|p| p.is_file())
}

/// Both spawns print a handful of lines and exit. Generous, but bounded: this runs outside the
/// jail and outside the monitor that reaps it, so a script that never exits would wedge the
/// install with nothing to collect it.
const RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The architecture argument only has to make `vcvarsall.bat` SUCCEED — none of the three
/// variables harvested from it encodes an architecture, and MSBuild takes its platform from
/// the `.vcxproj` that node-gyp generates. The host's own is therefore the right choice: it is
/// the one toolset an installation is certain to carry, where a cross toolset may be absent.
const VCVARS_ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "x64"
};

/// `cmd /c "<compound>"`, with the batch path quoted inside it.
///
/// `cmd` strips the FIRST and LAST quote of a `/c` argument that begins with one, so the outer
/// pair is what protects a batch path containing spaces (`C:\Program Files\...`) — the common
/// case, not an edge one. The batch script's own banner goes to `nul` so the only thing on
/// stdout is `set`'s output.
fn vcvars_command_line(bat: &str, arch: &str) -> String {
    format!("/c \"\"{bat}\" {arch} >nul && set\"")
}

fn first_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// `set`'s output, one `KEY=VALUE` per line. A value may contain `=`; a key may not.
fn parse_set_block(block: &str) -> BTreeMap<String, String> {
    block
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim_end_matches(['\r']).to_string()))
        .collect()
}

/// Windows env names fold ASCII case, and the harvested block carries whatever casing the
/// batch script used.
fn lookup<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a String> {
    env.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// Decide whether node-gyp would accept this environment, by running its own predicates.
///
/// Every rejection here is a case where stamping would have made `findVisualStudio` proceed on
/// a premise nub supplied and could not honour. `exists` is injected so the `existsSync` gate
/// — the one predicate that consults the filesystem — is exercised on any host.
fn accept(
    vars: &BTreeMap<String, String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<MsvcToolchain> {
    let vc_install_dir = lookup(vars, "VCINSTALLDIR")?.trim().to_string();
    let vscmd_ver = lookup(vars, "VSCMD_VER")?.trim().to_string();
    let sdk_version = lookup(vars, "WindowsSDKVersion")?.trim().to_string();
    if vc_install_dir.is_empty() {
        return None;
    }

    // `getVersionInfo`: `/^(\d+)\.(\d+)(?:\..*)?/`, then the major-to-year map. Only the years
    // `findVisualStudio2019OrNewerFromSpecifiedLocation` supports are accepted — 15/2017 is
    // reachable only below Node 22 and its MSBuild layout differs, so it declines to the
    // pre-existing behaviour instead of stamping a shape this has never measured.
    if !matches!(major_minor(&vscmd_ver)?.0, 16..=18) {
        return None;
    }

    // `findVSFromSpecifiedLocation`: `/^(\d+)\.(\d+)\.(\d+)\..*/` on `WindowsSDKVersion` is the
    // ONLY source of a `Windows10SDK.<n>.Desktop` package, and `getSDK` returning null makes
    // `processData` skip the installation entirely.
    if !sdk_version_parses(&sdk_version) {
        return None;
    }

    // node-gyp reads the VS root as `path.win32.resolve(VCINSTALLDIR, '..')`.
    let root = windows_parent(&vc_install_dir)?;

    // `getMSBuild`: for 2022 and 2026 both branches fall through to an `existsSync`, so a stamp
    // whose MSBuild is missing OR UNREADABLE FROM INSIDE THE JAIL yields `msBuild: null` and the
    // installation is skipped. 2019 returns its path unchecked, but a stamp naming an absent
    // MSBuild only defers the failure to the build, so it is checked the same way.
    //
    // The arm64 spelling is a candidate ONLY on arm64, which is node-gyp's rule and not an
    // accident of ordering: on x64 it consults the plain path alone, so accepting the arm64 one
    // there would stamp a trio node-gyp then rejects. It reads `process.arch` of the confined
    // Node while this reads nub's own — the same proxy [`VCVARS_ARCH`] makes, and wrong only on
    // a cross-architecture pairing that the interpreter staging does not produce.
    let mut candidates = vec![r"MSBuild\Current\Bin\MSBuild.exe"];
    if cfg!(target_arch = "aarch64") {
        candidates.insert(0, r"MSBuild\Current\Bin\arm64\MSBuild.exe");
    }
    if !candidates
        .iter()
        .any(|rel| exists(&PathBuf::from(format!(r"{root}\{rel}"))))
    {
        return None;
    }

    let mut reads = vec![PathBuf::from(&root)];
    // The SDK lives OUTSIDE the Visual Studio installation, so its root is a second grant.
    // `vcvarsall` reports it, which is the only reason this needs no registry read.
    if let Some(sdk_dir) = lookup(vars, "WindowsSdkDir").map(|d| d.trim_end_matches(['\\', '/']))
        && !sdk_dir.is_empty()
    {
        reads.push(PathBuf::from(sdk_dir));
    }
    Some(MsvcToolchain {
        vc_install_dir,
        vscmd_ver,
        sdk_version,
        reads,
    })
}

/// The leading `<major>.<minor>` of a version string, per node-gyp's `getVersionInfo` regex.
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let (major, rest) = split_digits(version)?;
    let (minor, _) = split_digits(rest.strip_prefix('.')?)?;
    Some((major, minor))
}

/// `/^(\d+)\.(\d+)\.(\d+)\./` — three numeric segments and a fourth dot. A trailing separator
/// (`10.0.26100.0\`, which is what a developer command prompt exports) is immaterial to it.
fn sdk_version_parses(version: &str) -> bool {
    let mut rest = version;
    for _ in 0..3 {
        let Some((_, tail)) = split_digits(rest) else {
            return false;
        };
        let Some(tail) = tail.strip_prefix('.') else {
            return false;
        };
        rest = tail;
    }
    true
}

fn split_digits(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    text[..end].parse().ok().map(|n| (n, &text[end..]))
}

/// `path.win32.resolve(value, '..')`, done on the string so the derivation is identical on
/// every host rather than only where `\` is a separator. Trailing separators are dropped
/// first: `VCINSTALLDIR` is exported WITH one, and treating it as a component would return the
/// directory itself and silently name `VC` as the Visual Studio root.
fn windows_parent(value: &str) -> Option<String> {
    let trimmed = value.trim_end_matches(['\\', '/']);
    let cut = trimmed.rfind(['\\', '/'])?;
    let parent = &trimmed[..cut];
    // A drive-relative root (`C:`) is not a directory; a bare `C:\VC` has no usable parent.
    (!parent.is_empty() && !parent.ends_with(':')).then(|| parent.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verbatim `set` excerpt from a Visual Studio 2022 developer command prompt.
    fn vs2022() -> BTreeMap<String, String> {
        parse_set_block(concat!(
            "VCINSTALLDIR=C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\VC\\\r\n",
            "VSCMD_ARG_app_plat=Desktop\r\n",
            "VSCMD_VER=17.14.7\r\n",
            "WindowsSDKVersion=10.0.26100.0\\\r\n",
            "WindowsSdkDir=C:\\Program Files (x86)\\Windows Kits\\10\\\r\n",
        ))
    }

    #[test]
    fn accepts_a_developer_prompt_block_and_grants_both_toolchain_roots() {
        let vs = accept(&vs2022(), &|_| true).expect("a real VS2022 block is acceptable");
        // VCINSTALLDIR is passed through UNCHANGED, trailing separator included: node-gyp
        // takes its parent, so stamping the root itself would name the 2022 directory.
        assert_eq!(
            vs.vc_install_dir,
            "C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\VC\\"
        );
        assert_eq!(vs.sdk_version, "10.0.26100.0\\");
        assert_eq!(
            vs.reads,
            vec![
                PathBuf::from("C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise"),
                PathBuf::from("C:\\Program Files (x86)\\Windows Kits\\10"),
            ]
        );

        let mut env = BTreeMap::new();
        vs.stamp(&mut env);
        assert_eq!(env.len(), 3, "the trio is stamped as a unit");
        assert_eq!(env["VSCMD_VER"], "17.14.7");
    }

    /// The MSBuild gate is the one predicate that reads the filesystem, and node-gyp runs it
    /// from INSIDE the jail — so this covers both an absent MSBuild and one the read grant
    /// fails to expose.
    #[test]
    fn declines_when_node_gyp_would_not_find_msbuild() {
        assert_eq!(accept(&vs2022(), &|_| false), None);

        let seen = std::cell::RefCell::new(Vec::new());
        accept(&vs2022(), &|p| {
            seen.borrow_mut().push(p.to_string_lossy().into_owned());
            false
        });
        let probed = seen.borrow();
        let root =
            "C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\MSBuild\\Current\\Bin";
        assert!(
            probed.contains(&format!("{root}\\MSBuild.exe")),
            "the plain spelling is node-gyp's only candidate on x64: {probed:?}"
        );
        // node-gyp consults the arm64 spelling only when `process.arch` is arm64. Probing it on
        // x64 would stamp a trio node-gyp then rejects, so the asymmetry is the assertion.
        assert_eq!(
            probed.contains(&format!("{root}\\arm64\\MSBuild.exe")),
            cfg!(target_arch = "aarch64"),
            "{probed:?}"
        );
    }

    /// Each rejection is a stamp that would have made `findVisualStudio` proceed on a premise
    /// nub supplied and could not honour — the partial-injection failure the module exists to
    /// prevent. A missing key stands for the whole family; the rest are malformed values that
    /// a present-but-unusable variable would produce.
    #[test]
    fn declines_every_trio_node_gyp_would_reject() {
        for (label, key, value) in [
            ("absent version", "VSCMD_VER", None),
            ("unparseable version", "VSCMD_VER", Some("Dev17")),
            ("no minor", "VSCMD_VER", Some("17")),
            ("unsupported year", "VSCMD_VER", Some("15.9.1")),
            ("absent sdk", "WindowsSDKVersion", None),
            ("three-segment sdk", "WindowsSDKVersion", Some("10.0.26100")),
            (
                "non-numeric sdk",
                "WindowsSDKVersion",
                Some("10.0.latest.0"),
            ),
            ("empty install dir", "VCINSTALLDIR", Some("")),
            ("rootless install dir", "VCINSTALLDIR", Some("VC")),
        ] {
            let mut vars = vs2022();
            match value {
                Some(v) => vars.insert(key.to_string(), v.to_string()),
                None => vars.remove(key),
            };
            assert_eq!(
                accept(&vars, &|_| true),
                None,
                "{label} must not be stamped"
            );
        }
    }

    #[test]
    fn vs_root_is_the_parent_whether_or_not_a_separator_trails() {
        assert_eq!(
            windows_parent("C:\\BuildTools\\VC\\").as_deref(),
            Some("C:\\BuildTools")
        );
        assert_eq!(
            windows_parent("C:\\BuildTools\\VC").as_deref(),
            Some("C:\\BuildTools")
        );
        assert_eq!(windows_parent("C:\\VC\\"), None, "a drive is not a VS root");
    }

    /// Asserted against the exact rendering because this string crosses into `cmd.exe`, which
    /// does not implement the argument rules Rust would otherwise apply — and because an
    /// over-escaped literal reaching a batch file is a bug this effort has already shipped
    /// once.
    #[test]
    fn vcvars_line_quotes_the_batch_path_inside_the_outer_pair() {
        assert_eq!(
            vcvars_command_line("C:\\Program Files\\MSVC\\vcvarsall.bat", "x64"),
            "/c \"\"C:\\Program Files\\MSVC\\vcvarsall.bat\" x64 >nul && set\""
        );
    }
}
