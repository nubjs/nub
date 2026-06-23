//! Per-package content fingerprints for delta installs.
//!
//! Today's warm path is all-or-nothing. One byte changed in the
//! lockfile, we redo the whole resolve + fetch + link pipeline for
//! every package. Adding one dep to a 500-package monorepo redoes
//! 499 packages of work.
//!
//! Fix. blake3 each package over the fields that actually change
//! what hits disk. name, version, integrity, sorted dependencies,
//! os, cpu, libc, alias_of, local_source. Diff the
//! old and new maps. Emit [`DeltaPlan`] with added, removed,
//! changed.
//!
//! Missing or corrupt fingerprints in the prior state cascade to a
//! full install. Feature is additive, never load-bearing.
//!
//! `LockedPackage` does not derive `Serialize`. Fingerprint feeds
//! raw field bytes into `blake3::Hasher` in a fixed order. Stable
//! across lockfile-writer serde churn.
//!
//! Excluded fields. peer_dependencies and peer_dependencies_meta
//! already folded into dependencies by the resolver. engines is
//! advisory. bin reads at link time from the extracted tarball.
//! bundled_dependencies ride inside the tarball so any change moves
//! the sha512 integrity. tarball_url is usually registry metadata
//! when integrity is present (and may disappear on a lockfile-derived
//! graph when `lockfileIncludeTarballUrl=false`), but it is content
//! identity for no-integrity, hosted-git, and non-derivable registry
//! URLs. remote tarball sources still include their URL through
//! `local_source`. yarn_checksum and deprecated are metadata, not
//! content. has_bin is a flag derived from bin.

use aube_lockfile::{LocalSource, LockedPackage, LockfileGraph};
use blake3::Hasher;
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// 2048-bit wide-add multiset hash over per-package fingerprints.
/// LtHash construction from Maitin-Shepard et al. 2017.
///
/// Each package add is one BLAKE3-XOF prehash to 256 bytes, split
/// into 128 `u16` lanes, lane-wise `wrapping_add`. Remove uses
/// `wrapping_sub` for the exact inverse. O(1) incremental update.
/// Order-independent. "Is this graph equivalent to the last one"
/// becomes a 32-byte compare instead of an O(N) map walk.
///
/// Why wide-add not XOR. XOR folds `h(x) ^ h(x) = 0`. A duplicate
/// package entry would vanish. Wide-add preserves multiset counts
/// and stays invertible under `wrapping_sub`. That is what the
/// delta path needs (add for added, sub for removed, both for
/// changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LtHash(pub [u16; 128]);

impl Default for LtHash {
    fn default() -> Self {
        Self([0u16; 128])
    }
}

impl LtHash {
    /// Fold a package fingerprint into the accumulator. BLAKE3-XOF
    /// prehash keeps us safe on adversarial inputs.
    pub fn add(&mut self, fingerprint_hex: &str) {
        self.mix(fingerprint_hex, u16::wrapping_add);
    }

    /// Exact inverse of [`Self::add`]. Caller is trusted to only
    /// remove packages that were added. A double-remove produces a
    /// valid but meaningless digest the caller catches by comparing
    /// against a from-scratch recompute.
    pub fn remove(&mut self, fingerprint_hex: &str) {
        self.mix(fingerprint_hex, u16::wrapping_sub);
    }

    /// Lane-wise wrapping add of two accumulators. Associative and
    /// commutative, so rayon can fold per-worker then pairwise
    /// combine into the final digest.
    pub fn combine(&mut self, other: &Self) {
        for (lane, rhs) in self.0.iter_mut().zip(other.0.iter()) {
            *lane = lane.wrapping_add(*rhs);
        }
    }

    /// 32-byte digest. BLAKE3 over the raw lane bytes so two
    /// accumulators collide only when every lane matches.
    pub fn digest(&self) -> [u8; 32] {
        let mut bytes = [0u8; 256];
        for (i, lane) in self.0.iter().enumerate() {
            let le = lane.to_le_bytes();
            bytes[2 * i] = le[0];
            bytes[2 * i + 1] = le[1];
        }
        *blake3::hash(&bytes).as_bytes()
    }

    fn mix(&mut self, fingerprint_hex: &str, op: fn(u16, u16) -> u16) {
        let mut xof = Hasher::new()
            .update(fingerprint_hex.as_bytes())
            .finalize_xof();
        let mut buf = [0u8; 256];
        xof.fill(&mut buf);
        for i in 0..128 {
            let lane = u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]);
            self.0[i] = op(self.0[i], lane);
        }
    }
}

