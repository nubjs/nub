//! The cross-platform matcher: turns the resolved IR's glob/host/CIDR strings
//! into runtime match decisions. Two axes with different shapes — fs paths
//! (`path`) and net hosts (`host`) — share this module so their normalization
//! rules stay in one place.

pub mod host;
pub mod path;

pub use host::HostMatcher;
pub use path::{
    Homes, PathMatcher, canonicalize_glob_prefix, canonicalize_including_nonexistent,
    expand_symbolic,
};
