//! Host platform detection and the two naming vocabularies it maps
//! into:
//!
//! - **lockfile vocabulary** (pnpm / Node `process.platform`):
//!   `darwin` / `linux` / `win32` / `freebsd`, `x64` / `arm64`,
//!   `libc: musl`;
//! - **dist-file vocabulary** (nodejs.org artifact names):
//!   `node-v{V}-darwin-arm64.tar.gz`, `node-v{V}-linux-x64-musl.tar.gz`,
//!   `node-v{V}-win-x64.zip`.

use crate::error::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `process.platform` vocabulary: `darwin` / `linux` / `win32`.
    pub os: String,
    /// `process.arch` vocabulary: `x64` / `arm64` (others passed
    /// through as-is from Rust's target arch mapping).
    pub cpu: String,
    /// `Some("musl")` on musl-libc Linux hosts.
    pub libc: Option<String>,
}

impl Platform {
    /// Detect the host platform.
    ///
    /// musl detection is a *runtime* check: aube ships static-musl
    /// Linux binaries, so `cfg!(target_env = "musl")` is true even on
    /// glibc hosts and cannot be trusted. The presence of musl's
    /// dynamic loader (`/lib/ld-musl-<arch>.so.1`) is the signal mise
    /// uses for the same decision.
    pub fn current() -> Result<Platform, Error> {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            "windows" => "win32",
            // FreeBSD reports Node's `process.platform` value verbatim.
            // nodejs.org ships no FreeBSD artifacts, so the download
            // path can't satisfy a request here — but resolution reaches
            // it only after the zero-network fast paths (PATH probe,
            // installed scan, mise delegation) miss, and a missing dist
            // then surfaces the normal `NoMatchingVersion` /
            // `UnsupportedPlatform` diagnostic. Detecting the platform
            // (rather than erroring outright) is what lets those local
            // paths, `os`-field package filtering, and multi-platform
            // lockfile pins behave correctly on FreeBSD.
            "freebsd" => "freebsd",
            other => {
                return Err(Error::UnsupportedPlatform {
                    platform: format!("{other}-{}", std::env::consts::ARCH),
                });
            }
        };
        let cpu = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "x86",
            "powerpc64" => "ppc64",
            "s390x" => "s390x",
            other => other,
        };
        let libc = (os == "linux" && detect_musl()).then(|| "musl".to_string());
        Ok(Platform {
            os: os.to_string(),
            cpu: cpu.to_string(),
            libc,
        })
    }

    /// The platform segment of a dist artifact name:
    /// `darwin-arm64`, `linux-x64-musl`, `win-x64`.
    pub fn dist_slug(&self) -> String {
        let os = if self.os == "win32" { "win" } else { &self.os };
        let musl = if self.libc.as_deref() == Some("musl") {
            "-musl"
        } else {
            ""
        };
        format!("{os}-{}{musl}", self.cpu)
    }

    /// The token nodejs.org's `index.json` `files[]` array uses for
    /// this platform. macOS entries use the legacy `osx-*` prefix
    /// with a `-tar` suffix; Windows uses `win-<arch>-zip`.
    ///
    /// musl builds never appear in the official index (they live on
    /// unofficial-builds.nodejs.org, whose index has the same shape
    /// but plain `linux-x64` tokens), so musl maps to the bare linux
    /// token for `files[]` gating purposes.
    pub fn index_files_token(&self) -> String {
        match self.os.as_str() {
            "darwin" => format!("osx-{}-tar", self.cpu),
            "win32" => format!("win-{}-zip", self.cpu),
            _ => format!("linux-{}", self.cpu),
        }
    }

    /// Archive extension for this platform's dist artifact.
    pub fn archive_ext(&self) -> &'static str {
        if self.os == "win32" { "zip" } else { "tar.gz" }
    }

    /// pnpm's `archive:` vocabulary for this platform.
    pub fn archive_kind(&self) -> &'static str {
        if self.os == "win32" { "zip" } else { "tarball" }
    }

    /// Human-readable label for error messages.
    pub fn label(&self) -> String {
        match &self.libc {
            Some(libc) => format!("{}-{} ({libc})", self.os, self.cpu),
            None => format!("{}-{}", self.os, self.cpu),
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_musl() -> bool {
    // Rust's arch names match musl's loader names for every
    // architecture Node ships (x86_64, aarch64), so the constant is
    // used verbatim.
    std::path::Path::new(&format!("/lib/ld-musl-{}.so.1", std::env::consts::ARCH)).exists()
}

#[cfg(not(target_os = "linux"))]
fn detect_musl() -> bool {
    false
}

/// The artifact filename for `version` on `platform`:
/// `node-v22.1.0-darwin-arm64.tar.gz`.
pub fn artifact_filename(version: &node_semver::Version, platform: &Platform) -> String {
    format!(
        "node-v{version}-{}.{}",
        platform.dist_slug(),
        platform.archive_ext()
    )
}

/// The top-level directory inside an artifact:
/// `node-v22.1.0-darwin-arm64`.
pub fn artifact_top_dir(version: &node_semver::Version, platform: &Platform) -> String {
    format!("node-v{version}-{}", platform.dist_slug())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plat(os: &str, cpu: &str, libc: Option<&str>) -> Platform {
        Platform {
            os: os.into(),
            cpu: cpu.into(),
            libc: libc.map(String::from),
        }
    }

    #[test]
    fn dist_slugs() {
        assert_eq!(plat("darwin", "arm64", None).dist_slug(), "darwin-arm64");
        assert_eq!(plat("linux", "x64", None).dist_slug(), "linux-x64");
        assert_eq!(
            plat("linux", "x64", Some("musl")).dist_slug(),
            "linux-x64-musl"
        );
        assert_eq!(plat("win32", "x64", None).dist_slug(), "win-x64");
        assert_eq!(plat("freebsd", "x64", None).dist_slug(), "freebsd-x64");
    }

    #[test]
    fn freebsd_token_falls_back_to_linux() {
        // nodejs.org has no FreeBSD `files[]` token, so availability
        // gating reuses the linux token. Download still fails later at
        // the SHASUMS lookup (no `node-v*-freebsd-*` artifact), which is
        // the intended graceful outcome — FreeBSD relies on system/mise
        // Node, not aube's self-download.
        assert_eq!(
            plat("freebsd", "x64", None).index_files_token(),
            "linux-x64"
        );
        assert_eq!(plat("freebsd", "x64", None).archive_ext(), "tar.gz");
    }

    #[test]
    fn index_tokens() {
        assert_eq!(
            plat("darwin", "arm64", None).index_files_token(),
            "osx-arm64-tar"
        );
        assert_eq!(
            plat("win32", "x64", None).index_files_token(),
            "win-x64-zip"
        );
        assert_eq!(
            plat("linux", "arm64", None).index_files_token(),
            "linux-arm64"
        );
        assert_eq!(
            plat("linux", "x64", Some("musl")).index_files_token(),
            "linux-x64"
        );
    }

    #[test]
    fn artifact_names() {
        let v: node_semver::Version = "22.1.0".parse().unwrap();
        assert_eq!(
            artifact_filename(&v, &plat("win32", "x64", None)),
            "node-v22.1.0-win-x64.zip"
        );
        assert_eq!(
            artifact_top_dir(&v, &plat("darwin", "arm64", None)),
            "node-v22.1.0-darwin-arm64"
        );
    }
}