/// Build an [`LtHash`] from a fingerprint map. Parallel fold via
/// rayon. Reduction order is free because `combine` commutes.
///
/// Keys (dep_paths) are intentionally dropped from the hash input.
/// `fingerprint()` already feeds `dep_path` into the per-package
/// BLAKE3, so two packages with different dep_paths produce
/// different fingerprint values. Hashing the value alone is
/// sufficient for multiset equivalence.
pub fn lthash_of(fingerprints: &BTreeMap<String, String>) -> LtHash {
    fingerprints
        .par_iter()
        .fold(LtHash::default, |mut acc, (_, fp)| {
            acc.add(fp);
            acc
        })
        .reduce(LtHash::default, |mut a, b| {
            a.combine(&b);
            a
        })
}

/// Outcome of diffing two fingerprint maps.
///
/// `added`, `removed`, `changed` are disjoint. Caller fetches
/// `added ∪ changed` and unlinks `removed`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeltaPlan {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// Pre-built `added ∪ changed` lookup. Built once at construction
    /// so `should_touch` stays O(log N) per probe. The linker walks
    /// the graph 4-5 times; without this cache each probe was a
    /// linear scan over both `Vec`s.
    ///
    /// Named `touch_set` (not `touched`) because `touched()` is the
    /// counter method on the same type and the field/method collision
    /// trips up future readers reaching for `plan.touched`.
    touch_set: BTreeSet<String>,
}

impl DeltaPlan {
    /// Count of packages the caller still has to act on. Zero means
    /// the new graph matches the prior install exactly.
    pub fn touched(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.touched() == 0
    }

    /// Membership test for `added ∪ changed`. Only these need fetch
    /// or re-link. `removed` runs through a separate unlink pass.
    pub fn should_touch(&self, dep_path: &str) -> bool {
        self.touch_set.contains(dep_path)
    }

    /// Borrowed view of the cached `added ∪ changed` set. Stable for
    /// the lifetime of the plan; cheaper than rebuilding per probe.
    pub fn touched_set(&self) -> &BTreeSet<String> {
        &self.touch_set
    }
}

/// blake3 fingerprint for every package in `graph`. Keys mirror
/// `graph.packages` so callers can join directly.
///
/// `patch_hashes` keys are `name@version`. Folding the patch content
/// hash into the fingerprint makes a re-patched package land in the
/// `changed` bucket on the next install. Without this, two installs
/// whose lockfile is identical but whose `pnpm.patchedDependencies`
/// changed would silently skip rebuild of the affected entries.
pub fn compute_package_hashes(
    graph: &LockfileGraph,
    patch_hashes: &BTreeMap<String, String>,
    project_root: &Path,
) -> BTreeMap<String, String> {
    // Pure, per-package, CPU-bound. Perfect rayon target. 500-pkg
    // graph drops from ~25 ms serial to ~6 ms on 4 cores.
    graph
        .packages
        .par_iter()
        .map(|(dep_path, pkg)| {
            let patch_key = format!("{}@{}", pkg.name, pkg.version);
            let patch = patch_hashes.get(&patch_key).map(String::as_str);
            (dep_path.clone(), fingerprint(pkg, patch, project_root))
        })
        .collect()
}

/// Per-package subtree hashes. Each entry rolls its leaf
/// fingerprint with its children's subtree hashes, bottom-up over
/// the dep DAG. Two graphs that share a subtree match for every
/// node in the shared region. Lets a future delta trim the re-link
/// set down to changed subtree roots only.
///
/// Cycles get collapsed by a Tarjan SCC pre-pass. Each SCC hashes
/// its members' sorted leaf fingerprints and feeds ancestors like
/// an acyclic node. Peer-dep cycles happen in aube-resolver's
/// peer_context. Rare but real, so SCC handling is correctness
/// not just robustness.
/// Compute leaf and subtree hashes in one pass. Callers that need
/// both should prefer this over calling each function separately so
/// the BLAKE3 leaf pass runs once. Returns `(leaf, subtree)`.
pub fn compute_leaf_and_subtree_hashes(
    graph: &LockfileGraph,
    patch_hashes: &BTreeMap<String, String>,
    project_root: &Path,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let leaf = compute_package_hashes(graph, patch_hashes, project_root);
    let subtree = compute_subtree_hashes_from_leaf(graph, &leaf);
    (leaf, subtree)
}

