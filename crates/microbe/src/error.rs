use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// No transport could be constructed: nothing on the host can speak HTTPS and the crate
    /// was built without in-binary TLS. Carries the list that was tried.
    NoTransport(Vec<&'static str>),
    /// A specific transport exists but failed on a request.
    Transport(String),
    /// The server answered with a non-success status.
    Status {
        url: String,
        status: u16,
    },
    /// The packument could not be parsed.
    Registry {
        name: String,
        detail: String,
    },
    /// No published version satisfies the requested range or tag.
    NoVersion {
        name: String,
        spec: String,
    },
    /// The downloaded tarball does not match `dist.integrity` / `dist.shasum`.
    Integrity {
        name: String,
        version: String,
    },
    /// A tarball entry would escape the package directory.
    UnsafePath(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoTransport(tried) => write!(
                f,
                "no way to reach the registry: tried {} (build with `--features tls` for in-binary TLS)",
                tried.join(", ")
            ),
            Error::Transport(detail) => write!(f, "transport: {detail}"),
            Error::Status { url, status } => write!(f, "HTTP {status} for {url}"),
            Error::Registry { name, detail } => {
                write!(f, "unreadable packument for {name}: {detail}")
            }
            Error::NoVersion { name, spec } => write!(f, "no version of {name} matches {spec:?}"),
            Error::Integrity { name, version } => {
                write!(f, "integrity mismatch for {name}@{version}")
            }
            Error::UnsafePath(p) => write!(f, "tarball entry escapes the package directory: {p}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
