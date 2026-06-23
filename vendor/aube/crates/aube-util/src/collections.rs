//! Hash-map type aliases backed by foldhash. Better avalanche than
//! FxHash on integer-suffixed string keys (the shape aube's dep_path
//! peer-context suffixes take). All inputs come from locked manifest
//! data so FixedState (no random seed) is fine, no DoS surface.

/// Drop-in for `std::collections::HashMap` with foldhash backing.
/// `default()` and `with_capacity_and_hasher` work identically to
/// rustc_hash's `FxHashMap`.
pub type FxMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::FixedState>;

/// Drop-in for `std::collections::HashSet` with foldhash backing.
pub type FxSet<T> = std::collections::HashSet<T, foldhash::fast::FixedState>;
