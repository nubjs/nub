mod checksum;
mod dep_path;
mod format;
mod raw;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use checksum::{package_extensions_checksum, pnpmfile_checksum};
pub use read::parse;
pub use write::write;

pub(super) fn tarball_url_is_hosted_git(url: &str) -> bool {
    let Some((host, path)) = http_url_host_and_path(url) else {
        return false;
    };
    match host.as_str() {
        "codeload.github.com" | "npm.pkg.github.com" => true,
        "gitlab.com" => path.contains("/-/archive/"),
        "bitbucket.org" => path.contains("/get/"),
        _ => false,
    }
}

fn http_url_host_and_path(url: &str) -> Option<(String, &str)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let before_query = rest.split_once('?').map_or(rest, |(before, _)| before);
    let before_fragment = before_query
        .split_once('#')
        .map_or(before_query, |(before, _)| before);
    let (authority, path) = before_fragment
        .split_once('/')
        .unwrap_or((before_fragment, ""));
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host_port
        .split_once(':')
        .map_or(host_port, |(host, _)| host)
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some((host, path))
    }
}