/// Variant of [`compute_subtree_hashes`] that reuses an already-built
/// leaf-fingerprint map. The caller wires this when it has both
/// fingerprints and subtree hashes to compute, so the BLAKE3 leaf
/// pass runs once instead of twice.
pub fn compute_subtree_hashes_from_leaf(
    graph: &LockfileGraph,
    leaf: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let sccs = tarjan_scc(graph);
    // dep_path to scc index.
    let mut scc_index: BTreeMap<String, usize> = BTreeMap::new();
    for (idx, members) in sccs.iter().enumerate() {
        for m in members {
            scc_index.insert(m.clone(), idx);
        }
    }
    // Condensation DAG. scc to child sccs, self edges dropped.
    let mut condensed: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); sccs.len()];
    for (dep_path, pkg) in &graph.packages {
        let Some(&from) = scc_index.get(dep_path) else {
            continue;
        };
        for (child_name, child_tail) in &pkg.dependencies {
            let child_dep_path = dependency_dep_path(child_name, child_tail);
            if let Some(&to) = scc_index.get(&child_dep_path)
                && to != from
            {
                condensed[from].insert(to);
            }
        }
    }
    // DFS post-order gives reverse topo over the condensation.
    let mut order = Vec::with_capacity(sccs.len());
    let mut seen = vec![false; sccs.len()];
    for start in 0..sccs.len() {
        dfs_post(start, &condensed, &mut seen, &mut order);
    }
    // Hash SCCs in post-order. Children always done before parents.
    let mut scc_hash: Vec<String> = vec![String::new(); sccs.len()];
    for idx in order {
        let mut h = Hasher::new();
        h.update(b"scc");
        let mut members: Vec<&String> = sccs[idx].iter().collect();
        members.sort();
        h.update(&(members.len() as u64).to_le_bytes());
        for m in members {
            if let Some(leaf_hex) = leaf.get(m) {
                update_field(&mut h, b"leaf", leaf_hex.as_bytes());
            }
        }
        let mut children: Vec<usize> = condensed[idx].iter().copied().collect();
        children.sort_by(|a, b| scc_hash[*a].cmp(&scc_hash[*b]));
        h.update(&(children.len() as u64).to_le_bytes());
        for c in children {
            update_field(&mut h, b"child", scc_hash[c].as_bytes());
        }
        scc_hash[idx] = h.finalize().to_hex().to_string();
    }
    // Expand condensed hashes back to per-package entries. Pure
    // independent clones, rayon wins cleanly on a 500-pkg map.
    scc_index
        .into_par_iter()
        .map(|(dep_path, idx)| (dep_path, scc_hash[idx].clone()))
        .collect()
}

/// Return the current dep_paths whose subtree hash changed since the
/// previous install. This is the minimal invalidation set for
/// per-project `.aube/<dep_path>` directories: if a child changed,
/// every ancestor's internal sibling links must be rebuilt even when
/// the ancestor's own leaf fingerprint is identical.
pub fn changed_subtree_roots(
    stored: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> Vec<String> {
    current
        .iter()
        .filter(|(dep_path, hash)| stored.get(*dep_path) != Some(*hash))
        .map(|(dep_path, _)| dep_path.clone())
        .collect()
}

/// Iterative Tarjan SCC over `graph.packages`. Returns each SCC as
/// a `Vec` of `dep_path` keys. Acyclic nodes show up as one-member
/// SCCs. Iterative because deep peer-suffix chains blew the stack
/// on a recursive port during early aube-resolver work.
fn tarjan_scc(graph: &LockfileGraph) -> Vec<Vec<String>> {
    // Dense indices keep bookkeeping off BTreeMap's String keys.
    let nodes: Vec<&String> = graph.packages.keys().collect();
    let index_of: BTreeMap<&String, usize> =
        nodes.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    let n = nodes.len();
    let mut index = vec![usize::MAX; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut next_index = 0usize;
    // Per-node child iterator. Lets us resume after recursing.
    let mut child_iters: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let pkg = graph.packages.get(nodes[i]).unwrap();
            pkg.dependencies
                .iter()
                .filter_map(|(child_name, child_tail)| {
                    let child_dep_path = dependency_dep_path(child_name, child_tail);
                    index_of.get(&child_dep_path).copied()
                })
                .collect()
        })
        .collect();
    // Reversed once so `pop()` walks children in original order.
    for iter in child_iters.iter_mut() {
        iter.reverse();
    }
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut call_stack: Vec<usize> = vec![start];
        index[start] = next_index;
        lowlink[start] = next_index;
        next_index += 1;
        stack.push(start);
        on_stack[start] = true;
        while let Some(&v) = call_stack.last() {
            if let Some(w) = child_iters[v].pop() {
                if index[w] == usize::MAX {
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call_stack.push(w);
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                if lowlink[v] == index[v] {
                    let mut component = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        component.push(nodes[w].clone());
                        if w == v {
                            break;
                        }
                    }
                    out.push(component);
                }
                call_stack.pop();
                if let Some(&parent) = call_stack.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
            }
        }
    }
    out
}

