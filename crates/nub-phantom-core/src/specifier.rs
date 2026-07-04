//! Classify an import/require specifier string into its shape, and extract the
//! *package name* a bare specifier resolves to.
//!
//! The package name is the unit the classifier reasons about: `lodash/fp` and
//! `lodash/get` both depend on the package `lodash`; `@babel/core/lib/x` depends
//! on `@babel/core`. A phantom is decided per package name, not per subpath.

/// What kind of thing an import specifier points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecKind {
    /// A relative or absolute path (`./x`, `../y`, `/abs`) — an intra-package
    /// module edge. Followed during the graph walk; never a dependency.
    Relative,
    /// A subpath-`imports` reference (`#internal`) — resolved against the
    /// package's own `imports` map; a self reference, never a dependency.
    ImportsHash,
    /// Not an installable npm package: a protocol URL (`http:`/`data:`/`file:`),
    /// a framework virtual module (`$app`, `$env`), a bundler template placeholder
    /// (`{{storiesFilename}}`), or a Node internal (`_http_common`). None can be a
    /// phantom, so they are dropped rather than counted.
    NonPackage,
    /// A bare specifier naming an external package. Carries the resolved package
    /// name (`@scope/name` or `name`).
    Bare(String),
}

/// Classify a specifier string.
pub fn classify(spec: &str) -> SpecKind {
    if spec.is_empty() {
        // Defensive: an empty specifier is not a package edge.
        return SpecKind::Relative;
    }
    if spec.starts_with('#') {
        return SpecKind::ImportsHash;
    }
    if spec.starts_with('.') || spec.starts_with('/') {
        return SpecKind::Relative;
    }
    // A `node:` specifier is a builtin, not a URL — keep it as a bare reference so
    // the classifier records it as a builtin (never a phantom).
    if spec.starts_with("node:") {
        return SpecKind::Bare(spec.to_string());
    }
    // A protocol specifier has a scheme before `:`. Any remaining `scheme:` is a
    // URL-ish non-package (http/https/data/file/…).
    if let Some(idx) = spec.find(':') {
        // Windows-drive-letter paths (`C:\`) would also match, but tarball
        // specifiers are POSIX; treat any pre-`:` scheme as a URL.
        if idx > 0 && spec[..idx].bytes().all(|b| b.is_ascii_alphabetic()) {
            return SpecKind::NonPackage;
        }
    }
    let name = package_name(spec);
    if !is_npm_name(&name) {
        return SpecKind::NonPackage;
    }
    SpecKind::Bare(name)
}

/// Could `name` be an installable npm package name? A reference whose head cannot
/// be one is a framework virtual (`$app`), a bundler/AMD placeholder or virtual id
/// (`{{x}}`, `env!env`, `tanstack-manifest:v`), or another runtime's internal
/// (`_http_common`) — never a phantom. The npm grammar allows only
/// `[A-Za-z0-9._~-]` in the name (plus `@`/`/` for a scope) and forbids a leading
/// `_`/`.` (`$` never appears). Legacy uppercase (`JSONStream`) is allowed; any
/// out-of-grammar character (`:`/`!`/`{`/whitespace/…) rejects. Deliberately
/// permissive on the character SET so no real dependency is dropped.
fn is_npm_name(name: &str) -> bool {
    let head = name.strip_prefix('@').unwrap_or(name);
    let Some(first) = head.chars().next() else {
        return false;
    };
    if matches!(first, '_' | '.' | '$') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '@' | '/'))
}

/// Extract the package name from a bare specifier.
///
/// `@scope/name/sub` → `@scope/name`; `name/sub` → `name`; `name` → `name`.
fn package_name(spec: &str) -> String {
    if let Some(scoped) = spec.strip_prefix('@') {
        // Scoped: keep `@scope/name` (first two segments).
        let mut parts = scoped.splitn(3, '/');
        let scope = parts.next().unwrap_or("");
        match parts.next() {
            Some(name) => format!("@{scope}/{name}"),
            // `@scope` with no name is malformed; return as-is so it can't be
            // silently treated as some other package.
            None => format!("@{scope}"),
        }
    } else {
        spec.split('/').next().unwrap_or(spec).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{SpecKind, classify};

    #[test]
    fn extracts_package_name_scoped_and_unscoped_with_subpaths() {
        assert_eq!(classify("lodash"), SpecKind::Bare("lodash".into()));
        assert_eq!(classify("lodash/fp"), SpecKind::Bare("lodash".into()));
        assert_eq!(
            classify("@babel/core"),
            SpecKind::Bare("@babel/core".into())
        );
        assert_eq!(
            classify("@babel/core/lib/index.js"),
            SpecKind::Bare("@babel/core".into())
        );
    }

    #[test]
    fn separates_relative_hash_and_url_from_bare() {
        assert_eq!(classify("./local"), SpecKind::Relative);
        assert_eq!(classify("../up"), SpecKind::Relative);
        assert_eq!(classify("#internal/x"), SpecKind::ImportsHash);
        assert_eq!(classify("https://esm.sh/x"), SpecKind::NonPackage);
        assert_eq!(classify("data:text/js,1"), SpecKind::NonPackage);
    }

    #[test]
    fn rejects_non_npm_names_virtuals_placeholders_internals() {
        // SvelteKit virtual, Storybook template placeholder, Node internal — none
        // is an installable npm package, so none is a phantom.
        assert_eq!(classify("$app/environment"), SpecKind::NonPackage);
        assert_eq!(classify("{{storiesFilename}}"), SpecKind::NonPackage);
        assert_eq!(classify("_http_common"), SpecKind::NonPackage);
        assert_eq!(classify("env!env"), SpecKind::NonPackage); // AMD plugin id
        assert_eq!(classify("tanstack-manifest:v"), SpecKind::NonPackage); // virtual id
        // A legacy uppercase name is still a real package.
        assert_eq!(classify("JSONStream"), SpecKind::Bare("JSONStream".into()));
    }
}