fn dependency_dep_path(name: &str, tail: &str) -> String {
    format!("{name}@{tail}")
}

fn dfs_post(start: usize, edges: &[BTreeSet<usize>], seen: &mut [bool], order: &mut Vec<usize>) {
    if seen[start] {
        return;
    }
    let mut stack: Vec<(usize, Vec<usize>)> = vec![(start, edges[start].iter().copied().collect())];
    seen[start] = true;
    while let Some((v, mut pending)) = stack.pop() {
        match pending.pop() {
            Some(child) if !seen[child] => {
                seen[child] = true;
                stack.push((v, pending));
                stack.push((child, edges[child].iter().copied().collect()));
            }
            Some(_) => stack.push((v, pending)),
            None => order.push(v),
        }
    }
}

/// Diff `stored` (last install's fingerprints) against `current`
/// (this install's). Emits every package the caller has to fetch,
/// re-link, or unlink.
///
/// Empty `stored` lands every entry in `added`. First install under
/// this feature and old state files both hit that path. Caller
/// falls through to the regular full-install flow.
pub fn diff(stored: &BTreeMap<String, String>, current: &BTreeMap<String, String>) -> DeltaPlan {
    let mut plan = DeltaPlan::default();
    let mut touched = BTreeSet::new();
    for (dep_path, new_hash) in current {
        match stored.get(dep_path) {
            None => {
                touched.insert(dep_path.clone());
                plan.added.push(dep_path.clone());
            }
            Some(old_hash) if old_hash != new_hash => {
                touched.insert(dep_path.clone());
                plan.changed.push(dep_path.clone());
            }
            Some(_) => {}
        }
    }
    for dep_path in stored.keys() {
        if !current.contains_key(dep_path) {
            plan.removed.push(dep_path.clone());
        }
    }
    plan.touch_set = touched;
    plan
}

/// Blake3 over the package fields that decide what lands on disk.
/// Field order matters. Changing it invalidates every stored
/// fingerprint on the next install. That still cascades to a full
/// install, just costs one "already up to date" miss.
fn fingerprint(pkg: &LockedPackage, patch_hash: Option<&str>, project_root: &Path) -> String {
    let mut h = Hasher::new();
    update_field(&mut h, b"name", pkg.name.as_bytes());
    update_field(&mut h, b"version", pkg.version.as_bytes());
    update_field(&mut h, b"dep_path", pkg.dep_path.as_bytes());
    update_optional(&mut h, b"integrity", pkg.integrity.as_deref());
    update_optional(&mut h, b"alias_of", pkg.alias_of.as_deref());
    update_optional(&mut h, b"patch", patch_hash);
    if let Some(tarball_url) = pkg.tarball_url.as_deref()
        && tarball_url_affects_content_identity(pkg, tarball_url)
    {
        update_field(&mut h, b"tarball_url", tarball_url.as_bytes());
    }
    // BTreeMap iteration is canonical. Length-prefix every entry so
    // "a" -> "bc" cannot collide with "ab" -> "c".
    h.update(b"deps");
    h.update(&(pkg.dependencies.len() as u64).to_le_bytes());
    for (k, v) in &pkg.dependencies {
        update_field(&mut h, b"k", k.as_bytes());
        update_field(&mut h, b"v", v.as_bytes());
    }
    update_list(&mut h, b"os", pkg.os.iter().map(String::as_str));
    update_list(&mut h, b"cpu", pkg.cpu.iter().map(String::as_str));
    update_list(&mut h, b"libc", pkg.libc.iter().map(String::as_str));
    // Canonical byte encoding per variant. Derive(Debug) output
    // can silently change when a variant gains a field or a crate
    // version tweaks the format. That would invalidate every
    // stored fingerprint and force a reinstall. Explicit match
    // pins the encoding to each variant tag plus its fields.
    if let Some(src) = &pkg.local_source {
        h.update(b"local_source");
        match src {
            LocalSource::Directory(p) => {
                h.update(b"dir");
                update_field(&mut h, b"path", p.to_string_lossy().as_bytes());
            }
            LocalSource::Tarball(p) => {
                h.update(b"tar");
                update_field(&mut h, b"path", p.to_string_lossy().as_bytes());
            }
            LocalSource::Link(p) => {
                h.update(b"link");
                update_field(&mut h, b"path", p.to_string_lossy().as_bytes());
            }
            LocalSource::Portal(p) => {
                h.update(b"portal");
                update_field(&mut h, b"path", p.to_string_lossy().as_bytes());
            }
            LocalSource::Exec(p) => {
                h.update(b"exec");
                update_field(&mut h, b"path", p.to_string_lossy().as_bytes());
                let script = if p.is_absolute() {
                    p.as_path().to_path_buf()
                } else {
                    project_root.join(p)
                };
                if let Ok(content) = std::fs::read(script) {
                    update_field(&mut h, b"content", &content);
                }
            }
            LocalSource::Git(g) => {
                h.update(b"git");
                update_field(&mut h, b"url", g.url.as_bytes());
                update_optional(&mut h, b"committish", g.committish.as_deref());
                update_field(&mut h, b"resolved", g.resolved.as_bytes());
            }
            LocalSource::RemoteTarball(t) => {
                h.update(b"remote_tarball");
                update_field(&mut h, b"url", t.url.as_bytes());
                update_field(&mut h, b"integrity", t.integrity.as_bytes());
            }
        }
    }
    h.finalize().to_hex().to_string()
}

fn tarball_url_affects_content_identity(pkg: &LockedPackage, tarball_url: &str) -> bool {
    pkg.integrity.is_none()
        || pkg.registry_git_hosted
        || registry_tarball_url_is_not_derivable(pkg.registry_name(), &pkg.version, tarball_url)
}

fn registry_tarball_url_is_not_derivable(name: &str, version: &str, tarball_url: &str) -> bool {
    let basename = name.rsplit('/').next().unwrap_or(name);
    let expected_suffix = format!("/-/{basename}-{version}.tgz");
    let path_only = tarball_url
        .split_once('?')
        .map_or(tarball_url, |(path, _)| path);
    let path_only = path_only
        .split_once('#')
        .map_or(path_only, |(path, _)| path);
    !path_only.ends_with(&expected_suffix)
}

fn update_field(h: &mut Hasher, tag: &[u8], bytes: &[u8]) {
    h.update(tag);
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

fn update_optional(h: &mut Hasher, tag: &[u8], value: Option<&str>) {
    match value {
        Some(s) => update_field(h, tag, s.as_bytes()),
        None => {
            h.update(tag);
            h.update(&u64::MAX.to_le_bytes());
        }
    }
}

fn update_list<'a, I: Iterator<Item = &'a str>>(h: &mut Hasher, tag: &[u8], items: I) {
    let collected: Vec<&str> = items.collect();
    h.update(tag);
    h.update(&(collected.len() as u64).to_le_bytes());
    for item in collected {
        update_field(h, b"i", item.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::LockedPackage;

    fn pkg(name: &str, version: &str) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            integrity: Some(format!("sha512-{name}{version}")),
            ..Default::default()
        }
    }

    fn pkg_with_deps(name: &str, version: &str, deps: &[(&str, &str)]) -> LockedPackage {
        let mut pkg = pkg(name, version);
        for (dep_name, dep_version) in deps {
            pkg.dependencies
                .insert((*dep_name).to_string(), (*dep_version).to_string());
        }
        pkg
    }

    fn graph_of(pkgs: &[LockedPackage]) -> LockfileGraph {
        let mut graph = LockfileGraph::default();
        for p in pkgs {
            graph.packages.insert(p.dep_path.clone(), p.clone());
        }
        graph
    }

    fn fp(p: &LockedPackage) -> String {
        fingerprint(p, None, Path::new(""))
    }

    fn cph(g: &LockfileGraph) -> BTreeMap<String, String> {
        compute_package_hashes(g, &BTreeMap::new(), Path::new(""))
    }

    fn csh(g: &LockfileGraph) -> BTreeMap<String, String> {
        compute_leaf_and_subtree_hashes(g, &BTreeMap::new(), Path::new("")).1
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let p = pkg("react", "18.2.0");
        assert_eq!(fp(&p), fp(&p.clone()));
    }

    #[test]
    fn fingerprint_changes_on_version_bump() {
        assert_ne!(fp(&pkg("react", "18.2.0")), fp(&pkg("react", "18.3.0")));
    }

    #[test]
    fn fingerprint_changes_on_integrity_swap() {
        let mut a = pkg("react", "18.2.0");
        let mut b = a.clone();
        a.integrity = Some("sha512-AAAA".into());
        b.integrity = Some("sha512-BBBB".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn fingerprint_changes_on_dependency_edit() {
        let mut a = pkg("react", "18.2.0");
        let mut b = a.clone();
        a.dependencies
            .insert("loose-envify".into(), "loose-envify@1.4.0".into());
        b.dependencies
            .insert("loose-envify".into(), "loose-envify@1.5.0".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn fingerprint_changes_on_exec_script_edit() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("generate.js");
        std::fs::write(&script, "module.exports = { name: 'before' }").unwrap();
        let rel = script.strip_prefix(temp.path()).unwrap();
        let mut pkg = pkg("generated", "1.0.0");
        pkg.local_source = Some(LocalSource::Exec(rel.to_path_buf()));

        let before = fingerprint(&pkg, None, temp.path());
        std::fs::write(&script, "module.exports = { name: 'after' }").unwrap();
        let after = fingerprint(&pkg, None, temp.path());

        assert_ne!(before, after);
    }

    #[test]
    fn fingerprint_stable_under_dep_insertion_order() {
        let mut a = pkg("react", "18.2.0");
        a.dependencies.insert("a".into(), "1".into());
        a.dependencies.insert("b".into(), "2".into());
        let mut b = pkg("react", "18.2.0");
        b.dependencies.insert("b".into(), "2".into());
        b.dependencies.insert("a".into(), "1".into());
        assert_eq!(fp(&a), fp(&b));
    }

    #[test]
    fn fingerprint_ignores_peer_metadata() {
        // peer_dependencies resolve into `dependencies` before this
        // runs, so the raw peer map shouldn't move the fingerprint.
        let mut a = pkg("react", "18.2.0");
        let b = a.clone();
        a.peer_dependencies.insert("something".into(), "*".into());
        assert_eq!(fp(&a), fp(&b));
    }

    #[test]
    fn length_prefix_prevents_concatenation_collisions() {
        let mut a = pkg("ab", "1");
        let mut b = pkg("a", "b1");
        a.integrity = None;
        b.integrity = None;
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn diff_empty_stored_marks_everything_added() {
        let graph = graph_of(&[pkg("a", "1"), pkg("b", "2")]);
        let current = cph(&graph);
        let plan = diff(&BTreeMap::new(), &current);
        assert_eq!(plan.added.len(), 2);
        assert_eq!(plan.removed.len(), 0);
        assert_eq!(plan.changed.len(), 0);
    }

    #[test]
    fn diff_identical_maps_is_empty() {
        let graph = graph_of(&[pkg("a", "1"), pkg("b", "2")]);
        let current = cph(&graph);
        let plan = diff(&current, &current);
        assert!(plan.is_empty());
    }

    #[test]
    fn diff_detects_added_removed_changed() {
        // `changed` hits when a dep_path is stable across installs
        // but the fingerprint differs. Simulates a republish on the
        // same version (integrity flip) or a peer-resolved edge
        // update that rewrote the dependencies map.
        let mut republished = pkg("keep", "1");
        republished.integrity = Some("sha512-after-republish".into());
        let before = cph(&graph_of(&[
            pkg("keep", "1"),
            pkg("gone", "1"),
            pkg("bump", "1"),
        ]));
        let after = cph(&graph_of(&[republished, pkg("new", "1"), pkg("bump", "2")]));
        let plan = diff(&before, &after);
        // Version bumps show up as distinct dep_paths: bump@1 gone,
        // bump@2 added. Only the stable-path republish lands in
        // `changed`.
        assert_eq!(plan.added, vec!["bump@2".to_string(), "new@1".to_string()]);
        assert_eq!(
            plan.removed,
            vec!["bump@1".to_string(), "gone@1".to_string()]
        );
        assert_eq!(plan.changed, vec!["keep@1".to_string()]);
    }

    #[test]
    fn diff_on_reordered_graph_is_empty() {
        let mut g1 = LockfileGraph::default();
        g1.packages.insert("a@1".into(), pkg("a", "1"));
        g1.packages.insert("b@1".into(), pkg("b", "1"));
        let mut g2 = LockfileGraph::default();
        g2.packages.insert("b@1".into(), pkg("b", "1"));
        g2.packages.insert("a@1".into(), pkg("a", "1"));
        let plan = diff(&cph(&g1), &cph(&g2));
        assert!(plan.is_empty());
    }

    #[test]
    fn changed_subtree_roots_includes_changed_ancestors() {
        let before = csh(&graph_of(&[
            pkg_with_deps("app", "1", &[("dep", "1")]),
            pkg("dep", "1"),
            pkg("peer", "1"),
        ]));
        let after = csh(&graph_of(&[
            pkg_with_deps("app", "1", &[("dep", "2")]),
            pkg("dep", "2"),
            pkg("peer", "1"),
        ]));

        let changed = changed_subtree_roots(&before, &after);

        assert_eq!(changed, vec!["app@1".to_string(), "dep@2".to_string()]);
    }

    #[test]
    fn changed_subtree_roots_uses_dependency_tail_values() {
        let app = pkg_with_deps("app", "1", &[("dep", "1")]);
        let mut before_dep = pkg("dep", "1");
        let mut after_dep = before_dep.clone();
        before_dep.integrity = Some("sha512-before".into());
        after_dep.integrity = Some("sha512-after".into());

        let before = csh(&graph_of(&[app.clone(), before_dep]));
        let after = csh(&graph_of(&[app, after_dep]));
        let changed = changed_subtree_roots(&before, &after);

        assert_eq!(changed, vec!["app@1".to_string(), "dep@1".to_string()]);
    }

    #[test]
    fn patch_hash_change_lands_in_changed_bucket() {
        // Same lockfile, same integrity. Only `pnpm.patchedDependencies`
        // moved. Without folding patch hash into the fingerprint this
        // would silently match prior state and the side-effects cache
        // would skip the rebuild.
        let graph = graph_of(&[pkg("react", "18.2.0")]);
        let mut before_patches = BTreeMap::new();
        before_patches.insert("react@18.2.0".to_string(), "patch-old".to_string());
        let mut after_patches = BTreeMap::new();
        after_patches.insert("react@18.2.0".to_string(), "patch-new".to_string());
        let before = compute_package_hashes(&graph, &before_patches, Path::new(""));
        let after = compute_package_hashes(&graph, &after_patches, Path::new(""));
        let plan = diff(&before, &after);
        assert_eq!(plan.changed, vec!["react@18.2.0".to_string()]);
        assert!(plan.added.is_empty());
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn no_patch_matches_empty_patch_map() {
        // Stable across the rollout: a package with no patch entry
        // hashes the same regardless of whether the patch map exists
        // at all.
        let graph = graph_of(&[pkg("react", "18.2.0")]);
        let empty = BTreeMap::new();
        let unrelated_patch: BTreeMap<String, String> =
            [("other@1".to_string(), "patch-x".to_string())]
                .into_iter()
                .collect();
        assert_eq!(
            compute_package_hashes(&graph, &empty, Path::new("")),
            compute_package_hashes(&graph, &unrelated_patch, Path::new(""))
        );
    }

    #[test]
    fn platform_list_edit_changes_fingerprint() {
        let mut a = pkg("native", "1");
        let mut b = a.clone();
        a.os.push("linux".into());
        b.os.push("darwin".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn tarball_url_edit_changes_fingerprint() {
        let mut a = pkg("a", "1");
        let mut b = a.clone();
        a.tarball_url = Some("https://cdn-a.example/downloads/a-1.tgz".into());
        b.tarball_url = Some("https://cdn-b.example/downloads/a-1.tgz".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn no_integrity_tarball_url_changes_fingerprint() {
        let mut a = pkg("a", "1");
        let mut b = a.clone();
        a.integrity = None;
        b.integrity = None;
        a.tarball_url = Some("https://registry-a.example/a/-/a-1.tgz".into());
        b.tarball_url = Some("https://registry-b.example/a/-/a-1.tgz".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn hosted_git_tarball_url_changes_fingerprint() {
        let mut a = pkg("a", "1");
        let mut b = a.clone();
        a.registry_git_hosted = true;
        b.registry_git_hosted = true;
        a.tarball_url = Some("https://codeload.github.com/org/a/tar.gz/aaa".into());
        b.tarball_url = Some("https://codeload.github.com/org/a/tar.gz/bbb".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn derivable_registry_tarball_url_with_integrity_is_not_identity() {
        let mut a = pkg("a", "1");
        let mut b = a.clone();
        a.tarball_url = Some("https://registry-a.example/a/-/a-1.tgz?cache=a".into());
        b.tarball_url = Some("https://registry-b.example/a/-/a-1.tgz#cache-b".into());
        assert_eq!(fp(&a), fp(&b));
    }

    #[test]
    fn alias_of_edit_changes_fingerprint() {
        let mut a = pkg("h3-v2", "2.0.0");
        let mut b = a.clone();
        a.alias_of = Some("h3".into());
        b.alias_of = Some("h3-beta".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn lthash_empty_is_zero() {
        assert_eq!(LtHash::default().0, [0u16; 128]);
    }

    #[test]
    fn lthash_add_then_remove_is_zero() {
        let mut h = LtHash::default();
        h.add("abc123");
        h.remove("abc123");
        assert_eq!(h.0, [0u16; 128]);
    }

    #[test]
    fn lthash_is_order_invariant() {
        let mut a = LtHash::default();
        a.add("aaa");
        a.add("bbb");
        a.add("ccc");
        let mut b = LtHash::default();
        b.add("ccc");
        b.add("aaa");
        b.add("bbb");
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn lthash_duplicate_package_is_detectable() {
        // Wide-add (not XOR) is the whole point: two copies of the
        // same package must NOT cancel out.
        let mut once = LtHash::default();
        once.add("dup");
        let mut twice = LtHash::default();
        twice.add("dup");
        twice.add("dup");
        assert_ne!(once.digest(), twice.digest());
    }

    #[test]
    fn lthash_of_matches_add_loop() {
        let fps: BTreeMap<String, String> = [
            ("a@1".to_string(), "fp-a".to_string()),
            ("b@1".to_string(), "fp-b".to_string()),
            ("c@1".to_string(), "fp-c".to_string()),
        ]
        .into_iter()
        .collect();
        let mut manual = LtHash::default();
        for fp in fps.values() {
            manual.add(fp);
        }
        assert_eq!(lthash_of(&fps).digest(), manual.digest());
    }

    #[test]
    fn lthash_detects_any_single_change() {
        let before = lthash_of(
            &[("a@1".to_string(), "old".to_string())]
                .into_iter()
                .collect(),
        );
        let after = lthash_of(
            &[("a@1".to_string(), "new".to_string())]
                .into_iter()
                .collect(),
        );
        assert_ne!(before.digest(), after.digest());
    }

    #[test]
    fn subtree_hash_stable_for_identical_graph() {
        let g1 = graph_of(&[pkg("a", "1"), pkg("b", "1")]);
        let g2 = graph_of(&[pkg("b", "1"), pkg("a", "1")]);
        assert_eq!(csh(&g1), csh(&g2));
    }

    #[test]
    fn subtree_hash_propagates_to_ancestor_on_leaf_edit() {
        // parent -> child. Bumping child's integrity must change
        // child's subtree hash AND parent's.
        let mut child = pkg("leaf", "1");
        let mut parent = pkg("root", "1");
        parent.dependencies.insert("leaf".into(), "1".into());
        let g_before = graph_of(&[parent.clone(), child.clone()]);
        child.integrity = Some("sha512-tampered".into());
        let g_after = graph_of(&[parent, child]);
        let h_before = csh(&g_before);
        let h_after = csh(&g_after);
        assert_ne!(h_before["root@1"], h_after["root@1"]);
        assert_ne!(h_before["leaf@1"], h_after["leaf@1"]);
    }

    #[test]
    fn subtree_hash_unchanged_sibling_after_peer_edit() {
        // Sibling trees shouldn't share subtree hashes just because
        // they sit at the same depth. Edits to sibling A must not
        // ripple into sibling B's subtree hash.
        let mut pa = pkg("pa", "1");
        pa.dependencies.insert("leaf-a".into(), "1".into());
        let mut pb = pkg("pb", "1");
        pb.dependencies.insert("leaf-b".into(), "1".into());
        let la = pkg("leaf-a", "1");
        let lb = pkg("leaf-b", "1");
        let g1 = graph_of(&[pa.clone(), pb.clone(), la.clone(), lb.clone()]);
        let mut la2 = la.clone();
        la2.integrity = Some("sha512-changed".into());
        let g2 = graph_of(&[pa, pb, la2, lb]);
        let h1 = csh(&g1);
        let h2 = csh(&g2);
        assert_ne!(h1["pa@1"], h2["pa@1"]);
        assert_eq!(h1["pb@1"], h2["pb@1"]);
        assert_eq!(h1["leaf-b@1"], h2["leaf-b@1"]);
    }

    #[test]
    fn subtree_hash_handles_cycle_via_peer_suffix() {
        // Rare but real: A -> B, B -> A via peer-resolved edge. Both
        // should land in the same SCC and produce a shared subtree
        // hash without infinite recursion.
        let mut a = pkg("a", "1");
        let mut b = pkg("b", "1");
        a.dependencies.insert("b".into(), "1".into());
        b.dependencies.insert("a".into(), "1".into());
        let g = graph_of(&[a, b]);
        let hashes = csh(&g);
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes["a@1"], hashes["b@1"]);
    }
}
